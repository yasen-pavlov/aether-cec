//! Golden and boundary coverage for the public MQTT contract.

use aether_proto::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::fmt::Debug;

// Escape every string value AND object key, exercising transient visit_str input.
fn escaped_json(value: &Value) -> String {
    use std::fmt::Write;
    match value {
        Value::String(text) => {
            let mut output = String::from("\"");
            for unit in text.encode_utf16() {
                write!(output, "\\u{unit:04X}").unwrap();
            }
            output.push('"');
            output
        }
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(escaped_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, value)| format!(
                    "{}:{}",
                    escaped_json(&Value::String(key.clone())),
                    escaped_json(value)
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => value.to_string(),
    }
}

fn round_trip<T>(text: &str)
where
    T: DeserializeOwned + Serialize + PartialEq + Debug,
{
    let first: T = serde_json::from_str(text).unwrap();
    let encoded = serde_json::to_string(&first).unwrap();
    let second: T = serde_json::from_str(&encoded).unwrap();
    assert_eq!(first, second, "{text} -> {encoded}");
    let escaped = escaped_json(&serde_json::from_str(text).unwrap());
    let escaped_value: T = serde_json::from_str(&escaped).unwrap();
    assert_eq!(first, escaped_value, "{text} -> {escaped}");
}

#[test]
fn every_documented_json_example_round_trips() {
    let document = include_str!("../../../docs/protocol.md");
    let mut section = 0;
    let mut inside = false;
    let mut block = String::new();
    let mut count = 0;
    for line in document.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            section = heading.split('.').next().unwrap().parse::<u8>().unwrap();
        }
        if !(3..=9).contains(&section) {
            continue;
        }
        if line == "```json" {
            inside = true;
            block.clear();
        } else if inside && line == "```" {
            inside = false;
            for example in serde_json::Deserializer::from_str(&block).into_iter::<Value>() {
                let example = example.unwrap();
                let text = serde_json::to_string(&example).unwrap();
                match section {
                    3 => round_trip::<Status>(&text),
                    4 => round_trip::<TxRequest>(&text),
                    5 => round_trip::<BusEvent>(&text),
                    6 => round_trip::<Telemetry>(&text),
                    7 => round_trip::<Config>(&text),
                    8 => round_trip::<CommandResult>(&text),
                    9 => round_trip::<OtaProgress>(&text),
                    _ => unreachable!(),
                }
                count += 1;
            }
        } else if inside && !line.trim_start().starts_with("//") {
            block.push_str(line.split("   //").next().unwrap());
            block.push('\n');
        }
    }
    assert_eq!(count, 28, "all concrete fenced examples in sections 3–9");
}

#[test]
fn unknown_keys_are_ignored_in_objects_and_nested_payloads() {
    let baseline = r#"{"ev":"tx","seq":1,"t_us":2,"origin":"local","from":0,"to":15,"data":"84","status":"ack","attempts":1,"reply_to":{"seq":0}}"#;
    let mut extended: Value = serde_json::from_str(baseline).unwrap();
    extended["future"] = json!({"nested": [null, 1, {"deep": true}]});
    extended["reply_to"]["new"] = json!([1, 2, 3]);
    let expected: BusEvent = serde_json::from_str(baseline).unwrap();
    let actual: BusEvent = serde_json::from_str(&extended.to_string()).unwrap();
    assert_eq!(actual, expected);
    round_trip::<TxRequest>(r#"{"id":"r","to":15,"data":"85","future":{"items":[1,2]}}"#);
    round_trip::<OtaProgress>(r#"{"id":"o","state":"rolled_back","future":true}"#);
    round_trip::<LogLine>(r#"{"t_us":1,"lvl":"warn","msg":"line fault","future":0}"#);
}

#[test]
fn request_ids_are_bounded_in_bytes_not_characters() {
    for id in ["a".repeat(16), "é".repeat(8)] {
        let text = json!({"id": id, "to": 8, "data": "8F"}).to_string();
        round_trip::<TxRequest>(&text);
    }
    for id in ["a".repeat(17), "é".repeat(9)] {
        let text = json!({"id": id, "to": 8, "data": "8F"}).to_string();
        assert!(serde_json::from_str::<TxRequest>(&text).is_err());
        let text = json!({"id": id, "verb": "restart", "ok": true}).to_string();
        assert!(serde_json::from_str::<CommandResult>(&text).is_err());
    }
}

#[test]
fn hex_parses_both_cases_and_serializes_uppercase_at_boundaries() {
    for (text, expected) in [
        ("", ""),
        ("aB09fF", "AB09FF"),
        (
            "0123456789abcdef0123456789abcd",
            "0123456789ABCDEF0123456789ABCD",
        ),
    ] {
        let hex: Hex = serde_json::from_str(&format!("\"{text}\"")).unwrap();
        assert_eq!(
            serde_json::to_string(&hex).unwrap(),
            format!("\"{expected}\"")
        );
    }
    for text in ["0", "GG", "8 F0", "00112233445566778899AABBCCDDEEFF"] {
        assert!(
            serde_json::from_str::<Hex>(&format!("\"{text}\"")).is_err(),
            "{text}"
        );
    }
}

#[test]
fn physical_addresses_validate_nibbles_and_format_four_components() {
    for (text, address, output) in [
        ("0.0.0.0", 0, "0.0.0.0"),
        ("2.1.0.0", 0x2100, "2.1.0.0"),
        ("a.b.c.d", 0xABCD, "A.B.C.D"),
        ("F.F.F.F", u16::MAX, "F.F.F.F"),
    ] {
        let parsed: PhysicalAddress = serde_json::from_str(&format!("\"{text}\"")).unwrap();
        assert_eq!(parsed.0, address);
        assert_eq!(
            serde_json::to_string(&parsed).unwrap(),
            format!("\"{output}\"")
        );
    }
    for text in ["1.2.3", "10.0.0.0", "1-2-3-4", "G.0.0.0", "1.2.3.4.5"] {
        assert!(serde_json::from_str::<PhysicalAddress>(&format!("\"{text}\"")).is_err());
    }
}

#[test]
fn transmit_statuses_have_exact_wire_spelling() {
    for (status, text) in [
        (TxStatus::Ack, "ack"),
        (TxStatus::Nack, "nack"),
        (TxStatus::ArbLost, "arb_lost"),
        (TxStatus::LineError, "line_error"),
        (TxStatus::Busy, "busy"),
        (TxStatus::Rejected, "rejected"),
    ] {
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            format!("\"{text}\"")
        );
        assert_eq!(
            serde_json::from_str::<TxStatus>(&format!("\"{text}\"")).unwrap(),
            status
        );
    }
    assert!(serde_json::from_str::<TxStatus>("\"Ack\"").is_err());
}

#[test]
fn rejected_data_is_preserved_but_never_accepted_as_received_or_sent_bytes() {
    let rejected = r#"{"ev":"tx","seq":1,"t_us":2,"id":"x","origin":"host","to":4,"data":"ZZ","status":"rejected","reason":"data_not_hex"}"#;
    round_trip::<BusEvent>(rejected);
    let parsed: BusEvent = serde_json::from_str(rejected).unwrap();
    assert!(matches!(
        parsed,
        BusEvent::Tx {
            data: Some(TxData::Rejected(_)),
            ..
        }
    ));
    assert!(serde_json::from_str::<BusEvent>(&rejected.replace("rejected", "ack")).is_err());
    assert!(
        serde_json::from_str::<BusEvent>(
            r#"{"ev":"rx","seq":1,"t_us":2,"from":0,"to":4,"data":"ZZ","ack":true}"#
        )
        .is_err()
    );
}

#[test]
fn bus_discriminator_can_follow_payload_and_required_fields_stay_required() {
    round_trip::<BusEvent>(
        r#"{"data":"8F","from":1,"to":0,"ack":true,"seq":1,"t_us":2,"ev":"rx"}"#,
    );
    for text in [
        r#"{"ev":"rx","seq":1,"t_us":2,"from":0,"to":4,"data":"8F"}"#,
        r#"{"ev":"tx","seq":1,"t_us":2,"to":4,"data":"8F","status":"ack"}"#,
        r#"{"ev":"err","seq":1,"t_us":2}"#,
        r#"{"ev":"future","seq":1,"t_us":2}"#,
    ] {
        assert!(serde_json::from_str::<BusEvent>(text).is_err());
    }
}

fn command(verb: Verb, text: &str) -> Command {
    Command::deserialize_for(verb, &mut serde_json::Deserializer::from_str(text)).unwrap()
}

#[test]
fn each_command_is_dispatched_by_topic_without_an_extra_payload_tag() {
    for (verb, text) in [
        (
            Verb::Cfg,
            r#"{"id":"c","pa":"edid","la":"auto","vendor":"aBcD01","gpio":{"hpd":null}}"#,
        ),
        (Verb::State, r#"{"id":"c","power":"to_standby"}"#),
        (Verb::Ping, r#"{"id":"c","echo":"abc"}"#),
        (Verb::Restart, r#"{"id":"c"}"#),
        (Verb::Reset, r#"{"id":"c","confirm":"aether"}"#),
    ] {
        let parsed = command(verb, text);
        assert_eq!(parsed.verb(), verb);
        let escaped = escaped_json(&serde_json::from_str(text).unwrap());
        assert_eq!(parsed, command(verb, &escaped));
        let serialized = serde_json::to_string(&parsed).unwrap();
        assert_eq!(parsed, command(verb, &serialized));
        let object: Value = serde_json::from_str(&serialized).unwrap();
        assert!(object.get("verb").is_none());
    }
    assert!(
        Command::deserialize_for(
            Verb::Ping,
            &mut serde_json::Deserializer::from_str(r#"{"id":"c"}"#)
        )
        .is_err()
    );
}

#[test]
fn partial_gpio_update_distinguishes_null_from_omission() {
    let unchanged = command(Verb::Cfg, r#"{"id":"c","gpio":{}}"#);
    let disabled = command(Verb::Cfg, r#"{"id":"c","gpio":{"hpd":null}}"#);
    let enabled = command(Verb::Cfg, r#"{"id":"c","gpio":{"hpd":12}}"#);
    assert_ne!(unchanged, disabled);
    assert_ne!(disabled, enabled);
    for update in [unchanged, disabled, enabled] {
        let serialized = serde_json::to_string(&update).unwrap();
        assert_eq!(update, command(Verb::Cfg, &serialized));
    }
}

#[test]
fn config_selector_and_vendor_encodings_reject_invalid_shapes() {
    for text in ["15", "256", "-1", "\"automatic\""] {
        assert!(serde_json::from_str::<LogicalAddress>(text).is_err());
    }
    for text in ["\"edid\"", "\"1.2.3.4\""] {
        round_trip::<PhysicalAddressConfig>(text);
    }
    for vendor in ["12345", "1234567", "GGGGGG"] {
        let text = json!({"id": "c", "vendor": vendor}).to_string();
        assert!(serde_json::from_str::<ConfigCommand>(&text).is_err());
    }
    let Command::Cfg(config) = command(Verb::Cfg, r#"{"id":"c","vendor":"012345"}"#) else {
        panic!()
    };
    assert_eq!(config.vendor, Some([0x01, 0x23, 0x45]));
}

#[test]
fn bounded_text_fields_reject_overlong_input() {
    for (field, length) in [("node", 16), ("osd", 14)] {
        let mut value = json!({"id":"c"});
        value[field] = json!("a".repeat(length));
        assert!(serde_json::from_str::<ConfigCommand>(&value.to_string()).is_ok());
        value[field] = json!("a".repeat(length + 1));
        assert!(serde_json::from_str::<ConfigCommand>(&value.to_string()).is_err());
    }
    for (field, length) in [("fw", 24), ("reason", 32)] {
        let mut value = json!({"id":"o", "state":"failed"});
        value[field] = json!("a".repeat(length));
        assert!(serde_json::from_str::<OtaProgress>(&value.to_string()).is_ok());
        value[field] = json!("a".repeat(length + 1));
        assert!(serde_json::from_str::<OtaProgress>(&value.to_string()).is_err());
    }
    let echo = json!({"id":"c", "echo":"x".repeat(33)}).to_string();
    assert!(serde_json::from_str::<PingCommand>(&echo).is_err());
}

#[test]
fn status_last_will_and_u64_timestamps_round_trip() {
    assert_eq!(LWT_PAYLOAD, r#"{"online":false}"#);
    let status: Status = serde_json::from_str(LWT_PAYLOAD).unwrap();
    assert_eq!(serde_json::to_string(&status).unwrap(), LWT_PAYLOAD);
    let allocating: Status =
        serde_json::from_str(r#"{"online":true,"la":null,"bus":null,"hpd":null}"#).unwrap();
    assert_eq!(
        serde_json::to_value(allocating).unwrap(),
        json!({"online":true,"la":null,"bus":null,"hpd":null})
    );
    for bus in ["ok", "stuck_low", "no_pullup"] {
        let value = json!({"online":true,"la":null,"bus":bus,"hpd":null});
        let status: Status = serde_json::from_value(value.clone()).unwrap();
        assert!(status.bus.is_some());
        assert_eq!(serde_json::to_value(status).unwrap(), value);
    }
    let text = json!({"ev":"line", "seq":u32::MAX, "t_us":u64::MAX, "hpd":false}).to_string();
    let parsed: BusEvent = serde_json::from_str(&text).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap()["t_us"], u64::MAX);
}

#[test]
fn topics_cover_the_contract_with_maximum_node_length() {
    let node = Node::try_from("0123456789abcdef").unwrap();
    for (actual, suffix) in [
        (Topics::status(&node), "status"),
        (Topics::bus(&node), "bus"),
        (Topics::tx(&node), "tx"),
        (Topics::telemetry(&node), "telemetry"),
        (Topics::cfg(&node), "cfg"),
        (Topics::result(&node), "cmd/result"),
        (Topics::ota(&node), "ota"),
        (Topics::log(&node), "log"),
    ] {
        assert_eq!(actual.as_str(), format!("aether/{node}/{suffix}"));
    }
    for verb in [
        Verb::Cfg,
        Verb::State,
        Verb::Ping,
        Verb::Restart,
        Verb::Reset,
    ] {
        assert_eq!(
            Topics::cmd(&node, verb).as_str(),
            format!("aether/{node}/cmd/{}", verb.as_str())
        );
    }
}

#[test]
fn escaped_strings_work_in_diagnostics_and_all_command_result_fields() {
    round_trip::<LogLine>(r#"{"t_us":1,"lvl":"warn","msg":"line fault"}"#);
    round_trip::<CommandResult>(
        r#"{"id":"x","verb":"ping","ok":false,"reason":"failed","echo":"abc"}"#,
    );
    round_trip::<CommandResult>(r#"{"verb":"ping","ok":false,"reason":"malformed"}"#);
    round_trip::<OtaProgress>(r#"{"id":"x","state":"failed","fw":"0.1.0","reason":"failed"}"#);
    round_trip::<ConfigCommand>(
        r#"{"id":"x","node":"bench","la":"auto","type":"playback","pa":"edid","osd":"Bench","vendor":"ABCDEF","cec_version":"1.3a","replies":["83"],"log":"debug"}"#,
    );
    round_trip::<ConfigCommand>(r#"{"id":"x","pa":"1.2.3.4"}"#);
    round_trip::<BusEvent>(r#"{"ev":"err","seq":1,"t_us":2,"kind":"line_low"}"#);
    round_trip::<BusEvent>(
        r#"{"ev":"tx","seq":1,"t_us":2,"origin":"host","status":"rejected","reason":"malformed"}"#,
    );
}

#[test]
fn http_health_errors_and_push_ota_match_the_contract() {
    round_trip::<Healthz>(r#"{"online":true,"fw":"0.2.0","boot":3}"#);
    round_trip::<HttpError>(r#"{"ok":false,"reason":"unauthorized"}"#);
    round_trip::<OtaAccepted>(r#"{"id":"o-1"}"#);
    for text in [
        r#"{"id":"o-1","state":"receiving","pct":37}"#,
        r#"{"id":"o-1","state":"verifying"}"#,
        r#"{"id":"o-1","state":"rebooting"}"#,
        r#"{"id":"o-1","state":"failed","reason":"sha256_mismatch"}"#,
        r#"{"id":"o-1","state":"confirmed","fw":"0.2.0"}"#,
        r#"{"id":"o-1","state":"rolled_back","fw":"0.1.0"}"#,
    ] {
        round_trip::<OtaProgress>(text);
    }
    assert!(serde_json::from_str::<Verb>(r#""ota""#).is_err());
    assert!(serde_json::from_str::<OtaProgress>(r#"{"id":"o-1","state":"downloading"}"#).is_err());
}

#[test]
fn attempt_diagnostics_round_trip_and_accept_older_telemetry() {
    let legacy = json!({
        "host": 1, "local": 0, "ack": 1, "nack": 0, "arb_lost": 2,
        "line_error": 0, "busy": 0, "rejected": 0, "retransmits": 4
    });
    let old: TxCounters = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(
        old.origin_missing
            + old.sample_late
            + old.launch_rejected
            + old.echo_error
            + old.edge_overrun
            + old.driven_low_mismatch,
        0
    );
    let mut current = legacy;
    current["origin_missing"] = json!(1);
    current["sample_late"] = json!(2);
    current["launch_rejected"] = json!(3);
    current["echo_error"] = json!(4);
    current["edge_overrun"] = json!(5);
    current["driven_low_mismatch"] = json!(6);
    current["line_error"] = json!(21);
    round_trip::<TxCounters>(&current.to_string());
    let parsed: TxCounters = serde_json::from_value(current.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), current);
}

#[test]
fn reset_reasons_are_exact_and_legacy_status_health_remain_readable() {
    for (reason, spelling) in [
        (ResetReason::PowerOn, "power_on"),
        (ResetReason::Software, "software"),
        (ResetReason::Panic, "panic"),
        (ResetReason::IntWdt, "int_wdt"),
        (ResetReason::TaskWdt, "task_wdt"),
        (ResetReason::Brownout, "brownout"),
        (ResetReason::Other, "other"),
    ] {
        assert_eq!(serde_json::to_value(reason).unwrap(), json!(spelling));
        let status =
            json!({"online":true,"boot":4,"reset_reason":spelling,"la":null,"bus":null,"hpd":null});
        let parsed: Status = serde_json::from_value(status.clone()).unwrap();
        assert_eq!(parsed.reset_reason, Some(reason));
        assert_eq!(serde_json::to_value(parsed).unwrap(), status);
        round_trip::<Status>(&status.to_string());
        let health = json!({"online":true,"fw":"0.0.0","boot":4,"reset_reason":spelling});
        let parsed: Healthz = serde_json::from_value(health.clone()).unwrap();
        assert_eq!(parsed.reset_reason, Some(reason));
        assert_eq!(serde_json::to_value(parsed).unwrap(), health);
        round_trip::<Healthz>(&health.to_string());
    }
    let legacy: Status = serde_json::from_str(r#"{"online":true,"boot":3}"#).unwrap();
    assert_eq!(legacy.reset_reason, None);
    let legacy: Healthz = serde_json::from_str(r#"{"online":true,"fw":"0.0.0","boot":3}"#).unwrap();
    assert_eq!(legacy.reset_reason, None);
    let offline: Status = serde_json::from_str(LWT_PAYLOAD).unwrap();
    assert_eq!(offline.reset_reason, None);
    assert_eq!(serde_json::to_string(&offline).unwrap(), LWT_PAYLOAD);
    assert!(serde_json::from_str::<ResetReason>(r#""unknown_future_reason""#).is_err());
}

#[test]
fn last_crash_is_optional_bounded_and_round_trips_escaped_text() {
    let text = "28 pc=40081234 task=cec-bus bt=40081234,400d5678";
    let status = json!({"online":true,"reset_reason":"panic","last_crash":text,"la":null,"bus":null,"hpd":null});
    round_trip::<Status>(&status.to_string());
    let parsed: Status = serde_json::from_value(status.clone()).unwrap();
    assert_eq!(parsed.last_crash.as_deref(), Some(text));
    assert_eq!(serde_json::to_value(parsed).unwrap(), status);
    for text in ["x".repeat(120), "é".repeat(60)] {
        round_trip::<Status>(&json!({"online":true,"last_crash":text}).to_string());
    }
    for text in ["x".repeat(121), "é".repeat(61)] {
        assert!(
            serde_json::from_value::<Status>(json!({"online":true,"last_crash":text})).is_err()
        );
    }
    let legacy: Status = serde_json::from_str(r#"{"online":true,"boot":3}"#).unwrap();
    assert!(legacy.last_crash.is_none());
    assert!(
        serde_json::to_value(legacy)
            .unwrap()
            .get("last_crash")
            .is_none()
    );
    let offline: Status = serde_json::from_str(LWT_PAYLOAD).unwrap();
    assert!(offline.last_crash.is_none());
    assert_eq!(serde_json::to_string(&offline).unwrap(), LWT_PAYLOAD);
}

#[test]
fn decoder_error_evidence_and_overrun_have_exact_golden_json() {
    for text in [
        r#"{"ev":"err","seq":826,"t_us":48900100,"kind":"bit_timing","from":4,"to":0,"bits":40,"level":"low","same_level":false,"dur_us":1900,"gap_us":1170,"bit":8,"hist":[1790,1500,900,620,1780,1510,890,730]}"#,
        r#"{"ev":"err","seq":820,"t_us":48711000,"kind":"overrun","dropped":4}"#,
        r#"{"ev":"err","seq":819,"t_us":48700100,"kind":"start_bit","bits":0}"#,
        r#"{"ev":"err","seq":827,"t_us":48910000,"kind":"line_low","same_level":false,"dur_us":4951,"gap_us":0,"hist":[]}"#,
        r#"{"ev":"err","seq":828,"t_us":48920000,"kind":"bit_timing","bits":1,"level":"low","same_level":true,"dur_us":7,"gap_us":7,"bit":1,"hist":[3700,800,600,1800]}"#,
    ] {
        let event: BusEvent = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&event).unwrap(), text);
        round_trip::<BusEvent>(text);
        let mut extended: Value = serde_json::from_str(text).unwrap();
        extended["future"] = json!({"deep": [null, {"new": true}]});
        assert_eq!(serde_json::from_value::<BusEvent>(extended).unwrap(), event);
    }
}

#[test]
fn decoder_error_history_is_bounded_and_levels_are_typed() {
    for extra in [
        r#""hist":[1,2,3,4,5,6,7,8,9]"#,
        r#""hist":[4294967296]"#,
        r#""level":"floating""#,
    ] {
        let text = format!(r#"{{"ev":"err","seq":1,"t_us":0,"kind":"bit_timing",{extra}}}"#);
        assert!(serde_json::from_str::<BusEvent>(&text).is_err(), "{text}");
    }
}
