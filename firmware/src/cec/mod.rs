//! Dedicated core-1 bus owner. Main only exchanges bounded commands/events.
use crate::{config::Result, ffi};
use aether_logic::{
    bus::Losses,
    cec::{AttemptFailure, Block, Health, Schedule},
    status::BusSnapshot,
};
use aether_proto::{BusEvent, BusState};
use cec_core::{
    bridge::{Bridge, Config, PowerStatus},
    rx::{AckSet, Decoder, Edge, Level, RxEvent},
    tx::{Tx, TxAction},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
    mpsc::{self, Receiver, SyncSender},
};

pub enum Command {
    Submit(Vec<u8>),
    Config { core: Config, listener: bool },
    Power(PowerStatus),
    Overrun(u32),
}
pub struct Link {
    commands: SyncSender<Command>,
    pending: Arc<AtomicU32>,
    pub events: Receiver<BusEvent>,
    pub snapshot: Arc<Mutex<BusSnapshot>>,
}
impl Link {
    pub fn new(cfg: &aether_proto::Config) -> Result<Self> {
        let core = aether_logic::config::core_config(cfg)?;
        let pin = cfg.gpio.cec;
        let listener = cfg.ack.is_empty();
        let (commands, rx) = mpsc::sync_channel(64);
        let (tx, events) = mpsc::sync_channel(128);
        let pending = Arc::new(AtomicU32::new(0));
        let pending_task = pending.clone();
        let snapshot = Arc::new(Mutex::new(BusSnapshot {
            la: core.la,
            ..Default::default()
        }));
        let shared = snapshot.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        // Hardware is created on the caller (core 0): creating the RMT channel from
        // the pinned core-1 thread stalled for a minute on the bench (o-10 trace).
        let hardware = ffi::cec::Hardware::new(pin).map_err(|e| format!("cec_init_failed {e}"))?;
        ffi::cec::spawn_bus(move || match Ok::<_, crate::config::Error>(hardware) {
            Ok(hardware) => {
                let _ = ready_tx.send(None);
                Runner::new(Init {
                    core,
                    listener,
                    commands: rx,
                    events: tx,
                    pending: pending_task,
                    snapshot: shared,
                    hardware,
                })
                .run();
            }
            Err(error) => {
                // Bench lesson (o-4 rolled back blind): a bus init failure must leave
                // MQTT/HTTP running so the error is readable. Drain commands forever.
                let _ = ready_tx.send(Some(error.to_string()));
                shared.lock().unwrap().idle = true;
                for _ in rx {
                    pending_task.fetch_sub(1, Ordering::Release);
                }
            }
        })?;
        match ready_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(None) => {}
            Ok(Some(error)) => log::error!("cec_init_failed {error}"),
            Err(_) => log::error!("cec_init_failed timeout"),
        }
        Ok(Self {
            commands,
            pending,
            events,
            snapshot,
        })
    }
    /// Main is the sole producer. Check before consuming MQTT/HTTP ingress.
    pub fn ready(&self) -> bool {
        self.pending.load(Ordering::Acquire) < 64
    }
    pub fn command(&self, command: Command) -> bool {
        self.pending.fetch_add(1, Ordering::AcqRel);
        if self.commands.try_send(command).is_err() {
            self.pending.fetch_sub(1, Ordering::Release);
            false
        } else {
            true
        }
    }
    pub fn idle(&self) -> bool {
        self.pending.load(Ordering::Acquire) == 0 && self.snapshot.lock().unwrap().idle
    }
    pub fn online(
        &self,
        cfg: &aether_proto::Config,
        snapshot: &aether_logic::status::Snapshot<'_>,
    ) -> aether_proto::Status {
        let mut status = aether_logic::status::online(cfg, snapshot);
        let bus = self.snapshot.lock().unwrap();
        status.la = bus.la;
        status.bus = bus.state;
        status
    }
    pub fn telemetry(
        &self,
        mut snapshot: aether_logic::status::Snapshot<'_>,
    ) -> aether_proto::Telemetry {
        snapshot.bus = self.snapshot.lock().unwrap().clone();
        aether_logic::status::telemetry(&snapshot)
    }
}
struct Init {
    core: Config,
    listener: bool,
    commands: Receiver<Command>,
    events: SyncSender<BusEvent>,
    pending: Arc<AtomicU32>,
    snapshot: Arc<Mutex<BusSnapshot>>,
    hardware: ffi::cec::Hardware,
}
struct Runner {
    bridge: Box<Bridge>,
    decoder: Decoder,
    tx: Tx,
    hardware: ffi::cec::Hardware,
    commands: Receiver<Command>,
    events: SyncSender<BusEvent>,
    pending: Arc<AtomicU32>,
    snapshot: Arc<Mutex<BusSnapshot>>,
    stats: BusSnapshot,
    health: Health,
    listener: bool,
    power: PowerStatus,
    deferred_config: Option<(Config, bool)>,
    schedule: Schedule,
    sample_offset: Option<u32>,
    waveform_end_us: Option<u64>,
    block_duration_us: u64,
    last_ours: bool,
    frame_acked: bool,
    lost: Losses,
    failure: Option<(AttemptFailure, &'static str)>,
    failure_log_at: u64,
}
impl Runner {
    fn new(init: Init) -> Self {
        let bridge = Box::new(Bridge::new(init.core, 0));
        let ack = if init.listener {
            AckSet::default()
        } else {
            bridge.config().ack
        };
        let now = ffi::now_us();
        let level = init.hardware.level();
        let mut decoder = Decoder::new(ack);
        if level == Level::Low {
            decoder.edge(Edge { level, t_us: now });
        }
        Self {
            stats: BusSnapshot {
                la: bridge.config().la,
                ..Default::default()
            },
            bridge,
            decoder,
            tx: Tx::new(),
            hardware: init.hardware,
            commands: init.commands,
            events: init.events,
            pending: init.pending,
            snapshot: init.snapshot,
            health: Health::new(now, level),
            listener: init.listener,
            power: PowerStatus::On,
            deferred_config: None,
            schedule: Schedule::default(),
            sample_offset: None,
            waveform_end_us: None,
            block_duration_us: 0,
            last_ours: false,
            frame_acked: false,
            lost: Losses::default(),
            failure: None,
            failure_log_at: 0,
        }
    }
    fn publish(&mut self, event: BusEvent) {
        if self.events.try_send(event).is_err() {
            self.lost.add(1);
        }
    }
    fn overrun(&mut self) {
        self.lost.report(&mut self.bridge, ffi::now_us(), |event| {
            self.events.try_send(event).is_ok()
        });
    }
    fn failure(&mut self, kind: AttemptFailure, code: &'static str) {
        // A decoder error can follow a launch/sample failure on the same attempt.
        // Keep the first cause; Abort counts and logs it only once.
        self.failure.get_or_insert((kind, code));
    }
    fn sample(&mut self, offset: u32, sample: Edge) {
        let failure = self.schedule.sample_failure(offset, sample);
        if let Some(kind) = failure {
            self.failure(kind, kind.code());
        }
        if failure == Some(AttemptFailure::DrivenLowMismatch) {
            self.tx.line_error(sample.t_us);
        } else {
            self.tx.line_sample(sample.level, sample.t_us);
        }
    }
    fn rx(&mut self, event: RxEvent, now: u64) {
        match &event {
            RxEvent::AckDue { fall_at_us } => {
                if !self.listener {
                    self.hardware.ack(*fall_at_us);
                }
                return;
            }
            RxEvent::Frame { .. } => {
                self.stats.rx.frames += 1;
                if self.frame_acked {
                    self.stats.rx.acked_by_us += 1;
                }
                self.frame_acked = false;
                self.health.frame();
                self.last_ours = self.sample_offset.is_some();
            }
            RxEvent::Error { .. } => {
                self.stats.rx.errors += 1;
                self.frame_acked = false;
                if self.schedule.active() {
                    self.failure(AttemptFailure::EchoError, "echo_error");
                    self.tx.line_error(now);
                    self.schedule.clear();
                }
            }
            RxEvent::Line { stuck_low } => {
                let previous = self.health.state();
                if !*stuck_low || previous != Some(BusState::NoPullup) {
                    self.health.line(*stuck_low);
                }
                if self.health.state() != previous {
                    let seq = self.bridge.next_seq();
                    self.publish(BusEvent::Line {
                        seq,
                        t_us: now,
                        bus: self.health.state(),
                        hpd: None,
                        la: None,
                    });
                }
                return;
            }
        }
        if let Some(event) = self.bridge.on_rx(event, now) {
            self.publish(event.into());
        }
    }
    fn drain(&mut self) {
        let now = ffi::now_us();
        while let Some(edge) = self.hardware.interrupts.edges.pop() {
            if let Some(origin) = self.schedule.edge(edge) {
                self.tx.block_started(origin);
                if let Some(end) = &mut self.waveform_end_us {
                    // SendBlock records a scheduled end; move it by the observed
                    // launch offset just as Tx moves its sample and chaining clock.
                    *end = origin + self.block_duration_us;
                }
            }
            self.health.edge(edge);
            // An ISR ACK count is per block; telemetry counts frames, not blocks.
            self.frame_acked |= self.hardware.ack_count() != 0;
            if let Some(event) = self.decoder.edge(edge) {
                self.rx(event, edge.t_us);
            }
        }
        // Health first distinguishes a boot-low from the decoder's stuck-low.
        if let Some(state) = self.health.poll(now) {
            let seq = self.bridge.next_seq();
            self.publish(BusEvent::Line {
                seq,
                t_us: now,
                bus: Some(state),
                hpd: None,
                la: None,
            });
        }
        while let Some(event) = self.decoder.poll(now) {
            self.rx(event, now);
        }
        let lost = self.hardware.interrupts.edges.take_lost();
        if lost != 0 {
            self.stats.rx.errors += u64::from(lost);
            self.decoder = Decoder::new(if self.listener {
                AckSet::default()
            } else {
                self.bridge.config().ack
            });
            self.decoder.set_transmitting(self.schedule.active());
            if self.schedule.active() {
                self.failure(AttemptFailure::EdgeOverrun, "edge_overrun");
                self.tx.line_error(now);
                self.schedule.clear();
            }
        }
    }
    fn configure(&mut self, core: Config, listener: bool) {
        self.bridge = Box::new(Bridge::new(core, self.bridge.seq()));
        self.bridge.set_power(self.power);
        self.listener = listener;
        self.decoder.set_ack_set(if listener {
            AckSet::default()
        } else {
            self.bridge.config().ack
        });
        self.stats.la = self.bridge.config().la;
    }
    fn commands(&mut self) {
        // Config is a barrier: do not let later ingress overtake it or discard
        // requests accepted by the old Bridge. Main remains nonblocking.
        if let Some((core, listener)) = self.deferred_config.take() {
            if self.tx.in_progress() || self.bridge.queue_depth() != 0 {
                self.deferred_config = Some((core, listener));
                return;
            }
            self.configure(core, listener);
        }
        for _ in 0..1 {
            let Ok(command) = self.commands.try_recv() else {
                break;
            };
            self.pending.fetch_sub(1, Ordering::Release);
            match command {
                Command::Submit(payload) => {
                    self.stats.tx.host += 1;
                    if let Some(event) =
                        aether_logic::bus::submit(&mut self.bridge, &payload, ffi::now_us())
                    {
                        if matches!(
                            event,
                            BusEvent::Tx {
                                status: aether_proto::TxStatus::Busy,
                                ..
                            }
                        ) {
                            self.stats.tx.busy += 1;
                        } else {
                            self.stats.tx.rejected += 1;
                        }
                        self.publish(event);
                    }
                }
                Command::Power(power) => {
                    self.power = power;
                    self.bridge.set_power(power);
                }
                Command::Overrun(dropped) => self.lost.add(dropped),
                Command::Config { core, listener } => {
                    self.deferred_config = Some((core, listener));
                    break;
                }
            }
        }
    }
    fn run(mut self) -> ! {
        if let Err(error) = self.hardware.arm() {
            log::error!("cec_arm_failed {error}");
        }
        loop {
            self.drain();
            self.commands();
            let now = ffi::now_us();
            if self.hardware.finish() {
                self.decoder.set_transmitting(false);
                self.sample_offset = None;
                self.waveform_end_us = None;
                self.schedule.clear();
            }
            if !self.tx.in_progress() && !self.hardware.ack_active() {
                if let Some(request) = self.bridge.next_request() {
                    self.decoder.set_ack_set(if self.listener {
                        AckSet::default()
                    } else {
                        self.bridge.config().ack
                    });
                    let _ = self.tx.start(
                        request,
                        now,
                        self.decoder.quiet_since_us(now),
                        self.last_ours,
                    );
                }
            }
            while let Some(sample) = self.hardware.interrupts.samples.pop() {
                if let Some(offset) = self.sample_offset {
                    self.sample(offset, sample);
                }
            }
            let step_now = ffi::now_us();
            let action = if let Some(deadline) = self.schedule.origin_deadline() {
                if step_now >= deadline {
                    self.failure(AttemptFailure::OriginMissing, "origin_missing");
                    self.tx.line_error(step_now);
                    self.tx
                        .step(step_now, self.decoder.quiet_since_us(step_now))
                } else {
                    TxAction::Wait { until_us: deadline }
                }
            } else {
                self.tx
                    .step(step_now, self.decoder.quiet_since_us(step_now))
            };
            let deadline = self.schedule.action(action);
            let mut wait = now + 1000;
            match action {
                TxAction::SendBlock {
                    start,
                    byte,
                    eom,
                    block_start_us,
                } => {
                    if start {
                        self.failure = None;
                    }
                    self.decoder.set_transmitting(true);
                    let block = Block::render(start, byte, eom);
                    self.block_duration_us = block.duration_us();
                    self.waveform_end_us = Some(block_start_us + self.block_duration_us);
                    self.sample_offset = None;
                    match self.hardware.send(&block, &block_start_us) {
                        Ok(submitted_us) => self.schedule.submitted(submitted_us),
                        Err(code) => {
                            self.failure(AttemptFailure::LaunchRejected, code);
                            self.tx.line_error(ffi::now_us());
                            self.schedule.clear();
                        }
                    }
                }
                TxAction::SampleAt { at_us } => {
                    self.sample_offset = Some(at_us);
                    let deadline = deadline.expect("sample action has deadline");
                    if !self.hardware.sample(deadline) {
                        self.sample(
                            at_us,
                            Edge {
                                level: self.hardware.level(),
                                t_us: ffi::now_us(),
                            },
                        );
                    }
                }
                TxAction::Abort => {
                    self.hardware.abort();
                    self.decoder.set_transmitting(false);
                    self.sample_offset = None;
                    self.waveform_end_us = None;
                    self.schedule.clear();
                    let (kind, code) = self
                        .failure
                        .take()
                        .unwrap_or((AttemptFailure::SampleLate, "sample_late"));
                    kind.record(&mut self.stats.tx);
                    // Abort/release comes before potentially blocking UART logging.
                    // Counters retain every attempt; logs are capped at one per second.
                    let failed_at = ffi::now_us();
                    if failed_at >= self.failure_log_at {
                        log::info!("tx_attempt_failed {code}");
                        self.failure_log_at = ffi::now_us().saturating_add(1_000_000);
                    }
                }
                TxAction::Done {
                    outcome,
                    final_attempt_start_us,
                } => {
                    self.drain();
                    self.hardware.finish();
                    self.decoder.set_transmitting(false);
                    self.sample_offset = None;
                    self.waveform_end_us = None;
                    self.schedule.clear();
                    self.last_ours = outcome.status == cec_core::TxStatus::Ack
                        || outcome.status == cec_core::TxStatus::Nack;
                    self.stats.tx.retransmits += u64::from(outcome.attempts.saturating_sub(1));
                    match outcome.status {
                        cec_core::TxStatus::Ack => self.stats.tx.ack += 1,
                        cec_core::TxStatus::Nack => self.stats.tx.nack += 1,
                        cec_core::TxStatus::ArbLost | cec_core::TxStatus::LineError => {}
                        cec_core::TxStatus::Busy => self.stats.tx.busy += 1,
                    }
                    if let Some(event) = self.bridge.on_tx_done(outcome, final_attempt_start_us) {
                        if matches!(
                            event,
                            cec_core::bridge::BusEvent::Tx {
                                origin: cec_core::bridge::Origin::Local { .. },
                                ..
                            }
                        ) {
                            self.stats.tx.local += 1;
                        }
                        self.publish(event.into());
                    }
                    self.decoder.set_ack_set(if self.listener {
                        AckSet::default()
                    } else {
                        self.bridge.config().ack
                    });
                }
                TxAction::Wait { until_us } => {
                    if self.tx.in_progress() {
                        wait = until_us;
                    }
                }
            }
            self.overrun();
            self.stats.la = self.bridge.config().la;
            self.stats.state = self.health.state();
            self.stats.depth = self.bridge.queue_depth() as u8;
            self.stats.idle = !self.tx.in_progress()
                && self.stats.depth == 0
                && self.deferred_config.is_none()
                && self.hardware.finished()
                && !self.hardware.ack_active();
            if let Ok(mut shared) = self.snapshot.try_lock() {
                *shared = self.stats.clone();
            }
            // Actions that expose the next action immediately must not sleep:
            // first SampleAt follows SendBlock, chained blocks follow ACK samples.
            if matches!(action, TxAction::Wait { .. }) {
                self.hardware.wait(wait);
            }
        }
    }
}
