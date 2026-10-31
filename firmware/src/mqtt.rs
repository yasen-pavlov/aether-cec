use crate::{config::Result, secrets::Mqtt};
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
    connects: AtomicU32,
    pending_puback: AtomicU32,
    shutdown_acked: AtomicBool,
}
pub struct MqttLink {
    pub client: EspMqttClient<'static>,
    pub events: Receiver<Message>,
    pub connects: u32,
    dropped: u64,
    ingress_dropped: Arc<AtomicU32>,
    expired: Arc<AtomicU32>,
    control: Arc<Control>,
    pending_overrun: u32,
    enqueue_recovered: bool,
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
            task_stack: 6144,
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
                    assembler.clear();
                    control_cb.connected.store(false, Ordering::Release);
                    None
                }
                EventPayload::Published(id) => {
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
            dropped: 0,
            ingress_dropped,
            expired,
            control,
            pending_overrun: 0,
            enqueue_recovered: false,
        })
    }
    pub fn connected(&self) -> bool {
        self.control.connected.load(Ordering::Acquire)
    }
    pub fn new_connection(&mut self) -> bool {
        let connects = self.control.connects.load(Ordering::Acquire);
        if self.connected() && self.connects != connects {
            self.connects = connects;
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
    pub fn publish(&mut self, topic: &str, qos: QoS, retain: bool, payload: &[u8]) -> Option<u32> {
        match self.client.enqueue(topic, qos, retain, payload) {
            Ok(id) => {
                self.enqueue_recovered = true;
                Some(id)
            }
            Err(_) => {
                self.dropped = self.dropped.saturating_add(1);
                self.pending_overrun = self.pending_overrun.saturating_add(1);
                self.enqueue_recovered = false;
                None
            }
        }
    }
    pub fn json(
        &mut self,
        topic: &str,
        qos: QoS,
        retain: bool,
        value: &impl serde::Serialize,
    ) -> Option<u32> {
        match serde_json::to_vec(value) {
            Ok(bytes) => self.publish(topic, qos, retain, &bytes),
            Err(_) => {
                self.dropped = self.dropped.saturating_add(1);
                self.pending_overrun = self.pending_overrun.saturating_add(1);
                None
            }
        }
    }
    pub fn report_overrun(
        &mut self,
        node: &aether_proto::Node,
        bridge: &mut cec_core::bridge::Bridge,
    ) {
        let expired = self.expired.swap(0, Ordering::Relaxed);
        self.dropped = self.dropped.saturating_add(u64::from(expired));
        self.pending_overrun = self
            .pending_overrun
            .saturating_add(expired)
            .saturating_add(self.ingress_dropped.swap(0, Ordering::Relaxed));
        if self.pending_overrun == 0 || !self.enqueue_recovered {
            return;
        }
        let dropped = self.pending_overrun;
        let event = aether_proto::BusEvent::Err {
            seq: bridge.next_seq(),
            t_us: crate::ffi::now_us(),
            kind: aether_proto::ErrorKind::Overrun,
            from: None,
            to: None,
            bits: None,
            dropped: Some(dropped),
        };
        if self
            .json(&Topics::bus(node), QoS::AtLeastOnce, false, &event)
            .is_some()
        {
            self.pending_overrun -= dropped;
        }
    }
    pub fn drops(&self) -> u64 {
        self.dropped
            .saturating_add(u64::from(self.expired.load(Ordering::Relaxed)))
    }
}
