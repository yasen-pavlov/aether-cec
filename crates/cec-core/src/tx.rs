//! Block-level transmitter. The executor renders blocks and performs requested samples.
use crate::{Frame, TxOutcome, TxStatus, rx::Level, timing};

/// One frame and its NACK retry budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxRequest {
    /// Frame to transmit.
    pub frame: Frame,
    /// Retransmissions after a NACK (other failures use the hard attempt cap).
    pub retries: u8,
}
/// Next operation for the hardware executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxAction {
    /// Call `step` again at this absolute time.
    Wait {
        /// Microseconds since boot.
        until_us: u64,
    },
    /// Queue one whole block, including the ACK bit's trailing high.
    SendBlock {
        /// Absolute first falling edge; subsequent blocks chain at the previous end.
        block_start_us: u64,
        /// Include the start bit before this header block.
        start: bool,
        /// Eight data bits, most significant first.
        byte: u8,
        /// End-of-message bit.
        eom: bool,
    },
    /// Sample relative to the current block's first falling edge (including start bit).
    SampleAt {
        /// Relative microseconds.
        at_us: u32,
    },
    /// Cancel the active waveform and release the line now; emitted once per abort.
    Abort,
    /// Finished request; emitted once, then the transmitter becomes idle.
    Done {
        /// Final classification and total attempts.
        outcome: TxOutcome,
        /// Start-bit timestamp of the final attempt (request time if never sent).
        final_attempt_start_us: u64,
    },
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    Free,
    Sample,
    AwaitSample,
    BlockEnd,
    Done,
}
/// Single-request, allocation-free transmitter.
#[derive(Debug, Default)]
pub struct Tx {
    request: Option<TxRequest>,
    state: State,
    attempts: u8,
    nacks: u8,
    final_attempt_start_us: u64,
    abort_pending: bool,
    done_not_before: u64,
    block: usize,
    sample: u8,
    block_start: u64,
    free_since: u64,
    free_for: u32,
    busy_since: Option<u64>,
    outcome: Option<TxStatus>,
}
impl Tx {
    /// Creates an idle transmitter.
    pub fn new() -> Self {
        Self::default()
    }
    /// Starts a request or returns it unchanged if another is still active.
    pub fn start(
        &mut self,
        request: TxRequest,
        now_us: u64,
        bus_free_since_us: u64,
        last_frame_was_ours: bool,
    ) -> Result<(), TxRequest> {
        if self.in_progress() {
            return Err(request);
        }
        self.request = Some(request);
        self.state = State::Free;
        self.attempts = 0;
        self.nacks = 0;
        self.final_attempt_start_us = now_us;
        self.abort_pending = false;
        self.done_not_before = now_us;
        self.free_since = bus_free_since_us.min(now_us);
        self.free_for = if last_frame_was_ours {
            timing::SIGNAL_FREE_SAME
        } else {
            timing::SIGNAL_FREE_NEW
        };
        self.busy_since = Some(now_us);
        self.outcome = None;
        Ok(())
    }
    /// Whether a request is active (including an undelivered completion).
    pub fn in_progress(&self) -> bool {
        self.request.is_some()
    }
    fn sample_offset(&self) -> u32 {
        let prefix = if self.block == 0 {
            timing::START_TOTAL.nominal
        } else {
            0
        };
        prefix + u32::from(self.sample) * timing::BIT + timing::SAMPLE_AT
    }
    fn block_end(&self) -> u64 {
        self.block_start
            + u64::from(if self.block == 0 {
                timing::START_TOTAL.nominal
            } else {
                0
            })
            + u64::from(10 * timing::BIT)
    }
    fn send(&mut self, now: u64) -> TxAction {
        let request = self
            .request
            .as_ref()
            .expect("active transmitter has a request");
        let start = self.block == 0;
        let byte = if start {
            request.frame.header()
        } else {
            request.frame.data[self.block - 1]
        };
        let eom = self.block + 1 == request.frame.blocks();
        if start {
            self.attempts += 1;
            self.final_attempt_start_us = now;
        }
        self.block_start = now;
        self.sample = if start { 0 } else { 9 };
        self.state = State::Sample;
        TxAction::SendBlock {
            block_start_us: now,
            start,
            byte,
            eom,
        }
    }
    /// Re-anchor the current block to its first falling edge observed by the executor.
    /// No-op outside Sample/AwaitSample; no sample may be delivered before this.
    pub fn block_started(&mut self, start_us: u64) {
        if matches!(self.state, State::Sample | State::AwaitSample) {
            self.block_start = start_us;
            if self.block == 0 {
                self.final_attempt_start_us = start_us;
            }
        }
    }
    fn fail(&mut self, status: TxStatus, now: u64) {
        let retries = self.request.as_ref().expect("active request").retries;
        if status == TxStatus::Nack {
            self.nacks += 1;
            // Let the decoder flush a NACK-terminated echo before publishing TX.
            self.done_not_before =
                self.block_end() - u64::from(timing::BIT) + u64::from(timing::DATA_TOTAL.max) + 1;
        } else {
            self.abort_pending = true;
        }
        let retry = self.attempts < timing::MAX_ATTEMPTS
            && (status != TxStatus::Nack || self.nacks <= retries);
        self.outcome = Some(status);
        if retry {
            self.state = State::Free;
            self.free_since = now;
            self.free_for = timing::SIGNAL_FREE_RETRY;
            self.busy_since = Some(now);
        } else {
            self.state = State::Done;
        }
    }
    /// Advances with the decoder's quiet origin (now when busy or held low).
    ///
    /// # Panics
    /// Panics if the internal active-request invariant is violated. The public API
    /// preserves this invariant, including calls made while idle.
    pub fn step(&mut self, now_us: u64, quiet_since_us: u64) -> TxAction {
        if self.abort_pending {
            self.abort_pending = false;
            return TxAction::Abort;
        }
        match self.state {
            State::Idle => TxAction::Wait { until_us: now_us },
            State::Free => {
                if quiet_since_us > self.free_since {
                    self.free_since = quiet_since_us;
                    self.free_for = timing::SIGNAL_FREE_NEW;
                }
                let until_us = self.free_since.saturating_add(u64::from(self.free_for));
                if now_us < until_us {
                    let since = *self.busy_since.get_or_insert(now_us);
                    if now_us.saturating_sub(since) > u64::from(timing::BUSY_GIVE_UP) {
                        self.outcome = Some(TxStatus::Busy);
                        self.state = State::Done;
                        return self.step(now_us, quiet_since_us);
                    }
                    TxAction::Wait { until_us }
                } else {
                    self.block = 0;
                    self.send(now_us)
                }
            }
            State::Sample | State::AwaitSample => {
                let until_us = self
                    .block_start
                    .saturating_add(u64::from(self.sample_offset()));
                if now_us > until_us.saturating_add(u64::from(timing::SAMPLE_SLACK)) {
                    self.fail(TxStatus::LineError, now_us);
                    return self.step(now_us, quiet_since_us);
                }
                if self.state == State::Sample {
                    self.state = State::AwaitSample;
                    TxAction::SampleAt {
                        at_us: self.sample_offset(),
                    }
                } else {
                    TxAction::Wait { until_us }
                }
            }
            State::BlockEnd => {
                let until_us = self.block_end();
                if self.block + 1
                    < self
                        .request
                        .as_ref()
                        .expect("active request")
                        .frame
                        .blocks()
                {
                    self.block += 1;
                    return self.send(until_us);
                }
                if now_us < until_us {
                    return TxAction::Wait { until_us };
                }
                self.outcome = Some(TxStatus::Ack);
                self.state = State::Done;
                self.step(now_us, quiet_since_us)
            }
            State::Done => {
                if now_us < self.done_not_before {
                    return TxAction::Wait {
                        until_us: self.done_not_before,
                    };
                }
                let outcome = TxOutcome {
                    status: self.outcome.take().expect("completion has status"),
                    attempts: self.attempts,
                };
                self.request = None;
                self.state = State::Idle;
                TxAction::Done {
                    outcome,
                    final_attempt_start_us: self.final_attempt_start_us,
                }
            }
        }
    }
    /// Delivers the currently requested sample. Extra samples are ignored.
    ///
    /// # Panics
    /// Panics if the internal active-request invariant is violated; no public
    /// operation can create that state.
    pub fn line_sample(&mut self, level: Level, now_us: u64) {
        if self.state != State::AwaitSample {
            return;
        }
        let deadline = self
            .block_start
            .saturating_add(u64::from(self.sample_offset()));
        if now_us < deadline {
            return;
        }
        if now_us > deadline.saturating_add(u64::from(timing::SAMPLE_SLACK)) {
            self.fail(TxStatus::LineError, now_us);
            return;
        }
        let frame = &self.request.as_ref().expect("active request").frame;
        if self.sample < 4 {
            let sent_one = frame.header() & (1 << (7 - self.sample)) != 0;
            if sent_one && level == Level::Low {
                self.fail(TxStatus::ArbLost, now_us);
                return;
            }
            self.sample += 1;
            if self.sample == 4 {
                self.sample = 9;
            }
            self.state = State::Sample;
        } else {
            let ack = if frame.is_broadcast() {
                level == Level::High
            } else {
                level == Level::Low
            };
            if ack {
                self.state = State::BlockEnd;
            } else {
                self.fail(TxStatus::Nack, self.block_end());
            }
        }
    }
    /// Aborts an active block after an executor-detected collision or line fault.
    pub fn line_error(&mut self, now_us: u64) {
        if matches!(
            self.state,
            State::Sample | State::AwaitSample | State::BlockEnd
        ) {
            self.fail(TxStatus::LineError, now_us);
        }
    }
}
