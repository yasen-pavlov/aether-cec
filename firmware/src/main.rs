//! Phase-2 executor: provisioning and MQTT, deliberately no GPIO/bus driver.
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
use cec_core::bridge::{Bridge, PowerStatus};
use config::{Result, Store};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop, hal::peripherals::Peripherals, mqtt::client::QoS,
};
use mqtt::MqttLink;
use std::{
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

const FW: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("AETHER_GIT_SHA"));
const _: () = assert!(FW.len() <= 24);

type BootHealth = ota::Health;

fn main() {
    esp_idf_svc::sys::link_patches();
    let mut health = BootHealth::new();
    events::initialize();
    ffi::private_logging();
    // Keep the receiver outside run(): an error must never terminate the console.
    let console = loop {
        match console::spawn() {
            Ok(console) => break console,
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
    loop {
        health.poll(false, None, |_| false);
        match console.recv_timeout(Duration::from_millis(100)) {
            Ok(ConsoleCommand::Show) => println!("mode=console-only fw={FW} startup_error"),
            Ok(ConsoleCommand::FactoryReset) => {
                if ffi::erase_nvs().is_ok() {
                    ffi::restart();
                }
                println!("storage_error");
            }
            Ok(ConsoleCommand::Reboot) => ffi::restart(),
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
    let node = cfg.node.clone(); // MQTT identity changes only on restart.
    let mut bridge = Box::new(Bridge::new(aether_logic::config::core_config(&cfg)?, 0));
    let mut power = PowerStatus::On;
    let peripherals = Peripherals::take()?;
    let mut net = net::Net::new(
        peripherals.modem,
        EspSystemEventLoop::take()?,
        secrets.wifi.as_ref(),
    )?;
    let mut mqtt = if secrets.wifi.is_some() {
        secrets
            .mqtt
            .as_ref()
            .map(|credentials| MqttLink::new(&node, credentials))
            .transpose()?
    } else {
        None
    };
    let hub = std::sync::Arc::new(events::Hub::default());
    if let Some(link) = mqtt.as_mut() {
        link.hub = hub.clone();
    }
    let ip = net.ip();
    let initial_status = status::online(&cfg, &snapshot(&store, &net, mqtt.as_ref(), &ip));
    let http = http::Http::new(http::Init {
        token: &secrets.token,
        cfg: &cfg,
        initial_status,
        boot: store.boot,
        storage: store.ota.clone(),
        hub: hub.clone(),
    })?;
    let mut telemetry_at = Instant::now();
    let mut status_retry_at = Instant::now();
    let mut snapshot_at = Instant::now();
    let mut hint_at = Instant::now() - Duration::from_secs(30);
    let mut shutdown: Option<Instant> = None;
    loop {
        net.poll();
        if (secrets.wifi.is_none() || secrets.mqtt.is_none())
            && hint_at.elapsed() >= Duration::from_secs(30)
        {
            log::info!("console-only: provision with set wifi / set mqtt, then reboot");
            hint_at = Instant::now();
        }
        if let Ok(command) = console.try_recv() {
            match command {
                ConsoleCommand::Wifi { ssid, pass } => {
                    secrets.wifi = Some(secrets::Wifi { ssid, pass });
                    report(secrets.save(&store));
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
                    report(secrets.save(&store));
                }
                ConsoleCommand::Node(node) => {
                    let mut candidate = cfg.clone();
                    candidate.node = node.as_str().try_into().expect("validated node");
                    match store.save_config(&candidate) {
                        Ok(()) => {
                            cfg = candidate;
                            println!("saved; reboot to apply");
                        }
                        Err(_) => println!("storage_error"),
                    }
                }
                ConsoleCommand::ShowToken => println!("{}", secrets.token),
                ConsoleCommand::Token(token) => {
                    let previous = std::mem::replace(&mut secrets.token, token);
                    if secrets.save_token(&store).is_ok() {
                        *http.shared.token.lock().unwrap() = secrets.token.clone();
                        println!("saved");
                    } else {
                        secrets.token = previous;
                        println!("storage_error");
                    }
                }
                ConsoleCommand::Show => {
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
                ConsoleCommand::Reboot => shutdown = Some(begin_shutdown(mqtt.as_mut(), &node)),
                ConsoleCommand::FactoryReset => {
                    // Erase the entire default partition, including system/node/secrets.
                    if ffi::erase_nvs().is_ok() {
                        shutdown = Some(begin_shutdown(mqtt.as_mut(), &node));
                    } else {
                        println!("storage_error");
                    }
                }
            }
        }
        if let Some(link) = mqtt.as_mut() {
            if link.new_connection() {
                let ip = net.ip();
                let published = link.json(
                    &Topics::status(&node),
                    QoS::AtLeastOnce,
                    true,
                    &status::online(&cfg, &snapshot(&store, &net, Some(link), &ip)),
                );
                if let Some(id) = published {
                    link.watch_status(id);
                }
                status_retry_at = Instant::now();
                link.json(&Topics::cfg(&node), QoS::AtLeastOnce, true, &cfg);
                let tx = link.client.subscribe(&Topics::tx(&node), QoS::AtLeastOnce);
                let cmd = link
                    .client
                    .subscribe(&format!("aether/{node}/cmd/+"), QoS::AtLeastOnce);
                if tx.is_err() || cmd.is_err() {
                    log::warn!("subscription_failed_reconnect");
                    ffi::reconnect_mqtt(&link.client);
                }
                telemetry_at = Instant::now();
            }
            if link.connected()
                && !link.status_published()
                && status_retry_at.elapsed() >= Duration::from_secs(5)
            {
                let ip = net.ip();
                if let Some(id) = link.json(
                    &Topics::status(&node),
                    QoS::AtLeastOnce,
                    true,
                    &status::online(&cfg, &snapshot(&store, &net, Some(link), &ip)),
                ) {
                    link.watch_status(id);
                }
                status_retry_at = Instant::now();
            }
            // Bound each turn so console/reconnect/telemetry are never starved.
            for _ in 0..32 {
                let Ok(mqtt::Message { topic, data }) = link.events.try_recv() else {
                    break;
                };
                if shutdown.is_some() {
                    continue;
                }
                if topic == Topics::tx(&node).as_str() {
                    let event = aether_logic::bus::reject(&mut bridge, &data, ffi::now_us());
                    link.json(&Topics::bus(&node), QoS::AtLeastOnce, false, &event);
                } else if let Some(verb) = topic
                    .strip_prefix(&format!("aether/{node}/cmd/"))
                    .and_then(commands::verb)
                {
                    let mut plan = commands::plan(verb, &data, &cfg, ffi::now_us());
                    apply_plan(&mut plan, &store, &mut cfg, &mut bridge, &mut power);
                    if plan.result.ok && matches!(plan.effect, Effect::Config) {
                        telemetry_at = Instant::now();
                        link.json(&Topics::cfg(&node), QoS::AtLeastOnce, true, &cfg);
                    }
                    link.json(
                        &Topics::result(&node),
                        QoS::AtLeastOnce,
                        false,
                        &plan.result,
                    );
                    if plan.result.ok && matches!(plan.effect, Effect::Restart | Effect::Reset) {
                        shutdown = Some(begin_shutdown(Some(link), &node));
                    }
                }
            }
            link.report_overrun(&node, &mut bridge);
            if shutdown.is_none()
                && link.connected()
                && cfg.telemetry_s != 0
                && telemetry_at.elapsed() >= Duration::from_secs(u64::from(cfg.telemetry_s))
            {
                let ip = net.ip();
                link.json(
                    &Topics::telemetry(&node),
                    QoS::AtMostOnce,
                    false,
                    &status::telemetry(&snapshot(&store, &net, Some(link), &ip)),
                );
                telemetry_at = Instant::now();
            }
        }
        if let Some(link) = mqtt.as_mut() {
            health.poll(link.status_published(), Some(&store), |event| {
                link.json(&Topics::ota(&node), QoS::AtLeastOnce, false, event)
                    .is_some()
            });
        } else {
            health.poll(false, Some(&store), |_| false);
        }
        // Both HTTP and MQTT execute on this owner, preserving command ordering
        // and persistence-before-acknowledgement semantics.
        for _ in 0..4 {
            let Ok(operation) = http.operations.try_recv() else {
                break;
            };
            if shutdown.is_some() || http.shared.busy.load(std::sync::atomic::Ordering::Acquire) {
                let _ = operation
                    .reply
                    .try_send((409, br#"{"ok":false,"reason":"busy"}"#.to_vec()));
                continue;
            }
            let (code, bytes) = if let Some(verb) = operation.verb {
                let mut plan = commands::plan(verb, &operation.data, &cfg, ffi::now_us());
                apply_plan(&mut plan, &store, &mut cfg, &mut bridge, &mut power);
                if plan.result.ok && matches!(plan.effect, Effect::Config) {
                    telemetry_at = Instant::now();
                    *http.shared.cfg.lock().unwrap() = cfg.clone();
                    if let Some(link) = mqtt.as_mut() {
                        link.json(&Topics::cfg(&node), QoS::AtLeastOnce, true, &cfg);
                    }
                }
                if let Some(link) = mqtt.as_mut() {
                    link.json(
                        &Topics::result(&node),
                        QoS::AtLeastOnce,
                        false,
                        &plan.result,
                    );
                }
                if plan.result.ok && matches!(plan.effect, Effect::Restart | Effect::Reset) {
                    shutdown = Some(begin_shutdown(mqtt.as_mut(), &node));
                }
                let code = if plan.result.ok {
                    200
                } else {
                    aether_logic::http::error_status(
                        plan.result
                            .reason
                            .as_ref()
                            .map_or("malformed", |r| r.as_str()),
                    )
                };
                (code, serde_json::to_vec(&plan.result)?)
            } else {
                let event = aether_logic::bus::reject(&mut bridge, &operation.data, ffi::now_us());
                let bytes = serde_json::to_vec(&event)?;
                if let Some(link) = mqtt.as_mut() {
                    link.publish(&Topics::bus(&node), QoS::AtLeastOnce, false, &bytes);
                } else {
                    hub.publish("bus", &bytes);
                }
                (202, b"{}".to_vec())
            };
            // Main's shutdown deadline survives a disconnected or timed-out waiter.
            let _ = operation.reply.try_send((code, bytes));
        }
        for progress in http.progress.try_iter() {
            let bytes = serde_json::to_vec(&progress)?;
            if let Some(link) = mqtt.as_mut() {
                link.publish(&Topics::ota(&node), QoS::AtLeastOnce, false, &bytes);
            } else {
                hub.publish("ota", &bytes);
            }
        }
        if snapshot_at.elapsed() >= Duration::from_secs(1) {
            let ip = net.ip();
            let snap = snapshot(&store, &net, mqtt.as_ref(), &ip);
            http.snapshot(&status::online(&cfg, &snap), &cfg);
            snapshot_at = Instant::now();
        }
        if http
            .shared
            .reboot
            .lock()
            .unwrap()
            .is_some_and(|at| at.elapsed() >= Duration::from_millis(500))
        {
            // Phase 2 has no physical transmitter; phase 3 must also gate this on TX idle.
            ffi::restart();
        }
        if shutdown.is_some_and(|since| {
            aether_logic::http::shutdown_due(
                since.elapsed().as_millis(),
                mqtt.as_ref().is_some_and(MqttLink::shutdown_acked),
            )
        }) {
            ffi::restart();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn snapshot<'a>(
    store: &Store,
    net: &net::Net,
    link: Option<&MqttLink>,
    ip: &'a str,
) -> Snapshot<'a> {
    let (heap_free, heap_min) = ffi::heap();
    Snapshot {
        fw: FW,
        boot: store.boot,
        t_us: ffi::now_us(),
        ip,
        rssi: net.rssi(),
        link_count: link.map_or(0, |link| link.connects),
        heap_free,
        heap_min,
        outbox_dropped: link.map_or(0, MqttLink::drops),
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
        if let Some(id) = link.publish(
            &Topics::status(node),
            QoS::AtLeastOnce,
            true,
            aether_proto::LWT_PAYLOAD.as_bytes(),
        ) {
            // If an unusually fast PUBACK preceded registration, the 2 s bound still reboots.
            link.wait_for_puback(id);
        }
    }
    Instant::now()
}

fn apply_plan(
    plan: &mut commands::Plan,
    store: &Store,
    cfg: &mut aether_proto::Config,
    bridge: &mut Box<Bridge>,
    power: &mut PowerStatus,
) {
    match plan.effect {
        Effect::Config => {
            let candidate = plan.config.take().expect("cfg plan contains candidate");
            match aether_logic::config::core_config(&candidate) {
                Ok(core) => {
                    if store.save_config(&candidate).is_ok() {
                        *bridge = Box::new(Bridge::new(core, bridge.seq()));
                        bridge.set_power(*power);
                        *cfg = candidate;
                    } else {
                        commands::fail(&mut plan.result, "storage_error");
                    }
                }
                Err(reason) => commands::fail(&mut plan.result, reason),
            }
        }
        Effect::Power(state) => {
            *power = match state {
                aether_proto::PowerStatus::On => PowerStatus::On,
                aether_proto::PowerStatus::Standby => PowerStatus::Standby,
                aether_proto::PowerStatus::ToOn => PowerStatus::ToOn,
                aether_proto::PowerStatus::ToStandby => PowerStatus::ToStandby,
            };
            bridge.set_power(*power);
        }
        Effect::Reset => {
            if store
                .save_config(plan.config.as_ref().expect("reset candidate"))
                .is_err()
            {
                commands::fail(&mut plan.result, "storage_error");
            }
        }
        Effect::None | Effect::Restart => {}
    }
}
