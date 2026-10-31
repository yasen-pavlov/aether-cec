//! Scripted executor exercises arbitration, ACK polarity, retry budgets and timing.
use cec_core::{Frame, Level, Tx, TxAction, TxOutcome, TxRequest, TxStatus, timing};

fn execute(
    frame: Frame,
    retries: u8,
    mut sample: impl FnMut(u8, usize, u8) -> Level,
) -> (TxOutcome, Vec<(u64, bool, u8, bool)>) {
    let mut tx = Tx::new();
    tx.start(TxRequest { frame, retries }, 0, 0, false).unwrap();
    let mut now = 0;
    let mut start_at = 0;
    let mut attempt = 0;
    let mut block = 0;
    let mut sent = Vec::new();
    for _ in 0..1000 {
        match tx.step(now, 0) {
            TxAction::Wait { until_us } => {
                assert!(until_us >= now);
                now = until_us;
            }
            TxAction::SendBlock {
                block_start_us,
                start,
                byte,
                eom,
            } => {
                start_at = block_start_us;
                if start {
                    attempt += 1;
                    block = 0;
                } else {
                    block += 1;
                }
                sent.push((block_start_us, start, byte, eom));
            }
            TxAction::SampleAt { at_us } => {
                let prefix = if block == 0 {
                    timing::START_TOTAL.nominal
                } else {
                    0
                };
                let bit = u8::try_from((at_us - prefix - timing::SAMPLE_AT) / timing::BIT).unwrap();
                now = start_at + u64::from(at_us);
                tx.line_sample(sample(attempt, block, bit), now);
            }
            TxAction::Abort => {}
            TxAction::Done {
                outcome,
                final_attempt_start_us,
            } => {
                assert_eq!(
                    final_attempt_start_us,
                    sent.iter().rev().find(|s| s.1).unwrap().0
                );
                assert!(!tx.in_progress());
                return (outcome, sent);
            }
        }
    }
    panic!("executor did not finish");
}
fn directed_sample(frame: &Frame, bit: u8) -> Level {
    if bit < 4 && frame.header() & (1 << (7 - bit)) != 0 {
        Level::High
    } else {
        Level::Low
    }
}
#[test]
fn tx_first_try_ack_and_only_initiator_nibble_arbitrates() {
    let frame = Frame::new(4, 8, &[0x8F]).unwrap();
    let (outcome, sent) = execute(frame.clone(), 0, |_, _, bit| directed_sample(&frame, bit));
    assert_eq!(
        outcome,
        TxOutcome {
            status: TxStatus::Ack,
            attempts: 1
        }
    );
    assert_eq!(sent.len(), 2);
    assert!(sent[0].1);
    assert!(!sent[1].1);
    assert!(!sent[0].3);
    assert!(sent[1].3);
}
#[test]
fn tx_nack_block_two_of_three_retries_whole_frame() {
    let frame = Frame::new(4, 0, &[0x41, 0x25]).unwrap();
    let (outcome, sent) = execute(frame.clone(), 1, |attempt, block, bit| {
        if attempt == 1 && block == 1 && bit == 9 {
            Level::High
        } else {
            directed_sample(&frame, bit)
        }
    });
    assert_eq!(
        outcome,
        TxOutcome {
            status: TxStatus::Ack,
            attempts: 2
        }
    );
    assert_eq!(sent.len(), 5);
    assert_eq!(
        sent.iter().map(|s| s.2).collect::<Vec<_>>(),
        [frame.header(), 0x41, frame.header(), 0x41, 0x25]
    );
    let failed_block_end = sent[1].0 + u64::from(10 * timing::BIT);
    assert_eq!(
        sent[2].0 - failed_block_end,
        u64::from(timing::SIGNAL_FREE_RETRY)
    );
}
#[test]
fn tx_poll_no_retries_preserves_nack() {
    let frame = Frame::new(4, 0, &[]).unwrap();
    let (outcome, _) = execute(frame.clone(), 0, |_, _, bit| {
        if bit == 9 {
            Level::High
        } else {
            directed_sample(&frame, bit)
        }
    });
    assert_eq!(
        outcome,
        TxOutcome {
            status: TxStatus::Nack,
            attempts: 1
        }
    );
}
#[test]
fn tx_arbitration_bit_two_then_success() {
    let frame = Frame::new(4, 0, &[0x8F]).unwrap();
    let (outcome, sent) = execute(frame.clone(), 0, |attempt, _, bit| {
        if attempt == 1 && bit == 1 {
            Level::Low
        } else {
            directed_sample(&frame, bit)
        }
    });
    assert_eq!(
        outcome,
        TxOutcome {
            status: TxStatus::Ack,
            attempts: 2
        }
    );
    assert_eq!(sent.iter().filter(|s| s.1).count(), 2);
}
#[test]
fn tx_five_arbitration_losses_exhaust_hard_cap() {
    let frame = Frame::new(4, 0, &[0x8F]).unwrap();
    let (outcome, sent) = execute(frame, 0, |_, _, _| Level::Low);
    assert_eq!(
        outcome,
        TxOutcome {
            status: TxStatus::ArbLost,
            attempts: timing::MAX_ATTEMPTS
        }
    );
    assert_eq!(sent.len(), usize::from(timing::MAX_ATTEMPTS));
}
#[test]
fn tx_broadcast_ack_polarity_and_nack_budget_clamp() {
    let frame = Frame::new(0, 15, &[0x84, 0, 0, 0]).unwrap();
    let (ack, _) = execute(frame.clone(), 0, |_, _, bit| {
        if bit == 9 { Level::High } else { Level::Low }
    });
    assert_eq!(ack.status, TxStatus::Ack);
    let (nack, _) = execute(frame, u8::MAX, |_, _, _| Level::Low);
    assert_eq!(
        nack,
        TxOutcome {
            status: TxStatus::Nack,
            attempts: timing::MAX_ATTEMPTS
        }
    );
}
#[test]
fn tx_busy_beyond_limit_and_free_interval_restarts_after_activity() {
    let mut tx = Tx::new();
    tx.start(
        TxRequest {
            frame: Frame::new(0, 4, &[]).unwrap(),
            retries: 0,
        },
        0,
        0,
        false,
    )
    .unwrap();
    assert!(matches!(tx.step(0, 0), TxAction::Wait { .. }));
    assert!(matches!(
        tx.step(
            u64::from(timing::BUSY_GIVE_UP),
            u64::from(timing::BUSY_GIVE_UP)
        ),
        TxAction::Wait { .. }
    ));
    assert_eq!(
        tx.step(
            u64::from(timing::BUSY_GIVE_UP) + 1,
            u64::from(timing::BUSY_GIVE_UP) + 1
        ),
        TxAction::Done {
            outcome: TxOutcome {
                status: TxStatus::Busy,
                attempts: 0
            },
            final_attempt_start_us: 0,
        }
    );
    tx.start(
        TxRequest {
            frame: Frame::new(0, 4, &[]).unwrap(),
            retries: 0,
        },
        0,
        0,
        false,
    )
    .unwrap();
    let busy_at = u64::from(timing::BIT);
    tx.step(busy_at, busy_at);
    let free_at = busy_at + u64::from(timing::BIT);
    assert_eq!(
        tx.step(free_at, free_at),
        TxAction::Wait {
            until_us: free_at + u64::from(timing::SIGNAL_FREE_NEW)
        }
    );
}
#[test]
fn tx_signal_free_exact_new_same_and_retry() {
    for (ours, duration) in [
        (false, timing::SIGNAL_FREE_NEW),
        (true, timing::SIGNAL_FREE_SAME),
    ] {
        let mut tx = Tx::new();
        tx.start(
            TxRequest {
                frame: Frame::new(4, 0, &[]).unwrap(),
                retries: 0,
            },
            0,
            0,
            ours,
        )
        .unwrap();
        assert_eq!(
            tx.step(u64::from(duration) - 1, 0),
            TxAction::Wait {
                until_us: u64::from(duration)
            }
        );
        assert!(matches!(
            tx.step(u64::from(duration), 0),
            TxAction::SendBlock { start: true, .. }
        ));
        let sample_time = u64::from(duration + timing::START_TOTAL.nominal + timing::SAMPLE_AT);
        assert!(matches!(
            tx.step(u64::from(duration), 0),
            TxAction::SampleAt { .. }
        ));
        tx.line_error(sample_time);
        assert_eq!(tx.step(sample_time, 0), TxAction::Abort);
        assert_eq!(
            tx.step(sample_time, 0),
            TxAction::Wait {
                until_us: sample_time + u64::from(timing::SIGNAL_FREE_RETRY)
            }
        );
        assert!(matches!(
            tx.step(sample_time + u64::from(timing::SIGNAL_FREE_RETRY), 0),
            TxAction::SendBlock { start: true, .. }
        ));
    }
}
#[test]
fn tx_line_errors_retry_without_nack_budget_and_mixed_failure_reports_last() {
    let mut tx = Tx::new();
    tx.start(
        TxRequest {
            frame: Frame::new(0, 4, &[]).unwrap(),
            retries: 0,
        },
        0,
        0,
        false,
    )
    .unwrap();
    let mut now = u64::from(timing::SIGNAL_FREE_NEW);
    for attempt in 1..=timing::MAX_ATTEMPTS {
        assert!(matches!(tx.step(now, 0), TxAction::SendBlock { .. }));
        tx.line_error(now);
        assert_eq!(tx.step(now, 0), TxAction::Abort);
        if attempt == timing::MAX_ATTEMPTS {
            assert_eq!(
                tx.step(now, 0),
                TxAction::Done {
                    outcome: TxOutcome {
                        status: TxStatus::LineError,
                        attempts: timing::MAX_ATTEMPTS
                    },
                    final_attempt_start_us: now,
                }
            );
        } else {
            now += u64::from(timing::SIGNAL_FREE_RETRY);
        }
    }
    let frame = Frame::new(4, 0, &[]).unwrap();
    let (outcome, _) = execute(frame.clone(), 0, |attempt, _, bit| {
        if attempt == 1 {
            Level::Low
        } else if bit == 9 {
            Level::High
        } else {
            directed_sample(&frame, bit)
        }
    });
    assert_eq!(
        outcome,
        TxOutcome {
            status: TxStatus::Nack,
            attempts: 2
        }
    );
}
#[test]
fn tx_rejects_overlapping_request_without_losing_original() {
    let mut tx = Tx::new();
    let request = TxRequest {
        frame: Frame::new(0, 4, &[]).unwrap(),
        retries: 0,
    };
    tx.start(request.clone(), 0, 0, false).unwrap();
    assert_eq!(tx.start(request.clone(), 0, 0, false), Err(request));
    assert!(matches!(
        tx.step(u64::from(timing::SIGNAL_FREE_NEW), 0),
        TxAction::SendBlock { byte: 4, .. }
    ));
}

#[test]
fn tx_arbitration_loss_mid_block_releases_line_exactly_once() {
    let mut tx = Tx::new();
    tx.start(
        TxRequest {
            frame: Frame::new(8, 0, &[]).unwrap(),
            retries: 0,
        },
        0,
        0,
        false,
    )
    .unwrap();
    let start = u64::from(timing::SIGNAL_FREE_NEW);
    assert!(matches!(tx.step(start, 0), TxAction::SendBlock { .. }));
    assert!(matches!(tx.step(start, 0), TxAction::SampleAt { .. }));
    let sample = start + u64::from(timing::START_TOTAL.nominal + timing::SAMPLE_AT);
    tx.line_sample(Level::Low, sample);
    assert_eq!(tx.step(sample, sample), TxAction::Abort);
    assert!(matches!(tx.step(sample, sample), TxAction::Wait { .. }));
}

#[test]
fn tx_arb_then_nack_retains_requested_nack_retry() {
    let frame = Frame::new(4, 0, &[0x8F]).unwrap();
    let (outcome, _) = execute(frame.clone(), 1, |attempt, _, bit| {
        if attempt == 1 && bit == 1 {
            Level::Low
        } else if attempt == 2 && bit == 9 {
            Level::High
        } else {
            directed_sample(&frame, bit)
        }
    });
    assert_eq!(
        outcome,
        TxOutcome {
            status: TxStatus::Ack,
            attempts: 3
        }
    );
    let (mixed, _) = execute(frame.clone(), 1, |attempt, _, bit| {
        if attempt == 1 && bit == 9 {
            Level::High
        } else if attempt > 1 {
            Level::Low
        } else {
            directed_sample(&frame, bit)
        }
    });
    assert_eq!(
        mixed,
        TxOutcome {
            status: TxStatus::ArbLost,
            attempts: timing::MAX_ATTEMPTS
        }
    );
}

#[test]
fn tx_missed_sample_deadline_aborts_instead_of_waiting_in_the_past() {
    for deliver_sample in [false, true] {
        let mut tx = Tx::new();
        tx.start(
            TxRequest {
                frame: Frame::new(0, 4, &[]).unwrap(),
                retries: 0,
            },
            0,
            0,
            false,
        )
        .unwrap();
        let start = u64::from(timing::SIGNAL_FREE_NEW);
        tx.step(start, 0);
        let TxAction::SampleAt { at_us } = tx.step(start, 0) else {
            panic!("sample required")
        };
        let deadline = start + u64::from(at_us + timing::SAMPLE_SLACK);
        assert!(matches!(tx.step(deadline, 0), TxAction::Wait { .. }));
        let late = deadline + 1;
        if deliver_sample {
            tx.line_sample(Level::Low, late);
        }
        assert_eq!(tx.step(late, 0), TxAction::Abort);
        assert_eq!(
            tx.step(late, 0),
            TxAction::Wait {
                until_us: late + u64::from(timing::SIGNAL_FREE_RETRY)
            }
        );
    }
}

#[test]
fn tx_chains_next_block_at_previous_end_before_ack_tail_finishes() {
    let mut tx = Tx::new();
    tx.start(
        TxRequest {
            frame: Frame::new(0, 4, &[0x8F]).unwrap(),
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
    for _ in 0..5 {
        let TxAction::SampleAt { at_us } = tx.step(now, 0) else {
            panic!("sample required")
        };
        now = start + u64::from(at_us);
        tx.line_sample(Level::Low, now);
    }
    let next = start + u64::from(timing::START_TOTAL.nominal + 10 * timing::BIT);
    assert!(now < next);
    assert_eq!(
        tx.step(now, 0),
        TxAction::SendBlock {
            block_start_us: next,
            start: false,
            byte: 0x8F,
            eom: true
        }
    );
}

#[test]
fn tx_rechecks_undecodable_activity_and_drops_same_initiator_wait() {
    let mut tx = Tx::new();
    tx.start(
        TxRequest {
            frame: Frame::new(0, 4, &[]).unwrap(),
            retries: 0,
        },
        0,
        0,
        true,
    )
    .unwrap();
    let activity = u64::from(timing::BIT);
    let deadline = activity + u64::from(timing::SIGNAL_FREE_NEW);
    assert_eq!(
        tx.step(activity, activity),
        TxAction::Wait { until_us: deadline }
    );
    assert!(matches!(
        tx.step(deadline - 1, activity),
        TxAction::Wait { .. }
    ));
    assert!(matches!(
        tx.step(deadline, activity),
        TxAction::SendBlock { .. }
    ));
}
