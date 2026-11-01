//! Bridge observations map to their exact ordered MQTT representation.

use aether_proto::{BusEvent, BusState, ErrorKind, Hex, Origin, ReplyTo, TxData, TxStatus};
use cec_core::{
    bridge,
    frame::{Frame, TxOutcome, TxStatus as CoreStatus},
    rx::RxError,
    timing,
};
use serde_json::json;

fn timestamp() -> u64 {
    u64::from(timing::BIT)
}

#[test]
fn received_frame_preserves_sequence_time_header_data_and_observed_ack() {
    let event: BusEvent = bridge::BusEvent::Rx {
        seq: 8,
        t_us: timestamp(),
        frame: Frame::new(15, 5, &[0x8C]).unwrap(),
        ack: true,
    }
    .into();
    assert_eq!(
        serde_json::to_value(event).unwrap(),
        json!({
            "ev":"rx", "seq":8, "t_us":timestamp(), "from":15, "to":5, "data":"8C", "ack":true,
        })
    );
}

#[test]
fn host_outcomes_preserve_correlation_and_map_all_core_statuses() {
    for (status, expected) in [
        (CoreStatus::Ack, TxStatus::Ack),
        (CoreStatus::Nack, TxStatus::Nack),
        (CoreStatus::ArbLost, TxStatus::ArbLost),
        (CoreStatus::LineError, TxStatus::LineError),
        (CoreStatus::Busy, TxStatus::Busy),
    ] {
        let event: BusEvent = bridge::BusEvent::Tx {
            seq: 9,
            t_us: timestamp(),
            origin: bridge::Origin::Host("s-1".try_into().unwrap()),
            frame: Frame::new(0, 8, &[0x8F]).unwrap(),
            outcome: TxOutcome {
                status,
                attempts: 3,
            },
        }
        .into();
        let BusEvent::Tx {
            seq,
            t_us,
            id,
            origin,
            from,
            to,
            data,
            status: actual,
            attempts,
            reason,
            reply_to,
        } = event
        else {
            panic!()
        };
        assert_eq!((seq, t_us), (9, timestamp()));
        assert_eq!(id.as_deref(), Some("s-1"));
        assert_eq!(origin, Origin::Host);
        assert_eq!((from, to), (Some(0), Some(8)));
        assert_eq!(
            data,
            Some(TxData::Hex(
                Hex(heapless::Vec::from_slice(&[0x8F]).unwrap())
            ))
        );
        assert_eq!(actual, expected);
        assert_eq!(
            attempts,
            if status == CoreStatus::Busy {
                None
            } else {
                Some(3)
            }
        );
        assert_eq!(reason, None);
        assert_eq!(reply_to, None);
    }
}

#[test]
fn local_outcome_uses_reply_sequence_and_omits_host_id() {
    let event: BusEvent = bridge::BusEvent::Tx {
        seq: 10,
        t_us: timestamp(),
        origin: bridge::Origin::Local { reply_to_seq: 7 },
        frame: Frame::new(0, 15, &[0x84, 0, 0, 0]).unwrap(),
        outcome: TxOutcome {
            status: CoreStatus::Ack,
            attempts: 1,
        },
    }
    .into();
    let BusEvent::Tx {
        id,
        origin,
        reply_to,
        ..
    } = &event
    else {
        panic!()
    };
    assert_eq!(*origin, Origin::Local);
    assert_eq!(*id, None);
    assert_eq!(*reply_to, Some(ReplyTo { seq: 7 }));
    let json = serde_json::to_value(event).unwrap();
    assert!(json.get("id").is_none());
    assert_eq!(json["reply_to"], json!({"seq":7}));
}

#[test]
fn decoding_errors_preserve_known_header_and_partial_bit_count() {
    for (kind, expected) in [
        (RxError::StartBit, ErrorKind::StartBit),
        (RxError::BitTiming, ErrorKind::BitTiming),
        (RxError::EomMissing, ErrorKind::EomMissing),
        (RxError::LineLow, ErrorKind::LineLow),
    ] {
        let event: BusEvent = bridge::BusEvent::Err {
            seq: 11,
            t_us: timestamp(),
            kind,
            from: Some(1),
            to: Some(0),
            bits: 17,
            detail: cec_core::rx::ErrorDetail {
                level: Some(cec_core::Level::Low),
                same_level: true,
                duration_us: 1900,
                gap_us: 7,
                bit: Some(1),
                history: [600, 1800].into_iter().collect(),
            },
        }
        .into();
        assert_eq!(
            event,
            BusEvent::Err {
                seq: 11,
                t_us: timestamp(),
                kind: expected,
                from: Some(1),
                to: Some(0),
                bits: Some(17),
                dropped: None,
                level: Some(aether_proto::Level::Low),
                same_level: Some(true),
                dur_us: Some(1900),
                gap_us: Some(7),
                bit: Some(1),
                hist: Some([600, 1800].into_iter().collect()),
            }
        );
    }
}

#[test]
fn stuck_low_and_recovery_are_distinct_line_health_events() {
    for (stuck_low, expected) in [(true, BusState::StuckLow), (false, BusState::Ok)] {
        let event: BusEvent = bridge::BusEvent::Line {
            seq: 12,
            t_us: timestamp(),
            stuck_low,
        }
        .into();
        assert_eq!(
            event,
            BusEvent::Line {
                seq: 12,
                t_us: timestamp(),
                bus: Some(expected),
                hpd: None,
                la: None
            }
        );
        let expected = if stuck_low { "stuck_low" } else { "ok" };
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({"ev":"line", "seq":12, "t_us":timestamp(), "bus":expected})
        );
    }
}

#[test]
fn allocation_completion_reports_success_and_exhaustion_in_ordered_line_stream() {
    for la in [4, 15] {
        let event: BusEvent = bridge::BusEvent::LaAllocated {
            seq: 13,
            t_us: timestamp(),
            la,
        }
        .into();
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({"ev":"line", "seq":13, "t_us":timestamp(), "la":la})
        );
    }
}
