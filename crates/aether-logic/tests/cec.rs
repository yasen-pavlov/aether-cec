//! CEC executor timing, health, listener and concurrent ring regressions.
use aether_logic::{
    bus,
    cec::{Block, EdgeRing, Health, Schedule, ack_in_window},
    config,
};
use aether_proto::{BusEvent, BusState, TxStatus};
use cec_core::{
    bridge::Bridge,
    rx::{AckSet, Decoder, Edge, Level, RxEvent},
    timing,
    tx::{Tx, TxAction},
};

#[test]
fn render_every_byte_start_eom_and_ack() {
    for start in [false, true] {
        for eom in [false, true] {
            for byte in 0..=255 {
                let block = Block::render(start, byte, eom);
                let prefix = usize::from(start);
                assert_eq!(block.len, 10 + prefix);
                if start {
                    assert_eq!(block.symbols[0].low, timing::START_LOW.nominal);
                    assert_eq!(
                        block.symbols[0].low + block.symbols[0].high,
                        timing::START_TOTAL.nominal
                    );
                }
                for bit in 0..8 {
                    let expected = if byte & (1 << (7 - bit)) != 0 {
                        timing::DATA_LOW_1.nominal
                    } else {
                        timing::DATA_LOW_0.nominal
                    };
                    assert_eq!(block.symbols[prefix + bit].low, expected);
                    assert_eq!(
                        block.symbols[prefix + bit].low + block.symbols[prefix + bit].high,
                        timing::BIT
                    );
                }
                for (symbol, low) in [
                    (
                        block.symbols[prefix + 8],
                        if eom {
                            timing::DATA_LOW_1.nominal
                        } else {
                            timing::DATA_LOW_0.nominal
                        },
                    ),
                    (block.symbols[prefix + 9], timing::DATA_LOW_1.nominal),
                ] {
                    assert_eq!(symbol.low, low);
                    assert_eq!(symbol.high, timing::BIT - low);
                    assert_eq!(symbol.low + symbol.high, timing::BIT);
                }
                assert_eq!(
                    block.duration_us(),
                    u64::from(
                        10 * timing::BIT
                            + if start {
                                timing::START_TOTAL.nominal
                            } else {
                                0
                            }
                    )
                );
            }
        }
    }
}

#[test]
fn ring_drops_newest_counts_and_reuses_all_slots() {
    let ring = EdgeRing::<256>::default();
    assert!(ring.is_empty());
    for cycle in 0..4 {
        for index in 0..256 {
            assert!(ring.push(Edge {
                level: if index % 2 == 0 {
                    Level::Low
                } else {
                    Level::High
                },
                t_us: (1_u64 << 40) + cycle * 256 + index
            }));
        }
        assert!(!ring.is_empty());
        for _ in 0..3 {
            assert!(!ring.push(Edge {
                level: Level::Low,
                t_us: 0
            }));
        }
        assert_eq!(ring.take_lost(), 3);
        assert_eq!(ring.take_lost(), 0);
        for index in 0..256 {
            let edge = ring.pop().unwrap();
            assert_eq!(edge.t_us, (1_u64 << 40) + cycle * 256 + index);
            assert_eq!(
                edge.level,
                if index % 2 == 0 {
                    Level::Low
                } else {
                    Level::High
                }
            );
        }
        assert_eq!(ring.pop(), None);
        assert!(ring.is_empty());
    }
}

#[test]
fn ring_concurrent_producer_preserves_full_timestamps() {
    let ring = std::sync::Arc::new(EdgeRing::<512>::default());
    let producer = ring.clone();
    let thread = std::thread::spawn(move || {
        for stamp in 0..20_000_u64 {
            let edge = Edge {
                level: Level::High,
                t_us: (stamp << 32) | stamp,
            };
            while !producer.push(edge) {
                std::thread::yield_now();
            }
        }
    });
    for stamp in 0..20_000_u64 {
        let edge = loop {
            if let Some(edge) = ring.pop() {
                break edge;
            }
            std::thread::yield_now();
        };
        assert_eq!(edge.t_us, (stamp << 32) | stamp);
    }
    thread.join().unwrap();
}

#[test]
fn sample_schedule_uses_absolute_block_origin_and_chains() {
    let mut tx = Tx::new();
    let frame = cec_core::Frame::new(4, 5, &[0x8f]).unwrap();
    tx.start(cec_core::tx::TxRequest { frame, retries: 0 }, 0, 0, false)
        .unwrap();
    let mut schedule = Schedule::default();
    let mut now = u64::from(timing::SIGNAL_FREE_NEW);
    let origin = now;
    let mut offsets = Vec::new();
    loop {
        let action = tx.step(now, 0);
        let deadline = schedule.action(action);
        match action {
            TxAction::SendBlock {
                block_start_us,
                start,
                ..
            } => {
                assert_eq!(
                    schedule.edge(Edge {
                        level: Level::Low,
                        t_us: block_start_us
                    }),
                    Some(block_start_us)
                );
                if !start {
                    assert_eq!(
                        block_start_us,
                        origin + u64::from(timing::START_TOTAL.nominal + 10 * timing::BIT)
                    );
                }
            }
            TxAction::SampleAt { at_us } => {
                now = deadline.unwrap();
                offsets.push(at_us);
                let level =
                    if at_us == timing::START_TOTAL.nominal + timing::BIT + timing::SAMPLE_AT {
                        Level::High
                    } else {
                        Level::Low
                    };
                tx.line_sample(level, now);
            }
            TxAction::Wait { until_us } => now = until_us,
            TxAction::Done { outcome, .. } => {
                assert_eq!(outcome.status, cec_core::TxStatus::Ack);
                break;
            }
            TxAction::Abort => panic!("unexpected abort"),
        }
    }
    assert_eq!(
        offsets,
        [
            timing::START_TOTAL.nominal + timing::SAMPLE_AT,
            timing::START_TOTAL.nominal + timing::BIT + timing::SAMPLE_AT,
            timing::START_TOTAL.nominal + 2 * timing::BIT + timing::SAMPLE_AT,
            timing::START_TOTAL.nominal + 3 * timing::BIT + timing::SAMPLE_AT,
            timing::START_TOTAL.nominal + 9 * timing::BIT + timing::SAMPLE_AT,
            9 * timing::BIT + timing::SAMPLE_AT
        ]
    );
}

#[test]
fn bus_health_boot_low_idle_recovery_and_frame() {
    let boot = 123;
    let mut health = Health::new(boot, Level::Low);
    assert_eq!(health.poll(boot + u64::from(timing::STUCK_LOW)), None);
    assert_eq!(
        health.poll(boot + u64::from(timing::STUCK_LOW) + 1),
        Some(BusState::NoPullup)
    );
    assert_eq!(health.poll(boot + 1_000_000), None);
    health.edge(Edge {
        level: Level::High,
        t_us: boot + 1_000_000,
    });
    health.line(false);
    assert_eq!(health.state(), Some(BusState::Ok));
    health.edge(Edge {
        level: Level::Low,
        t_us: boot + 2_000_000,
    });
    assert_eq!(
        health.poll(boot + 2_000_000 + u64::from(timing::STUCK_LOW) + 1),
        Some(BusState::StuckLow)
    );
    health.frame();
    assert_eq!(health.state(), Some(BusState::Ok));
    let mut idle = Health::new(boot, Level::High);
    assert_eq!(idle.poll(boot + 999_999), None);
    assert_eq!(idle.poll(boot + 1_000_000), Some(BusState::Ok));
    assert_eq!(idle.poll(boot + 2_000_000), None);
}

#[test]
fn valid_submissions_queue_and_busy_retains_request_id() {
    let mut bridge = Bridge::new(config::core_config(&config::defaults()).unwrap(), 7);
    for index in 0..16 {
        assert!(
            bus::submit(
                &mut bridge,
                format!(r#"{{"id":"{index}","to":5,"data":"8F"}}"#).as_bytes(),
                1
            )
            .is_none()
        );
    }
    assert_eq!(bridge.queue_depth(), 16);
    assert!(
        matches!(bus::submit(&mut bridge, br#"{"id":"full","to":5,"data":"8F"}"#, 2), Some(BusEvent::Tx { seq: 8, status: TxStatus::Busy, id: Some(id), attempts: None, reason: None, .. }) if id == "full")
    );
    bridge.next_request().unwrap();
    assert_eq!(bridge.queue_depth(), 15);
    let mut cfg = config::core_config(&config::defaults()).unwrap();
    cfg.la = None;
    let mut bridge = Bridge::new(cfg, 0);
    assert!(
        matches!(bus::submit(&mut bridge, br#"{"id":"auto","to":5,"data":"8F"}"#, 0), Some(BusEvent::Tx { status: TxStatus::Rejected, reason: Some(reason), .. }) if reason == "no_la")
    );
}

#[test]
fn empty_ack_listener_decodes_without_any_ack_due_and_fixed_la_does_not_poll() {
    let mut cfg = config::defaults();
    cfg.ack.clear();
    cfg.replies.clear();
    let mut bridge = Bridge::new(config::core_config(&cfg).unwrap(), 0);
    assert!(bridge.next_request().is_none());
    // Executor deliberately masks Bridge's implicit own ACK entry when wire
    // ack is empty. Decode a poll addressed to fixed LA0 with that effective set.
    let mut decoder = Decoder::new(AckSet::default());
    let mut stamp = 10;
    let block = Block::render(true, 0x40, true);
    let mut events = Vec::new();
    for symbol in &block.symbols[..block.len] {
        if let Some(event) = decoder.edge(Edge {
            level: Level::Low,
            t_us: stamp,
        }) {
            events.push(event);
        }
        stamp += u64::from(symbol.low);
        if let Some(event) = decoder.edge(Edge {
            level: Level::High,
            t_us: stamp,
        }) {
            events.push(event);
        }
        stamp += u64::from(symbol.high);
    }
    while let Some(event) = decoder.poll(stamp) {
        events.push(event);
    }
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, RxEvent::AckDue { .. }))
    );
    let frame = events
        .into_iter()
        .find(|event| matches!(event, RxEvent::Frame { .. }))
        .unwrap();
    bridge.on_rx(frame, stamp);
    assert!(bridge.next_request().is_none());
    assert!(bus::submit(&mut bridge, br#"{"id":"host","to":5,"data":"8F"}"#, stamp).is_none());
    assert!(bridge.next_request().is_some());
}

#[test]
fn block_samples_follow_each_isr_origin_without_accumulating_launch_delay() {
    let mut schedule = Schedule::default();
    for (scheduled, delay, start) in [(10_000, 250, true), (38_750, 280, false)] {
        schedule.action(TxAction::SendBlock {
            block_start_us: scheduled,
            start,
            byte: 0,
            eom: false,
        });
        assert!(schedule.active());
        assert_eq!(
            schedule.action(TxAction::SampleAt {
                at_us: timing::SAMPLE_AT
            }),
            None
        );
        assert_eq!(
            schedule.origin_deadline(),
            Some(scheduled + Schedule::ORIGIN_TIMEOUT_US)
        );
        for edge in [
            Edge {
                level: Level::Low,
                t_us: scheduled - 1,
            },
            Edge {
                level: Level::High,
                t_us: scheduled + 1,
            },
        ] {
            assert_eq!(schedule.edge(edge), None);
        }
        let physical = scheduled + delay;
        assert_eq!(
            schedule.edge(Edge {
                level: Level::Low,
                t_us: physical
            }),
            Some(physical)
        );
        assert_eq!(schedule.origin_deadline(), None);
        assert_eq!(
            schedule.edge(Edge {
                level: Level::Low,
                t_us: physical + 100
            }),
            None
        );
        assert_eq!(
            schedule.action(TxAction::SampleAt {
                at_us: timing::SAMPLE_AT
            }),
            Some(physical + u64::from(timing::SAMPLE_AT))
        );
    }
    schedule.clear();
    assert!(!schedule.active());
    assert_eq!(schedule.origin_deadline(), None);
}

#[test]
fn missing_or_late_origin_never_produces_a_scheduled_sample() {
    for delay in [Schedule::ORIGIN_TIMEOUT_US, Schedule::ORIGIN_TIMEOUT_US + 1] {
        let mut schedule = Schedule::default();
        schedule.action(TxAction::SendBlock {
            block_start_us: 10_000,
            start: false,
            byte: 0,
            eom: true,
        });
        let captured = schedule.edge(Edge {
            level: Level::Low,
            t_us: 10_000 + delay,
        });
        assert_eq!(captured.is_some(), delay == Schedule::ORIGIN_TIMEOUT_US);
        assert_eq!(schedule.origin_deadline().is_some(), captured.is_none());
        assert_eq!(
            schedule
                .action(TxAction::SampleAt {
                    at_us: timing::SAMPLE_AT
                })
                .is_some(),
            captured.is_some()
        );
    }
    let boundary = 10_000 - Schedule::CHAIN_MARGIN_US;
    assert!(Schedule::can_chain(boundary - 1, 10_000));
    assert!(!Schedule::can_chain(boundary, 10_000));
    assert!(!Schedule::can_chain(boundary + 1, 10_000));
}

#[test]
fn ack_window_uses_full_timestamp_across_low_word_wrap() {
    let at = (1_u64 << 32) + 100;
    let early = u64::from(timing::BIT - timing::DATA_TOTAL.min);
    let late = u64::from(timing::DATA_TOTAL.max - timing::BIT);
    assert!(ack_in_window(at - early, at));
    assert!(!ack_in_window(at - early - 1, at));
    assert!(ack_in_window(at + late, at));
    assert!(!ack_in_window(at + late + 1, at));
    assert!(!ack_in_window(at + (1_u64 << 32), at));
    assert!(!ack_in_window(at - (1_u64 << 32), at));
}

#[test]
fn overrun_report_retries_preserve_count_and_sequence_until_accepted() {
    let mut bridge = Bridge::new(config::core_config(&config::defaults()).unwrap(), u32::MAX);
    let mut tries = 0;
    for _ in 0..2000 {
        assert!(!bus::report_overrun(&mut bridge, 1, 10, |event| {
            assert!(matches!(
                event,
                BusEvent::Err {
                    seq: 0,
                    dropped: Some(1),
                    ..
                }
            ));
            tries += 1;
            false
        }));
        assert_eq!(bridge.seq(), u32::MAX);
    }
    assert_eq!(tries, 2000);
    assert!(bus::report_overrun(&mut bridge, 1, 11, |event| {
        assert!(matches!(
            event,
            BusEvent::Err {
                seq: 0,
                dropped: Some(1),
                ..
            }
        ));
        true
    }));
    assert_eq!(bridge.seq(), 0);
    assert!(bus::report_overrun(&mut bridge, 0, 12, |_| panic!(
        "no losses"
    )));
    assert_eq!(bridge.seq(), 0);
}

#[test]
fn launch_guard_distinguishes_bus_acquisition_from_ack_time_queueing() {
    let header = Block::render(true, 0x04, false);
    let chained = Block::render(false, 0x90, true);
    let end = u64::from(timing::START_TOTAL.nominal + 10 * timing::BIT);
    let ack_sample = end - u64::from(timing::BIT - timing::SAMPLE_AT);
    assert!(header.can_launch(0, 0, Level::High));
    assert!(!header.can_launch(0, 0, Level::Low));
    for level in [Level::Low, Level::High] {
        assert!(chained.can_launch(ack_sample, end, level));
        assert!(chained.can_launch(end - Schedule::CHAIN_MARGIN_US - 1, end, level));
        assert!(!chained.can_launch(end - Schedule::CHAIN_MARGIN_US, end, level));
        assert!(!chained.can_launch(end, end, level));
    }
}

/// Render the wired-AND echo, including a follower stretching only the ACK low.
/// Queueing a block does not advance the physical line to its future origin.
fn transmit_with_echo(frame: &cec_core::Frame, ack_low: u32, launch_delay: u64) {
    let mut tx = Tx::new();
    tx.start(
        cec_core::tx::TxRequest {
            frame: frame.clone(),
            retries: 0,
        },
        0,
        0,
        false,
    )
    .unwrap();
    let mut decoder = Decoder::new(AckSet::new(&[0]));
    decoder.set_transmitting(true);
    let mut schedule = Schedule::default();
    let mut edges = std::collections::VecDeque::<Edge>::new();
    let mut events = Vec::new();
    let mut level = Level::High;
    let mut now = u64::from(timing::SIGNAL_FREE_NEW);
    let mut sample = None;
    let mut blocks = 0;
    let mut queued_while_low = false;
    for _ in 0..1000 {
        while edges.front().is_some_and(|edge| edge.t_us <= now) {
            let edge = edges.pop_front().unwrap();
            level = edge.level;
            if let Some(origin) = schedule.edge(edge) {
                tx.block_started(origin);
            }
            if let Some(event) = decoder.edge(edge) {
                observe_echo(event, &mut tx, &mut schedule, now, &mut events);
            }
        }
        while let Some(event) = decoder.poll(now) {
            observe_echo(event, &mut tx, &mut schedule, now, &mut events);
        }
        if let Some((deadline, offset)) = sample {
            if now >= deadline {
                if schedule.driven_low(offset) && level != Level::Low {
                    tx.line_error(now);
                } else {
                    tx.line_sample(level, now);
                }
                sample = None;
            }
        }
        let action = if let Some(deadline) = schedule.origin_deadline() {
            assert!(now < deadline, "missing block origin");
            TxAction::Wait { until_us: deadline }
        } else {
            tx.step(now, decoder.quiet_since_us(now))
        };
        let deadline = schedule.action(action);
        match action {
            TxAction::SendBlock {
                start,
                byte,
                eom,
                block_start_us,
            } => {
                let block = Block::render(start, byte, eom);
                assert!(block.can_launch(now, block_start_us, level));
                queued_while_low |= !start && level == Level::Low;
                blocks += 1;
                let origin = block_start_us + if start { launch_delay } else { 0 };
                queue_echo(&block, origin, ack_low, &mut edges);
            }
            TxAction::SampleAt { at_us } => sample = Some((deadline.unwrap(), at_us)),
            TxAction::Wait { until_us } => {
                now = until_us;
                if let Some(edge) = edges.front() {
                    now = now.min(edge.t_us);
                }
                if let Some((deadline, _)) = sample {
                    now = now.min(deadline);
                }
            }
            TxAction::Abort => panic!("valid ACK echo must not abort"),
            TxAction::Done { outcome, .. } => {
                assert_echo_outcome(frame, ack_low, outcome, blocks, queued_while_low, &events);
                return;
            }
        }
    }
    panic!("executor failed to complete");
}

fn queue_echo(
    block: &Block,
    origin: u64,
    ack_low: u32,
    edges: &mut std::collections::VecDeque<Edge>,
) {
    let mut fall = origin;
    for (index, symbol) in block.symbols[..block.len].iter().enumerate() {
        edges.push_back(Edge {
            level: Level::Low,
            t_us: fall,
        });
        edges.push_back(Edge {
            level: Level::High,
            t_us: fall
                + u64::from(if index + 1 == block.len {
                    ack_low
                } else {
                    symbol.low
                }),
        });
        fall += u64::from(symbol.low + symbol.high);
    }
}

fn observe_echo(
    event: RxEvent,
    tx: &mut Tx,
    schedule: &mut Schedule,
    now: u64,
    events: &mut Vec<RxEvent>,
) {
    if matches!(event, RxEvent::Error { .. }) && schedule.active() {
        tx.line_error(now);
        schedule.clear();
    }
    events.push(event);
}

fn assert_echo_outcome(
    frame: &cec_core::Frame,
    ack_low: u32,
    outcome: cec_core::TxOutcome,
    blocks: usize,
    queued_while_low: bool,
    events: &[RxEvent],
) {
    let ack = if frame.is_broadcast() {
        ack_low <= timing::SAMPLE_AT
    } else {
        ack_low > timing::SAMPLE_AT
    };
    assert_eq!(outcome.attempts, 1);
    assert_eq!(blocks, if ack { frame.blocks() } else { 1 });
    assert_eq!(
        outcome.status,
        if ack {
            cec_core::TxStatus::Ack
        } else {
            cec_core::TxStatus::Nack
        }
    );
    assert_eq!(queued_while_low, ack && ack_low > timing::SAMPLE_AT);
    assert_eq!(events.len(), 1, "exactly one echo event, no own ACK");
    if !ack && frame.is_broadcast() {
        // Only directed NACKs complete truncated frames in Decoder.
        assert!(matches!(
            &events[0],
            RxEvent::Error {
                kind: cec_core::rx::RxError::BitTiming,
                bits: 8,
                ..
            }
        ));
    } else {
        assert!(matches!(
            &events[0],
            RxEvent::Frame { frame: echo, ack: observed, .. }
                if *observed == ack && echo.from == frame.from
                    && echo.to == frame.to
                    && echo.data.as_slice() == if ack { frame.data.as_slice() } else { &[] }
        ));
    }
}

#[test]
fn directed_ack_low_queues_following_blocks_and_decodes_own_echo() {
    for (to, data) in [(5, &[0x8f][..]), (4, &[0x90, 0x00]), (4, &[0x9e, 0x05])] {
        for ack_low in [
            timing::DATA_LOW_0.min,
            timing::ACK_LOW,
            timing::DATA_LOW_0.max,
        ] {
            for delay in [0, Schedule::ORIGIN_TIMEOUT_US - 1] {
                transmit_with_echo(&cec_core::Frame::new(0, to, data).unwrap(), ack_low, delay);
            }
        }
    }
}

#[test]
fn directed_high_ack_sample_is_nack_not_line_error() {
    transmit_with_echo(
        &cec_core::Frame::new(0, 5, &[0x8f]).unwrap(),
        timing::DATA_LOW_1.nominal,
        0,
    );
}

#[test]
fn broadcast_ack_polarity_and_zero_initiator_samples() {
    for data in [&[0x85][..], &[0x87, 0x01, 0x23, 0x45]] {
        for ack_low in [timing::DATA_LOW_1.nominal, timing::ACK_LOW] {
            transmit_with_echo(
                &cec_core::Frame::new(0, cec_core::BROADCAST, data).unwrap(),
                ack_low,
                Schedule::ORIGIN_TIMEOUT_US - 1,
            );
        }
    }
}
