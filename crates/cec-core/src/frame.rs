//! CEC frame as seen by hosts, the codec and the transmit state machine.

use heapless::Vec;

/// Logical address used for broadcast destinations and unregistered initiators.
pub const BROADCAST: u8 = 15;
/// Maximum opcode + operand bytes in one frame (16 blocks minus the header).
pub const MAX_DATA: usize = 15;

/// One CEC frame: header nibbles plus opcode and operands.
///
/// `data` is empty for a polling message. `to == BROADCAST` is a broadcast;
/// `from == BROADCAST` is an unregistered initiator.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    /// Initiator logical address (0–15).
    pub from: u8,
    /// Destination logical address (0–15).
    pub to: u8,
    /// Opcode followed by operands; empty for a poll.
    pub data: Vec<u8, MAX_DATA>,
}

impl Frame {
    /// Builds a frame, returning `None` if an address exceeds 15 or `data` is too long.
    pub fn new(from: u8, to: u8, data: &[u8]) -> Option<Self> {
        if from > BROADCAST || to > BROADCAST {
            return None;
        }
        let data = Vec::from_slice(data).ok()?;
        Some(Self { from, to, data })
    }

    /// Header block: initiator in the high nibble, destination in the low nibble.
    pub fn header(&self) -> u8 {
        (self.from << 4) | self.to
    }

    /// True for a header-only polling message.
    pub fn is_poll(&self) -> bool {
        self.data.is_empty()
    }

    /// True when addressed to every device.
    pub fn is_broadcast(&self) -> bool {
        self.to == BROADCAST
    }

    /// Opcode, if any.
    pub fn opcode(&self) -> Option<u8> {
        self.data.first().copied()
    }

    /// Number of 10-bit blocks on the wire (header + data).
    pub fn blocks(&self) -> usize {
        1 + self.data.len()
    }
}

/// Final result of a transmission attempt sequence (`docs/protocol.md` §5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxStatus {
    /// Last block acknowledged (directed: target pulled low; broadcast: nobody pulled low).
    Ack,
    /// A block was not acknowledged on the final attempt.
    Nack,
    /// Lost arbitration in the header on the final attempt.
    ArbLost,
    /// Expected line level or sample deadline missed on the final attempt.
    LineError,
    /// Bus continuously busy, or the request queue was full.
    Busy,
}

/// Outcome of one transmission request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxOutcome {
    /// Final status.
    pub status: TxStatus,
    /// Number of start bits sent; `1` when the first attempt succeeded.
    pub attempts: u8,
}
