//! Validated host ingress on the bus owner and ordered rejections.
use aether_proto::{BusEvent, Origin, RawTxRequest, TxStatus};
use cec_core::bridge::{Bridge, SubmitError};

/// Main-owned, bounded FIFO for events waiting for MQTT's status/cfg preamble.
/// Drop newest on overflow; callers account for the returned event with Losses.
pub struct EventBacklog {
    events: std::collections::VecDeque<(BusEvent, Vec<u8>)>,
}
impl Default for EventBacklog {
    fn default() -> Self {
        Self {
            events: std::collections::VecDeque::with_capacity(Self::CAPACITY),
        }
    }
}
impl EventBacklog {
    /// At 10 frames/s this retains over three seconds of startup traffic.
    pub const CAPACITY: usize = 32;

    /// Move the already-serialized SSE payload into the FIFO without copying or
    /// serializing again on reconnect. Preserve the event for loss accounting.
    pub fn push(&mut self, event: BusEvent, bytes: Vec<u8>) -> Option<BusEvent> {
        if self.events.len() == Self::CAPACITY {
            Some(event)
        } else {
            self.events.push_back((event, bytes));
            None
        }
    }

    /// Only flush after the current MQTT connection's status and cfg are queued.
    /// The consumer owns MQTT outbox-failure accounting, just as for live events.
    pub fn flush(&mut self, ready: bool, mut send: impl FnMut(BusEvent, Vec<u8>)) {
        if ready {
            while let Some((event, bytes)) = self.events.pop_front() {
                send(event, bytes);
            }
        }
    }
}

/// Submit valid input. Return an immediate ordered event only on rejection or
/// queue saturation. Accepted requests finish asynchronously through Bridge.
///
/// # Panics
/// Only if a compiled-in reason exceeds the protocol bound.
pub fn submit(bridge: &mut Bridge, payload: &[u8], now_us: u64) -> Option<BusEvent> {
    let raw = serde_json::from_slice::<RawTxRequest>(payload);
    let mut status = TxStatus::Rejected;
    let (id, reason, from, to, data) = match raw {
        Ok(raw) => {
            let (id, reason, from) = match raw.validate(bridge.config()) {
                Ok(submission) => {
                    let id = submission.id.clone();
                    let from = Some(submission.frame.from);
                    match bridge.submit(submission, now_us) {
                        Ok(()) => return None,
                        Err(error) => {
                            let reason = match error {
                                SubmitError::QueueFull => {
                                    status = TxStatus::Busy;
                                    None
                                }
                                SubmitError::BadTo => Some("bad_to"),
                                SubmitError::FromNotAllowed => Some("from_not_allowed"),
                                SubmitError::NoLa => Some("no_la"),
                            };
                            (
                                Some(id),
                                reason.map(|r| r.try_into().expect("bounded reason")),
                                from,
                            )
                        }
                    }
                }
                Err((id, reason)) => (id, Some(reason), None),
            };
            (
                id,
                reason,
                from,
                u8::try_from(raw.to).ok().filter(|&to| to <= 15),
                raw.rejected_data(),
            )
        }
        Err(_) => (
            None,
            Some("malformed".try_into().expect("bounded reason")),
            None,
            None,
            None,
        ),
    };
    Some(BusEvent::Tx {
        seq: bridge.next_seq(),
        t_us: now_us,
        id,
        origin: Origin::Host,
        from,
        to,
        data,
        status,
        attempts: None,
        reason,
        reply_to: None,
    })
}

/// Pending event losses, coalesced until an ordered report can be delivered.
#[derive(Default)]
pub struct Losses(u32);
impl Losses {
    /// Add real losses; retrying a report does not itself lose an event.
    pub fn add(&mut self, dropped: u32) {
        self.0 = self.0.saturating_add(dropped);
    }
    /// A discarded report represents its original losses, not one new loss.
    /// This prevents disconnected consumers from feeding reports back forever.
    pub fn record(&mut self, event: &BusEvent) -> u32 {
        let dropped = match event {
            BusEvent::Err {
                kind: aether_proto::ErrorKind::Overrun,
                dropped: Some(dropped),
                ..
            } => *dropped,
            _ => 1,
        };
        self.add(dropped);
        dropped
    }
    /// Count to transfer to the bus owner.
    pub fn pending(&self) -> u32 {
        self.0
    }
    /// Clear only after the report or transfer was accepted.
    pub fn clear(&mut self) {
        self.0 = 0;
    }
    /// Retry without consuming a sequence or changing the count on failure.
    pub fn report(
        &mut self,
        bridge: &mut Bridge,
        now_us: u64,
        mut send: impl FnMut(BusEvent) -> bool,
    ) -> bool {
        if self.0 == 0 {
            return true;
        }
        let event = BusEvent::Err {
            seq: bridge.seq().wrapping_add(1),
            t_us: now_us,
            kind: aether_proto::ErrorKind::Overrun,
            from: None,
            to: None,
            bits: None,
            dropped: Some(self.0),
            level: None,
            same_level: None,
            dur_us: None,
            gap_us: None,
            bit: None,
            hist: None,
            pin: None,
            pin_high_age_us: None,
            ack_age_us: None,
            ack_held_us: None,
        };
        if send(event) {
            bridge.next_seq();
            self.clear();
            true
        } else {
            false
        }
    }
}
