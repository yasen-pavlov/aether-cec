//! HTTP authentication, transport bounds and update-health policy regressions.
use aether_logic::{console, http::*};
use aether_proto::{LogLevel, LogLine, OtaState, Verb};

#[test]
fn bearer_rejects_missing_prefix_length_and_each_differing_byte() {
    let token = "0123456789abcdef".repeat(4);
    assert!(bearer(Some(&format!("Bearer {token}")), &token));
    for bad in [None, Some(""), Some(token.as_str()), Some("bearer abc")] {
        assert!(!bearer(bad, &token));
    }
    for index in 0..64 {
        let mut changed = token.clone().into_bytes();
        changed[index] = b'X';
        assert!(!bearer(
            Some(&format!("Bearer {}", String::from_utf8(changed).unwrap())),
            &token
        ));
    }
    assert!(!bearer(Some(&format!("Bearer {token}0")), &token));
    assert!(!bearer(Some("Bearer "), ""));
    assert!(matches!(
        console::parse(&format!("set token {token}")),
        Ok(console::ConsoleCommand::Token(_))
    ));
    assert!(matches!(
        console::parse("show token"),
        Ok(console::ConsoleCommand::ShowToken)
    ));
    assert!(console::parse("set token abc").is_err());
}
#[test]
fn exact_routes_and_error_codes() {
    for (method, path, expected) in [
        ("GET", "/", Route::Index),
        ("GET", "/healthz", Route::Health),
        ("GET", "/api/status", Route::Status),
        ("GET", "/api/cfg", Route::Config),
        ("PUT", "/api/cfg", Route::Command(Verb::Cfg)),
        ("POST", "/api/cmd/ping", Route::Command(Verb::Ping)),
        ("POST", "/api/cmd/state", Route::Command(Verb::State)),
        ("POST", "/api/cmd/restart", Route::Command(Verb::Restart)),
        ("POST", "/api/cmd/reset", Route::Command(Verb::Reset)),
        ("POST", "/api/tx", Route::Tx),
        ("GET", "/api/events", Route::Events),
        ("GET", "/api/log", Route::Log),
        ("GET", "/api/telemetry", Route::Telemetry),
        ("POST", "/api/ota", Route::Ota),
    ] {
        assert_eq!(route(method, path), Some(expected));
    }
    for (method, path) in [
        ("GET", "/api/cmd/ping"),
        ("POST", "/api/cmd/ota"),
        ("GET", "/api/status/"),
        ("DELETE", "/api/cfg"),
        ("GET", "/missing"),
    ] {
        assert_eq!(route(method, path), None);
    }
    for (reason, code) in [
        ("unauthorized", 401),
        ("not_found", 404),
        ("too_large", 413),
        ("busy", 409),
        ("malformed", 400),
        ("sha256_mismatch", 400),
        ("storage_error", 500),
    ] {
        assert_eq!(error_status(reason), code);
    }
}
#[test]
fn handler_auth_exempts_only_public_get_routes_including_native_sse() {
    let token = "ab".repeat(32);
    let valid = format!("Bearer {token}");
    for path in ["/", "/healthz"] {
        let public = route("GET", path);
        assert!(authorized(public, None, &token));
        assert!(authorized(public, Some("Bearer wrong"), &token));
        // A public path with a different method is not an exemption.
        assert!(!authorized(route("POST", path), None, &token));
    }
    for (method, path) in [
        ("GET", "/api/events"), // Native SSE uses the exact same policy.
        ("GET", "/api/status"),
        ("GET", "/api/cfg"),
        ("GET", "/api/log"),
        ("GET", "/api/telemetry"),
        ("PUT", "/api/cfg"),
        ("POST", "/api/tx"),
        ("POST", "/api/ota"),
        ("POST", "/api/cmd/ping"),
        ("POST", "/api/cmd/state"),
        ("POST", "/api/cmd/restart"),
        ("POST", "/api/cmd/reset"),
        ("GET", "/unknown"),
    ] {
        let protected = route(method, path);
        assert!(!authorized(protected, None, &token), "{path}");
        assert!(
            !authorized(protected, Some("Bearer wrong"), &token),
            "{path}"
        );
        assert!(authorized(protected, Some(&valid), &token), "{path}");
    }
}
#[test]
fn upload_headers_validate_boundaries_before_writing() {
    let sha = "aB".repeat(32);
    assert_eq!(
        ota_headers(Some(4096), Some(&sha), 4096),
        Ok((4096, [0xab; 32]))
    );
    assert_eq!(ota_headers(Some(4097), Some(&sha), 4096), Err("too_large"));
    for bad in [
        None,
        Some(""),
        Some("0"),
        Some(&"z".repeat(64)),
        Some(&"0".repeat(63)),
        Some(&"0".repeat(65)),
    ] {
        assert_eq!(ota_headers(Some(1), bad, 4096), Err("malformed"));
    }
    for length in [None, Some(0)] {
        assert_eq!(ota_headers(length, Some(&sha), 4096), Err("malformed"));
    }
}
#[test]
fn ring_evicts_oldest_and_sse_has_exact_framing() {
    let mut ring = LogRing::default();
    for t_us in 0..100 {
        ring.push(LogLine {
            t_us,
            lvl: LogLevel::Info,
            msg: "static_code".try_into().unwrap(),
        });
    }
    assert_eq!(ring.entries().len(), 64);
    assert_eq!(ring.entries().front().unwrap().t_us, 36);
    assert_eq!(ring.entries().back().unwrap().t_us, 99);
    assert_eq!(
        sse("bus", r#"{"ev":"rx"}"#).unwrap(),
        "event: bus\ndata: {\"ev\":\"rx\"}\n\n"
    );
    for suffix in ["status", "ota", "telemetry"] {
        assert!(sse(suffix, "{}").is_some());
    }
    assert!(sse("cfg", "{}").is_none());
    assert!(sse("bus", "{}\nevent: forged").is_none());
}
#[test]
fn ota_confirmation_and_rollback_require_successful_status_publication() {
    use BootAction::{ConfirmUpdate, ReportRollback, Rollback, Wait};
    use BootKind::{Pending, RolledBack, Valid};
    for time in [0, 59] {
        assert_eq!(boot_action(Pending, false, time), Wait);
    }
    assert_eq!(boot_action(Pending, false, 60), Rollback);
    assert_eq!(boot_action(Pending, false, 3600), Rollback);
    assert_eq!(boot_action(Pending, true, 59), ConfirmUpdate);
    assert_eq!(boot_action(Pending, true, 60), ConfirmUpdate);
    for time in [0, 60, 3600] {
        assert_eq!(boot_action(Valid, false, time), Wait);
        assert_eq!(boot_action(Valid, true, time), Wait);
    }
    assert_eq!(boot_action(RolledBack, false, 3600), Wait);
    assert_eq!(boot_action(RolledBack, true, 0), ReportRollback);
}

#[test]
fn bootloader_pending_state_survives_absent_failed_or_stale_record_loads() {
    let running = 0x0020_0000;
    for record in [None, Some(running), Some(0x0002_0000)] {
        // None includes an unreadable/malformed record: early recovery stays pending.
        let kind = boot_kind(true, running, record);
        assert_eq!(kind, BootKind::Pending);
        assert_eq!(boot_action(kind, false, 60), BootAction::Rollback);
    }
    assert_eq!(boot_kind(false, running, None), BootKind::Valid);
    // Power loss after mark-valid but before record removal cannot cause a reboot loop.
    let stale = boot_kind(false, running, Some(running));
    assert_eq!(stale, BootKind::Valid);
    assert_eq!(boot_action(stale, false, 3600), BootAction::Wait);
    assert_eq!(
        boot_kind(false, running, Some(0x0002_0000)),
        BootKind::RolledBack
    );
}

#[test]
fn shutdown_deadline_does_not_depend_on_http_waiter_or_response_delivery() {
    assert!(!shutdown_due(0, false));
    assert!(!shutdown_due(1999, false));
    assert!(shutdown_due(2000, false));
    assert!(shutdown_due(5000, false)); // Handler's recv_timeout has already expired.
    assert!(shutdown_due(0, true)); // PUBACK is an early completion, not a prerequisite.
}

#[test]
fn successful_upload_progress_orders_receiving_verifying_rebooting() {
    let id = "o-7".try_into().unwrap();
    let mut upload = UploadProgress::new(id, 1000);
    let start = upload.received(0).unwrap();
    assert_eq!(start.state, OtaState::Receiving);
    assert_eq!(start.pct, Some(0));
    assert!(upload.received(1).is_none()); // Still zero percent.
    let halfway = upload.received(499).unwrap();
    assert_eq!(halfway.state, OtaState::Receiving);
    assert_eq!(halfway.pct, Some(50));
    let full = upload.received(500).unwrap();
    assert_eq!(full.pct, Some(100));
    let verifying = upload.verifying();
    let rebooting = upload.finished(Ok(()));
    assert_eq!(verifying.state, OtaState::Verifying);
    assert_eq!(rebooting.state, OtaState::Rebooting);
    for event in [start, halfway, full, verifying, rebooting] {
        assert_eq!(event.id.as_str(), "o-7");
        assert!(event.fw.is_none());
        assert!(event.reason.is_none());
        if event.state != OtaState::Receiving {
            assert!(event.pct.is_none());
        }
    }
}

#[test]
fn upload_failures_are_terminal_progress_at_receiving_or_verifying() {
    for reason in [
        "malformed",
        "storage_error",
        "sha256_mismatch",
        "project_mismatch",
    ] {
        let mut upload = UploadProgress::new("o-8".try_into().unwrap(), 4096);
        assert_eq!(upload.received(0).unwrap().state, OtaState::Receiving);
        if matches!(reason, "sha256_mismatch" | "project_mismatch") {
            assert_eq!(upload.received(4096).unwrap().pct, Some(100));
            assert_eq!(upload.verifying().state, OtaState::Verifying);
        } else {
            assert_eq!(upload.received(1024).unwrap().pct, Some(25));
        }
        let failed = upload.finished(Err(reason));
        assert_eq!(failed.id.as_str(), "o-8");
        assert_eq!(failed.state, OtaState::Failed);
        assert_eq!(failed.reason.as_deref(), Some(reason));
        assert!(failed.pct.is_none());
        assert!(failed.fw.is_none());
    }
}
