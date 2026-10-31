# cec-core and aether-proto design (API contract, phase 1)

`docs/protocol.md` defines what goes over MQTT. This file defines the Rust surface
the firmware executor and the host adapter are built on. Changes agreed in review
are recorded in `docs/decisions.md`.

## 1. Principles

- `cec-core` is pure: no I/O, no clocks, no allocation, no `unsafe`. Every
  function takes `now_us: u64` (monotonic microseconds since boot) when it
  needs time. The executor owns pins, RMT and timers and is a thin loop:
  *feed edges in, carry actions out*.
- Block granularity for transmit. The core decides *what* to put on the wire and
  *when*; hardware (RMT) renders the bit waveform of one block. The core never
  asks for sub-bit scheduling except "sample the line at `t`".
- Edge granularity for receive. A GPIO edge ISR delivers `(level, t_us)` edges
  with microsecond timestamps. RMT is TX-only: RMT RX cannot deliver per-edge ACK triggers.
- One timing module. All microsecond constants live in `cec_core::timing` as
  `Window { nominal, min, max }` and are the only numbers in the crate.
- Executor ordering: read `now`, drain GPIO edges in timestamp order, call
  `poll(now)` until no event remains, then advance TX and publish its completion.
  Do this at timer/sample wakes too; drain echoes before handing out the next request.
  Set `Decoder::set_transmitting(true)` before rendering our waveform and clear it
  on completion or `Abort`; continue receiving the winner after arbitration loss.

## 2. Shared types (`cec_core::frame`, already present)

```rust
pub const BROADCAST: u8 = 15;
pub const MAX_DATA: usize = 15;                 // 16 blocks incl. header
pub struct Frame { pub from: u8, pub to: u8, pub data: heapless::Vec<u8, MAX_DATA> }
pub enum TxStatus { Ack, Nack, ArbLost, LineError, Busy }
pub struct TxOutcome { pub status: TxStatus, pub attempts: u8 }
```

## 3. `cec_core::timing`

```rust
pub struct Window { pub nominal: u32, pub min: u32, pub max: u32 }   // µs
pub const BIT: u32 = 2400;
pub const START_LOW: Window;        // 3700  [3400, 4000]  rise after start fall
pub const START_TOTAL: Window;      // 4500  [4050, 4950]  next fall after start fall
pub const DATA_LOW_1: Window;       // 600   [300, 900]
pub const DATA_LOW_0: Window;       // 1500  [1200, 1800]
pub const DATA_TOTAL: Window;       // 2400  [1950, 2850]  next fall after this fall
pub const SAMPLE_AT: u32 = 1050;    // from the bit's falling edge
pub const SAMPLE_SLACK: u32 = 200;  // maximum tolerated late TX sample
pub const STUCK_LOW: u32 = 100_000; // separate bus-health threshold, not LineLow
pub const ACK_LOW: u32 = 1500;      // follower holds low this long (a '0')
pub const SIGNAL_FREE_NEW: u32 = 5 * BIT;     // new initiator
pub const SIGNAL_FREE_SAME: u32 = 7 * BIT;    // previous frame was ours
pub const SIGNAL_FREE_RETRY: u32 = 3 * BIT;   // retransmission
pub const MAX_ATTEMPTS: u8 = 5;               // spec max 5 transmissions in total
pub const BUSY_GIVE_UP: u32 = 2_000_000;      // µs of continuous bus activity before TxStatus::Busy
```
Windows are the Tasmota-proven values (`docs/protocol.md` §0); the spec nominals are
the `nominal` field. Tests must reference these constants, never literals.

## 4. `cec_core::rx` — edge decoder

```rust
pub enum Level { Low, High }
pub struct Edge { pub level: Level, pub t_us: u64 }   // level AFTER the edge

pub enum RxError { StartBit, BitTiming, EomMissing, LineLow }   // LineLow: low > START_TOTAL.max with no edge
pub enum RxEvent {
    /// A complete, valid frame. `start_us` = falling edge of its start bit.
    Frame { frame: Frame, ack: bool, start_us: u64 },
    /// Decoding gave up; `bits` = data bits received before the error, header nibbles if decoded.
    Error { kind: RxError, start_us: u64, from: Option<u8>, to: Option<u8>, bits: u8 },
    Line { stuck_low: bool },
    /// Arm ACK on the actual falling edge via GPIO ISR and hold low for ACK_LOW.
    /// Emitted on EOM rising; fall_at_us predicts timing for scheduling only.
    AckDue { fall_at_us: u64 },
}

pub struct Decoder { .. }
impl Decoder {
    pub fn new(ack_set: AckSet) -> Self;
    pub fn set_ack_set(&mut self, ack_set: AckSet);
    pub fn set_transmitting(&mut self, transmitting: bool);
    /// Feed one edge; may return one event.
    pub fn edge(&mut self, edge: Edge) -> Option<RxEvent>;
    /// Called at least once per 5 ms after draining edges; flushes timeouts and health transitions.
    pub fn poll(&mut self, now_us: u64) -> Option<RxEvent>;
    /// True while inside a frame (between start bit and last ACK bit).
    pub fn busy(&self) -> bool;
    /// Last edge, extended to ACK fall + BIT on a complete frame.
    pub fn last_activity_us(&self) -> u64;
    /// now_us while busy or low; otherwise last_activity_us().
    pub fn quiet_since_us(&self, now_us: u64) -> u64;
}
pub struct AckSet(u16);   // bit i = acknowledge frames addressed to logical address i; bit 15 ignored
```

Rules: valid timing and EOM within 16 blocks normally complete a frame. A directed
frame may stop after a NACKed block: once high past `DATA_TOTAL.max` from that ACK
fall, emit `Frame { ack: false }` with the blocks received so far, even without EOM.
Idle data-width low pulses are silently discarded when joining mid-frame. Emit at
most one frame error until a valid start pulse re-arms decoding. Edge ordering
uses a separate timestamp that `poll` never advances.

ACK is sampled from pulse length (`Low` at `SAMPLE_AT` for directed = acked;
inverse for broadcast); a rise exactly at `SAMPLE_AT` means High. ACK pulses can
span the combined data-low range because initiator and follower share the line.
Never force ACK true from set membership. `AckDue` fires on every addressed
block, except while `set_transmitting(true)`; own edges are still decoded and
published, while the policy layer suppresses recursive local replies.
`LineLow` is a frame error after `START_TOTAL.max`; `Line` separately reports low
past `STUCK_LOW` once and recovery to High once.

## 5. `cec_core::tx` — block-level transmit state machine

```rust
pub struct TxRequest { pub frame: Frame, pub retries: u8 }   // retries after NACK; arb/line errors always retried up to MAX_ATTEMPTS

pub enum TxAction {
    /// Do nothing until `until_us`, then call `step` again.
    Wait { until_us: u64 },
    /// Queue the complete block including ACK trailing high, at block_start_us.
    /// Positive ACK queues the next block immediately, chained at the previous end.
    SendBlock { block_start_us: u64, start: bool, byte: u8, eom: bool },
    /// Sample the line at `at_us` (µs since the block's first falling edge) and call
    /// `line_sample(level)`. Issued for each initiator-nibble bit (arbitration) and for the ACK bit.
    SampleAt { at_us: u32 },
    /// Cancel the running waveform and release the line now; emitted exactly once.
    Abort,
    /// Request finished; publish after draining RX. Time is authoritative, never echo-derived.
    Done { outcome: TxOutcome, final_attempt_start_us: u64 },
}

pub struct Tx { .. }
impl Tx {
    pub fn new() -> Self;
    /// Accept a request; `Err(request)` if one is already in progress.
    pub fn start(&mut self, request: TxRequest, now_us: u64, bus_free_since_us: u64, last_frame_was_ours: bool) -> Result<(), TxRequest>;
    /// Advance using Decoder::quiet_since_us(now_us), not merely Decoder::busy().
    pub fn step(&mut self, now_us: u64, quiet_since_us: u64) -> TxAction;
    /// Deliver a sample requested by `SampleAt`, at or after its due time. A sample
    /// delivered before `block_start_us + at_us` is ignored; the attempt then fails
    /// as a line error once `timing::SAMPLE_SLACK` passes (observed in the lead's smoke
    /// run: an executor that sampled immediately on `SampleAt` aborted five times).
    pub fn line_sample(&mut self, level: Level, now_us: u64);
    /// Executor detected a level it did not drive mid-block (collision). Aborts the attempt.
    pub fn line_error(&mut self, now_us: u64);
    pub fn in_progress(&self) -> bool;
}
```

Semantics (mirrors `docs/protocol.md` §4/§5.2):
- Signal-free wait: `SIGNAL_FREE_SAME` if `last_frame_was_ours`, else
  `SIGNAL_FREE_NEW`; `SIGNAL_FREE_RETRY` before a retransmission. Recheck the
  quiet origin on every step; any intervening foreign activity restarts a NEW
  interval (including during retry). Quiet time begins at ACK fall + `BIT`, not
  ACK rise. No sufficiently quiet interval for `BUSY_GIVE_UP` → `Done` with Busy.
- Arbitration: sample the header initiator nibble; Low where we sent 1 loses.
  Arbitration loss or line error returns `Abort` exactly once before waiting/retrying.
  Samples more than `SAMPLE_SLACK` late are line errors, including a sample never delivered.
- ACK: directed Low = acked; broadcast High = acked. NACK ends the attempt.
  Count NACKs separately: retry if `nacks <= retries` and attempts `< MAX_ATTEMPTS`.
  Arbitration/line errors do not consume the NACK budget. Poll defaults are policy.
- `attempts` counts start bits. Status always classifies the final attempt, including
  mixed failures. `Done.final_attempt_start_us` is the final start-bit falling edge,
  or request time if Busy before any attempt.
- Final NACK completion waits past the decoder's ACK timeout, allowing its truncated
  frame echo to be polled and published before `tx`. Successful completion waits for
  the full final ACK bit; non-final blocks are queued before that trailing high ends.

## 6. `cec_core::bridge` — policy

```rust
pub struct Config {
    pub la: Option<u8>,              // None = allocate for `device_type`
    pub device_type: DeviceType,     // Tv, Playback, Audio, Recorder, Tuner
    pub physical_address: u16,
    pub osd: heapless::String<14>,
    pub vendor: [u8; 3],
    pub cec_version: u8,             // 0x04 1.3a, 0x05 1.4, 0x06 2.0
    pub replies: ReplySet,           // opcodes answered locally; 0x85 and 0x82 rejected by the constructor
    pub ack: AckSet,
    pub allow_from: bool,            // permit submissions with `from` != la (bench only)
}
pub enum PowerStatus { On = 0, Standby = 1, ToOn = 2, ToStandby = 3 }

pub struct Submission { pub id: RequestId, pub frame: Frame, pub retries: Option<u8> }   // RequestId = heapless::String<16>
pub enum SubmitError { BadTo, FromNotAllowed, NoLa, QueueFull }

pub enum Origin { Host(RequestId), Local { reply_to_seq: u32 } }
pub enum BusEvent {
    Rx { seq: u32, t_us: u64, frame: Frame, ack: bool },
    Tx { seq: u32, t_us: u64, origin: Origin, frame: Frame, outcome: TxOutcome },
    Err { seq: u32, t_us: u64, kind: RxError, from: Option<u8>, to: Option<u8>, bits: u8 },
    Line { seq: u32, t_us: u64, stuck_low: bool },
    LaAllocated { seq: u32, t_us: u64, la: u8 },      // after polling; 15 if none free
}

pub struct Bridge { .. }
impl Bridge {
    pub fn new(config: Config, boot_seq_start: u32) -> Self;
    pub fn config(&self) -> &Config;
    pub fn set_power(&mut self, p: PowerStatus);
    pub fn submit(&mut self, s: Submission, now_us: u64) -> Result<(), SubmitError>;   // queue depth 16
    /// Route a decoder event; may enqueue a local reply or complete LA allocation.
    pub fn on_rx(&mut self, ev: RxEvent, now_us: u64) -> Option<BusEvent>;
    /// Route a transmit completion; internal allocation polls emit no TX event.
    pub fn on_tx_done(&mut self, outcome: TxOutcome, start_us: u64) -> Option<BusEvent>;
    /// Next request to hand to `Tx::start`, if the transmitter is idle.
    pub fn next_request(&mut self) -> Option<TxRequest>;
    pub fn seq(&self) -> u32;
    pub fn next_seq(&mut self) -> u32; // also for executor-owned events
}
```

Rules (from `docs/protocol.md` §4, §7.1 and `AGENTS.md`):
- Local replies only for directed frames `to == la`, `from != 15`, opcode in
  `replies`; reply targets per §7.1; `8F` answers with the current `PowerStatus`.
  Replies are submitted with `retries = 1`, jump the host queue, and are published
  as `Tx { origin: Local { reply_to_seq } }`.
- Host submissions: `retries` default 0 for polls, 1 otherwise, clamp to
  `MAX_ATTEMPTS - 1`. Validation errors are returned, not queued (the firmware
  publishes them as `rejected`).
- `from` other than our LA is `FromNotAllowed` unless `Config.allow_from`.
- LA allocation: poll candidates for the type (Tv: 0, 14; Playback: 4, 8, 11;
  Audio: 5; Recorder: 1, 2, 9; Tuner: 3, 6, 7, 10); NACK on a poll = free, take
  it; ACK = taken, next; aborted poll = re-poll the same candidate; none free → 15.
  Then broadcast `84 <pa> <type>` once. ACK set is the selected LA plus configured
  extras; while polling exclude the candidate. The configured fixed LA (or first
  type candidate in auto mode) is the implicit own-address entry, not an extra.
- `seq` increments by one per event of any kind. Executor-produced rejected/busy,
  HPD, boot-line and overrun events allocate from `next_seq()` too.

## 7. `aether_proto`

Serde mirrors of every object in `docs/protocol.md` §3–§9: `Status`, `TxRequest`
(typed outbound `tx`), `RawTxRequest` (lenient inbound `tx`), `BusEvent` with
`#[serde(tag = "ev")]` variants `rx | tx | err | line`, `Telemetry`, `Config`
(wire `cfg`), `Command` (`cmd/<verb>` dispatch via `Verb`) and `CommandResult`,
`OtaRequest`, `StateRequest`, `OtaProgress`, `LogLine`. `Command::State` carries
`StateRequest`; the other command payloads are `ConfigCommand`, `PingCommand`,
`RestartCommand`, and `ResetCommand`.

Field names and enum strings match the wire doc; `#[serde(default)]` on every
optional; unknown keys ignored. Unknown enum values, including `kind` and
`status`, are errors rather than fabricated outcomes. Every string visitor
accepts transient `visit_str` input, including escaped JSON. The manual
allocation-free `BusEvent` decoder validates `from`/`to` as header nibbles (≤15).
Hex `data` serialises uppercase via `Hex`; `TxStatus` uses §5.2 snake strings.
`From<cec_core::bridge::BusEvent> for aether_proto::BusEvent` maps all variants,
including both line-health transitions; `RxError::LineLow` maps to `err: line_low`.

`RawTxRequest` owns `heapless::String<32>` id and `String<64>` data plus `u64`
destination and optional `u64` retries/initiator. These wider bounds preserve
invalid input for correlated rejections without allocation and accept escaped
JSON (unlike borrowed `&str`). `validate(&self, cfg: &cec_core::bridge::Config)`
returns `Result<cec_core::bridge::Submission, (Option<RequestId>, Reason)>`.
Validation order: id length, destination, data length, hexadecimal content,
initiator override, allocated LA. The wire reason is exact; only `id_too_long`
returns no id. Any explicit `from` requires `allow_from` and must be <15;
an invalid override uses `from_not_allowed`. Retries clamp to `MAX_ATTEMPTS - 1`.
Malformed JSON, wrong types, missing required fields, numeric overflow, or strings
beyond the ingress bounds fail deserialization; the executor publishes a
`rejected` event with no id and `reason: malformed`.

In `BusEvent::Tx`, `to: Option<u8>` and `data: Option<TxData>` allow rejected input
to omit an invalid destination or overlong data. Both remain required when
decoding every non-rejected status. `RawTxRequest::rejected_data()` preserves
bounded rejected input, returning `None` when it cannot fit the 30-byte wire
buffer. IDs unavailable to the executor are omitted, including malformed input
and `id_too_long`; local events also omit id.

`no_std` + `alloc`-free: strings are `heapless::String<N>` with the wire bounds
(`id` 16, `osd` 14, `node` 16, `fw` 24, reasons 32), except lenient ingress above.
`serde_json` is optional and activated only by `std` for hosts; it is also a
dev-dependency for tests. Without `std`, the production dependency graph enables
neither `serde_json` nor serde's `alloc` feature.

## 8. Tests (phase 1 acceptance)

- `timing`: none (constants).
- `rx`: encode→decode round-trip for every frame in `tests/fixtures/rx-frames.jsonl`
  via a test-only edge renderer; jitter sweep across each window edge (inside
  passes, 1 µs outside fails with the right `RxError`); 16-block frame without EOM
  → `EomMissing`; low past `START_TOTAL.max` → `LineLow`; sustained low past
  `STUCK_LOW` and recovery each → one `Line`; `AckDue` per addressed block, none
  during own TX or broadcast; sampled ACK boundary at `SAMPLE_AT`; mid-frame join
  and error resync without storms; poll-before-late-edge; NACK-truncated frame;
  quiet origin at ACK fall + `BIT`. ACK membership concerns `to`, not `from`.
- `tx`: scripted executor harness driving `step`/`line_sample`: first-try ack;
  NACK on block 2 of 3 with `retries = 1` → second attempt, `attempts = 2`; poll
  with `retries = 0` → `Nack, attempts = 1`; arbitration loss on bit 2 of the
  initiator nibble then success → `Ack, attempts = 2`; five arbitration losses →
  `ArbLost, attempts = 5`; line errors with explicit `retries = 0` retry to the hard
  cap; mixed failures use final status; arbitration loss does not spend NACK budget;
  busy beyond `BUSY_GIVE_UP`; exact new/same/retry and foreign activity waits;
  missed sample deadlines abort; one Abort per mid-block failure; immediate RMT
  block chaining and authoritative final-attempt timestamp.
- `bridge`: every §7.1 reply with exact bytes and target; no reply to `85`, `82`,
  `from == 15`, broadcast, or opcode outside `replies`; `ReplySet` refuses `85`/`82`;
  submission validation table; poll default retries; LA allocation for each type
  incl. aborted-poll re-poll and candidate ACK exclusion; shared executor/core
  `seq` monotonicity and wrap; late identical echo cannot stamp next request;
  NACK echo is flushed before completion; the `tests/fixtures/all-off-takeover.jsonl`
  scenario replays to the expected outcomes.
- `aether_proto`: golden JSON from every example in `docs/protocol.md` parses and
  re-serialises byte-identically (modulo key order); unknown keys ignored;
  `id` of 17 bytes fails; `From<BusEvent>` mapping for each variant.
