//! Protocol and provisioning acceptance tests without ESP-IDF.
use aether_logic::{
    bus,
    commands::{self, Effect},
    config,
    console::{self, ConsoleCommand},
    status,
};
use aether_proto::{Config, ConfigCommand, Verb};
use cec_core::bridge::Bridge;
use serde_json::{Value, json};

#[test]
fn config_merge_validation_table() {
    let cfg = config::defaults();
    for (patch, reason) in [
        (json!({"id":"c","node":"BAD"}), "bad_node"),
        (json!({"id":"c","node":""}), "bad_node"),
        (json!({"id":"c","replies":["85"]}), "forbidden_reply"),
        (json!({"id":"c","replies":["82"]}), "forbidden_reply"),
        (json!({"id":"c","pa":"edid"}), "edid_unsupported"),
        (json!({"id":"c","ack":[15]}), "bad_ack"),
        (json!({"id":"c","gpio":{"cec":34}}), "bad_cec_gpio"),
        (json!({"id":"c","gpio":{"hpd":4}}), "gpio_conflict"),
    ] {
        let patch = serde_json::from_value::<ConfigCommand>(patch).unwrap();
        assert_eq!(config::merge(&cfg, patch), Err(reason));
        assert_eq!(cfg, config::defaults());
    }
    for invalid in [
        json!({"id":"c","la":15}),
        json!({"id":"c","telemetry_s":65536}),
        json!({"id":"c","telemetry_s":-1}),
    ] {
        assert!(serde_json::from_value::<ConfigCommand>(invalid).is_err());
    }
    let mut missing = serde_json::to_value(&cfg).unwrap();
    missing["gpio"].as_object_mut().unwrap().remove("cec");
    assert!(serde_json::from_value::<Config>(missing).is_err());
    for period in [0, 1, 65535] {
        let patch = serde_json::from_value(
            json!({"id":"c","telemetry_s":period,"la":"auto","gpio":{"hpd":34}}),
        )
        .unwrap();
        let merged = config::merge(&cfg, patch).unwrap();
        assert_eq!(merged.telemetry_s, period);
        assert_eq!(merged.gpio.cec, 4);
        assert_eq!(merged.gpio.hpd, Some(34));
        assert_eq!(config::core_config(&merged).unwrap().la, None);
        let patch = serde_json::from_value(json!({"id":"c","gpio":{"hpd":null}})).unwrap();
        assert_eq!(config::merge(&merged, patch).unwrap().gpio.hpd, None);
    }
}

#[test]
fn console_quoted_password_and_bounds() {
    match console::parse("set wifi test \"two words\"").unwrap() {
        ConsoleCommand::Wifi { ssid, pass } => {
            assert_eq!(ssid, "test");
            assert_eq!(pass, "two words");
        }
        _ => panic!("wrong command"),
    }
    match console::parse("set mqtt broker.example 8883 user \"two words\"").unwrap() {
        ConsoleCommand::Mqtt {
            tls, port, pass, ..
        } => {
            assert!(tls);
            assert_eq!(port, 8883);
            assert_eq!(pass, "two words");
        }
        _ => panic!("wrong command"),
    }
    assert!(matches!(
        console::parse("set mqtt broker.example 1883 user \"\" plain"),
        Ok(ConsoleCommand::Mqtt { tls: false, .. })
    ));
    assert_eq!(console::words(&"x".repeat(201)), Err("line_too_long"));
    assert_eq!(console::words("\"unfinished"), Err("unclosed_quote"));
    assert!(console::parse("set wifi ssid two words").is_err());
    assert!(console::parse("set mqtt host 0 user pass").is_err());
    assert!(console::parse("set node BAD").is_err());
    assert_eq!(console::words(&"x".repeat(200)).unwrap()[0].len(), 200);
}

#[test]
fn wifi_passphrases_validate_before_persistence() {
    for pass in [
        String::new(),
        "a".repeat(8),
        " ".repeat(63),
        "aB09".repeat(16),
    ] {
        assert!(console::valid_wifi("ssid", &pass));
        assert!(matches!(
            console::parse(&format!("set wifi ssid \"{pass}\"")),
            Ok(ConsoleCommand::Wifi { .. })
        ));
    }
    for pass in [
        "a".repeat(7),
        "a".repeat(65),
        "z".repeat(64),
        "é".repeat(8),
        "abcd\tefgh".into(),
    ] {
        assert!(!console::valid_wifi("ssid", &pass));
        assert!(console::parse(&format!("set wifi ssid \"{pass}\"")).is_err());
    }
    assert!(!console::valid_wifi("", "password"));
    assert!(!console::valid_wifi(&"s".repeat(33), "password"));
    assert!(!console::valid_mqtt("broker.example", 0, "user", "pass"));
    assert!(!console::valid_mqtt(
        "broker.example",
        8883,
        "user",
        "bad\npass"
    ));
}

#[test]
fn remote_reset_preserves_node_but_restores_cec_config() {
    let mut cfg = config::defaults();
    cfg.node = "bench".try_into().unwrap();
    cfg.telemetry_s = 9;
    cfg.allow_from = true;
    let reset = commands::plan(Verb::Reset, br#"{"id":"r","confirm":"bench"}"#, &cfg, 0);
    assert!(reset.result.ok);
    assert!(matches!(reset.effect, Effect::Reset));
    let restored = reset.config.unwrap();
    assert_eq!(restored.node, cfg.node);
    let mut expected = config::defaults();
    expected.node = cfg.node;
    assert_eq!(restored, expected);
}

#[test]
fn command_results_and_effects() {
    let cfg = config::defaults();
    let ping = commands::plan(
        Verb::Ping,
        br#"{"id":"p","echo":"abc","unknown":1}"#,
        &cfg,
        42,
    );
    assert_eq!(
        serde_json::to_value(ping.result).unwrap(),
        json!({"id":"p","verb":"ping","ok":true,"t_us":42,"echo":"abc"})
    );
    for payload in [
        b"not JSON".as_slice(),
        br#"{"id":"p"}"#,
        br#"{"id":"p","echo":"x"} true"#,
    ] {
        let malformed = commands::plan(Verb::Ping, payload, &cfg, 42);
        assert_eq!(
            serde_json::to_value(malformed.result).unwrap(),
            json!({"verb":"ping","ok":false,"reason":"malformed"})
        );
        assert!(matches!(malformed.effect, Effect::None));
    }
    assert!(matches!(
        commands::plan(Verb::Restart, br#"{"id":"r"}"#, &cfg, 0).effect,
        Effect::Restart
    ));
    assert!(matches!(
        commands::plan(Verb::State, br#"{"id":"s","power":"standby"}"#, &cfg, 0).effect,
        Effect::Power(aether_proto::PowerStatus::Standby)
    ));
    let reset = commands::plan(Verb::Reset, br#"{"id":"r","confirm":"wrong"}"#, &cfg, 0);
    assert_eq!(reset.result.reason.unwrap(), "confirm_mismatch");
    assert!(matches!(
        commands::plan(Verb::Reset, br#"{"id":"r","confirm":"aether"}"#, &cfg, 0).effect,
        Effect::Reset
    ));
    assert!(commands::verb("ota").is_none());
    let cfg_plan = commands::plan(Verb::Cfg, br#"{"id":"c","telemetry_s":0}"#, &cfg, 0);
    assert!(matches!(cfg_plan.effect, Effect::Config));
    assert_eq!(cfg_plan.config.unwrap().telemetry_s, 0);
    assert!(commands::verb("result").is_none());
}

#[test]
fn tx_rejections_use_shared_sequence_and_preserve_fields() {
    let mut bridge = Bridge::new(config::core_config(&config::defaults()).unwrap(), 9);
    for (payload, reason, id) in [
        (r#"{"id":"t","to":8,"data":"8F"}"#, "no_la", Some("t")),
        (r#"{"id":"t","to":16,"data":"8F"}"#, "bad_to", Some("t")),
        (
            r#"{"id":"t","to":8,"data":"ZZ"}"#,
            "data_not_hex",
            Some("t"),
        ),
        (
            r#"{"id":"t","to":8,"data":"8F","from":0}"#,
            "from_not_allowed",
            Some("t"),
        ),
        (
            r#"{"id":"12345678901234567","to":8,"data":"8F"}"#,
            "id_too_long",
            None,
        ),
        (
            r#"{"id":"t","to":8,"data":"1234567890123456789012345678901"}"#,
            "data_too_long",
            Some("t"),
        ),
        (r#"{"id":"t","to":8}"#, "malformed", None),
        ("{} trailing", "malformed", None),
    ] {
        let seq = bridge.seq() + 1;
        let event =
            serde_json::to_value(bus::reject(&mut bridge, payload.as_bytes(), 123)).unwrap();
        assert_eq!(event["status"], "rejected");
        assert_eq!(event["reason"], reason);
        assert_eq!(event["seq"], seq);
        assert_eq!(event.get("id").and_then(Value::as_str), id);
        assert!(event.get("attempts").is_none());
        if reason == "bad_to" {
            assert!(event.get("to").is_none());
        }
        if reason == "data_too_long" {
            assert!(event.get("data").is_none());
        }
    }
}

#[test]
fn status_and_telemetry_exact_fields() {
    let cfg = config::defaults();
    let snapshot = status::Snapshot {
        fw: "0.0.0+abc",
        boot: 3,
        t_us: 123,
        ip: "192.0.2.2",
        rssi: -58,
        link_count: 2,
        heap_free: 1000,
        heap_min: 900,
        outbox_dropped: 7,
    };
    let online = serde_json::to_value(status::online(&cfg, &snapshot)).unwrap();
    assert_eq!(
        online,
        json!({"online":true,"v":1,"fw":"0.0.0+abc","boot":3,"t_us":123,"la":null,"pa":"0.0.0.0","type":"tv","ip":"192.0.2.2","rssi":-58,"bus":null,"hpd":null})
    );
    let telemetry = serde_json::to_value(status::telemetry(&snapshot)).unwrap();
    assert_eq!(telemetry["heap_free"], 1000);
    assert_eq!(telemetry["heap_min"], 900);
    assert_eq!(telemetry["outbox_dropped"], 7);
    assert_eq!(telemetry["link_count"], 2);
    assert_eq!(telemetry["t_us"], 123);
    assert_eq!(telemetry["boot"], 3);
    assert_eq!(telemetry["rssi"], -58);
    assert_eq!(telemetry["queue"]["depth"], 0);
    assert_eq!(telemetry["queue"]["max"], 16);
    // These bus measurements are explicitly deferred until the GPIO executor.
    assert_eq!(telemetry["rx"]["frames"], 0);
    assert_eq!(telemetry["rx"]["errors"], 0);
    assert_eq!(telemetry["rx"]["acked_by_us"], 0);
    assert_eq!(telemetry["tx"]["host"], 0);
    assert_eq!(telemetry["tx"]["local"], 0);
    assert_eq!(telemetry["tx"]["ack"], 0);
    assert_eq!(telemetry["tx"]["nack"], 0);
    assert_eq!(telemetry["tx"]["arb_lost"], 0);
    assert_eq!(telemetry["tx"]["line_error"], 0);
    assert_eq!(telemetry["tx"]["busy"], 0);
    assert_eq!(telemetry["tx"]["rejected"], 0);
    assert_eq!(telemetry["tx"]["retransmits"], 0);
}
