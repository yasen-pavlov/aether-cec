//! Lossless conversion of bridge observations to ordered wire events.

use crate::{BusEvent, BusState, ErrorKind, Hex, Origin, ReplyTo, TxData};
use cec_core::{bridge, rx::RxError};

impl From<bridge::BusEvent> for BusEvent {
    fn from(event: bridge::BusEvent) -> Self {
        match event {
            bridge::BusEvent::Rx {
                seq,
                t_us,
                frame,
                ack,
            } => Self::Rx {
                seq,
                t_us,
                from: frame.from,
                to: frame.to,
                data: Hex(frame.data),
                ack,
            },
            bridge::BusEvent::Tx {
                seq,
                t_us,
                origin,
                frame,
                outcome,
            } => {
                let (origin, id, reply_to) = match origin {
                    bridge::Origin::Host(id) => (Origin::Host, Some(id), None),
                    bridge::Origin::Local { reply_to_seq } => {
                        (Origin::Local, None, Some(ReplyTo { seq: reply_to_seq }))
                    }
                };
                Self::Tx {
                    seq,
                    t_us,
                    id,
                    origin,
                    from: Some(frame.from),
                    to: Some(frame.to),
                    data: Some(TxData::Hex(Hex(frame.data))),
                    status: outcome.status.into(),
                    attempts: if outcome.status == cec_core::frame::TxStatus::Busy {
                        None
                    } else {
                        Some(outcome.attempts)
                    },
                    reason: None,
                    reply_to,
                }
            }
            bridge::BusEvent::Err {
                seq,
                t_us,
                kind,
                from,
                to,
                bits,
            } => {
                let kind = match kind {
                    RxError::StartBit => ErrorKind::StartBit,
                    RxError::BitTiming => ErrorKind::BitTiming,
                    RxError::EomMissing => ErrorKind::EomMissing,
                    RxError::LineLow => ErrorKind::LineLow,
                };
                Self::Err {
                    seq,
                    t_us,
                    kind,
                    from,
                    to,
                    bits: Some(bits),
                    dropped: None,
                }
            }
            bridge::BusEvent::Line {
                seq,
                t_us,
                stuck_low,
            } => Self::Line {
                seq,
                t_us,
                bus: Some(if stuck_low {
                    BusState::StuckLow
                } else {
                    BusState::Ok
                }),
                hpd: None,
                la: None,
            },
            bridge::BusEvent::LaAllocated { seq, t_us, la } => Self::Line {
                seq,
                t_us,
                bus: None,
                hpd: None,
                la: Some(la),
            },
        }
    }
}
