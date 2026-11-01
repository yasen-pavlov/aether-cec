//! Bridge identity, queueing, allocation and real all-off scenario tests.
mod common;
use cec_core::bridge::{BusEvent, Origin, Submission, SubmitError};
use cec_core::{
    AckSet, Bridge, Config, DeviceType, Frame, PowerStatus, ReplySet, RxError, RxEvent, TxOutcome,
    TxStatus, timing,
};
use common::{field, fixture_frame};
fn submission(frame: Frame, retries: Option<u8>) -> Submission {
    Submission {
        id: "host-1".try_into().unwrap(),
        frame,
        retries,
    }
}
fn receive(bridge: &mut Bridge, from: u8, to: u8, data: &[u8]) -> BusEvent {
    bridge
        .on_rx(
            RxEvent::Frame {
                frame: Frame::new(from, to, data).unwrap(),
                ack: true,
                start_us: 0,
            },
            0,
        )
        .unwrap()
}
fn done(bridge: &mut Bridge, status: TxStatus) -> Option<BusEvent> {
    bridge.on_tx_done(
        TxOutcome {
            status,
            attempts: 1,
        },
        0,
    )
}
#[test]
fn bridge_every_local_reply_exact_bytes_target_and_priority() {
    let mut config = Config {
        physical_address: 0x1234,
        vendor: [0xAB, 0xCD, 0xEF],
        cec_version: 0x06,
        osd: "Bridge".try_into().unwrap(),
        ..Config::default()
    };
    config.ack = AckSet::new(&[0]);
    let cases: &[(u8, u8, &[u8])] = &[
        (0x83, 15, &[0x84, 0x12, 0x34, 0]),
        (0x8C, 15, &[0x87, 0xAB, 0xCD, 0xEF]),
        (0x8F, 4, &[0x90, 1]),
        (0x9F, 4, &[0x9E, 6]),
        (0x46, 4, b"\x47Bridge"),
        (0x91, 15, b"\x32eng"),
    ];
    for &(op, to, bytes) in cases {
        let mut bridge = Bridge::new(config.clone(), 40);
        bridge.set_power(PowerStatus::Standby);
        bridge
            .submit(submission(Frame::new(0, 8, &[0x36]).unwrap(), None), 0)
            .unwrap();
        assert!(matches!(
            receive(&mut bridge, 4, 0, &[op]),
            BusEvent::Rx { seq: 41, .. }
        ));
        let request = bridge.next_request().unwrap();
        assert_eq!(request.frame, Frame::new(0, to, bytes).unwrap());
        assert_eq!(request.retries, 1);
        assert!(bridge.next_request().is_none());
        assert!(matches!(
            done(&mut bridge, TxStatus::Ack),
            Some(BusEvent::Tx {
                seq: 42,
                origin: Origin::Local { reply_to_seq: 41 },
                ..
            })
        ));
        assert_eq!(bridge.next_request().unwrap().frame.to, 8);
    }
}
#[test]
fn bridge_power_status_runtime_all_transitions() {
    let mut bridge = Bridge::new(Config::default(), 0);
    for power in [
        PowerStatus::On,
        PowerStatus::Standby,
        PowerStatus::ToOn,
        PowerStatus::ToStandby,
    ] {
        bridge.set_power(power);
        receive(&mut bridge, 4, 0, &[0x8F]);
        assert_eq!(
            bridge.next_request().unwrap().frame.data.as_slice(),
            &[0x90, power as u8]
        );
        done(&mut bridge, TxStatus::Ack);
    }
}
#[test]
fn bridge_never_answers_active_source_unregistered_broadcast_or_disabled() {
    let mut bridge = Bridge::new(Config::default(), 0);
    for (from, to, opcode) in [
        (4, 0, 0x85),
        (4, 0, 0x82),
        (15, 0, 0x83),
        (4, 15, 0x83),
        (4, 5, 0x83),
        (4, 0, 0xA6),
    ] {
        receive(&mut bridge, from, to, &[opcode]);
        assert!(bridge.next_request().is_none());
    }
    let mut bridge = Bridge::new(
        Config {
            replies: ReplySet::empty(),
            ..Config::default()
        },
        0,
    );
    receive(&mut bridge, 4, 0, &[0x8F]);
    assert!(bridge.next_request().is_none());
    for opcode in [0x82, 0x85] {
        assert!(ReplySet::new(&[opcode]).is_err());
    }
}
#[test]
fn bridge_submission_validation_table_and_poll_defaults() {
    let mut bridge = Bridge::new(Config::default(), 0);
    let mut bad_to = Frame::new(0, 0, &[]).unwrap();
    bad_to.to = 16;
    let mut bad_from = Frame::new(0, 0, &[]).unwrap();
    bad_from.from = 16;
    for (frame, expected) in [
        (bad_to, SubmitError::BadTo),
        (bad_from, SubmitError::FromNotAllowed),
        (Frame::new(4, 0, &[]).unwrap(), SubmitError::FromNotAllowed),
        (Frame::new(15, 0, &[]).unwrap(), SubmitError::FromNotAllowed),
    ] {
        assert_eq!(bridge.submit(submission(frame, None), 0), Err(expected));
        assert!(bridge.next_request().is_none());
    }
    for la in [None, Some(15)] {
        let mut bridge = Bridge::new(
            Config {
                la,
                ..Config::default()
            },
            0,
        );
        assert_eq!(
            bridge.submit(submission(Frame::new(0, 4, &[]).unwrap(), None), 0),
            Err(SubmitError::NoLa)
        );
    }
    // Oversized values cannot be constructed, so rejection belongs at the parser boundary.
    assert!(cec_core::bridge::RequestId::try_from("12345678901234567").is_err());
    assert!(Frame::new(0, 0, &[0; 16]).is_none());
    for (data, retries, expected) in [
        (&[][..], None, 0),
        (&[0x8F][..], None, 1),
        (&[][..], Some(2), 2),
        (&[0x8F][..], Some(u8::MAX), timing::MAX_ATTEMPTS - 1),
    ] {
        bridge
            .submit(submission(Frame::new(0, 4, data).unwrap(), retries), 0)
            .unwrap();
        assert_eq!(bridge.next_request().unwrap().retries, expected);
        done(&mut bridge, TxStatus::Ack);
    }
    let mut bench = Bridge::new(
        Config {
            allow_from: true,
            ..Config::default()
        },
        0,
    );
    bench
        .submit(submission(Frame::new(4, 0, &[]).unwrap(), None), 0)
        .unwrap();
    assert_eq!(bench.next_request().unwrap().frame.from, 4);
}
#[test]
fn bridge_host_queue_sixteen_deep_local_replies_have_reserved_capacity() {
    let mut bridge = Bridge::new(Config::default(), 0);
    for _ in 0..16 {
        bridge
            .submit(submission(Frame::new(0, 4, &[]).unwrap(), None), 0)
            .unwrap();
    }
    assert_eq!(
        bridge.submit(submission(Frame::new(0, 4, &[]).unwrap(), None), 0),
        Err(SubmitError::QueueFull)
    );
    receive(&mut bridge, 4, 0, &[0x83]);
    assert_eq!(bridge.next_request().unwrap().frame.opcode(), Some(0x84));
    done(&mut bridge, TxStatus::Ack);
    for _ in 0..16 {
        assert!(bridge.next_request().unwrap().frame.is_poll());
        done(&mut bridge, TxStatus::Ack);
    }
    assert!(bridge.next_request().is_none());
}
#[test]
fn bridge_la_allocation_each_type_free_taken_aborted_and_exhausted() {
    for device_type in [
        DeviceType::Tv,
        DeviceType::Playback,
        DeviceType::Audio,
        DeviceType::Recorder,
        DeviceType::Tuner,
    ] {
        let candidates = device_type.candidates();
        for free_index in 0..=candidates.len() {
            let mut bridge = Bridge::new(
                Config {
                    la: None,
                    device_type,
                    ack: AckSet::default(),
                    physical_address: 0x2100,
                    ..Config::default()
                },
                0,
            );
            for (index, &la) in candidates.iter().enumerate() {
                assert_eq!(
                    bridge.next_request().unwrap().frame,
                    Frame::new(la, la, &[]).unwrap()
                );
                assert!(bridge.next_request().is_none());
                for error in [TxStatus::ArbLost, TxStatus::LineError, TxStatus::Busy] {
                    assert!(done(&mut bridge, error).is_none());
                    let repoll = bridge.next_request().unwrap();
                    assert_eq!(repoll.frame, Frame::new(la, la, &[]).unwrap());
                    assert_eq!(repoll.retries, 0);
                }
                let free = index == free_index;
                let result = done(
                    &mut bridge,
                    if free { TxStatus::Nack } else { TxStatus::Ack },
                );
                if free || index + 1 == candidates.len() {
                    let selected = if free { la } else { 15 };
                    assert!(
                        matches!(result, Some(BusEvent::LaAllocated { seq: 1, la, .. }) if la == selected)
                    );
                    assert_eq!(bridge.config().la, Some(selected));
                    assert_eq!(bridge.config().ack.contains(selected), selected < 15);
                    let report = bridge.next_request().unwrap();
                    assert_eq!(
                        report.frame,
                        Frame::new(selected, 15, &[0x84, 0x21, 0, device_type as u8]).unwrap()
                    );
                    assert!(matches!(
                        done(&mut bridge, TxStatus::Ack),
                        Some(BusEvent::Tx {
                            origin: Origin::Local { reply_to_seq: 1 },
                            ..
                        })
                    ));
                    assert!(bridge.next_request().is_none());
                    break;
                }
                assert!(result.is_none());
            }
        }
    }
}
#[test]
fn bridge_seq_monotonic_across_rx_err_host_local_and_allocation() {
    let mut bridge = Bridge::new(
        Config {
            la: None,
            ..Config::default()
        },
        100,
    );
    bridge.next_request().unwrap();
    assert!(matches!(
        done(&mut bridge, TxStatus::Nack),
        Some(BusEvent::LaAllocated { seq: 101, .. })
    ));
    let report = bridge.next_request().unwrap();
    assert!(matches!(
        bridge.on_rx(
            RxEvent::Frame {
                frame: report.frame,
                ack: true,
                start_us: 9
            },
            10
        ),
        Some(BusEvent::Rx { seq: 102, .. })
    ));
    assert!(matches!(
        bridge.on_tx_done(
            TxOutcome {
                status: TxStatus::Ack,
                attempts: 1
            },
            11
        ),
        Some(BusEvent::Tx {
            seq: 103,
            t_us: 11,
            ..
        })
    ));
    assert!(matches!(
        bridge.on_rx(
            RxEvent::Error {
                kind: RxError::BitTiming,
                from: None,
                to: None,
                bits: 0,
                start_us: 0,
                detail: cec_core::rx::ErrorDetail::default(),
            },
            12
        ),
        Some(BusEvent::Err {
            seq: 104,
            t_us: 12,
            ..
        })
    ));
    bridge
        .submit(submission(Frame::new(0, 4, &[0x36]).unwrap(), None), 0)
        .unwrap();
    bridge.next_request().unwrap();
    assert!(matches!(
        done(&mut bridge, TxStatus::Nack),
        Some(BusEvent::Tx {
            seq: 105,
            origin: Origin::Host(_),
            ..
        })
    ));
    assert_eq!(bridge.seq(), 105);
    assert!(bridge.on_rx(RxEvent::AckDue { fall_at_us: 0 }, 0).is_none());
    assert_eq!(bridge.seq(), 105);
}
#[test]
fn bridge_own_echo_is_published_without_recursive_reply() {
    let mut bridge = Bridge::new(Config::default(), 0);
    let frame = Frame::new(0, 0, &[0x83]).unwrap();
    bridge.submit(submission(frame.clone(), None), 0).unwrap();
    bridge.next_request().unwrap();
    bridge.on_rx(
        RxEvent::Frame {
            frame,
            ack: true,
            start_us: 1,
        },
        2,
    );
    done(&mut bridge, TxStatus::Ack);
    assert!(bridge.next_request().is_none());
}
#[test]
fn bridge_failed_retry_uses_final_start_not_stale_earlier_echo() {
    let mut bridge = Bridge::new(Config::default(), 0);
    let frame = Frame::new(0, 4, &[0x36]).unwrap();
    bridge.submit(submission(frame.clone(), None), 0).unwrap();
    bridge.next_request().unwrap();
    bridge.on_rx(
        RxEvent::Frame {
            frame,
            ack: false,
            start_us: 1,
        },
        2,
    );
    assert!(matches!(
        bridge.on_tx_done(
            TxOutcome {
                status: TxStatus::LineError,
                attempts: 2
            },
            3
        ),
        Some(BusEvent::Tx { t_us: 3, .. })
    ));
}
#[test]
fn all_off_takeover_real_fixture_expected_outcomes_and_power_reply() {
    let fixture = include_str!("../../../tests/fixtures/all-off-takeover.jsonl");
    let mut bridge = Bridge::new(Config::default(), 0);
    bridge.set_power(PowerStatus::Standby);
    let mut expected = Vec::new();
    let mut rx_count = 0;
    for line in fixture.lines() {
        if line.contains("\"tx\"") {
            let frame = fixture_frame(line);
            expected.push(frame.clone());
            bridge.submit(submission(frame, None), 0).unwrap();
        } else if line.contains("\"result\"") {
            let ack = field(line, "ack").unwrap() == "true";
            let request = bridge.next_request().unwrap();
            assert_eq!(request.frame, expected.remove(0));
            let outcome = run_tx(request, ack);
            assert_eq!(
                outcome.status,
                if ack { TxStatus::Ack } else { TxStatus::Nack }
            );
            assert_eq!(outcome.attempts, if ack { 1 } else { 2 });
            assert!(matches!(
                bridge.on_tx_done(outcome, 0),
                Some(BusEvent::Tx {
                    origin: Origin::Host(_),
                    ..
                })
            ));
        } else {
            let frame = fixture_frame(line);
            if frame.data.as_slice() == [0x90, 1] {
                assert_eq!(frame.from, 4);
            }
            bridge
                .on_rx(
                    RxEvent::Frame {
                        frame,
                        ack: true,
                        start_us: 0,
                    },
                    0,
                )
                .unwrap();
            rx_count += 1;
        }
    }
    assert_eq!(expected, [] as [Frame; 0]);
    assert_eq!(rx_count, 17);
    let mut replies = Vec::new();
    while let Some(request) = bridge.next_request() {
        replies.push(request.frame.data.clone());
        done(&mut bridge, TxStatus::Ack);
    }
    assert_eq!(
        replies.iter().map(|data| data[0]).collect::<Vec<_>>(),
        [0x84, 0x90, 0x9E, 0x87]
    );
    assert_eq!(replies[1].as_slice(), &[0x90, 1]);
}
fn run_tx(request: cec_core::TxRequest, ack: bool) -> TxOutcome {
    let mut tx = cec_core::Tx::new();
    let frame = request.frame.clone();
    tx.start(request, 0, 0, false).unwrap();
    let mut now = 0;
    let mut block_start = 0;
    let mut first = true;
    for _ in 0..1000 {
        match tx.step(now, 0) {
            cec_core::TxAction::Wait { until_us } => now = until_us,
            cec_core::TxAction::SendBlock {
                start,
                block_start_us,
                ..
            } => {
                block_start = block_start_us;
                first = start;
            }
            cec_core::TxAction::SampleAt { at_us } => {
                now = block_start + u64::from(at_us);
                let prefix = if first {
                    timing::START_TOTAL.nominal
                } else {
                    0
                };
                let bit = (at_us - prefix - timing::SAMPLE_AT) / timing::BIT;
                let high = if bit < 4 {
                    frame.header() & (1 << (7 - bit)) != 0
                } else {
                    !ack
                };
                tx.line_sample(
                    if high {
                        cec_core::Level::High
                    } else {
                        cec_core::Level::Low
                    },
                    now,
                );
            }
            cec_core::TxAction::Abort => {}
            cec_core::TxAction::Done { outcome, .. } => return outcome,
        }
    }
    panic!("transmit did not finish");
}

#[test]
fn bridge_allocation_excludes_own_candidate_then_rebuilds_ack_extras() {
    let mut bridge = Bridge::new(
        Config {
            la: None,
            ack: AckSet::new(&[0, 5]),
            ..Config::default()
        },
        0,
    );
    let request = bridge.next_request().unwrap();
    assert_eq!(request.frame, Frame::new(0, 0, &[]).unwrap());
    assert!(!bridge.config().ack.contains(0));
    assert!(bridge.config().ack.contains(5));
    done(&mut bridge, TxStatus::Ack);
    assert_eq!(bridge.next_request().unwrap().frame.to, 14);
    assert!(!bridge.config().ack.contains(14));
    done(&mut bridge, TxStatus::Nack);
    assert!(bridge.config().ack.contains(14));
    assert!(bridge.config().ack.contains(5));
    assert!(!bridge.config().ack.contains(0));
}

#[test]
fn bridge_late_identical_echo_cannot_stamp_next_host_request() {
    let mut bridge = Bridge::new(Config::default(), 0);
    let frame = Frame::new(0, 4, &[0x36]).unwrap();
    for id in ["a", "b"] {
        bridge
            .submit(
                Submission {
                    id: id.try_into().unwrap(),
                    frame: frame.clone(),
                    retries: None,
                },
                0,
            )
            .unwrap();
    }
    bridge.next_request().unwrap();
    done(&mut bridge, TxStatus::Ack);
    bridge.next_request().unwrap();
    let old_start = u64::from(timing::BIT);
    let final_start = u64::from(timing::SIGNAL_FREE_SAME);
    bridge.on_rx(
        RxEvent::Frame {
            frame,
            ack: true,
            start_us: old_start,
        },
        final_start,
    );
    assert!(
        matches!(bridge.on_tx_done(TxOutcome { status: TxStatus::Ack, attempts: 1 }, final_start),
        Some(BusEvent::Tx { t_us, origin: Origin::Host(id), .. }) if t_us == final_start && id == "b")
    );
}

#[test]
fn bridge_executor_sequences_share_counter_and_wrap() {
    let mut bridge = Bridge::new(Config::default(), u32::MAX - 1);
    assert_eq!(bridge.next_seq(), u32::MAX);
    assert!(matches!(
        bridge.on_rx(RxEvent::Line { stuck_low: true }, 0),
        Some(BusEvent::Line { seq: 0, .. })
    ));
    assert_eq!(bridge.next_seq(), 1);
    assert!(matches!(
        receive(&mut bridge, 4, 0, &[]),
        BusEvent::Rx { seq: 2, .. }
    ));
}

#[test]
fn nack_terminated_echo_is_flushed_before_tx_completion() {
    use cec_core::{Decoder, Tx, TxAction, TxRequest};
    let frame = Frame::new(0, 8, &[0x8F]).unwrap();
    let mut tx = Tx::new();
    tx.start(
        TxRequest {
            frame: frame.clone(),
            retries: 0,
        },
        0,
        0,
        false,
    )
    .unwrap();
    let start = u64::from(timing::SIGNAL_FREE_NEW);
    tx.step(start, 0);
    let mut now = start;
    for index in 0..5 {
        let TxAction::SampleAt { at_us } = tx.step(now, 0) else {
            panic!("sample required")
        };
        now = start + u64::from(at_us);
        tx.line_sample(
            if index == 4 {
                cec_core::Level::High
            } else {
                cec_core::Level::Low
            },
            now,
        );
    }
    let TxAction::Wait { until_us } = tx.step(now, 0) else {
        panic!("must wait for RX")
    };
    let mut decoder = Decoder::default();
    decoder.set_transmitting(true);
    let edges = common::render(&frame, false, start, None);
    for &edge in &edges[..22] {
        assert_eq!(decoder.edge(edge), None);
    }
    assert!(matches!(
        decoder.poll(until_us),
        Some(RxEvent::Frame { ack: false, .. })
    ));
    assert!(
        matches!(tx.step(until_us, decoder.quiet_since_us(until_us)), TxAction::Done { outcome: TxOutcome { status: TxStatus::Nack, .. }, final_attempt_start_us } if final_attempt_start_us == start)
    );
}
