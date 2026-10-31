//! Phase-2 no-driver bus ingress. The real Bridge owns the one sequence counter.
use aether_proto::{BusEvent, Origin, RawTxRequest, TxStatus};
use cec_core::bridge::Bridge;

/// Validate a request, then explicitly reject valid frames because no LA is in use yet.
///
/// # Panics
/// Only if a compiled-in rejection reason exceeds the protocol's reason bound.
pub fn reject(bridge: &mut Bridge, payload: &[u8], now_us: u64) -> BusEvent {
    let raw = serde_json::from_slice::<RawTxRequest>(payload);
    let (id, reason, from, to, data) = match raw {
        Ok(raw) => {
            let (id, reason, from) = match raw.validate(bridge.config()) {
                Ok(submission) => (
                    Some(submission.id),
                    "no_la".try_into().expect("bounded reason"),
                    Some(submission.frame.from),
                ),
                Err((id, reason)) => (id, reason, None),
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
            "malformed".try_into().expect("bounded reason"),
            None,
            None,
            None,
        ),
    };
    BusEvent::Tx {
        seq: bridge.next_seq(),
        t_us: now_us,
        id,
        origin: Origin::Host,
        from,
        to,
        data,
        status: TxStatus::Rejected,
        attempts: None,
        reason: Some(reason),
        reply_to: None,
    }
}
