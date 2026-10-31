//! CEC timing in microseconds; accepted receive windows include both endpoints.

/// Nominal duration and inclusive receiver tolerance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// Transmitter duration, in microseconds.
    pub nominal: u32,
    /// Shortest accepted duration.
    pub min: u32,
    /// Longest accepted duration.
    pub max: u32,
}
impl Window {
    /// Whether a measured interval is inside the inclusive window.
    pub const fn contains(self, us: u64) -> bool {
        us >= self.min as u64 && us <= self.max as u64
    }
}
/// Nominal bit period.
pub const BIT: u32 = 2400;
/// Start-bit low interval.
pub const START_LOW: Window = Window {
    nominal: 3700,
    min: 3400,
    max: 4000,
};
/// Start-bit falling edge to first data-bit falling edge.
pub const START_TOTAL: Window = Window {
    nominal: 4500,
    min: 4050,
    max: 4950,
};
/// Logical-one low interval.
pub const DATA_LOW_1: Window = Window {
    nominal: 600,
    min: 300,
    max: 900,
};
/// Logical-zero low interval.
pub const DATA_LOW_0: Window = Window {
    nominal: 1500,
    min: 1200,
    max: 1800,
};
/// Data-bit falling-edge period.
pub const DATA_TOTAL: Window = Window {
    nominal: BIT,
    min: 1950,
    max: 2850,
};
/// Line sampling offset from a bit's falling edge.
pub const SAMPLE_AT: u32 = 1050;
/// Maximum executor lateness when sampling a transmitted bit.
pub const SAMPLE_SLACK: u32 = 200;
/// Continuous low duration before declaring the bus stuck.
pub const STUCK_LOW: u32 = 100_000;
/// Follower ACK low interval.
pub const ACK_LOW: u32 = DATA_LOW_0.nominal;
/// Signal-free duration for a new initiator.
pub const SIGNAL_FREE_NEW: u32 = 5 * BIT;
/// Signal-free duration after our own frame.
pub const SIGNAL_FREE_SAME: u32 = 7 * BIT;
/// Signal-free duration before retrying.
pub const SIGNAL_FREE_RETRY: u32 = 3 * BIT;
/// Maximum number of transmitted start bits per request.
pub const MAX_ATTEMPTS: u8 = 5;
/// Continuous activity limit before a request is abandoned.
pub const BUSY_GIVE_UP: u32 = 2_000_000;
