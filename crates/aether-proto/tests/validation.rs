//! Raw wire ingress retains correlation on bounded validation failures.

use aether_proto::{BusEvent, Origin, RawTxRequest, TxStatus};
use cec_core::{bridge::Config, timing};
use serde_json::json;

fn request(text: &str) -> RawTxRequest {
    serde_json::from_str(text).unwrap()
}

#[test]
fn raw_validation_reports_exact_id_and_reason_in_precedence_order() {
    let mut cfg = Config::default();
    let cases = [
        (
            r#"{"id":"12345678901234567","to":300,"data":"ZZ"}"#,
            None,
            "id_too_long",
        ),
        (r#"{"id":"x","to":300,"data":"ZZ"}"#, Some("x"), "bad_to"),
        (r#"{"id":"x","to":16,"data":""}"#, Some("x"), "bad_to"),
        (
            r#"{"id":"x","to":18446744073709551615,"data":""}"#,
            Some("x"),
            "bad_to",
        ),
        (
            r#"{"id":"x","to":8,"data":"ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ"}"#,
            Some("x"),
            "data_too_long",
        ),
        (
            r#"{"id":"x","to":8,"data":"ZZ"}"#,
            Some("x"),
            "data_not_hex",
        ),
        (r#"{"id":"x","to":8,"data":"8"}"#, Some("x"), "data_not_hex"),
        (
            r#"{"id":"x","to":8,"data":"8F","from":0}"#,
            Some("x"),
            "from_not_allowed",
        ),
        (
            r#"{"id":"x","to":8,"data":"8F","from":99}"#,
            Some("x"),
            "from_not_allowed",
        ),
    ];
    for (text, id, reason) in cases {
        let failure = request(text).validate(&cfg).unwrap_err();
        assert_eq!(failure.0.as_deref(), id, "{text}");
        assert_eq!(failure.1.as_str(), reason, "{text}");
    }
    cfg.allow_from = true;
    for from in [15, 99, u64::MAX] {
        let raw = request(&json!({"id":"x", "to":8, "data":"8F", "from":from}).to_string());
        let failure = raw.validate(&cfg).unwrap_err();
        assert_eq!(failure.0.as_deref(), Some("x"));
        assert_eq!(failure.1.as_str(), "from_not_allowed");
    }
    cfg.la = None;
    let failure = request(r#"{"id":"x","to":8,"data":"8F"}"#)
        .validate(&cfg)
        .unwrap_err();
    assert_eq!(failure.0.as_deref(), Some("x"));
    assert_eq!(failure.1.as_str(), "no_la");
    cfg.la = Some(15);
    assert_eq!(
        request(r#"{"id":"x","to":8,"data":""}"#)
            .validate(&cfg)
            .unwrap_err()
            .1
            .as_str(),
        "no_la"
    );
}

#[test]
fn raw_validation_accepts_escaped_strings_and_clamps_retry_counts_without_wrapping() {
    let cfg = Config {
        allow_from: true,
        ..Config::default()
    };
    let raw = request(
        r#"{"id":"\u0078","to":8,"data":"\u0038f","from":14,"retries":18446744073709551615}"#,
    );
    let submission = raw.validate(&cfg).unwrap();
    assert_eq!(submission.id.as_str(), "x");
    assert_eq!((submission.frame.from, submission.frame.to), (14, 8));
    assert_eq!(submission.frame.data.as_slice(), &[0x8F]);
    assert_eq!(submission.retries, Some(timing::MAX_ATTEMPTS - 1));
    for data in ["", "8F"] {
        let raw = request(&json!({"id":"x", "to":15, "data":data}).to_string());
        let submission = raw.validate(&cfg).unwrap();
        assert_eq!(
            submission.retries, None,
            "bridge chooses poll/non-poll default"
        );
        assert_eq!(submission.frame.from, 0);
    }
    let raw = request(r#"{"id":"x","to":8,"data":"8F","retries":0}"#);
    assert_eq!(raw.validate(&cfg).unwrap().retries, Some(0));
}

#[test]
fn raw_string_bounds_preserve_rejections_and_malformed_ingress_is_detectable() {
    for (length, expected) in [
        (16, None),
        (17, Some("id_too_long")),
        (32, Some("id_too_long")),
    ] {
        let raw = request(&json!({"id":"a".repeat(length), "to":8, "data":"8F"}).to_string());
        let result = raw.validate(&Config::default());
        if let Some(reason) = expected {
            assert_eq!(result.unwrap_err(), (None, reason.try_into().unwrap()));
        } else {
            assert!(result.is_ok());
        }
    }
    for length in [30, 31, 64] {
        let raw = request(&json!({"id":"x", "to":8, "data":"A".repeat(length)}).to_string());
        if length > 30 {
            assert_eq!(
                raw.validate(&Config::default()).unwrap_err().1.as_str(),
                "data_too_long"
            );
        } else {
            assert!(raw.validate(&Config::default()).is_ok());
        }
    }
    for text in [
        json!({"id":"a".repeat(33), "to":8, "data":"8F"}).to_string(),
        json!({"id":"x", "to":8, "data":"A".repeat(65)}).to_string(),
        r#"{"id":"x","to":-1,"data":"8F"}"#.into(),
        r#"{"id":"x","to":"8","data":"8F"}"#.into(),
        r#"{"id":"x","to":8}"#.into(),
        "{".into(),
    ] {
        assert!(
            serde_json::from_str::<RawTxRequest>(&text).is_err(),
            "{text}"
        );
    }
}

#[test]
fn overlong_rejected_data_and_invalid_destination_are_omitted_from_wire_event() {
    for (to, expected_reason) in [(300, "bad_to"), (8, "data_too_long")] {
        let raw = request(
            &json!({
                "id":"x", "to":to, "data":"00112233445566778899AABBCCDDEEFF"
            })
            .to_string(),
        );
        let (id, reason) = raw.validate(&Config::default()).unwrap_err();
        let event = BusEvent::Tx {
            seq: 1,
            t_us: 2,
            id,
            origin: Origin::Host,
            from: None,
            to: if to == 8 { Some(8) } else { None },
            data: raw.rejected_data(),
            status: TxStatus::Rejected,
            attempts: None,
            reason: Some(reason),
            reply_to: None,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["reason"], expected_reason);
        assert_eq!(wire["id"], "x");
        assert!(wire.get("data").is_none());
        assert_eq!(wire.get("to").is_some(), to == 8);
        assert_eq!(serde_json::from_value::<BusEvent>(wire).unwrap(), event);
    }
    for data in ["ZZ", "8F"] {
        let raw = request(&json!({"id":"x", "to":300, "data":data}).to_string());
        assert!(raw.rejected_data().is_some());
    }
}

#[test]
fn manual_event_decoder_range_checks_addresses_and_rejects_unknown_enum_values() {
    for event in [
        json!({"ev":"rx", "seq":1, "t_us":2, "from":99, "to":0, "data":"", "ack":true}),
        json!({"ev":"tx", "seq":1, "t_us":2, "origin":"host", "from":0, "to":300, "data":"", "status":"ack"}),
        json!({"ev":"tx", "seq":1, "t_us":2, "origin":"host", "from":0, "to":16, "data":"", "status":"ack"}),
        json!({"ev":"err", "seq":1, "t_us":2, "from":99, "kind":"bit_timing"}),
        json!({"ev":"tx", "seq":1, "t_us":2, "origin":"host", "to":0, "data":"", "status":"future"}),
        json!({"ev":"err", "seq":1, "t_us":2, "kind":"future"}),
    ] {
        assert!(serde_json::from_value::<BusEvent>(event).is_err());
    }
    for status in ["ack", "nack", "arb_lost", "line_error", "busy"] {
        for missing in ["to", "data"] {
            let mut event = json!({"ev":"tx", "seq":1, "t_us":2, "origin":"host", "from":0, "to":0, "data":"", "status":status});
            event.as_object_mut().unwrap().remove(missing);
            assert!(serde_json::from_value::<BusEvent>(event).is_err());
        }
    }
}
