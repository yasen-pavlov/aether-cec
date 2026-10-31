//! Allocation-free, promiscuous edge decoder and follower acknowledgement policy.
use crate::{BROADCAST, Frame, timing};

/// Electrical line level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Level {
    /// Bus asserted.
    Low,
    /// Bus released.
    #[default]
    High,
}
/// Line level after a transition and its monotonic timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    /// Level after the edge.
    pub level: Level,
    /// Microseconds since boot.
    pub t_us: u64,
}
/// Addresses for which the executor must assert follower ACK.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AckSet(u16);
impl AckSet {
    /// Builds a set; invalid addresses and broadcast are ignored.
    pub fn new(addresses: &[u8]) -> Self {
        let mut set = Self::default();
        for &address in addresses {
            set.insert(address);
        }
        set
    }
    /// Adds a directed logical address; broadcast and invalid values are ignored.
    pub fn insert(&mut self, address: u8) {
        if address < BROADCAST {
            self.0 |= 1 << address;
        }
    }
    /// Removes a directed address.
    pub fn remove(&mut self, address: u8) {
        if address < BROADCAST {
            self.0 &= !(1 << address);
        }
    }
    /// Whether this directed destination is acknowledged.
    pub const fn contains(self, address: u8) -> bool {
        address < BROADCAST && self.0 & (1 << address) != 0
    }
}
/// Reason a partial frame was discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxError {
    /// Start pulse or its period was invalid.
    StartBit,
    /// A data, EOM or ACK pulse or period was invalid.
    BitTiming,
    /// Sixteen blocks arrived without end-of-message.
    EomMissing,
    /// The line stayed asserted beyond the maximum start interval.
    LineLow,
}
/// An observation produced by the decoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RxEvent {
    /// Complete valid frame.
    Frame {
        /// Header and data bytes.
        frame: Frame,
        /// Last block's acknowledgement (inverted for broadcasts).
        ack: bool,
        /// Start-bit falling edge timestamp.
        start_us: u64,
    },
    /// Discarded partial frame.
    Error {
        /// Failure classification.
        kind: RxError,
        /// Start-bit falling edge timestamp.
        start_us: u64,
        /// Initiator nibble, when available.
        from: Option<u8>,
        /// Destination nibble, when available.
        to: Option<u8>,
        /// Number of received data bits, excluding EOM and ACK.
        bits: u8,
    },
    /// Bus health changed after a sustained low or its recovery.
    Line {
        /// Whether the line has remained low past `timing::STUCK_LOW`.
        stuck_low: bool,
    },
    /// Arm the ACK on its actual falling edge and hold low for `timing::ACK_LOW`.
    AckDue {
        /// Predicted falling edge for scheduling only; trigger from the GPIO ISR.
        fall_at_us: u64,
    },
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    Start,
    Data,
}
/// Edge-level receive state. Our own transmissions must also be fed into it.
// Independent electrical, executor, and per-frame flags, not one state enum.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub struct Decoder {
    ack_set: AckSet,
    state: State,
    level: Level,
    last_edge_us: u64,
    quiet_since: u64,
    transmitting: bool,
    suppress_error: bool,
    stuck_low: bool,
    last_ack: bool,
    block_complete: bool,
    fall: u64,
    start: u64,
    frame: Frame,
    byte: u8,
    bit: u8,
    bits: u8,
    blocks: u8,
    eom: bool,
}
impl Decoder {
    /// Creates an idle decoder with the given follower addresses.
    pub fn new(ack_set: AckSet) -> Self {
        Self {
            ack_set,
            ..Self::default()
        }
    }
    /// Changes the follower addresses without interrupting decoding.
    pub fn set_ack_set(&mut self, ack_set: AckSet) {
        self.ack_set = ack_set;
    }
    /// Suppresses follower ACKs while the executor transmits our waveform.
    pub fn set_transmitting(&mut self, transmitting: bool) {
        self.transmitting = transmitting;
    }
    /// True between the start falling edge and the final ACK rising edge.
    pub fn busy(&self) -> bool {
        self.state != State::Idle
    }
    /// Last edge or the nominal end of the final ACK bit, whichever is later.
    pub fn last_activity_us(&self) -> u64 {
        self.quiet_since
    }
    /// Signal-free origin: no quiet time accrues while decoding or held low.
    pub fn quiet_since_us(&self, now_us: u64) -> u64 {
        if self.busy() || self.level == Level::Low {
            now_us
        } else {
            self.quiet_since
        }
    }
    fn error(&mut self, kind: RxError) -> Option<RxEvent> {
        let event = RxEvent::Error {
            kind,
            start_us: self.start,
            from: (self.bits >= 4).then_some(self.frame.from),
            to: (self.bits >= 8).then_some(self.frame.to),
            bits: self.bits,
        };
        self.state = State::Idle;
        let emit = !self.suppress_error;
        self.suppress_error = true;
        emit.then_some(event)
    }
    fn begin(&mut self, now: u64) {
        self.state = State::Start;
        self.start = now;
        self.fall = now;
        self.frame = Frame::default();
        self.byte = 0;
        self.bit = 0;
        self.bits = 0;
        self.blocks = 0;
        self.eom = false;
        self.block_complete = false;
    }
    /// Consumes one edge. Malformed transitions discard the current frame.
    pub fn edge(&mut self, edge: Edge) -> Option<RxEvent> {
        if edge.t_us < self.last_edge_us || edge.level == self.level {
            return if self.busy() {
                self.error(RxError::BitTiming)
            } else {
                None
            };
        }
        self.level = edge.level;
        self.last_edge_us = edge.t_us;
        self.quiet_since = self.quiet_since.max(edge.t_us);
        if self.stuck_low && edge.level == Level::High {
            self.stuck_low = false;
            self.state = State::Idle;
            return Some(RxEvent::Line { stuck_low: false });
        }
        if self.state == State::Idle {
            if edge.level == Level::Low {
                self.begin(edge.t_us);
            }
            return None;
        }
        let duration = edge.t_us.saturating_sub(self.fall);
        if edge.level == Level::Low {
            let (window, error) = if self.state == State::Start {
                (timing::START_TOTAL, RxError::StartBit)
            } else {
                (timing::DATA_TOTAL, RxError::BitTiming)
            };
            if !window.contains(duration) {
                return self.error(error);
            }
            self.state = State::Data;
            self.fall = edge.t_us;
            self.block_complete = false;
            return None;
        }
        if self.state == State::Start {
            if timing::START_LOW.contains(duration) {
                self.suppress_error = false;
                return None;
            }
            if timing::DATA_LOW_1.contains(duration) || timing::DATA_LOW_0.contains(duration) {
                self.state = State::Idle;
                return None;
            }
            return self.error(RxError::StartBit);
        }
        let one = timing::DATA_LOW_1.contains(duration);
        let valid_ack = self.bit == 9
            && duration >= u64::from(timing::DATA_LOW_1.min)
            && duration <= u64::from(timing::DATA_LOW_0.max);
        if !one && !timing::DATA_LOW_0.contains(duration) && !valid_ack {
            return self.error(RxError::BitTiming);
        }
        match self.bit {
            0..=7 => {
                self.byte = (self.byte << 1) | u8::from(one);
                self.bits += 1;
                if self.blocks == 0 {
                    if self.bit == 3 {
                        self.frame.from = self.byte;
                    }
                    if self.bit == 7 {
                        self.frame.to = self.byte & BROADCAST;
                    }
                }
                self.bit += 1;
                None
            }
            8 => {
                self.eom = one;
                self.bit = 9;
                if !self.transmitting && self.ack_set.contains(self.frame.to) {
                    Some(RxEvent::AckDue {
                        fall_at_us: self.fall + u64::from(timing::DATA_TOTAL.nominal),
                    })
                } else {
                    None
                }
            }
            _ => self.finish_block(duration),
        }
    }
    fn finish_block(&mut self, low: u64) -> Option<RxEvent> {
        if self.blocks != 0 && self.frame.data.push(self.byte).is_err() {
            return self.error(RxError::EomMissing);
        }
        self.blocks += 1;
        let sampled_low = low > u64::from(timing::SAMPLE_AT);
        self.last_ack = if self.frame.is_broadcast() {
            !sampled_low
        } else {
            sampled_low
        };
        self.block_complete = true;
        if self.eom {
            return Some(self.complete_frame());
        }
        if usize::from(self.blocks) == crate::MAX_DATA + 1 {
            return self.error(RxError::EomMissing);
        }
        self.bit = 0;
        self.byte = 0;
        None
    }
    fn complete_frame(&mut self) -> RxEvent {
        self.state = State::Idle;
        self.quiet_since = self.fall + u64::from(timing::BIT);
        RxEvent::Frame {
            frame: core::mem::take(&mut self.frame),
            ack: self.last_ack,
            start_us: self.start,
        }
    }
    /// Flushes timeouts. Read `now`, drain captured edges, then call `poll(now)`.
    pub fn poll(&mut self, now_us: u64) -> Option<RxEvent> {
        if self.level == Level::Low {
            if self.busy() && now_us.saturating_sub(self.fall) > u64::from(timing::START_TOTAL.max)
            {
                if let Some(event) = self.error(RxError::LineLow) {
                    return Some(event);
                }
            }
            if !self.stuck_low
                && now_us.saturating_sub(self.last_edge_us) > u64::from(timing::STUCK_LOW)
            {
                self.stuck_low = true;
                return Some(RxEvent::Line { stuck_low: true });
            }
        } else if self.busy() {
            let (limit, error) = if self.state == State::Start {
                (timing::START_TOTAL.max, RxError::StartBit)
            } else {
                (timing::DATA_TOTAL.max, RxError::BitTiming)
            };
            if now_us.saturating_sub(self.fall) > u64::from(limit) {
                if self.block_complete && !self.last_ack && !self.frame.is_broadcast() {
                    return Some(self.complete_frame());
                }
                return self.error(error);
            }
        }
        None
    }
}
