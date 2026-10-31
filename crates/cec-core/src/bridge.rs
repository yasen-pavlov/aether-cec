//! Host submission validation, local identity replies and logical-address allocation.
use crate::{
    BROADCAST, Frame, TxOutcome, TxStatus,
    rx::{AckSet, RxError, RxEvent},
    timing,
    tx::TxRequest,
};
use heapless::{Deque, String};

/// Opaque host request identifier (at most 16 UTF-8 bytes).
pub type RequestId = String<16>;
/// CEC device class and physical-address report type byte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum DeviceType {
    /// Television.
    #[default]
    Tv = 0,
    /// Recorder.
    Recorder = 1,
    /// Tuner.
    Tuner = 3,
    /// Playback device.
    Playback = 4,
    /// Audio system.
    Audio = 5,
}
impl DeviceType {
    /// Allocation candidates, in CEC priority order.
    pub const fn candidates(self) -> &'static [u8] {
        match self {
            Self::Tv => &[0, 14],
            Self::Playback => &[4, 8, 11],
            Self::Audio => &[5],
            Self::Recorder => &[1, 2, 9],
            Self::Tuner => &[3, 6, 7, 10],
        }
    }
}
/// Attempt to enable an opcode that the bridge must never answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidReply(pub u8);
/// Set of locally answered opcodes. Active-source opcodes cannot be inserted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplySet([u64; 4]);
impl ReplySet {
    /// Creates an empty set.
    pub const fn empty() -> Self {
        Self([0; 4])
    }
    /// Validates and creates a set of opcodes.
    pub fn new(opcodes: &[u8]) -> Result<Self, InvalidReply> {
        let mut set = Self::empty();
        for &opcode in opcodes {
            set.insert(opcode)?;
        }
        Ok(set)
    }
    /// Adds an opcode, rejecting `82` and `85` unconditionally.
    pub fn insert(&mut self, opcode: u8) -> Result<(), InvalidReply> {
        if matches!(opcode, 0x82 | 0x85) {
            return Err(InvalidReply(opcode));
        }
        self.0[usize::from(opcode / 64)] |= 1 << (opcode % 64);
        Ok(())
    }
    /// Whether an opcode is locally enabled.
    pub const fn contains(self, opcode: u8) -> bool {
        self.0[(opcode / 64) as usize] & (1 << (opcode % 64)) != 0
    }
}
impl Default for ReplySet {
    fn default() -> Self {
        Self::new(&[0x83, 0x8C, 0x8F, 0x9F, 0x46, 0x91]).expect("safe identity opcodes")
    }
}
/// Bridge identity and bench policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Fixed LA, or `None` to allocate.
    pub la: Option<u8>,
    /// Device type for allocation and `84` replies.
    pub device_type: DeviceType,
    /// Physical address in network byte order.
    pub physical_address: u16,
    /// OSD name, at most fourteen bytes.
    pub osd: String<14>,
    /// Vendor ID in wire order.
    pub vendor: [u8; 3],
    /// CEC version byte.
    pub cec_version: u8,
    /// Enabled local identity requests.
    pub replies: ReplySet,
    /// Follower ACK addresses.
    pub ack: AckSet,
    /// Allow bench submissions impersonating another LA.
    pub allow_from: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            la: Some(0),
            device_type: DeviceType::Tv,
            physical_address: 0,
            osd: String::try_from("Aether").expect("bounded name"),
            vendor: [0x01, 0x23, 0x45],
            cec_version: 0x05,
            replies: ReplySet::default(),
            ack: AckSet::new(&[0]),
            allow_from: false,
        }
    }
}
/// Runtime power status, independent of persisted configuration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum PowerStatus {
    /// On.
    #[default]
    On = 0,
    /// Standby.
    Standby = 1,
    /// Transitioning to on.
    ToOn = 2,
    /// Transitioning to standby.
    ToStandby = 3,
}
/// Parsed and bounded host request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    /// Opaque correlation ID.
    pub id: RequestId,
    /// Frame with the effective initiator.
    pub frame: Frame,
    /// Optional NACK retry count.
    pub retries: Option<u8>,
}
/// Rejection reasons. Length errors are detected by the wire parser before constructing `Submission`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// Destination is outside 0–15.
    BadTo,
    /// Initiator override is invalid or forbidden.
    FromNotAllowed,
    /// No registered logical address yet.
    NoLa,
    /// Host queue has sixteen waiting requests.
    QueueFull,
}
/// Source of an actual transmitted frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Correlated host request.
    Host(RequestId),
    /// Firmware-generated frame linked to a bus event.
    Local {
        /// Triggering receive or allocation event sequence.
        reply_to_seq: u32,
    },
}
/// One ordered bridge observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusEvent {
    /// Frame seen on the wire (including our own transmissions).
    Rx {
        /// Per-boot event sequence.
        seq: u32,
        /// Frame start time.
        t_us: u64,
        /// Observed frame.
        frame: Frame,
        /// Final block acknowledgement.
        ack: bool,
    },
    /// Final result for a host or local frame.
    Tx {
        /// Per-boot event sequence.
        seq: u32,
        /// Final attempt start time; see `Bridge::on_tx_done`.
        t_us: u64,
        /// Correlation metadata.
        origin: Origin,
        /// Transmitted frame.
        frame: Frame,
        /// Final attempt classification and count.
        outcome: TxOutcome,
    },
    /// Malformed frame.
    Err {
        /// Per-boot event sequence.
        seq: u32,
        /// Detection time.
        t_us: u64,
        /// Decoder failure.
        kind: RxError,
        /// Decoded initiator, if known.
        from: Option<u8>,
        /// Decoded destination, if known.
        to: Option<u8>,
        /// Number of received data bits.
        bits: u8,
    },
    /// Bus health changed after a sustained low or recovery.
    Line {
        /// Per-boot event sequence.
        seq: u32,
        /// Detection time.
        t_us: u64,
        /// Whether the bus is stuck low.
        stuck_low: bool,
    },
    /// Automatic allocation finished.
    LaAllocated {
        /// Per-boot event sequence.
        seq: u32,
        /// Allocation completion time.
        t_us: u64,
        /// Selected address, or 15 if all candidates are occupied.
        la: u8,
    },
}
#[derive(Debug)]
struct Pending {
    request: TxRequest,
    origin: Origin,
}
/// Policy engine with sixteen host slots and sixteen reserved local-reply slots.
#[derive(Debug)]
pub struct Bridge {
    config: Config,
    ack_extras: AckSet,
    power: PowerStatus,
    seq: u32,
    host: Deque<Pending, 16>,
    local: Deque<Pending, 16>,
    active: Option<Pending>,
    candidate: usize,
    allocating: bool,
    allocation_in_flight: bool,
}
impl Bridge {
    /// Creates a bridge. `boot_seq_start` is the last emitted sequence number.
    pub fn new(mut config: Config, boot_seq_start: u32) -> Self {
        let allocating = config.la.is_none();
        let mut ack_extras = config.ack;
        // The default candidate's ACK entry is the implicit own-address entry.
        ack_extras.remove(config.la.unwrap_or(config.device_type.candidates()[0]));
        config.ack = ack_extras;
        if let Some(la) = config.la {
            config.ack.insert(la);
        }
        Self {
            config,
            ack_extras,
            power: PowerStatus::On,
            seq: boot_seq_start,
            host: Deque::new(),
            local: Deque::new(),
            active: None,
            candidate: 0,
            allocating,
            allocation_in_flight: false,
        }
    }
    /// Effective identity; allocation updates `la` and the ACK set.
    pub fn config(&self) -> &Config {
        &self.config
    }
    /// Updates the value returned by Give Device Power Status.
    pub fn set_power(&mut self, p: PowerStatus) {
        self.power = p;
    }
    /// Last emitted event sequence. Wraps modulo `u32` at exhaustion.
    pub fn seq(&self) -> u32 {
        self.seq
    }
    /// Allocates the next sequence for core or executor-generated bus events.
    pub fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }
    /// Host requests waiting to be transmitted (capacity 16).
    pub fn queue_depth(&self) -> usize {
        self.host.len()
    }
    /// Validates and queues a host frame. Length bounds are enforced by its types.
    pub fn submit(&mut self, s: Submission, _now_us: u64) -> Result<(), SubmitError> {
        if s.frame.to > BROADCAST {
            return Err(SubmitError::BadTo);
        }
        let la = self
            .config
            .la
            .filter(|&la| la < BROADCAST)
            .ok_or(SubmitError::NoLa)?;
        if s.frame.from >= BROADCAST || (s.frame.from != la && !self.config.allow_from) {
            return Err(SubmitError::FromNotAllowed);
        }
        let retries = s
            .retries
            .unwrap_or(u8::from(!s.frame.is_poll()))
            .min(timing::MAX_ATTEMPTS - 1);
        self.host
            .push_back(Pending {
                request: TxRequest {
                    frame: s.frame,
                    retries,
                },
                origin: Origin::Host(s.id),
            })
            .map_err(|_| SubmitError::QueueFull)
    }
    fn physical_report(&self, la: u8) -> Frame {
        let [hi, lo] = self.config.physical_address.to_be_bytes();
        Frame::new(
            la,
            BROADCAST,
            &[0x84, hi, lo, self.config.device_type as u8],
        )
        .expect("bounded physical report")
    }
    fn enqueue_local(&mut self, frame: Frame, seq: u32) {
        // Local traffic has separate reserved capacity, never evicting an accepted host request.
        let _ = self.local.push_back(Pending {
            request: TxRequest { frame, retries: 1 },
            origin: Origin::Local { reply_to_seq: seq },
        });
    }
    fn reply(&mut self, frame: &Frame, seq: u32) {
        let Some(la) = self.config.la.filter(|&la| la < BROADCAST) else {
            return;
        };
        if frame.to != la || frame.from == BROADCAST {
            return;
        }
        let Some(opcode) = frame
            .opcode()
            .filter(|&op| self.config.replies.contains(op))
        else {
            return;
        };
        let mut data = heapless::Vec::<u8, 15>::new();
        let to = match opcode {
            0x83 => {
                self.enqueue_local(self.physical_report(la), seq);
                return;
            }
            0x8C => {
                data.push(0x87).expect("bounded reply");
                data.extend_from_slice(&self.config.vendor)
                    .expect("bounded reply");
                BROADCAST
            }
            0x8F => {
                data.extend_from_slice(&[0x90, self.power as u8])
                    .expect("bounded reply");
                frame.from
            }
            0x9F => {
                data.extend_from_slice(&[0x9E, self.config.cec_version])
                    .expect("bounded reply");
                frame.from
            }
            0x46 => {
                data.push(0x47).expect("bounded reply");
                data.extend_from_slice(self.config.osd.as_bytes())
                    .expect("bounded reply");
                frame.from
            }
            0x91 => {
                data.extend_from_slice(&[0x32, b'e', b'n', b'g'])
                    .expect("bounded reply");
                BROADCAST
            }
            _ => return,
        };
        self.enqueue_local(Frame { from: la, to, data }, seq);
    }
    /// Routes a decoder observation, preserving echoes but not answering our own transmissions.
    pub fn on_rx(&mut self, ev: RxEvent, now_us: u64) -> Option<BusEvent> {
        match ev {
            RxEvent::AckDue { .. } => None,
            RxEvent::Line { stuck_low } => Some(BusEvent::Line {
                seq: self.next_seq(),
                t_us: now_us,
                stuck_low,
            }),
            RxEvent::Error {
                kind,
                from,
                to,
                bits,
                ..
            } => Some(BusEvent::Err {
                seq: self.next_seq(),
                t_us: now_us,
                kind,
                from,
                to,
                bits,
            }),
            RxEvent::Frame {
                frame,
                ack,
                start_us,
            } => {
                let seq = self.next_seq();
                let echo = self
                    .active
                    .as_ref()
                    .is_some_and(|pending| pending.request.frame == frame);
                if !echo {
                    self.reply(&frame, seq);
                }
                Some(BusEvent::Rx {
                    seq,
                    t_us: start_us,
                    frame,
                    ack,
                })
            }
        }
    }
    /// Takes the next allocation poll, priority local reply, or host request.
    /// A second call while a request is active returns `None`.
    pub fn next_request(&mut self) -> Option<TxRequest> {
        if self.active.is_some() || self.allocation_in_flight {
            return None;
        }
        if self.allocating {
            let la = self.config.device_type.candidates()[self.candidate];
            self.config.ack = self.ack_extras;
            self.config.ack.remove(la);
            self.allocation_in_flight = true;
            return Some(TxRequest {
                frame: Frame {
                    from: la,
                    to: la,
                    data: heapless::Vec::new(),
                },
                retries: 0,
            });
        }
        self.active = self.local.pop_front().or_else(|| self.host.pop_front());
        self.active.as_ref().map(|pending| pending.request.clone())
    }
    /// Routes a transmit result. Internal allocation polls have no host/local TX event.
    /// The executor supplies `final_attempt_start_us` from `TxAction::Done`;
    /// decoder echoes never override this authoritative timestamp.
    ///
    /// # Panics
    /// Panics if no request was handed out by `next_request`.
    pub fn on_tx_done(&mut self, outcome: TxOutcome, start_us: u64) -> Option<BusEvent> {
        if self.allocation_in_flight {
            self.allocation_in_flight = false;
            let candidates = self.config.device_type.candidates();
            let la = match outcome.status {
                TxStatus::Nack => candidates[self.candidate],
                TxStatus::Ack => {
                    self.candidate += 1;
                    if self.candidate < candidates.len() {
                        return None;
                    }
                    BROADCAST
                }
                _ => return None,
            };
            self.allocating = false;
            self.config.la = Some(la);
            self.config.ack = self.ack_extras;
            self.config.ack.insert(la);
            let seq = self.next_seq();
            self.enqueue_local(self.physical_report(la), seq);
            return Some(BusEvent::LaAllocated {
                seq,
                t_us: start_us,
                la,
            });
        }
        let pending = self
            .active
            .take()
            .expect("completion requires an active request");
        Some(BusEvent::Tx {
            seq: self.next_seq(),
            t_us: start_us,
            origin: pending.origin,
            frame: pending.request.frame,
            outcome,
        })
    }
}
