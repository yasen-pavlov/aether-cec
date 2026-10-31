//! Validated host ingress on the bus owner and ordered rejections.
use aether_proto::{BusEvent, Origin, RawTxRequest, TxStatus};
use cec_core::bridge::{Bridge, SubmitError};

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

/// Retry loss reporting without inventing an event or consuming a sequence on
/// each failed enqueue. The caller keeps the count until this returns true.
pub fn report_overrun(
    bridge: &mut Bridge,
    dropped: u32,
    now_us: u64,
    mut send: impl FnMut(BusEvent) -> bool,
) -> bool {
    if dropped == 0 {
        return true;
    }
    let event = BusEvent::Err {
        seq: bridge.seq().wrapping_add(1),
        t_us: now_us,
        kind: aether_proto::ErrorKind::Overrun,
        from: None,
        to: None,
        bits: None,
        dropped: Some(dropped),
    };
    if send(event) {
        bridge.next_seq();
        true
    } else {
        false
    }
}
