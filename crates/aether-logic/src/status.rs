//! Status and telemetry construction shared by firmware and host tests.
use aether_proto::{
    Config, PhysicalAddressConfig, QueueCounters, RxCounters, Status, Telemetry, TxCounters,
};

/// Latest observations copied out of the dedicated bus task.
#[derive(Clone)]
pub struct BusSnapshot {
    /// Effective address, including automatic allocation.
    pub la: Option<u8>,
    /// Measured electrical health.
    pub state: Option<aether_proto::BusState>,
    /// Receiver counters.
    pub rx: RxCounters,
    /// Transmitter counters.
    pub tx: TxCounters,
    /// Queued host requests (excludes the active transmitter).
    pub depth: u8,
    /// No waveform or queued request remains.
    pub idle: bool,
}
impl Default for BusSnapshot {
    fn default() -> Self {
        Self {
            la: None,
            state: None,
            depth: 0,
            idle: true,
            rx: RxCounters {
                frames: 0,
                errors: 0,
                acked_by_us: 0,
            },
            tx: TxCounters {
                host: 0,
                local: 0,
                ack: 0,
                nack: 0,
                arb_lost: 0,
                line_error: 0,
                origin_missing: 0,
                sample_late: 0,
                launch_rejected: 0,
                echo_error: 0,
                edge_overrun: 0,
                driven_low_mismatch: 0,
                busy: 0,
                rejected: 0,
                retransmits: 0,
            },
        }
    }
}
/// Actual runtime observations, sampled by the executor.
pub struct Snapshot<'a> {
    /// Firmware version and short SHA.
    pub fw: &'a str,
    /// Persistent boot counter.
    pub boot: u32,
    /// Monotonic microseconds.
    pub t_us: u64,
    /// Current STA IP.
    pub ip: &'a str,
    /// Current AP signal strength.
    pub rssi: i16,
    /// MQTT connects this boot.
    pub link_count: u32,
    /// Current free heap.
    pub heap_free: u32,
    /// Minimum free heap since boot.
    pub heap_min: u32,
    /// Failed outbound publications.
    pub outbox_dropped: u64,
    /// CEC observations, never read from GPIO on the network task.
    pub bus: BusSnapshot,
}

/// All and only the online status fields from protocol §3.
///
/// # Panics
/// If the executor supplies firmware text over 24 bytes or IP text over 45 bytes.
pub fn online(cfg: &Config, snapshot: &Snapshot<'_>) -> Status {
    Status {
        online: true,
        v: Some(1),
        fw: Some(snapshot.fw.try_into().expect("firmware version <=24 bytes")),
        boot: Some(snapshot.boot),
        t_us: Some(snapshot.t_us),
        la: snapshot.bus.la,
        pa: match cfg.pa {
            PhysicalAddressConfig::Fixed(pa) => Some(pa),
            PhysicalAddressConfig::Edid => None,
        },
        device_type: Some(cfg.device_type),
        ip: Some(snapshot.ip.try_into().expect("IP <=45 bytes")),
        rssi: Some(snapshot.rssi),
        bus: snapshot.bus.state,
        hpd: None,
    }
}

/// Runtime counters sampled without sharing the mutable policy engine.
pub fn telemetry(snapshot: &Snapshot<'_>) -> Telemetry {
    Telemetry {
        t_us: snapshot.t_us,
        boot: snapshot.boot,
        rssi: snapshot.rssi,
        link_count: snapshot.link_count,
        heap_free: snapshot.heap_free,
        heap_min: snapshot.heap_min,
        rx: snapshot.bus.rx.clone(),
        tx: snapshot.bus.tx.clone(),
        queue: QueueCounters {
            depth: snapshot.bus.depth,
            max: 16,
        },
        outbox_dropped: snapshot.outbox_dropped,
    }
}
