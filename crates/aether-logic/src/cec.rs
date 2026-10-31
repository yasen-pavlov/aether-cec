//! Hardware-independent executor primitives. No ESP-IDF dependency.
use cec_core::{
    rx::{Edge, Level},
    timing,
    tx::TxAction,
};
use std::sync::atomic::{AtomicU32, Ordering};

/// Full-width ACK-window guard, inlined into the firmware's IRAM edge ISR.
#[allow(clippy::inline_always)]
#[inline(always)]
pub fn ack_in_window(now_us: u64, fall_at_us: u64) -> bool {
    now_us >= fall_at_us.saturating_sub(u64::from(timing::BIT - timing::DATA_TOTAL.min))
        && now_us <= fall_at_us.saturating_add(u64::from(timing::DATA_TOTAL.max - timing::BIT))
}

/// One low/high RMT symbol at one tick per microsecond.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Symbol {
    /// Asserted portion.
    pub low: u32,
    /// Released portion.
    pub high: u32,
}
/// A complete block, including ACK trailing high; no allocation per block.
#[derive(Clone, Copy, Debug)]
pub struct Block {
    /// At most a start symbol and ten bit symbols.
    pub symbols: [Symbol; 11],
    /// Number of populated symbols.
    pub len: usize,
}
impl Block {
    /// Render the action using the core's timing constants.
    pub fn render(start: bool, byte: u8, eom: bool) -> Self {
        let mut block = Self {
            symbols: [Symbol::default(); 11],
            len: 0,
        };
        if start {
            block.push(timing::START_LOW.nominal, timing::START_TOTAL.nominal);
        }
        for bit in (0..8).rev() {
            block.bit(byte & (1 << bit) != 0);
        }
        block.bit(eom);
        block.bit(true); // Initiator releases ACK after the short low.
        block
    }
    fn push(&mut self, low: u32, total: u32) {
        self.symbols[self.len] = Symbol {
            low,
            high: total - low,
        };
        self.len += 1;
    }
    fn bit(&mut self, one: bool) {
        self.push(
            if one {
                timing::DATA_LOW_1.nominal
            } else {
                timing::DATA_LOW_0.nominal
            },
            timing::BIT,
        );
    }
    /// Whole waveform duration.
    pub fn duration_us(&self) -> u64 {
        self.symbols[..self.len]
            .iter()
            .map(|s| u64::from(s.low + s.high))
            .sum()
    }
    /// A fresh header acquires a released bus; a data block is queued while the
    /// previous ACK may still be held low by a follower. Its waveform starts
    /// only at the previous block's end, not at this queueing instant.
    pub fn can_launch(&self, now_us: u64, block_start_us: u64, level: Level) -> bool {
        if self.len == 11 {
            level == Level::High
        } else {
            Schedule::can_chain(now_us, block_start_us)
        }
    }
}
/// Current block and physical sample schedule derived from TX actions and echoes.
#[derive(Default)]
pub struct Schedule {
    start_us: u64,
    observed: bool,
    active: bool,
    start: bool,
    byte: u8,
}
impl Schedule {
    /// Maximum wait for the ISR timestamp of a block's first falling edge.
    pub const ORIGIN_TIMEOUT_US: u64 = 300;
    /// Chained blocks must be queued before the previous waveform ends.
    pub const CHAIN_MARGIN_US: u64 = 100;

    /// Reject a chained launch at or inside the queueing margin.
    pub fn can_chain(now_us: u64, block_start_us: u64) -> bool {
        now_us < block_start_us.saturating_sub(Self::CHAIN_MARGIN_US)
    }

    /// Record a block; samples have no deadline until its physical origin arrives.
    pub fn action(&mut self, action: TxAction) -> Option<u64> {
        match action {
            TxAction::SendBlock {
                block_start_us,
                start,
                byte,
                ..
            } => {
                self.start_us = block_start_us;
                self.observed = false;
                self.active = true;
                self.start = start;
                self.byte = byte;
                None
            }
            TxAction::SampleAt { at_us } if self.observed => {
                Some(self.start_us.saturating_add(u64::from(at_us)))
            }
            _ => None,
        }
    }
    /// Capture only the first falling edge of this block, using its ISR timestamp.
    pub fn edge(&mut self, edge: Edge) -> Option<u64> {
        if self.active
            && !self.observed
            && edge.level == Level::Low
            && edge.t_us >= self.start_us
            && edge.t_us <= self.start_us.saturating_add(Self::ORIGIN_TIMEOUT_US)
        {
            self.start_us = edge.t_us;
            self.observed = true;
            Some(edge.t_us)
        } else {
            None
        }
    }
    /// Scheduled-origin fallback is used only to fail a missing edge, never sample.
    pub fn origin_deadline(&self) -> Option<u64> {
        (self.active && !self.observed)
            .then_some(self.start_us.saturating_add(Self::ORIGIN_TIMEOUT_US))
    }
    /// Stop classifying decoder errors as our own echo.
    pub fn clear(&mut self) {
        self.active = false;
        self.observed = false;
    }
    /// Whether an initiator waveform is being observed (including origin wait).
    pub fn active(&self) -> bool {
        self.active
    }
    /// At arbitration samples a driven zero must remain low. Ones may lose
    /// arbitration (classified by Tx); the follower owns the ACK sample.
    pub fn driven_low(&self, at_us: u32) -> bool {
        let prefix = if self.start {
            timing::START_TOTAL.nominal
        } else {
            0
        };
        let bit = at_us.saturating_sub(prefix) / timing::BIT;
        bit < 8 && self.byte & (1 << (7 - bit)) == 0
    }
}

#[derive(Default)]
struct Slot {
    low: AtomicU32,
    high: AtomicU32,
    level: AtomicU32,
}
/// Bounded lock-free single-producer/single-consumer edge ring. Atomics are
/// 32-bit on Xtensa; release/acquire publishes the entire 64-bit timestamp.
/// Exactly one context must push and exactly one must pop. Slots are atomic
/// so even accidental misuse never creates a Rust data race.
pub struct EdgeRing<const N: usize> {
    slots: [Slot; N],
    head: AtomicU32,
    tail: AtomicU32,
    lost: AtomicU32,
}
impl<const N: usize> Default for EdgeRing<N> {
    fn default() -> Self {
        assert!(N > 0 && N.is_power_of_two() && u16::try_from(N).is_ok());
        Self {
            slots: std::array::from_fn(|_| Slot::default()),
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
            lost: AtomicU32::new(0),
        }
    }
}
impl<const N: usize> EdgeRing<N> {
    /// ISR producer: drop newest when full, preserving edge order.
    // This body must remain inside the firmware's IRAM ISR, not in flash.
    #[allow(clippy::inline_always)]
    #[inline(always)]
    pub fn push(&self, edge: Edge) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        if head.wrapping_sub(self.tail.load(Ordering::Acquire)) as usize == N {
            self.lost.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let slot = &self.slots[head as usize & (N - 1)];
        let bytes = edge.t_us.to_le_bytes();
        slot.low.store(
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            Ordering::Relaxed,
        );
        slot.high.store(
            u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            Ordering::Relaxed,
        );
        slot.level
            .store(u32::from(edge.level == Level::High), Ordering::Relaxed);
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }
    /// Task consumer.
    pub fn pop(&self) -> Option<Edge> {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail == self.head.load(Ordering::Acquire) {
            return None;
        }
        let slot = &self.slots[tail as usize & (N - 1)];
        let edge = Edge {
            t_us: u64::from(slot.low.load(Ordering::Relaxed))
                | (u64::from(slot.high.load(Ordering::Relaxed)) << 32),
            level: if slot.level.load(Ordering::Relaxed) == 0 {
                Level::Low
            } else {
                Level::High
            },
        };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(edge)
    }
    /// Consumer-side bus-start guard. A concurrent edge can make this stale;
    /// the hardware adapter also checks the physical pin immediately before TX.
    pub fn is_empty(&self) -> bool {
        self.tail.load(Ordering::Relaxed) == self.head.load(Ordering::Acquire)
    }
    /// Atomically collect overrun accounting.
    pub fn take_lost(&self) -> u32 {
        self.lost.swap(0, Ordering::Relaxed)
    }
}

/// Measured bus health; startup low is distinguished from a later stuck line.
pub struct Health {
    boot_us: u64,
    last_edge_us: u64,
    high_since: Option<u64>,
    saw_edge: bool,
    state: Option<aether_proto::BusState>,
}
impl Health {
    /// Sample the physical line immediately after releasing it at boot.
    pub fn new(now_us: u64, level: Level) -> Self {
        Self {
            boot_us: now_us,
            last_edge_us: now_us,
            high_since: (level == Level::High).then_some(now_us),
            saw_edge: false,
            state: None,
        }
    }
    /// Record all physical edges, including own echoes.
    pub fn edge(&mut self, edge: Edge) {
        self.saw_edge = true;
        self.last_edge_us = edge.t_us;
        self.high_since = (edge.level == Level::High).then_some(edge.t_us);
    }
    /// A valid frame establishes a working bus immediately.
    pub fn frame(&mut self) {
        self.state = Some(aether_proto::BusState::Ok);
    }
    /// Decoder line transitions override ordinary idle status.
    pub fn line(&mut self, stuck_low: bool) {
        self.state = Some(if stuck_low {
            aether_proto::BusState::StuckLow
        } else {
            aether_proto::BusState::Ok
        });
    }
    /// Returns only changed health; one second of released idle establishes OK.
    pub fn poll(&mut self, now_us: u64) -> Option<aether_proto::BusState> {
        let next = if self
            .high_since
            .is_some_and(|since| now_us.saturating_sub(since) >= 1_000_000)
        {
            Some(aether_proto::BusState::Ok)
        } else if self.high_since.is_none()
            && now_us.saturating_sub(self.last_edge_us) > u64::from(timing::STUCK_LOW)
        {
            Some(
                if !self.saw_edge
                    && now_us.saturating_sub(self.boot_us) > u64::from(timing::STUCK_LOW)
                {
                    aether_proto::BusState::NoPullup
                } else {
                    aether_proto::BusState::StuckLow
                },
            )
        } else {
            self.state
        };
        if next == self.state {
            None
        } else {
            self.state = next;
            next
        }
    }
    /// Latest measured state.
    pub fn state(&self) -> Option<aether_proto::BusState> {
        self.state
    }
}
