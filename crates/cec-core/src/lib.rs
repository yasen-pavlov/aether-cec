//! Pure HDMI-CEC core: edge-level codec, block-level transmit state machine and
//! bridge policy. No I/O, no clocks, no allocation; time is an injected `u64` of
//! microseconds. See `docs/design.md` for the API contract and
//! `docs/protocol.md` §0 for the timing evidence.
#![no_std]

pub mod bridge;
pub mod frame;
pub mod rx;
pub mod timing;
pub mod tx;

pub use bridge::{Bridge, Config, DeviceType, PowerStatus, ReplySet, RequestId};
pub use frame::{BROADCAST, Frame, MAX_DATA, TxOutcome, TxStatus};
pub use rx::{AckSet, Decoder, Edge, Level, RxError, RxEvent};
pub use tx::{Tx, TxAction, TxRequest};
