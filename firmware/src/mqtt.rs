use crate::{config::Result, secrets::Mqtt};
use aether_logic::bus::{EventBacklog, Losses};
use aether_proto::{LWT_PAYLOAD, Topics};
use esp_idf_svc::{
    mqtt::client::{
        Details, EspMqttClient, EventPayload, LwtConfiguration, MqttClientConfiguration,
        MqttProtocolVersion, QoS,
    },
    sys,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver},
    },
    time::Duration,
};

// Only data uses the bounded channel. Control state must survive ingress floods.
pub struct Message {
    pub topic: String,
    pub data: Vec<u8>,
}
#[derive(Default)]
struct Control {
    connected: AtomicBool,
    disconnected: AtomicBool,
    connects: AtomicU32,
    pending_puback: AtomicU32,
    shutdown_acked: AtomicBool,
    status_id: AtomicU32,
    status_acked: AtomicBool,
    last_published: AtomicU32,
}
pub struct Publication<'a, T: ?Sized> {
    pub topic: &'a str,
    pub qos: QoS,
    pub retain: bool,
    pub value: &'a T,
}
pub struct MqttLink {
    pub client: EspMqttClient<'static>,
    pub events: Receiver<Message>,
    pub connects: u32,
    pub hub: Arc<crate::events::Hub>,
    dropped: u64,
    ingress_dropped: Arc<AtomicU32>,
    expired: Arc<AtomicU32>,
    control: Arc<Control>,
    pending_overrun: Losses,
    enqueue_recovered: bool,
    bus_generation: u32,
}
impl MqttLink {
    pub fn new(node: &aether_proto::Node, credentials: &Mqtt) -> Result<Self> {
        let (sender, events) = mpsc::sync_channel(32);
        let ingress_dropped = Arc::new(AtomicU32::new(0));
        let lost = ingress_dropped.clone();
        let expired = Arc::new(AtomicU32::new(0));
        let expired_cb = expired.clone();
        let control = Arc::new(Control::default());
        let control_cb = control.clone();
        let client_id = format!("aether-{node}");
        let status = Topics::status(node);
        let url = format!(
            "{}://{}:{}",
            if credentials.tls { "mqtts" } else { "mqtt" },
            credentials.host,
            credentials.port
        );
        let config = MqttClientConfiguration {
            protocol_version: Some(MqttProtocolVersion::V3_1_1),
            client_id: Some(&client_id),
            keep_alive_interval: Some(Duration::from_secs(15)),
            lwt: Some(LwtConfiguration {
                topic: &status,
                payload: LWT_PAYLOAD.as_bytes(),
                qos: QoS::AtLeastOnce,
                retain: true,
            }),
            username: Some(&credentials.user),
            password: Some(&credentials.pass),
            crt_bundle_attach: credentials.tls.then_some(sys::esp_crt_bundle_attach),
            buffer_size: 2048,
            out_buffer_size: 2048,
            outbox_limit: Some(16 * 1024),
            // 6144 is esp-mqtt's default; TLS with the certificate bundle needs
            // more during (re)connect, and a stack overflow here is a `panic` reset.
            task_stack: 10240,
            ..Default::default()
        };
        let mut assembler = aether_logic::ingress::Assembler::default();
        let client = EspMqttClient::new_cb(&url, &config, move |event| {
            let message = match event.payload() {
                EventPayload::Connected(_) => {
                    control_cb.connects.fetch_add(1, Ordering::Release);
                    control_cb.connected.store(true, Ordering::Release);
                    None
                }
                EventPayload::Disconnected => {
                    control_cb.disconnected.store(true, Ordering::Release);
                    assembler.clear();
                    control_cb.connected.store(false, Ordering::Release);
                    control_cb.status_acked.store(false, Ordering::Release);
                    None
                }
                EventPayload::Published(id) => {
                    control_cb.last_published.store(id, Ordering::Release);
                    if control_cb.status_id.load(Ordering::Acquire) == id {
                        control_cb.status_acked.store(true, Ordering::Release);
                    }
                    if control_cb.pending_puback.load(Ordering::Acquire) == id {
                        control_cb.shutdown_acked.store(true, Ordering::Release);
                    }
                    None
                }
                EventPayload::Deleted(_) => {
                    expired_cb.fetch_add(1, Ordering::Relaxed);
                    None
                }
                EventPayload::Received {
                    topic,
                    data,
                    details,
                    ..
                } => {
                    let message = match details {
                        Details::Complete => {
                            topic.and_then(|topic| assembler.start(topic, data, data.len()))
                        }
                        Details::InitialChunk(chunk) => topic
                            .and_then(|topic| assembler.start(topic, data, chunk.total_data_size)),
                        Details::SubsequentChunk(chunk) => {
                            assembler.append(data, chunk.current_data_offset, chunk.total_data_size)
                        }
                    };
                    message.map(|message| Message {
                        topic: message.topic,
                        data: message.data,
                    })
                }
                _ => None,
            };
            if let Some(message) = message {
                // Never block esp-mqtt's task while it holds its client mutex.
                if sender.try_send(message).is_err() {
                    lost.fetch_add(1, Ordering::Relaxed);
                }
            }
        })?;
        Ok(Self {
            client,
            events,
            connects: 0,
            hub: Arc::new(crate::events::Hub::default()),
            dropped: 0,
            ingress_dropped,
            expired,
            control,
            pending_overrun: Losses::default(),
            enqueue_recovered: false,
            bus_generation: 0,
        })
    }
    pub fn connected(&self) -> bool {
        self.control.connected.load(Ordering::Acquire)
    }
    pub fn new_connection(&mut self) -> bool {
        if self.control.disconnected.swap(false, Ordering::AcqRel) {
            log::info!("mqtt_disconnected");
        }
        let connects = self.control.connects.load(Ordering::Acquire);
        if self.connected() && self.connects != connects {
            self.connects = connects;
            log::info!("mqtt_connected");
            true
        } else {
            false
        }
    }
    pub fn wait_for_puback(&self, id: u32) {
        self.control.shutdown_acked.store(false, Ordering::Release);
        self.control.pending_puback.store(id, Ordering::Release);
    }
    pub fn shutdown_acked(&self) -> bool {
        self.control.shutdown_acked.load(Ordering::Acquire)
    }
    pub fn watch_status(&self, id: u32) {
        self.control.status_acked.store(false, Ordering::Release);
        self.control.status_id.store(id, Ordering::Release);
        if self.control.last_published.load(Ordering::Acquire) == id {
            self.control.status_acked.store(true, Ordering::Release);
        }
    }
    pub fn status_published(&self) -> bool {
        self.connected() && self.control.status_acked.load(Ordering::Acquire)
    }
    /// Do not release the offline FIFO until both retained messages were queued.
    pub fn bus_preamble(&mut self, status: bool, cfg: bool) {
        if status && cfg {
            self.bus_generation = self.connects;
        }
    }
    pub fn bus_ready(&self) -> bool {
        self.connected()
            && self.bus_generation != 0
            && self.bus_generation == self.control.connects.load(Ordering::Acquire)
    }
    pub fn lose_bus_event(&mut self, event: &aether_proto::BusEvent) {
        let dropped = self.pending_overrun.record(event);
        self.dropped = self.dropped.saturating_add(u64::from(dropped));
    }
    pub fn flush_bus(&mut self, node: &aether_proto::Node, backlog: &mut EventBacklog) {
        backlog.flush(self.bus_ready(), |event, bytes| {
            self.bus_event(node, &event, &bytes);
        });
    }
    pub fn publish(&mut self, message: Publication<'_, [u8]>) -> Option<u32> {
        self.hub.publish(message.topic, message.value);
        self.enqueue(message)
    }
    fn enqueue(&mut self, message: Publication<'_, [u8]>) -> Option<u32> {
        let Publication {
            topic,
            qos,
            retain,
            value: payload,
        } = message;
        match self.client.enqueue(topic, qos, retain, payload) {
            Ok(id) => {
                self.enqueue_recovered = true;
                Some(id)
            }
            Err(_) => {
                self.dropped = self.dropped.saturating_add(1);
                self.pending_overrun.add(1);
                self.enqueue_recovered = false;
                None
            }
        }
    }
    pub fn bus_event(
        &mut self,
        node: &aether_proto::Node,
        event: &aether_proto::BusEvent,
        bytes: &[u8],
    ) {
        let published = self.enqueue(Publication {
            topic: &Topics::bus(node),
            qos: QoS::AtLeastOnce,
            retain: false,
            value: bytes,
        });
        if published.is_none() {
            if let aether_proto::BusEvent::Err {
                kind: aether_proto::ErrorKind::Overrun,
                dropped: Some(dropped),
                ..
            } = event
            {
                // enqueue counted the failed report itself; preserve its losses.
                self.pending_overrun.add(*dropped);
            }
        }
    }
    pub fn json(&mut self, message: Publication<'_, impl serde::Serialize>) -> Option<u32> {
        let Publication {
            topic,
            qos,
            retain,
            value,
        } = message;
        match serde_json::to_vec(value) {
            Ok(bytes) => self.publish(Publication {
                topic,
                qos,
                retain,
                value: &bytes,
            }),
            Err(_) => {
                self.dropped = self.dropped.saturating_add(1);
                self.pending_overrun.add(1);
                None
            }
        }
    }
    pub fn report_overrun(&mut self, bus: &crate::cec::Link) {
        let expired = self.expired.swap(0, Ordering::Relaxed);
        self.dropped = self.dropped.saturating_add(u64::from(expired));
        self.pending_overrun.add(expired);
        self.pending_overrun
            .add(self.ingress_dropped.swap(0, Ordering::Relaxed));
        if self.bus_ready()
            && self.pending_overrun.pending() != 0
            && self.enqueue_recovered
            && bus.ready()
            && bus.command(crate::cec::Command::Overrun(self.pending_overrun.pending()))
        {
            self.pending_overrun.clear();
        }
    }
    pub fn drops(&self) -> u64 {
        self.dropped
            .saturating_add(u64::from(self.expired.load(Ordering::Relaxed)))
    }
}
