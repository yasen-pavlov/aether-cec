//! CEC executor timing, health, listener and concurrent ring regressions.
use aether_logic::{
    bus,
    cec::{AttemptFailure, Block, EdgeRing, Health, Schedule, ack_in_window},
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
    let mut losses = bus::Losses::default();
    losses.add(1);
    let mut tries = 0;
    for _ in 0..2000 {
        assert!(!losses.report(&mut bridge, 10, |event| {
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
    assert!(losses.report(&mut bridge, 11, |event| {
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
    assert!(losses.report(&mut bridge, 12, |_| panic!("no losses")));
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
    transmit_with_echo_after_preparation(frame, ack_low, launch_delay, 0);
}

fn transmit_with_echo_after_preparation(
    frame: &cec_core::Frame,
    ack_low: u32,
    launch_delay: u64,
    preparation_us: u64,
) {
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
        drain_echo(
            &mut tx,
            &mut decoder,
            &mut schedule,
            &mut edges,
            &mut events,
            &mut level,
            now,
        );
        sample_echo(&mut tx, &schedule, level, now, &mut sample);
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
                let origin = if start {
                    now += preparation_us;
                    schedule.submitted(now);
                    now + launch_delay
                } else {
                    block_start_us
                };
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

fn drain_echo(
    tx: &mut Tx,
    decoder: &mut Decoder,
    schedule: &mut Schedule,
    edges: &mut std::collections::VecDeque<Edge>,
    events: &mut Vec<RxEvent>,
    level: &mut Level,
    now: u64,
) {
    while edges.front().is_some_and(|edge| edge.t_us <= now) {
        let edge = edges.pop_front().unwrap();
        *level = edge.level;
        if let Some(origin) = schedule.edge(edge) {
            tx.block_started(origin);
        }
        if let Some(event) = decoder.edge(edge) {
            observe_echo(event, tx, schedule, now, events);
        }
    }
    while let Some(event) = decoder.poll(now) {
        observe_echo(event, tx, schedule, now, events);
    }
}

fn sample_echo(
    tx: &mut Tx,
    schedule: &Schedule,
    level: Level,
    now: u64,
    sample: &mut Option<(u64, u32)>,
) {
    if let Some((deadline, offset)) = *sample {
        if now >= deadline {
            if schedule.driven_low(offset) && level != Level::Low {
                tx.line_error(now);
            } else {
                tx.line_sample(level, now);
            }
            *sample = None;
        }
    }
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

#[test]
fn fresh_host_launch_excludes_preparation_from_origin_timeout() {
    // Rendering/copying/routing can consume the entire old 300-us window before
    // rmt_transmit is even called. Keep the window at 300 us after submission.
    for preparation in [0, Schedule::ORIGIN_TIMEOUT_US + 1, 5_000] {
        for (to, data, ack_low) in [
            (5, &[0x8f][..], timing::ACK_LOW),
            (4, &[0x8f][..], timing::ACK_LOW),
            (cec_core::BROADCAST, &[0x85][..], timing::DATA_LOW_1.nominal),
            (
                cec_core::BROADCAST,
                &[0x87, 1, 0x23, 0x45][..],
                timing::DATA_LOW_1.nominal,
            ),
        ] {
            transmit_with_echo_after_preparation(
                &cec_core::Frame::new(0, to, data).unwrap(),
                ack_low,
                Schedule::ORIGIN_TIMEOUT_US - 1,
                preparation,
            );
        }
    }
}

#[test]
fn submission_reanchors_only_fresh_headers_and_keeps_origin_window_strict() {
    for start in [true, false] {
        let mut schedule = Schedule::default();
        schedule.action(TxAction::SendBlock {
            block_start_us: 10_000,
            start,
            byte: 0,
            eom: true,
        });
        schedule.submitted(12_000);
        let expected = if start { 12_000 } else { 10_000 };
        assert_eq!(
            schedule.origin_deadline(),
            Some(expected + Schedule::ORIGIN_TIMEOUT_US)
        );
        assert_eq!(
            schedule.edge(Edge {
                level: Level::Low,
                t_us: expected + Schedule::ORIGIN_TIMEOUT_US + 1,
            }),
            None
        );
        assert_eq!(
            schedule.edge(Edge {
                level: Level::Low,
                t_us: expected + Schedule::ORIGIN_TIMEOUT_US,
            }),
            Some(expected + Schedule::ORIGIN_TIMEOUT_US)
        );
        schedule.submitted(99_000);
        assert_eq!(schedule.origin_deadline(), None);
        assert_eq!(
            schedule.action(TxAction::SampleAt {
                at_us: timing::SAMPLE_AT,
            }),
            Some(expected + Schedule::ORIGIN_TIMEOUT_US + u64::from(timing::SAMPLE_AT))
        );
    }
}

#[test]
fn attempt_sample_diagnostics_follow_arbitration_and_slack_boundaries() {
    let mut schedule = Schedule::default();
    schedule.action(TxAction::SendBlock {
        block_start_us: 10_000,
        start: true,
        byte: 0x4f,
        eom: false,
    });
    schedule.edge(Edge {
        level: Level::Low,
        t_us: 10_000,
    });
    let zero = timing::START_TOTAL.nominal + timing::SAMPLE_AT;
    let one = zero + timing::BIT;
    for offset in [zero, one] {
        let deadline = 10_000 + u64::from(offset);
        assert_eq!(
            schedule.sample_failure(
                offset,
                Edge {
                    level: Level::High,
                    t_us: deadline - 1
                }
            ),
            None
        );
        for delay in [0, timing::SAMPLE_SLACK] {
            assert_eq!(
                schedule.sample_failure(
                    offset,
                    Edge {
                        level: Level::High,
                        t_us: deadline + u64::from(delay)
                    },
                ),
                (offset == zero).then_some(AttemptFailure::DrivenLowMismatch)
            );
            assert_eq!(
                schedule.sample_failure(
                    offset,
                    Edge {
                        level: Level::Low,
                        t_us: deadline + u64::from(delay)
                    },
                ),
                (offset == one).then_some(AttemptFailure::ArbLost)
            );
        }
        assert_eq!(
            schedule.sample_failure(
                offset,
                Edge {
                    level: Level::Low,
                    t_us: deadline + u64::from(timing::SAMPLE_SLACK) + 1,
                },
            ),
            Some(AttemptFailure::SampleLate)
        );
    }
    let ack = timing::START_TOTAL.nominal + 9 * timing::BIT + timing::SAMPLE_AT;
    assert_eq!(
        schedule.sample_failure(
            ack,
            Edge {
                level: Level::Low,
                t_us: 10_000 + u64::from(ack)
            }
        ),
        None,
        "the follower owns the ACK; low is not arbitration loss"
    );
}

#[test]
fn attempt_failure_counters_include_failures_before_a_successful_outcome() {
    let mut tx = aether_logic::status::BusSnapshot::default().tx;
    for failure in [
        AttemptFailure::OriginMissing,
        AttemptFailure::SampleLate,
        AttemptFailure::LaunchRejected,
        AttemptFailure::EchoError,
        AttemptFailure::EdgeOverrun,
        AttemptFailure::DrivenLowMismatch,
        AttemptFailure::ArbLost,
        AttemptFailure::ArbLost,
    ] {
        failure.record(&mut tx);
    }
    tx.ack += 1; // A later successful request must not erase the failed attempts.
    assert_eq!(tx.arb_lost, 2);
    assert_eq!(tx.line_error, 6);
    for count in [
        tx.origin_missing,
        tx.sample_late,
        tx.launch_rejected,
        tx.echo_error,
        tx.edge_overrun,
        tx.driven_low_mismatch,
    ] {
        assert_eq!(count, 1);
    }
    assert_eq!(tx.ack, 1);
    assert_eq!(tx.nack, 0);
}

#[test]
fn boot_event_and_reconnect_backlog_follow_status_cfg_without_losses() {
    let mut backlog = bus::EventBacklog::default();
    let mut expected = Vec::new();
    for seq in 1..=3 {
        let event = BusEvent::Line {
            seq,
            t_us: u64::from(seq) * 100,
            bus: Some(BusState::Ok),
            hpd: None,
            la: None,
        };
        expected.push(event.clone());
        let bytes = serde_json::to_vec(&event).unwrap();
        assert!(backlog.push(event, bytes).is_none());
        backlog.flush(false, |_, _| panic!("offline or preamble not ready"));
    }
    let mut published = vec![serde_json::json!("status"), serde_json::json!("cfg")];
    backlog.flush(true, |event, bytes| {
        assert_eq!(bytes, serde_json::to_vec(&event).unwrap());
        published.push(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap());
    });
    assert_eq!(published.len(), 5);
    for (actual, expected) in published[2..].iter().zip(expected) {
        assert_eq!(actual, &serde_json::to_value(expected).unwrap());
    }
    backlog.flush(true, |_, _| panic!("events must not be replayed twice"));

    // A later disconnect uses the same FIFO, not the previous generation's readiness.
    let event = BusEvent::Line {
        seq: 4,
        t_us: 1000,
        bus: Some(BusState::StuckLow),
        hpd: None,
        la: None,
    };
    let bytes = serde_json::to_vec(&event).unwrap();
    assert!(backlog.push(event.clone(), bytes).is_none());
    backlog.flush(false, |_, _| {
        panic!("new connection needs its own preamble")
    });
    let mut reconnected = vec![serde_json::json!("status"), serde_json::json!("cfg")];
    backlog.flush(true, |event, bytes| {
        assert_eq!(bytes, serde_json::to_vec(&event).unwrap());
        reconnected.push(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap());
    });
    assert_eq!(reconnected[2], serde_json::to_value(event).unwrap());
}

#[test]
fn only_real_backlog_overflow_is_lost_and_reports_preserve_original_count() {
    let mut bridge = Bridge::new(config::core_config(&config::defaults()).unwrap(), 0);
    let mut backlog = bus::EventBacklog::default();
    let mut losses = bus::Losses::default();
    for _ in 0..35 {
        let event = BusEvent::Line {
            seq: bridge.next_seq(),
            t_us: 0,
            bus: Some(BusState::Ok),
            hpd: None,
            la: None,
        };
        let bytes = serde_json::to_vec(&event).unwrap();
        if let Some(event) = backlog.push(event, bytes) {
            losses.record(&event);
        }
    }
    assert_eq!(losses.pending(), 3);
    // A report lost at this boundary represents its original count, not one loss.
    let mut producer_losses = bus::Losses::default();
    producer_losses.add(125);
    assert!(producer_losses.report(&mut bridge, 1, |event| {
        let bytes = serde_json::to_vec(&event).unwrap();
        losses.record(&backlog.push(event, bytes).expect("backlog is full"));
        true
    }));
    assert_eq!(losses.pending(), 128);
    backlog.flush(false, |_, _| panic!("still disconnected"));
    let mut seqs = Vec::new();
    backlog.flush(true, |event, bytes| {
        assert_eq!(bytes, serde_json::to_vec(&event).unwrap());
        let BusEvent::Line { seq, .. } = event else {
            panic!("retained line")
        };
        seqs.push(seq);
    });
    assert_eq!(seqs, (1..=32).collect::<Vec<_>>());
    assert!(losses.report(&mut bridge, 2, |event| {
        assert!(matches!(
            event,
            BusEvent::Err {
                seq: 37,
                dropped: Some(128),
                ..
            }
        ));
        true
    }));
    assert_eq!(losses.pending(), 0);
    assert!(losses.report(&mut bridge, 3, |_| panic!("no recursive overrun")));
}

fn ack_capture() -> aether_logic::cec::AckCapture {
    let assertion = 1_u64 << 33;
    let due = assertion + u64::from(timing::ACK_LOW);
    aether_logic::cec::AckCapture {
        state_before: 1,
        assert_before_us: assertion,
        now_us: assertion + 5000,
        interrupt: true,
        alarm_enabled: false,
        alarm_ticks: due * 2,
        task_deadline_us: due + 100,
        isr_deadline_us: due + 3,
        state_after: 1,
        assert_after_us: assertion,
        held_us: timing::ACK_LOW,
    }
}

fn evidence(t_us: u64, capture: &aether_logic::cec::AckCapture) -> aether_logic::cec::AckEvidence {
    let mut result = aether_logic::cec::AckEvidence::default();
    aether_logic::cec::ack_evidence(t_us, capture, &mut result);
    result
}

#[test]
fn ack_capture_uses_one_assertion_and_capture_time_not_event_time_for_deadlines() {
    use aether_logic::cec::AckEvidence;
    let capture = ack_capture();
    let age = 2000;
    let due = i32::try_from(timing::ACK_LOW).unwrap() - 5000;
    assert_eq!(
        evidence(capture.assert_before_us + age, &capture),
        AckEvidence {
            ack_age_us: Some(u32::try_from(age).unwrap()),
            ack_held_us: None,
            tmr_int: Some(true),
            tmr_en: Some(false),
            tmr_alarm_in_us: Some(due),
            tmr_isr_in_us: Some(due + 3),
            tmr_task_in_us: Some(due + 100),
            ack_due_in_us: Some(due),
        }
    );
    assert_eq!(
        evidence(capture.assert_before_us - 1, &capture).ack_age_us,
        Some(0)
    );
}

#[test]
fn ack_capture_rejects_changed_states_assertions_and_stale_records() {
    use aether_logic::cec::{AckEvidence, ack_evidence};
    let capture = ack_capture();
    for (before, after) in [(0, 0), (0, 1), (1, 0), (1, 2), (2, 1), (3, 3)] {
        let mut changed = capture.clone();
        changed.state_before = before;
        changed.state_after = after;
        assert_eq!(evidence(changed.now_us, &changed), AckEvidence::default());
    }
    let mut changed = capture.clone();
    changed.assert_after_us += 1;
    assert_eq!(evidence(changed.now_us, &changed), AckEvidence::default());
    changed = capture.clone();
    // An event older than the cutoff clears every field in a reused output.
    let mut result = evidence(capture.now_us, &capture);
    ack_evidence(
        changed.assert_before_us + u64::from(timing::STUCK_LOW) + 1,
        &changed,
        &mut result,
    );
    assert_eq!(result, AckEvidence::default());
    changed = capture;
    changed.now_us = changed.assert_before_us + u64::from(timing::STUCK_LOW);
    let result = evidence(changed.now_us, &changed);
    assert_eq!(result.ack_age_us, Some(timing::STUCK_LOW));
    assert_eq!(result.tmr_int, Some(true));
}

#[test]
fn released_ack_has_coherent_age_and_hold_but_no_timer_group() {
    use aether_logic::cec::AckEvidence;
    let mut capture = ack_capture();
    capture.state_before = 2;
    capture.state_after = 2;
    capture.held_us = u32::MAX;
    assert_eq!(
        evidence(capture.now_us, &capture),
        AckEvidence {
            ack_age_us: Some(5000),
            ack_held_us: Some(u32::MAX),
            ..AckEvidence::default()
        }
    );
}

#[test]
fn delayed_capture_preserves_event_ages_but_omits_stale_timer_group() {
    use aether_logic::cec::AckEvidence;
    let mut capture = ack_capture();
    let event_age = u64::from(timing::STUCK_LOW) - 1000;
    let event_us = capture.assert_before_us + event_age;
    capture.now_us = event_us + 2000;
    for state in [1, 2] {
        capture.state_before = state;
        capture.state_after = state;
        assert_eq!(
            evidence(event_us, &capture),
            AckEvidence {
                ack_age_us: Some(99000),
                ack_held_us: (state == 2).then_some(capture.held_us),
                ..AckEvidence::default()
            }
        );
    }
}

#[test]
fn timer_capture_preserves_false_bits_and_independently_absent_deadlines() {
    let mut capture = ack_capture();
    capture.interrupt = false;
    capture.alarm_enabled = true;
    for (task, isr) in [(u64::MAX, u64::MAX), (u64::MAX, 0), (0, u64::MAX), (0, 0)] {
        capture.task_deadline_us = task;
        capture.isr_deadline_us = isr;
        let result = evidence(capture.now_us, &capture);
        assert_eq!(result.tmr_int, Some(false));
        assert_eq!(result.tmr_en, Some(true));
        assert_eq!(
            result.tmr_task_in_us,
            (task != u64::MAX).then_some(i32::MIN)
        );
        assert_eq!(result.tmr_isr_in_us, (isr != u64::MAX).then_some(i32::MIN));
    }
}

#[test]
fn timer_deadlines_convert_ticks_and_saturate_signed_distances_at_boundaries() {
    let mut capture = ack_capture();
    let now = capture.now_us;
    for delta in [
        i64::from(i32::MIN) - 1,
        i64::from(i32::MIN),
        -1,
        0,
        1,
        i64::from(i32::MAX),
        i64::from(i32::MAX) + 1,
    ] {
        let deadline = now.checked_add_signed(delta).unwrap();
        let expected =
            i32::try_from(delta.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).unwrap();
        // The odd tick is truncated just as IDF truncates its counter to us.
        capture.alarm_ticks = deadline * 2 + 1;
        capture.task_deadline_us = deadline;
        capture.isr_deadline_us = deadline;
        let result = evidence(now, &capture);
        assert_eq!(result.tmr_alarm_in_us, Some(expected));
        assert_eq!(result.tmr_task_in_us, Some(expected));
        assert_eq!(result.tmr_isr_in_us, Some(expected));
    }
    capture.isr_deadline_us = u64::MAX - 1;
    capture.task_deadline_us = 0;
    capture.alarm_ticks = u64::MAX;
    let result = evidence(now, &capture);
    assert_eq!(result.tmr_isr_in_us, Some(i32::MAX));
    assert_eq!(result.tmr_task_in_us, Some(i32::MIN));
    assert_eq!(result.tmr_alarm_in_us, Some(i32::MAX));
    capture.now_us = capture.assert_before_us;
    assert_eq!(
        evidence(now, &capture).ack_due_in_us,
        Some(i32::try_from(timing::ACK_LOW).unwrap())
    );
    capture.now_us = u64::MAX;
    capture.assert_before_us = u64::MAX - 1;
    capture.assert_after_us = capture.assert_before_us;
    assert_eq!(evidence(u64::MAX, &capture).ack_due_in_us, Some(0));
}

#[test]
fn ack_late_starts_above_the_accepted_zero_low_window() {
    use aether_logic::cec::ack_late;
    assert!(!ack_late(timing::DATA_LOW_0.max));
    assert!(ack_late(timing::DATA_LOW_0.max + 1));
}

#[test]
fn early_ack_alarm_rearms_only_a_valid_remainder_outside_tolerance() {
    use aether_logic::cec::ack_release_remainder;
    let assertion = 1_u64 << 33;
    let due = assertion + u64::from(timing::ACK_LOW);
    assert_eq!(ack_release_remainder(assertion, None), None);
    assert_eq!(
        ack_release_remainder(assertion, Some(assertion)),
        Some(timing::ACK_LOW)
    );
    assert_eq!(
        ack_release_remainder(assertion + 1, Some(assertion)),
        Some(timing::ACK_LOW - 1)
    );
    assert_eq!(ack_release_remainder(due - 11, Some(assertion)), Some(11));
    for now in [due - 10, due - 1, due, due + 1, u64::MAX] {
        assert_eq!(ack_release_remainder(now, Some(assertion)), None);
    }
    // Before assertion the remainder is over range; a backwards clock near
    // u64::MAX must also fail open even when saturating addition hides that.
    assert_eq!(ack_release_remainder(assertion - 1, Some(assertion)), None);
    assert_eq!(ack_release_remainder(0, Some(u64::MAX)), None);
    assert_eq!(
        ack_release_remainder(u64::MAX - 20, Some(u64::MAX - 19)),
        None
    );
    assert_eq!(
        ack_release_remainder(u64::MAX - 11, Some(u64::MAX - 20)),
        Some(11)
    );
    assert_eq!(ack_release_remainder(u64::MAX, Some(u64::MAX)), None);
    assert_eq!(ack_release_remainder(0, Some(0)), Some(timing::ACK_LOW));
}

#[test]
fn pin_high_age_is_absent_saturating_and_bounded() {
    use aether_logic::cec::pin_high_age;
    let high = 1_u64 << 33;
    assert_eq!(pin_high_age(high, None), None);
    assert_eq!(pin_high_age(high - 1, Some(high)), Some(0));
    assert_eq!(pin_high_age(high, Some(high)), Some(0));
    assert_eq!(pin_high_age(high + 1, Some(high)), Some(1));
    for age in [u64::from(u32::MAX), u64::from(u32::MAX) + 1] {
        assert_eq!(pin_high_age(high + age, Some(high)), Some(u32::MAX));
    }
}
