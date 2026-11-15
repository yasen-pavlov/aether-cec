//! Network/config owner; the dedicated CEC task owns the physical bus.
mod bootstage;
mod cec;
mod config;
mod console;
mod events;
mod ffi;
mod http;
mod mqtt;
mod net;
mod ota;
mod secrets;

use aether_logic::{
    commands::{self, Effect},
    console::ConsoleCommand,
    status::{self, Snapshot},
};
use aether_proto::Topics;
use cec_core::bridge::PowerStatus;
use config::{Result, Store};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop, hal::peripherals::Peripherals, mqtt::client::QoS,
};
use mqtt::{MqttLink, Publication};
use std::{
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

const FW: &str = env!("AETHER_FW");
const _: () = assert!(FW.len() <= 24);

type BootHealth = ota::Health;

fn main() {
    esp_idf_svc::sys::link_patches();
    let mut health = BootHealth::new();
    events::initialize();
    ffi::private_logging();
    bootstage::begin();
    let _ = ffi::last_crash();
    // Keep the receiver outside run(): an error must never terminate the console.
    let console = loop {
        match console::spawn() {
            Ok(console) => {
                bootstage::mark(bootstage::Stage::Console);
                break console;
            }
            Err(_) => log::error!("console_init_failed"),
        }
        health.poll(false, None, |_| false);
        std::thread::sleep(Duration::from_secs(1));
    };
    if run(&console, &mut health).is_err() {
        // SDK errors can contain provisioning; only emit static scrubbed codes.
        log::error!("startup_failed_console_only");
        recovery(&console, &mut health);
    }
}

fn recovery(console: &Receiver<ConsoleCommand>, health: &mut BootHealth) -> ! {
    let mut stacks_reported = false;
    loop {
        health.poll(false, None, |_| false);
        if !stacks_reported && ffi::now_us() >= 60_000_000 {
            ffi::log_stack_watermarks();
            stacks_reported = true;
        }
        match console.recv_timeout(Duration::from_millis(100)) {
            Ok(ConsoleCommand::Show) => println!("mode=console-only fw={FW} startup_error"),
            Ok(ConsoleCommand::FactoryReset) => {
                log::info!("factory_reset_requested");
                if ffi::erase_nvs().is_ok() {
                    ffi::restart();
                }
                println!("storage_error");
            }
            Ok(ConsoleCommand::Reboot) => {
                log::info!("restart_requested");
                ffi::restart();
            }
            Ok(_) => println!("startup_error: use show / factory-reset / reboot"),
            Err(_) => {}
        }
    }
}

fn run(console: &Receiver<ConsoleCommand>, health: &mut BootHealth) -> Result<()> {
    let store = Store::open()?;
    health.load(&store)?;
    let mut cfg = store.load_config()?;
    let mut secrets = secrets::Secrets::load(&store)?;
    bootstage::mark(bootstage::Stage::Store);
    let node = cfg.node.clone(); // MQTT identity changes only on restart.
    let peripherals = Peripherals::take()?;
    let mut net = net::Net::new(
        peripherals.modem,
        EspSystemEventLoop::take()?,
        secrets.wifi.as_ref(),
    )?;
    bootstage::mark(bootstage::Stage::Net);
    let mut mqtt = if secrets.wifi.is_some() {
        secrets
            .mqtt
            .as_ref()
            .map(|credentials| MqttLink::new(&node, credentials))
            .transpose()?
    } else {
        None
    };
    bootstage::mark(bootstage::Stage::Mqtt);
    let hub = std::sync::Arc::new(events::Hub::default());
    if let Some(link) = mqtt.as_mut() {
        link.hub = hub.clone();
    }
    let ip = net.ip();
    // The bus has not started yet; publish an unmeasured startup snapshot.
    let initial_status = status::online(&cfg, &snapshot(&store, &net, mqtt.as_ref(), &ip));
    let http = http::Http::new(http::Init {
        token: &secrets.token,
        cfg: &cfg,
        initial_status,
        boot: store.boot,
        storage: store.ota.clone(),
        hub: hub.clone(),
    })?;
    bootstage::mark(bootstage::Stage::Http);
    // Start the producer only after network/HTTP constructors return. From here
    // main drains bus events every turn, even without credentials or MQTT.
    let bus = cec::Link::new(&cfg, http.shared.busy.clone())?;
    bootstage::mark(bootstage::Stage::Bus);
    let mut backlog = aether_logic::bus::EventBacklog::default();
    let mut context = MainContext {
        store: &store,
        cfg: &mut cfg,
        secrets: &mut secrets,
        node: &node,
        net: &mut net,
        hub: &hub,
        http: &http,
        bus: &bus,
        backlog: &mut backlog,
        telemetry_at: Instant::now(),
        status_retry_at: Instant::now(),
        snapshot_at: Instant::now(),
        hint_at: Instant::now() - Duration::from_secs(30),
        shutdown: None,
        observed_bus: (None, None),
        stacks_reported: false,
    };
    loop {
        context.housekeeping();
        context.console(console, &mut mqtt);
        if let Some(link) = mqtt.as_mut() {
            context.mqtt_connection(link);
            context.mqtt_retry(link);
            context.mqtt_ingress(link);
        }
        context.telemetry(&mut mqtt)?;
        context.bus_events(&mut mqtt)?;
        context.observe_bus(&mut mqtt)?;
        context.boot_health(health, &mut mqtt);
        context.http_operations(&mut mqtt)?;
        context.ota_progress(&mut mqtt)?;
        context.periodic_snapshot(&mqtt);
        context.shutdown(&mqtt);
        std::thread::sleep(Duration::from_millis(10));
    }
}

// Borrow owners rather than moving them: startup/error-path destruction stays in run.
// MQTT is reborrowed per phase so its link can be passed alongside this context.
struct MainContext<'a> {
    store: &'a Store,
    cfg: &'a mut aether_proto::Config,
    secrets: &'a mut secrets::Secrets,
    node: &'a aether_proto::Node,
    net: &'a mut net::Net,
    hub: &'a events::Hub,
    http: &'a http::Http,
    bus: &'a cec::Link,
    backlog: &'a mut aether_logic::bus::EventBacklog,
    telemetry_at: Instant,
    status_retry_at: Instant,
    snapshot_at: Instant,
    hint_at: Instant,
    shutdown: Option<Instant>,
    observed_bus: (Option<u8>, Option<aether_proto::BusState>),
    stacks_reported: bool,
}

impl MainContext<'_> {
    fn housekeeping(&mut self) {
        let secrets = &mut *self.secrets;
        let net = &mut *self.net;
        bootstage::mark(bootstage::Stage::Loop);
        net.poll();
        if !self.stacks_reported && ffi::now_us() >= 60_000_000 {
            ffi::log_stack_watermarks();
            self.stacks_reported = true;
        }
        if (secrets.wifi.is_none() || secrets.mqtt.is_none())
            && self.hint_at.elapsed() >= Duration::from_secs(30)
        {
            log::info!("console-only: provision with set wifi / set mqtt, then reboot");
            self.hint_at = Instant::now();
        }
    }

    // Keep phase-local aggregates out of the long-lived run frame.
    #[inline(never)]
    fn console(&mut self, console: &Receiver<ConsoleCommand>, mqtt: &mut Option<MqttLink>) {
        let store = self.store;
        let cfg = &mut *self.cfg;
        let secrets = &mut *self.secrets;
        let node = self.node;
        let http = self.http;
        if let Ok(command) = console.try_recv() {
            match command {
                ConsoleCommand::Wifi { ssid, pass } => {
                    secrets.wifi = Some(secrets::Wifi { ssid, pass });
                    report(secrets.save(store));
                }
                ConsoleCommand::Mqtt {
                    host,
                    port,
                    user,
                    pass,
                    tls,
                } => {
                    secrets.mqtt = Some(secrets::Mqtt {
                        host,
                        port,
                        user,
                        pass,
                        tls,
                    });
                    report(secrets.save(store));
                }
                ConsoleCommand::Node(node) => {
                    let mut candidate = cfg.clone();
                    candidate.node = node.as_str().try_into().expect("validated node");
                    match store.save_config(&candidate) {
                        Ok(()) => {
                            *cfg = candidate;
                            log::info!("cfg_change_applied");
                            println!("saved; reboot to apply");
                        }
                        Err(_) => println!("storage_error"),
                    }
                }
                ConsoleCommand::ShowToken => println!("{}", secrets.token),
                ConsoleCommand::Token(token) => {
                    let previous = std::mem::replace(&mut secrets.token, token);
                    if secrets.save_token(store).is_ok() {
                        *http.shared.token.lock().unwrap() = secrets.token.clone();
                        println!("saved");
                    } else {
                        secrets.token = previous;
                        println!("storage_error");
                    }
                }
                ConsoleCommand::Show => {
                    self.show_console(mqtt);
                }
                ConsoleCommand::Reboot => {
                    log::info!("restart_requested");
                    self.shutdown = Some(begin_shutdown(mqtt.as_mut(), node));
                }
                ConsoleCommand::FactoryReset => {
                    log::info!("factory_reset_requested");
                    // Erase the entire default partition, including system/node/secrets.
                    if ffi::erase_nvs().is_ok() {
                        self.shutdown = Some(begin_shutdown(mqtt.as_mut(), node));
                    } else {
                        println!("storage_error");
                    }
                }
            }
        }
    }

    fn show_console(&mut self, mqtt: &Option<MqttLink>) {
        let store = self.store;
        let cfg = &mut *self.cfg;
        let secrets = &mut *self.secrets;
        let net = &mut *self.net;
        println!(
            "http=on ssid={} wifi_pass=<redacted> host={} port={} user={} mqtt_pass=<redacted> tls={} node={} ip={} mqtt_connected={} boot={} fw={}",
            if secrets.wifi.is_some() {
                "<redacted>"
            } else {
                "<unset>"
            },
            if secrets.mqtt.is_some() {
                "<redacted>"
            } else {
                "<unset>"
            },
            secrets.mqtt.as_ref().map_or(0, |m| m.port),
            if secrets.mqtt.is_some() {
                "<redacted>"
            } else {
                "<unset>"
            },
            secrets.mqtt.as_ref().is_none_or(|m| m.tls),
            cfg.node,
            net.ip(),
            mqtt.as_ref().is_some_and(MqttLink::connected),
            store.boot,
            FW
        );
    }

    // Keep phase-local aggregates out of the long-lived run frame.
    #[inline(never)]
    fn mqtt_connection(&mut self, link: &mut MqttLink) {
        let store = self.store;
        let cfg = &mut *self.cfg;
        let node = self.node;
        let net = &mut *self.net;
        let bus = self.bus;
        let backlog = &mut *self.backlog;
        if link.new_connection() {
            let ip = net.ip();
            let published = link.json(Publication {
                topic: &Topics::status(node),
                qos: QoS::AtLeastOnce,
                retain: true,
                value: &bus.online(cfg, &snapshot(store, net, Some(link), &ip)),
            });
            if let Some(id) = published {
                link.watch_status(id);
            }
            self.status_retry_at = Instant::now();
            let configured = link.json(Publication {
                topic: &Topics::cfg(node),
                qos: QoS::AtLeastOnce,
                retain: true,
                value: cfg,
            });
            link.bus_preamble(published.is_some(), configured.is_some());
            link.flush_bus(node, backlog);
            let tx = link.client.subscribe(&Topics::tx(node), QoS::AtLeastOnce);
            let cmd = link
                .client
                .subscribe(&format!("aether/{node}/cmd/+"), QoS::AtLeastOnce);
            if tx.is_err() || cmd.is_err() {
                log::warn!("subscription_failed_reconnect");
                ffi::reconnect_mqtt(&link.client);
            }
            self.telemetry_at = Instant::now();
        }
    }

    // Keep phase-local aggregates out of the long-lived run frame.
    #[inline(never)]
    fn mqtt_retry(&mut self, link: &mut MqttLink) {
        let store = self.store;
        let cfg = &mut *self.cfg;
        let node = self.node;
        let net = &mut *self.net;
        let bus = self.bus;
        let backlog = &mut *self.backlog;
        if link.connected()
            && (!link.status_published() || !link.bus_ready())
            && self.status_retry_at.elapsed() >= Duration::from_secs(5)
        {
            let ip = net.ip();
            let published = link.json(Publication {
                topic: &Topics::status(node),
                qos: QoS::AtLeastOnce,
                retain: true,
                value: &bus.online(cfg, &snapshot(store, net, Some(link), &ip)),
            });
            if let Some(id) = published {
                link.watch_status(id);
            }
            let configured = link.json(Publication {
                topic: &Topics::cfg(node),
                qos: QoS::AtLeastOnce,
                retain: true,
                value: cfg,
            });
            link.bus_preamble(published.is_some(), configured.is_some());
            link.flush_bus(node, backlog);
            self.status_retry_at = Instant::now();
        }
    }

    // Keep phase-local aggregates out of the long-lived run frame.
    #[inline(never)]
    fn mqtt_ingress(&mut self, link: &mut MqttLink) {
        let store = self.store;
        let cfg = &mut *self.cfg;
        let node = self.node;
        let bus = self.bus;
        // Bound each turn so console/reconnect/telemetry are never starved.
        for _ in 0..32 {
            if !bus.ready() {
                break;
            }
            let Ok(mqtt::Message { topic, data }) = link.events.try_recv() else {
                break;
            };
            if self.shutdown.is_some() {
                continue;
            }
            if topic == Topics::tx(node).as_str() {
                bus.command(cec::Command::Submit(data));
            } else if let Some(verb) = topic
                .strip_prefix(&format!("aether/{node}/cmd/"))
                .and_then(commands::verb)
            {
                let mut plan = commands::plan(verb, &data, cfg, ffi::now_us());
                apply_plan(ApplyPlan {
                    plan: &mut plan,
                    store,
                    cfg,
                    bus,
                });
                if plan.result.ok && matches!(plan.effect, Effect::Config) {
                    self.telemetry_at = Instant::now();
                    link.json(Publication {
                        topic: &Topics::cfg(node),
                        qos: QoS::AtLeastOnce,
                        retain: true,
                        value: cfg,
                    });
                }
                link.json(Publication {
                    topic: &Topics::result(node),
                    qos: QoS::AtLeastOnce,
                    retain: false,
                    value: &plan.result,
                });
                if plan.result.ok && matches!(plan.effect, Effect::Restart | Effect::Reset) {
                    self.shutdown = Some(begin_shutdown(Some(link), node));
                }
            }
        }
        link.report_overrun(bus);
    }

    // Keep phase-local aggregates out of the long-lived run frame.
    #[inline(never)]
    fn telemetry(&mut self, mqtt: &mut Option<MqttLink>) -> Result<()> {
        let store = self.store;
        let cfg = &mut *self.cfg;
        let node = self.node;
        let net = &mut *self.net;
        let hub = self.hub;
        let bus = self.bus;
        if self.shutdown.is_none()
            && cfg.telemetry_s != 0
            && self.telemetry_at.elapsed() >= Duration::from_secs(u64::from(cfg.telemetry_s))
        {
            let ip = net.ip();
            let telemetry = bus.telemetry(snapshot(store, net, mqtt.as_ref(), &ip));
            hub.publish("telemetry", &serde_json::to_vec(&telemetry)?);
            if let Some(link) = mqtt.as_mut().filter(|link| link.connected()) {
                link.json(Publication {
                    topic: &Topics::telemetry(node),
                    qos: QoS::AtMostOnce,
                    retain: false,
                    value: &telemetry,
                });
            }
            self.telemetry_at = Instant::now();
        }
        Ok(())
    }

    // Keep phase-local aggregates out of the long-lived run frame.
    #[inline(never)]
    fn bus_events(&mut self, mqtt: &mut Option<MqttLink>) -> Result<()> {
        let node = self.node;
        let hub = self.hub;
        let bus = self.bus;
        let backlog = &mut *self.backlog;
        // SSE sees events immediately. MQTT retains a bounded startup/offline
        // FIFO, flushed only after this connection's retained status and cfg.
        for event in bus.events.try_iter().take(128) {
            let bytes = serde_json::to_vec(&event)?;
            hub.publish("bus", &bytes);
            if let Some(link) = mqtt.as_mut() {
                link.flush_bus(node, backlog);
                if link.bus_ready() {
                    link.bus_event(node, &event, &bytes);
                } else if let Some(event) = backlog.push(event, bytes) {
                    link.lose_bus_event(&event);
                }
            }
        }
        if let Some(link) = mqtt.as_mut() {
            link.flush_bus(node, backlog);
        }
        Ok(())
    }

    // Keep phase-local aggregates out of the long-lived run frame.
    #[inline(never)]
    fn observe_bus(&mut self, mqtt: &mut Option<MqttLink>) -> Result<()> {
        let store = self.store;
        let cfg = &mut *self.cfg;
        let node = self.node;
        let net = &mut *self.net;
        let hub = self.hub;
        let http = self.http;
        let bus = self.bus;
        let measured = {
            let snapshot = bus.snapshot.lock().unwrap();
            (snapshot.la, snapshot.state)
        };
        if measured != self.observed_bus {
            self.observed_bus = measured;
            let ip = net.ip();
            let snap = snapshot(store, net, mqtt.as_ref(), &ip);
            let online = bus.online(cfg, &snap);
            http.snapshot(&online, cfg, &bus.telemetry(snap));
            if let Some(link) = mqtt.as_mut() {
                link.json(Publication {
                    topic: &Topics::status(node),
                    qos: QoS::AtLeastOnce,
                    retain: true,
                    value: &online,
                });
            } else {
                hub.publish("status", &serde_json::to_vec(&online)?);
            }
        }
        Ok(())
    }

    fn boot_health(&mut self, health: &mut BootHealth, mqtt: &mut Option<MqttLink>) {
        let store = self.store;
        let node = self.node;
        if let Some(link) = mqtt.as_mut() {
            health.poll(link.status_published(), Some(store), |event| {
                link.json(Publication {
                    topic: &Topics::ota(node),
                    qos: QoS::AtLeastOnce,
                    retain: false,
                    value: event,
                })
                .is_some()
            });
        } else {
            health.poll(false, Some(store), |_| false);
        }
    }

    // Keep phase-local aggregates out of the long-lived run frame.
    #[inline(never)]
    fn http_operations(&mut self, mqtt: &mut Option<MqttLink>) -> Result<()> {
        let store = self.store;
        let cfg = &mut *self.cfg;
        let node = self.node;
        let http = self.http;
        let bus = self.bus;
        // Both HTTP and MQTT execute on this owner, preserving command ordering
        // and persistence-before-acknowledgement semantics.
        for _ in 0..4 {
            if !bus.ready() {
                break;
            }
            let Ok(operation) = http.operations.try_recv() else {
                break;
            };
            if self.shutdown.is_some()
                || http.shared.busy.load(std::sync::atomic::Ordering::Acquire)
            {
                let _ = operation
                    .reply
                    .try_send((409, br#"{"ok":false,"reason":"busy"}"#.to_vec()));
                continue;
            }
            let (code, bytes) = if let Some(verb) = operation.verb {
                let mut plan = commands::plan(verb, &operation.data, cfg, ffi::now_us());
                apply_plan(ApplyPlan {
                    plan: &mut plan,
                    store,
                    cfg,
                    bus,
                });
                if plan.result.ok && matches!(plan.effect, Effect::Config) {
                    self.telemetry_at = Instant::now();
                    *http.shared.cfg.lock().unwrap() = cfg.clone();
                    publish_http_config(mqtt.as_mut(), node, cfg);
                }
                if let Some(link) = mqtt.as_mut() {
                    link.json(Publication {
                        topic: &Topics::result(node),
                        qos: QoS::AtLeastOnce,
                        retain: false,
                        value: &plan.result,
                    });
                }
                if plan.result.ok && matches!(plan.effect, Effect::Restart | Effect::Reset) {
                    self.shutdown = Some(begin_shutdown(mqtt.as_mut(), node));
                }
                let code = http_result_code(&plan);
                (code, serde_json::to_vec(&plan.result)?)
            } else {
                bus.command(cec::Command::Submit(operation.data));
                (202, b"{}".to_vec())
            };
            // Main's shutdown deadline survives a disconnected or timed-out waiter.
            let _ = operation.reply.try_send((code, bytes));
        }
        Ok(())
    }

    // Keep phase-local aggregates out of the long-lived run frame.
    #[inline(never)]
    fn ota_progress(&mut self, mqtt: &mut Option<MqttLink>) -> Result<()> {
        let node = self.node;
        let hub = self.hub;
        let http = self.http;
        for progress in http.progress.try_iter() {
            let bytes = serde_json::to_vec(&progress)?;
            if let Some(link) = mqtt.as_mut() {
                link.publish(Publication {
                    topic: &Topics::ota(node),
                    qos: QoS::AtLeastOnce,
                    retain: false,
                    value: &bytes,
                });
            } else {
                hub.publish("ota", &bytes);
            }
        }
        Ok(())
    }

    // Keep phase-local aggregates out of the long-lived run frame.
    #[inline(never)]
    fn periodic_snapshot(&mut self, mqtt: &Option<MqttLink>) {
        let store = self.store;
        let cfg = &mut *self.cfg;
        let net = &mut *self.net;
        let http = self.http;
        let bus = self.bus;
        if self.snapshot_at.elapsed() >= Duration::from_secs(1) {
            let ip = net.ip();
            let snap = snapshot(store, net, mqtt.as_ref(), &ip);
            let online = bus.online(cfg, &snap);
            http.snapshot(&online, cfg, &bus.telemetry(snap));
            self.snapshot_at = Instant::now();
        }
    }

    fn shutdown(&mut self, mqtt: &Option<MqttLink>) {
        let http = self.http;
        let bus = self.bus;
        if http
            .shared
            .reboot
            .lock()
            .unwrap()
            .is_some_and(|at| at.elapsed() >= Duration::from_millis(500))
            && bus.upload_idle()
        {
            ffi::restart();
        }
        if self.shutdown.is_some_and(|since| {
            aether_logic::http::shutdown_due(
                since.elapsed().as_millis(),
                mqtt.as_ref().is_some_and(MqttLink::shutdown_acked),
            )
        }) {
            ffi::restart();
        }
    }
}

fn publish_http_config(
    link: Option<&mut MqttLink>,
    node: &aether_proto::Node,
    cfg: &aether_proto::Config,
) {
    if let Some(link) = link {
        link.json(Publication {
            topic: &Topics::cfg(node),
            qos: QoS::AtLeastOnce,
            retain: true,
            value: cfg,
        });
    }
}

fn http_result_code(plan: &commands::Plan) -> u16 {
    if plan.result.ok {
        200
    } else {
        aether_logic::http::error_status(
            plan.result
                .reason
                .as_ref()
                .map_or("malformed", |r| r.as_str()),
        )
    }
}
fn snapshot<'a>(
    store: &Store,
    net: &net::Net,
    link: Option<&MqttLink>,
    ip: &'a str,
) -> Snapshot<'a> {
    let (heap_free, heap_min, heap_largest) = ffi::heap();
    Snapshot {
        fw: FW,
        boot: store.boot,
        reset_reason: ffi::reset_reason(),
        last_crash: ffi::last_crash(),
        last_stage: bootstage::last_stage(),
        t_us: ffi::now_us(),
        ip,
        rssi: net.rssi(),
        link_count: link.map_or(0, |link| link.connects),
        heap_free,
        heap_min,
        heap_largest,
        outbox_dropped: link.map_or(0, MqttLink::drops),
        bus: status::BusSnapshot::default(),
    }
}
fn report(result: Result<()>) {
    println!(
        "{}",
        if result.is_ok() {
            "saved; reboot to apply"
        } else {
            "storage_error"
        }
    );
}
fn begin_shutdown(link: Option<&mut MqttLink>, node: &aether_proto::Node) -> Instant {
    if let Some(link) = link {
        if let Some(id) = link.publish(Publication {
            topic: &Topics::status(node),
            qos: QoS::AtLeastOnce,
            retain: true,
            value: aether_proto::LWT_PAYLOAD.as_bytes(),
        }) {
            // If an unusually fast PUBACK preceded registration, the 2 s bound still reboots.
            link.wait_for_puback(id);
        }
    }
    Instant::now()
}

struct ApplyPlan<'a> {
    plan: &'a mut commands::Plan,
    store: &'a Store,
    cfg: &'a mut aether_proto::Config,
    bus: &'a cec::Link,
}
fn apply_plan(context: ApplyPlan<'_>) {
    let ApplyPlan {
        plan,
        store,
        cfg,
        bus,
    } = context;
    match plan.effect {
        Effect::Config => {
            let candidate = plan.config.take().expect("cfg plan contains candidate");
            match aether_logic::config::core_config(&candidate) {
                Ok(core) => {
                    if !bus.ready() {
                        commands::fail(&mut plan.result, "busy");
                    } else if store.save_config(&candidate).is_ok() {
                        bus.command(cec::Command::Config {
                            core,
                            listener: candidate.ack.is_empty(),
                        });
                        *cfg = candidate;
                        log::info!("cfg_change_applied");
                    } else {
                        commands::fail(&mut plan.result, "storage_error");
                    }
                }
                Err(reason) => commands::fail(&mut plan.result, reason),
            }
        }
        Effect::Power(state) => {
            let power = match state {
                aether_proto::PowerStatus::On => PowerStatus::On,
                aether_proto::PowerStatus::Standby => PowerStatus::Standby,
                aether_proto::PowerStatus::ToOn => PowerStatus::ToOn,
                aether_proto::PowerStatus::ToStandby => PowerStatus::ToStandby,
            };
            bus.command(cec::Command::Power(power));
        }
        Effect::Reset => {
            log::info!("reset_requested");
            if store
                .save_config(plan.config.as_ref().expect("reset candidate"))
                .is_err()
            {
                commands::fail(&mut plan.result, "storage_error");
            }
        }
        Effect::Restart => log::info!("restart_requested"),
        Effect::None => {}
    }
}
