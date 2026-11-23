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
                    attempts: (outcome.status != cec_core::frame::TxStatus::Busy)
                        .then_some(outcome.attempts),
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
                detail,
            } => Self::Err {
                seq,
                t_us,
                kind: error_kind(kind),
                from,
                to,
                bits: Some(bits),
                dropped: None,
                level: detail.level.map(Into::into),
                same_level: Some(detail.same_level),
                dur_us: Some(detail.duration_us),
                gap_us: Some(detail.gap_us),
                bit: detail.bit,
                hist: Some(detail.history),
                pin: None,
                pin_high_age_us: None,
                ack_age_us: None,
                ack_held_us: None,
                tmr_int: None,
                tmr_en: None,
                tmr_alarm_in_us: None,
                tmr_isr_in_us: None,
                tmr_task_in_us: None,
                ack_due_in_us: None,
            },
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

fn error_kind(kind: RxError) -> ErrorKind {
    match kind {
        RxError::StartBit => ErrorKind::StartBit,
        RxError::BitTiming => ErrorKind::BitTiming,
        RxError::EomMissing => ErrorKind::EomMissing,
        RxError::LineLow => ErrorKind::LineLow,
    }
}

impl From<cec_core::Level> for crate::Level {
    fn from(level: cec_core::Level) -> Self {
        match level {
            cec_core::Level::Low => Self::Low,
            cec_core::Level::High => Self::High,
        }
    }
}
