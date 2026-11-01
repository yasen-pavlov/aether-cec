//! Receive timing and real-bus fixture acceptance tests.
mod common;
use cec_core::{AckSet, Bridge, Config, Decoder, Edge, Frame, Level, RxError, RxEvent, timing};
use common::{fixture_frame, render};
fn decode(edges: &[Edge], ack: AckSet) -> Vec<RxEvent> {
    let mut decoder = Decoder::new(ack);
    edges
        .iter()
        .filter_map(|&edge| decoder.edge(edge))
        .collect()
}
fn frame_event(events: &[RxEvent]) -> Option<&Frame> {
    events.iter().find_map(|ev| {
        if let RxEvent::Frame { frame, .. } = ev {
            Some(frame)
        } else {
            None
        }
    })
}
#[test]
fn rx_fixture_round_trip_every_frame_and_ack_polarity() {
    let fixture = include_str!("../../../tests/fixtures/rx-frames.jsonl");
    assert_eq!(fixture.lines().count(), 79);
    for line in fixture.lines() {
        let frame = fixture_frame(line);
        for ack in [false, true] {
            let events = decode(&render(&frame, ack, 0, None), AckSet::default());
            assert_eq!(
                events,
                [RxEvent::Frame {
                    frame: frame.clone(),
                    ack,
                    start_us: 0
                }],
                "{line}"
            );
        }
    }
}
#[test]
fn rx_jitter_sweep_low_windows_inclusive_and_one_outside() {
    let frame = Frame::new(4, 0, &[0x8F]).unwrap();
    // Header bit 0 is zero, bit 1 is one.
    for (rise, window, error) in [
        (1, timing::START_LOW, RxError::StartBit),
        (3, timing::DATA_LOW_0, RxError::BitTiming),
        (5, timing::DATA_LOW_1, RxError::BitTiming),
    ] {
        for (duration, valid) in [
            (window.min - 1, false),
            (window.min, true),
            (window.max, true),
            (window.max + 1, false),
        ] {
            let delta = i64::from(duration) - i64::from(window.nominal);
            let edges = render(
                &frame,
                true,
                0,
                Some(&|i| if i == rise { delta } else { 0 }),
            );
            let events = decode(&edges, AckSet::default());
            if valid {
                assert_eq!(frame_event(&events), Some(&frame));
            } else {
                assert!(
                    matches!(events.first(), Some(RxEvent::Error { kind, .. }) if *kind == error)
                );
            }
        }
    }
}
#[test]
fn rx_jitter_sweep_total_windows_inclusive_and_one_outside() {
    let frame = Frame::new(4, 0, &[0x8F]).unwrap();
    for (fall, window, error) in [
        (2, timing::START_TOTAL, RxError::StartBit),
        (4, timing::DATA_TOTAL, RxError::BitTiming),
    ] {
        for (duration, valid) in [
            (window.min - 1, false),
            (window.min, true),
            (window.max, true),
            (window.max + 1, false),
        ] {
            let delta = i64::from(duration) - i64::from(window.nominal);
            let edges = render(
                &frame,
                true,
                0,
                Some(&|i| if i >= fall { delta } else { 0 }),
            );
            let events = decode(&edges, AckSet::default());
            if valid {
                assert_eq!(frame_event(&events), Some(&frame));
            } else {
                assert!(
                    matches!(events.first(), Some(RxEvent::Error { kind, .. }) if *kind == error)
                );
            }
        }
    }
}
#[test]
fn rx_sixteen_blocks_without_eom_is_eom_missing() {
    let frame = Frame::new(1, 0, &[0; 15]).unwrap();
    let mut edges = render(&frame, true, 0, None);
    let eom_rise = 2 + 15 * 20 + 17;
    edges[eom_rise].t_us += u64::from(timing::DATA_LOW_0.nominal - timing::DATA_LOW_1.nominal);
    assert!(matches!(
        decode(&edges, AckSet::default()).last(),
        Some(RxEvent::Error {
            kind: RxError::EomMissing,
            bits: 128,
            ..
        })
    ));
}
#[test]
fn rx_stuck_low_and_missing_edges_flush_and_recover() {
    let mut decoder = Decoder::default();
    decoder.edge(Edge {
        level: Level::Low,
        t_us: 0,
    });
    assert!(decoder.busy());
    assert_eq!(decoder.poll(u64::from(timing::START_TOTAL.max)), None);
    assert!(matches!(
        decoder.poll(u64::from(timing::START_TOTAL.max) + 1),
        Some(RxEvent::Error {
            kind: RxError::LineLow,
            ..
        })
    ));
    assert!(!decoder.busy());
    let recover = u64::from(timing::SIGNAL_FREE_NEW);
    decoder.edge(Edge {
        level: Level::High,
        t_us: recover,
    });
    let frame = Frame::new(4, 0, &[]).unwrap();
    let events: Vec<_> = render(&frame, true, recover * 2, None)
        .into_iter()
        .filter_map(|edge| decoder.edge(edge))
        .collect();
    assert_eq!(frame_event(&events), Some(&frame));
    let mut decoder = Decoder::default();
    decoder.edge(Edge {
        level: Level::Low,
        t_us: 0,
    });
    decoder.edge(Edge {
        level: Level::High,
        t_us: u64::from(timing::START_LOW.nominal),
    });
    assert!(matches!(
        decoder.poll(u64::from(timing::START_TOTAL.max) + 1),
        Some(RxEvent::Error {
            kind: RxError::StartBit,
            ..
        })
    ));
}
#[test]
fn rx_ack_due_every_block_only_members_including_unregistered_initiator() {
    for from in [4, 15] {
        for to in [0, 5, 15] {
            let frame = Frame::new(from, to, &[0x90, 0]).unwrap();
            let edges = render(&frame, false, 0, None);
            let events = decode(&edges, AckSet::new(&[0, 15]));
            let due: Vec<_> = events
                .iter()
                .filter_map(|ev| {
                    if let RxEvent::AckDue { fall_at_us } = ev {
                        Some(*fall_at_us)
                    } else {
                        None
                    }
                })
                .collect();
            if to == 0 {
                assert_eq!(due.len(), frame.blocks());
                for (block, &at) in due.iter().enumerate() {
                    assert_eq!(at, edges[2 + block * 20 + 18].t_us);
                }
                assert!(matches!(
                    events.last(),
                    Some(RxEvent::Frame { ack: false, .. })
                ));
            } else {
                assert_eq!(due, [] as [u64; 0]);
            }
        }
    }
}
#[test]
fn rx_error_retains_decoded_header_and_data_bit_count() {
    let frame = Frame::new(4, 5, &[0x8F]).unwrap();
    let mut edges = render(&frame, true, 0, None);
    edges[23].t_us = edges[22].t_us + u64::from(timing::DATA_LOW_0.max) + 1;
    assert!(matches!(
        decode(&edges, AckSet::default()).first(),
        Some(RxEvent::Error {
            from: Some(4),
            to: Some(5),
            bits: 8,
            kind: RxError::BitTiming,
            ..
        })
    ));
}
#[test]
fn kodi_boot_storm_all_120_decode_and_seq_monotonic() {
    let fixture = include_str!("../../../tests/fixtures/kodi-boot-storm.jsonl");
    let mut decoder = Decoder::new(AckSet::new(&[0]));
    let mut bridge = Bridge::new(Config::default(), 0);
    let mut start = 0;
    let mut count = 0;
    for line in fixture.lines() {
        let frame = fixture_frame(line);
        let edges = render(&frame, true, start, None);
        start = edges.last().unwrap().t_us + u64::from(timing::SIGNAL_FREE_NEW);
        for edge in edges {
            if let Some(ev) = decoder.edge(edge) {
                assert!(!matches!(ev, RxEvent::Error { .. }));
                if let Some(event) = bridge.on_rx(ev, edge.t_us) {
                    count += 1;
                    assert!(
                        matches!(event, cec_core::bridge::BusEvent::Rx { seq, .. } if seq == count)
                    );
                }
            }
        }
    }
    assert_eq!(count, 120);
}

#[test]
fn rx_mid_frame_join_and_error_remainder_do_not_storm() {
    let frame = Frame::new(4, 0, b"\x47ABC").unwrap();
    let edges = render(&frame, true, 0, None);
    assert_eq!(decode(&edges[4..], AckSet::default()), [] as [RxEvent; 0]);
    let mut broken = edges.clone();
    broken[5].t_us = broken[4].t_us + u64::from(timing::DATA_LOW_0.max) + 1;
    let events = decode(&broken, AckSet::default());
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, RxEvent::Error { .. }))
            .count(),
        1
    );
    let mut decoder = Decoder::default();
    for edge in broken {
        decoder.edge(edge);
    }
    let start = edges.last().unwrap().t_us + u64::from(timing::SIGNAL_FREE_NEW);
    let recovered: Vec<_> = render(&frame, true, start, None)
        .into_iter()
        .filter_map(|edge| decoder.edge(edge))
        .collect();
    assert_eq!(frame_event(&recovered), Some(&frame));
}

#[test]
fn rx_poll_before_captured_late_edge_does_not_change_edge_order() {
    let frame = Frame::new(4, 0, &[0x8F]).unwrap();
    let edges = render(&frame, true, 0, None);
    let mut decoder = Decoder::default();
    let mut events = Vec::new();
    for edge in edges {
        if edge.level == Level::High {
            assert_eq!(decoder.poll(edge.t_us + 1), None);
        }
        events.extend(decoder.edge(edge));
    }
    assert_eq!(frame_event(&events), Some(&frame));
    assert!(!events.iter().any(|e| matches!(e, RxEvent::Error { .. })));
}

#[test]
fn rx_nack_truncated_zero_to_eight_8f_is_a_complete_header() {
    let frame = Frame::new(0, 8, &[0x8F]).unwrap();
    let edges = render(&frame, false, 0, None);
    let mut decoder = Decoder::default();
    for &edge in &edges[..22] {
        assert_eq!(decoder.edge(edge), None);
    }
    let ack_fall = edges[20].t_us;
    assert_eq!(
        decoder.poll(ack_fall + u64::from(timing::DATA_TOTAL.max)),
        None
    );
    assert_eq!(
        decoder.poll(ack_fall + u64::from(timing::DATA_TOTAL.max) + 1),
        Some(RxEvent::Frame {
            frame: Frame::new(0, 8, &[]).unwrap(),
            ack: false,
            start_us: 0
        })
    );
    assert_eq!(
        decoder.quiet_since_us(ack_fall + u64::from(timing::SIGNAL_FREE_NEW)),
        ack_fall + u64::from(timing::BIT)
    );
}

#[test]
fn rx_own_allocation_poll_never_acks_itself_and_reports_sampled_level() {
    let frame = Frame::new(0, 0, &[]).unwrap();
    let mut decoder = Decoder::new(AckSet::new(&[0]));
    decoder.set_transmitting(true);
    let events: Vec<_> = render(&frame, false, 0, None)
        .into_iter()
        .filter_map(|edge| decoder.edge(edge))
        .collect();
    assert_eq!(
        events,
        [RxEvent::Frame {
            frame,
            ack: false,
            start_us: 0
        }]
    );
}

#[test]
fn rx_ack_sample_boundary_and_quiet_origin_are_exact() {
    for low in [
        timing::SAMPLE_AT - 1,
        timing::SAMPLE_AT,
        timing::SAMPLE_AT + 1,
    ] {
        let frame = Frame::new(4, 0, &[]).unwrap();
        let mut edges = render(&frame, true, 0, None);
        let ack_fall = edges[20].t_us;
        edges[21].t_us = ack_fall + u64::from(low);
        let mut decoder = Decoder::new(AckSet::new(&[0]));
        let events: Vec<_> = edges
            .into_iter()
            .filter_map(|edge| decoder.edge(edge))
            .collect();
        assert!(
            matches!(events.last(), Some(RxEvent::Frame { ack, .. }) if *ack == (low > timing::SAMPLE_AT))
        );
        assert_eq!(
            decoder.last_activity_us(),
            ack_fall + u64::from(timing::BIT)
        );
    }
}

#[test]
fn rx_short_low_is_frame_error_not_stuck_and_long_low_recovers_once() {
    let mut decoder = Decoder::default();
    let mut bridge = Bridge::new(Config::default(), 0);
    decoder.edge(Edge {
        level: Level::Low,
        t_us: 0,
    });
    let glitch = u64::from(4 * timing::DATA_LOW_0.nominal);
    let error = decoder.poll(glitch).unwrap();
    assert!(matches!(
        bridge.on_rx(error, glitch),
        Some(cec_core::bridge::BusEvent::Err {
            kind: RxError::LineLow,
            ..
        })
    ));
    assert_eq!(
        decoder.edge(Edge {
            level: Level::High,
            t_us: glitch
        }),
        None
    );
    assert_eq!(decoder.poll(u64::from(timing::STUCK_LOW) + 1), None);
    let mut decoder = Decoder::default();
    let mut bridge = Bridge::new(Config::default(), 0);
    decoder.edge(Edge {
        level: Level::Low,
        t_us: 0,
    });
    let error = decoder
        .poll(u64::from(timing::START_TOTAL.max) + 1)
        .unwrap();
    assert!(matches!(
        bridge.on_rx(error, glitch),
        Some(cec_core::bridge::BusEvent::Err {
            seq: 1,
            kind: RxError::LineLow,
            ..
        })
    ));
    assert_eq!(decoder.poll(u64::from(timing::STUCK_LOW)), None);
    let at = u64::from(timing::STUCK_LOW) + 1;
    let stuck = decoder.poll(at).unwrap();
    assert_eq!(stuck, RxEvent::Line { stuck_low: true });
    assert!(matches!(
        bridge.on_rx(stuck, at),
        Some(cec_core::bridge::BusEvent::Line {
            seq: 2,
            stuck_low: true,
            ..
        })
    ));
    assert_eq!(decoder.poll(at + u64::from(timing::BIT)), None);
    let recovery = decoder
        .edge(Edge {
            level: Level::High,
            t_us: at + u64::from(timing::BIT),
        })
        .unwrap();
    assert!(matches!(
        bridge.on_rx(recovery, at),
        Some(cec_core::bridge::BusEvent::Line {
            seq: 3,
            stuck_low: false,
            ..
        })
    ));
    assert_eq!(decoder.poll(at + u64::from(timing::SIGNAL_FREE_NEW)), None);
}

fn detail(event: RxEvent) -> cec_core::rx::ErrorDetail {
    let RxEvent::Error { detail, .. } = event else {
        panic!("expected a decoder error, got {event:?}");
    };
    detail
}

fn edge_intervals(edges: &[Edge]) -> Vec<u32> {
    edges
        .windows(2)
        .map(|pair| u32::try_from(pair[1].t_us.saturating_sub(pair[0].t_us)).unwrap_or(u32::MAX))
        .collect()
}

#[test]
fn rx_same_level_glitch_preserves_spacing_and_preceding_history() {
    let frame = Frame::new(4, 0, &[]).unwrap();
    let mut edges = render(&frame, true, 0, None);
    let mut decoder = Decoder::default();
    for &edge in &edges[..4] {
        assert_eq!(decoder.edge(edge), None);
    }
    edges.insert(
        4,
        Edge {
            level: edges[3].level,
            t_us: edges[3].t_us + 7,
        },
    );
    let evidence = detail(decoder.edge(edges[4]).unwrap());
    assert_eq!(evidence.level, Some(Level::High));
    assert!(evidence.same_level);
    assert_eq!(evidence.gap_us, edge_intervals(&edges[3..=4])[0]);
    assert_eq!(
        evidence.duration_us,
        edge_intervals(&edges[2..=4]).iter().sum::<u32>()
    );
    assert_eq!(evidence.bit, Some(1));
    assert_eq!(evidence.history.as_slice(), edge_intervals(&edges[..4]));
}

#[test]
fn rx_short_data_period_reports_current_bit_and_history_before_failing_edge() {
    let frame = Frame::new(4, 0, &[]).unwrap();
    let mut edges = render(&frame, true, 0, None);
    let fall = 2 + 5 * 2;
    edges[fall].t_us = edges[fall - 2].t_us + u64::from(timing::DATA_TOTAL.min - 50);
    let evidence = detail(decode(&edges[..=fall], AckSet::default()).remove(0));
    assert_eq!(evidence.level, Some(Level::Low));
    assert!(!evidence.same_level);
    assert_eq!(
        evidence.duration_us,
        edge_intervals(&edges[fall - 2..=fall]).iter().sum::<u32>()
    );
    assert_eq!(evidence.bit, Some(5));
    assert_eq!(
        evidence.history.as_slice(),
        edge_intervals(&edges[fall - 9..fall])
    );
    assert_eq!(evidence.gap_us, edge_intervals(&edges[fall - 1..=fall])[0]);
}

#[test]
fn rx_long_ack_low_reports_ack_position_and_exact_width() {
    let frame = Frame::new(4, 0, &[]).unwrap();
    let mut edges = render(&frame, true, 0, None);
    let rise = edges.len() - 1;
    edges[rise].t_us = edges[rise - 1].t_us + u64::from(timing::DATA_LOW_0.max + 50);
    let evidence = detail(decode(&edges, AckSet::default()).remove(0));
    assert_eq!(evidence.level, Some(Level::High));
    assert_eq!(evidence.bit, Some(9));
    let width = edge_intervals(&edges[rise - 1..=rise])[0];
    assert_eq!(evidence.duration_us, width);
    assert_eq!(evidence.gap_us, width);
    assert_eq!(
        evidence.history.as_slice(),
        edge_intervals(&edges[rise - 9..rise])
    );
}

#[test]
fn rx_eom_and_ack_period_failures_report_the_next_due_bit() {
    let frame = Frame::new(4, 0, &[0x8F]).unwrap();
    for (fall, bit) in [(20, 9), (22, 0)] {
        let mut edges = render(&frame, true, 0, None);
        edges[fall].t_us = edges[fall - 2].t_us + u64::from(timing::DATA_TOTAL.min - 50);
        let events = decode(&edges[..=fall], AckSet::default());
        let [
            event @ RxEvent::Error {
                kind: RxError::BitTiming,
                bits: 8,
                ..
            },
        ] = events.as_slice()
        else {
            panic!("expected one block-boundary timing error, got {events:?}");
        };
        let evidence = detail(event.clone());
        assert_eq!(evidence.level, Some(Level::Low));
        assert!(!evidence.same_level);
        assert_eq!(evidence.bit, Some(bit));
        assert_eq!(
            evidence.duration_us,
            edge_intervals(&edges[fall - 2..=fall]).iter().sum::<u32>()
        );
        assert_eq!(evidence.gap_us, edge_intervals(&edges[fall - 1..=fall])[0]);
        assert_eq!(
            evidence.history.as_slice(),
            edge_intervals(&edges[fall - 9..fall])
        );
    }
}

#[test]
fn rx_poll_timeouts_have_no_edge_and_keep_elapsed_duration() {
    let frame = Frame::new(4, 0, &[]).unwrap();
    let edges = render(&frame, true, 0, None);
    for (count, limit, bit) in [
        (1, timing::START_TOTAL.max, None),
        (2, timing::START_TOTAL.max, None),
        (3, timing::START_TOTAL.max, Some(0)),
        (4, timing::DATA_TOTAL.max, Some(1)),
    ] {
        let mut decoder = Decoder::default();
        for &edge in &edges[..count] {
            assert_eq!(decoder.edge(edge), None);
        }
        let fall_index = (count - 1) & !1;
        let now = edges[fall_index].t_us + u64::from(limit) + 1;
        let timeout = Edge {
            level: edges[count - 1].level,
            t_us: now,
        };
        let evidence = detail(decoder.poll(now).unwrap());
        assert_eq!(evidence.level, None);
        assert!(!evidence.same_level);
        assert_eq!(evidence.gap_us, 0);
        assert_eq!(
            evidence.duration_us,
            edge_intervals(&[edges[fall_index], timeout])[0]
        );
        assert_eq!(evidence.history.as_slice(), edge_intervals(&edges[..count]));
        assert_eq!(evidence.bit, bit);
    }
}

#[test]
fn rx_history_resets_on_start_and_includes_saturated_preceding_idle_gap() {
    let frame = Frame::new(4, 0, &[]).unwrap();
    let mut decoder = Decoder::default();
    let first = render(&frame, true, 0, None);
    for &edge in &first {
        decoder.edge(edge);
    }
    let start = first.last().unwrap().t_us + u64::from(u32::MAX) + 1;
    let mut next = render(&frame, true, start, None);
    next[1].t_us = start + u64::from(timing::START_LOW.max) + 1;
    assert_eq!(decoder.edge(next[0]), None);
    let evidence = detail(decoder.edge(next[1]).unwrap());
    assert_eq!(
        evidence.history.as_slice(),
        edge_intervals(&[*first.last().unwrap(), next[0]])
    );
    assert_eq!(evidence.bit, None);
    assert_eq!(evidence.duration_us, edge_intervals(&next[..2])[0]);
    assert_eq!(evidence.level, Some(Level::High));
}

#[test]
fn rx_out_of_order_edge_and_large_timeout_saturate_without_wrapping() {
    let frame = Frame::new(4, 0, &[]).unwrap();
    let edges = render(&frame, true, 0, None);
    let mut decoder = Decoder::default();
    for &edge in &edges[..4] {
        decoder.edge(edge);
    }
    let out_of_order = Edge {
        level: Level::Low,
        t_us: 0,
    };
    let evidence = detail(decoder.edge(out_of_order).unwrap());
    assert!(!evidence.same_level);
    assert_eq!(
        evidence.gap_us,
        edge_intervals(&[edges[3], out_of_order])[0]
    );
    assert_eq!(
        evidence.duration_us,
        edge_intervals(&[edges[2], out_of_order])[0]
    );
    assert_eq!(evidence.history.as_slice(), edge_intervals(&edges[..4]));
    let mut decoder = Decoder::default();
    decoder.edge(edges[0]);
    let timeout = Edge {
        level: edges[0].level,
        t_us: u64::MAX,
    };
    let evidence = detail(decoder.poll(timeout.t_us).unwrap());
    assert_eq!(
        evidence.duration_us,
        edge_intervals(&[edges[0], timeout])[0]
    );
    assert_eq!(evidence.gap_us, 0);
    assert_eq!(evidence.history.as_slice(), edge_intervals(&edges[..1]));
}

#[test]
fn rx_bridge_preserves_diagnostic_evidence_unchanged() {
    let frame = Frame::new(4, 0, &[]).unwrap();
    let edges = render(&frame, true, 0, None);
    let mut decoder = Decoder::default();
    decoder.edge(edges[0]);
    let event = decoder
        .poll(u64::from(timing::START_TOTAL.max) + 1)
        .unwrap();
    let expected = detail(event.clone());
    let mut bridge = Bridge::new(Config::default(), 0);
    let Some(cec_core::bridge::BusEvent::Err { detail, .. }) = bridge.on_rx(event, 0) else {
        panic!("expected bridge error");
    };
    assert_eq!(detail, expected);
}
