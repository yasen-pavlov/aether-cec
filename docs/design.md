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
    /// Re-anchor the current block's sample/chaining clock to its observed first
    /// falling edge; also updates the final-attempt start for the header block.
    /// Call before delivering any sample. No-op outside Sample/AwaitSample.
    pub fn block_started(&mut self, start_us: u64);
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
    Err { seq: u32, t_us: u64, kind: RxError, from: Option<u8>, to: Option<u8>, bits: u8, detail: rx::ErrorDetail },
    Line { seq: u32, t_us: u64, stuck_low: bool },
    LaAllocated { seq: u32, t_us: u64, la: u8 },      // after polling; 15 if none free
}

// rx.rs — edge-level evidence attached to every decoder error (phase 3.1 diagnostics;
// all fields are facts the decoder already had in hand, no new allocation):
pub struct ErrorDetail {
    pub level: Option<Level>,   // level reported by the failing edge; None for poll() timeouts
    pub same_level: bool,       // the failing edge reported the previous edge's level (glitch / merged edge)
    pub duration_us: u32,       // failed fall→fall period, low-pulse width, or now−last fall for timeouts; saturated to u32
    pub gap_us: u32,            // interval since the previous edge of any level (0 for timeouts); saturated to u32
    // Bit whose low phase is in progress or whose falling edge is due:
    // 0–7 data, 8 EOM, 9 ACK; None in Start. For level: Low failures and
    // high-phase timeouts the failed interval belongs to bit−1
    // (bit: 0 means the previous block's ACK).
    pub bit: Option<u8>,
    // Up to eight preceding edge intervals, oldest first; resets at the start
    // fall. history[0] may be the preceding idle gap, up to u32::MAX.
    // The failing interval appears only in gap_us.
    pub history: heapless::Vec<u32, 8>,
    // Same-level and out-of-order failures report duration_us = t − last fall,
    // saturated to u32, without comparing a timing window.
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
    pub fn queue_depth(&self) -> usize;  // waiting host requests, for telemetry
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
`StateRequest`, `OtaProgress`, `LogLine`, `Healthz`, `HttpError`, `OtaAccepted`. `Command::State` carries
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

## 9. Firmware executor layout

`firmware/` is a standalone ESP-IDF package, excluded from host workspace checks.
`crates/aether-logic` is a plain-std workspace member containing configuration
merge/validation, bounded quoted-console parsing, command plans, validated TX
submission, bounded MQTT reassembly, CEC executor primitives and runtime snapshots.
Protocol structs remain the one
wire definition. `CommandResult.id` is now optional and omitted for malformed
commands; valid commands always echo their ID. `pa:edid` is rejected as
`edid_unsupported` until the configuration contract includes DDC pins.

The main ESP task (32768-byte stack) owns NVS, effective wire Config,
Wi-Fi and the `EspMqttClient` handle. The core-1 bus task owns runtime power
and the single boxed `cec_core::bridge::Bridge`. Main polls Wi-Fi reconnection
every five seconds after a disconnect and
runs MQTT and HTTP command effects serially through the same `apply_plan`. SDK defaults are resolved via the absolute
Cargo-relative `ESP_IDF_SDKCONFIG_DEFAULTS`, including out-of-tree worker targets.
There is no SNTP client: certificate chain/hostname validation is enabled but
validity dates are not checked (`CONFIG_MBEDTLS_HAVE_TIME_DATE` unset), avoiding
a hard NTP dependency. Protocol time always uses the monotonic ESP timer.

The console reader has an explicit 12288-byte stack. A UART0 driver with a
512-byte RX ring backs blocking stdin reads, retaining pasted 200-byte lines.
The reader sends parsed operations through a four-entry sync channel; main alone
writes NVS. It never echoes input; responses and EspLogger share UART0.
Credentials are a JSON blob in NVS `secrets/json`; the HTTP token is a separate
raw UTF-8 blob in `secrets/token`, excluded from credential serialization.
Config is an `aether_proto::Config` JSON blob in `cfg`, and `system/boot` is separate.
Wi-Fi SDK NVS storage is disabled. MQTT reset persists defaults preserving node,
credentials and boot; console factory-reset erases the entire default NVS partition.
WPA input must be empty (open), 8–63 printable ASCII bytes, or 64 hex digits.
The same credential validators run on stored secrets. Invalid stored cfg/secrets
fall back to defaults with a static scrubbed code, never rewriting the credential
blob on a failed load. Token creation/replacement writes only `secrets/token`.
NVS NO_FREE_PAGES or NEW_VERSION_FOUND triggers a logged erase and one init retry.
Wi-Fi configuration,
start and initial-connect failures are nonfatal. The console is started before
fallible startup; its receiver survives every error. Remaining fatal errors enter
a console-only loop supporting show, factory-reset (even without a Store), reboot.
Before loading credentials, firmware disables SDK/dependency log targets, whose
diagnostics can include broker URLs or SSIDs, and enables only its own scrubbed
`aether` application records at info. Errors originating in storage/network
libraries are never formatted into application logs or console responses.

The ESP-MQTT task has an explicit 10240-byte stack, separate 2048-byte input/output
buffers and an 8-KiB outbox. Its callback copies only data messages into a 32-entry channel
without blocking while ESP-MQTT holds its internal mutex. Connection state,
connection generation and the pending shutdown PUBACK id/ack flag are atomic,
not lossy channel events. Fragmented payloads are bounded to 2048 bytes;
oversized input is delivered as malformed. Full ingress increments an atomic
overrun count. Publication uses nonblocking enqueue, dropping newest on failure;
failed enqueues and expired entries count toward telemetry. After enqueue recovers,
main requests an accumulated overrun event from the bus owner's `Bridge::next_seq()`.
Deleted-message callbacks are enabled; failed subscriptions force a disconnect
and esp-mqtt auto-reconnect, ensuring subscriptions are retried.
The callback never logs topics, payloads or credentials.

On connection, main enqueues retained online status, then cfg, then subscribes
to tx and cmd/+. Restart/reset enqueue the command result followed by retained
offline status; main waits for the offline PUBACK, bounded to two seconds,
before rebooting (LWT covers loss of connection). Config changes persist before
the effective value is replaced or acknowledged. Node/topics and GPIO wiring
take effect only after restart. Power is in-memory only and survives cfg changes.
The MQTT log topic is intentionally absent; cfg.log is stored but inert.

Main owns `bus::EventBacklog`, a preallocated 32-event drop-newest FIFO for
MQTT-offline events. SSE receives each event immediately, exactly once. After a
connection's retained status and cfg both enqueue successfully, main flushes
the backlog in original sequence/timestamp order before newer bus traffic.
The FIFO moves the already-serialized SSE bytes alongside each event, avoiding
another allocation/serialization at reconnect; connected traffic bypasses it.
Readiness is gated on the current connection generation, not merely the atomic
connected flag; a failed preamble retries without releasing the FIFO. Only real
FIFO overflow is transferred to `bus::Losses`; offline time alone is not loss.
Main drains the bus channel every turn, even without MQTT credentials. Start the
bus task after network/HTTP constructors, immediately before this drain loop.

Memory instrumentation adds `Snapshot.heap_largest: u32` and optional
`Telemetry.heap_largest: Option<u32>` (old payloads omit it). At 60 seconds,
main snapshots live tasks using `uxTaskGetSystemState`; ESP-IDF's Xtensa
`StackType_t` is a byte, so high-water values are already bytes. Keep main's
32768-byte configured stack until a real boot exercises startup, traffic and
OTA: serde_json output/NVS input are already heap-backed Vecs, and Bridge
is boxed on the bus task, not resident on main's stack. The snapshot is bounded
to 32 tasks, uses no heap allocation, and reports an explicit unavailable code
if that capacity is exceeded. `ffi::MqttDiagnostics` owns an additional SDK event
registration because esp-idf-svc 0.53 discards the detailed MQTT error handle.
Its stable boxed deduplication state is unregistered before client destruction;
only numeric errors reach the application log. See the dated memory budget in
`decisions.md` for SDK configuration and allocation bounds.

The 4-MB custom partition table reserves NVS at 0x9000 (0x6000), otadata at
0xF000 (0x2000), phy_init at 0x11000 (0x1000), and 0x1E0000-byte OTA slots at
0x20000 and 0x200000. The final 0x20000 bytes are split into a 64-KiB `coredump`
partition (data/coredump, 0x3E0000) and unused 64-KiB `storage` NVS (0x3F0000).
Existing NVS, otadata, phy_init and OTA offsets/sizes are unchanged. A one-time USB
flash must install the new table; application OTA cannot change it.
Flash with this table and the IDF-built rollback-enabled bootloader,
not espflash defaults. Boot kind comes from the bootloader's running partition
state before fallible startup: only PENDING_VERIFY requires confirmation.
Non-OTA/USB boots are already valid, including the bootloader's blank-otadata
initialization; there is no provisioning-confirmation path.
Cargo's `ESP_IDF_GLOB_AETHER_*` settings stage and track `partitions.csv` in the
generated esp-idf-sys project, where IDF resolves the custom CSV filename.

Crash diagnostics use IDF's supported flash ELF core dump writer with CRC32 and
an independent 2048-byte panic stack, not a custom exception/abort hook. After
installing the log ring and before console/network startup, `ffi::last_crash()`
checks partition presence and `esp_core_dump_image_check()`. For panic, interrupt
WDT or task WDT resets it calls `esp_core_dump_get_summary()`, formats EXCCAUSE,
PC, task name and up to eight backtrace PCs, logs `last_crash <text>` at error
level and erases the consumed dump with `esp_core_dump_image_erase()`. A valid
dump on another reset cause is cleared without attributing it to that boot.
Read/check/erase errors are static application codes with numeric IDF errors;
an absent partition emits `coredump_unavailable_usb_table_required`.
`esp_core_dump_image_check()` returning `ESP_ERR_INVALID_SIZE` is the erased
partition's `0xFFFFFFFF` length word and is silent, not a check failure.
`bootstage.rs` complements the dump with a boot-progress record in `.rtc_noinit`
(magic, stage, `t_ms`, checksum; a `.ctors` global constructor copies it out
and claims it as `pre_main` before IDF secondary init, `main` formats it once
the logger exists, `mark()` is called at each startup checkpoint and every
main-loop turn). After a panic/WDT reset the previous boot's last checkpoint is
kept as `Status.last_stage: Option<LastStage>` (`heapless::String<32>`,
`"<stage> t_ms=<n>"`) for the whole boot; it is the only crash evidence when the
panic happens before IDF's `init_coredump` has run.
`Status.last_crash: Option<LastCrash>` is additive, where
`LastCrash = heapless::String<120>`; `status::Snapshot.last_crash` borrows it.
The summary stays in RAM for all status/HTTP publications that boot, independent
of the flash erase. The allocation-free host-tested
`status::crash_summary(u32, u32, &[u8], &[u32]) -> LastCrash` caps/scrubs the
16-byte task name, takes at most eight PCs and fits even maximal values in
119 bytes. No existing function signatures or core CEC APIs change.
The prior Rust/NVS panic hook remains useful for Rust panics; it cannot catch
CPU exceptions or IDF aborts. No RTC fallback is installed: IDF 5.4.2 exposes no
supported application hook in its panic path (see the linked source rationale
in README). Full DRAM capture stays disabled, but IDF still records task stacks;
a dump exceeding 64 KiB or a fault inside panic handling may leave no valid
summary. Actual exception capture and partition rollout need the lead's USB
bench test; build/host tests do not demonstrate crash persistence.

### CEC bus executor (phase 3)

`firmware/src/cec/mod.rs` owns Decoder, Tx, boxed Bridge and all bus policy in
`cec-bus`: 12288-byte stack, priority 20, pinned to core 1. Its unsafe hardware
boundary is `firmware/src/cec/hardware.rs`, imported under `ffi::cec`.
Main sends bounded FIFO commands (64 slots) and receives ordered proto events
(128 slots), never waits for bus completion, and leaves MQTT/SSE on main.
Main backpressures MQTT/HTTP consumption when the command mailbox is full.
Receive edges and timeouts are drained before every TX action and completion.
A 1-ms notification timeout polls Decoder at least 200 Hz; edge/sample ISRs wake
the task immediately. Read-only counters/status use a short snapshot mutex:
the bus owner only `try_lock`s, so network work cannot block CEC.

GPIO is input/output open-drain with no internal pull resistors; external sink
pull-up is required. GPIO/RMT resources and their interrupts are initialized on
main/core 0; `Hardware::arm` binds the bus task's notification handle before
enabling edges. The GPIO ISR is IRAM, level 3. Its 256-entry lock-free SPSC ring uses 32-bit atomic slots
and release/acquire indices, preserving full 64-bit ESP timer timestamps.
Drop-newest edge overruns increment telemetry `rx.errors` and reset decoding;
they are not counted as lost bus events. Event-channel/outbox losses accumulate
separately as `err kind:overrun dropped:N`. The host-tested `bus::Losses`
accumulator retains the true count across failed report enqueues; retrying a
report neither adds a loss nor consumes a sequence. Its next sequence is committed
only after enqueue. MQTT pending losses are not transferred to the bus owner until
the current connection's preamble is ready. An overflowing backlog report retains
its original dropped count rather than turning into a recursive one-loss report.
The IRAM ISR and ISR-dispatched sample/ACK-release timers perform no allocation,
logging, serialization or network calls. Timer interrupts are pinned to core 1.
IDF 5.4.2 requires `ESP_TIMER_SHOW_EXPERIMENTAL` to make the CPU1 affinity
choices effective; generated bindings were checked for both CPU1 choices.
CPU1 affinity puts flash-resident `esp_timer_init_os` on core 1 during IDF's
SECONDARY init, where IDF's pre-scheduler flash guard assumes the other core is
parked in IRAM; core 0's `init_coredump` then loads the partition table from
flash and core 1 faults. `bootstage.rs`'s `.ctors` constructor (after the CORE
list, before core 1 resumes) calls `esp_partition_find_first` once so that load
happens while core 1 really is parked; the timer affinity and CEC timing are
unchanged.
The release also enables `RMT_ISR_IRAM_SAFE` and GPIO control in IRAM.
IDF 5.4.2 `esp_timer.c:180-210,294-342,371-387,465-494` marks start_once,
list/lock helpers and ISR dispatch IRAM unconditionally; there is no
`ESP_TIMER_IN_IRAM` switch in this IDF. `esp_timer_impl_lac.c:95-133` keeps
time reads/alarm updates in IRAM, and timer objects use internal-capability
allocation (`esp_timer.c:110`). Boxed ISR context/rings and RMT symbol buffers
are internal DRAM on the PSRAM-less WROOM; ACK guards/timing constants inline
as arithmetic/immediates. Compile-time guards require GPIO control in IRAM,
RMT ISR safety and timer ISR dispatch, preventing a cache-unsafe configuration.

The bus task subscribes itself to the task watchdog at entry to `run`, before
reconfiguring it to monitor only IDLE0 plus the bus task (`idle_core_mask=1`,
timeout 5 s, panic enabled). Reconfiguration removes IDLE1's hook/subscription
together; merely deleting IDLE1 would leave its hook trying to feed an unwatched
task. The bus feeds once per loop iteration, so a wedged owner still trips the
watchdog. IDLE0 and the interrupt watchdog remain monitored. A continuously active
priority-20 CEC task legitimately owns core 1 and must not depend on IDLE1 getting
scheduled. Between frames with no active Decoder/Tx/follower ACK it still uses
`vTaskDelay(1)` to share the core; active TX samples/chaining and ACK arming never
receive that delay. Status and Healthz retain the typed `esp_reset_reason()`
report; the bench observed `task_wdt` after failed uploads, followed by `panic`.

During flash operations IDF 5.4.2 suspends both schedulers, parks the other CPU in
an IRAM IPC loop and disables both flash caches (`spi_flash/cache_utils.c:114-147,
150-217`); IRAM interrupts still run. Backlogged edges plus the priority-20 bus
loop can starve IDLE1 between cache-off intervals. The upload's `Shared.busy`
is an `Arc<AtomicBool>` shared directly with the bus task and GPIO ISR, avoiding
main-loop mirroring latency. On every upload iteration the bus releases RMT and
any follower ACK, clears ACK membership, cancels only the active TX with a
correlated Busy completion, drains/decodes RX, updates snapshots and blocks with
`vTaskDelay(2)` regardless of Decoder activity. The ISR also refuses an ACK arm
when busy. Commands and unstarted requests remain queued; no new TX launches.
On failure the handler clears busy and normal service resumes. On success busy
stays set until reboot. The bus explicitly reports upload quiescence only after
it observes suspension and detached RMT work finishes; queued work does not
prevent the reboot. No core Rust API signatures change.

On EOM rise Decoder emits AckDue; the task arms a predicted-time window. The
actual following falling-edge ISR asserts GPIO low immediately, then starts an
ISR-dispatched one-shot to release after `timing::ACK_LOW` (1500 us).
The prediction only guards the valid next-bit period; it never triggers the
pull-down. The first falling edge consumes the arm even outside the window,
and the window compares the complete 64-bit timestamp, not its wrapping low word.
Expected falling-edge-to-assert latency is several to tens of us,
with a required bench maximum below 200 us under Wi-Fi/HTTP/OTA load.
These are design expectations, **not measurements**. The EOM-rise-to-arm path
must also complete before the shortest remaining high interval (nominal 900 us).
Oscilloscope/logic-analyzer measurements are a required release gate.

RMT TX runs at 1 MHz with 64 memory symbols and queue depth 4. A host-tested
allocation-free renderer emits optional start, eight MSB-first bits, EOM and
initiator ACK including trailing high. Four owned symbol buffers outlive queued
transactions. Every block calls task-context `rmt_transmit` directly; there is no
start timer. A fresh header requires a High pin and an empty edge ring; chained
blocks may queue during a legitimate follower ACK Low while routed RMT is active.
Chained blocks must queue strictly before `block_start_us - 100 us`, otherwise
the executor reports a line error rather than launching a late block.
Each block's first falling edge is timestamped by the GPIO ISR and re-anchors
`Schedule` and `Tx::block_started`, including chaining and the final-attempt start.
For a fresh header, `Schedule::submitted(at_us)` starts the 300-us origin deadline
at the timestamp immediately before `rmt_transmit`, excluding render/copy/routing
preparation. Chained blocks retain their predicted physical start. These fallback
origins are only failure deadlines: if no matching falling edge is seen within
300 us, fail as a line error, never sample from the prediction.
ISR sample timers use that block's physical origin plus the core offset and
retain actual sample timestamps; missed/late samples fail through Tx.
Driven zero read high or a decoder error on our own transmitting echo is a line
error; initiator ones read low are arbitration losses classified by Tx.
ACK polarity remains target-dependent.
Every aborted attempt increments `tx.arb_lost` or `tx.line_error`, even if a later
attempt succeeds. The first cause wins; six detail counters partition line errors
as defined by protocol §6. Only after releasing on Abort, the bus task emits a
rate-limited `tx_attempt_failed <code>` info record (at most one per second).

RMT's matrix output is saved from its allocated channel, not hardcoded to channel
0. During follower operation the matrix is switched to the released GPIO latch
so GPIO ACK writes actually control the pad; during transmit RMT owns the
open-drain output (high releases, never sources). Abort sets the GPIO latch high
and immediately routes the pad to that latch; RMT keeps running detached.
Never call `rmt_disable`, or re-enable RMT after initialization, on classic ESP32:
IDF 5.4.2's synchronous stop path may keep driving or spin with interrupts masked.
An IRAM `on_trans_done` atomic in-flight counter and nonblocking
`rmt_tx_wait_all_done(ch, 0)` gate re-routing; detached queued work must finish
before another send can own the pad. Enabled RMT resources/context remain
retained even on initialization failure, rather than invoking that unsafe stop.
Normal completion simply unroutes once the driver reports done, without abort.
Done clears the waveform deadline and never releases an unrelated follower ACK,
including a Busy result with no initiator waveform.

IDF initializes a newly allocated RMT channel's idle level low. Startup therefore
holds the already-released open-drain pad while priming RMT with an all-high
symbol and high end level, then switches to the GPIO latch before releasing hold.
The priming transaction never reaches the CEC wire, including in listener mode.

Both MQTT and HTTP TX enter the same validated `bus::submit` path on the owner.
Queue-full returns busy; other errors preserve correlated rejected fields.
Host queue depth comes from `Bridge::queue_depth`, max remains capacity 16.
Counters report actual frames/errors, frames ACKed by us, host requests, local
completions, outcomes and extra attempts. Config commands are FIFO barriers:
drain accepted host requests before rebuilding Bridge with its current sequence,
preserve power, and do not let later submissions overtake the change. Pending
old-identity local replies may be dropped on that rebuild. Wire cfg persistence
and command acknowledgement remain on main; actual bus application is asynchronous.
Fixed LA never allocates; auto LA uses core polls and updates status/line.la.
An empty wire `ack` explicitly masks the core's implicit own-LA ACK entry to an
empty Decoder set, including after allocation. With fixed LA, replies=[] and
ack=[] there are no unsolicited line drives; explicit host TX remains allowed.

Bus health starts unknown, becomes ok on a valid frame or one second of high
idle, reports boot low with no edges after 100 ms as no_pullup, and reports later
sustained low/recovery as stuck_low/ok. Main republishes retained status on
health/LA changes. HPD stays null; EDID and the MQTT log topic remain deferred.
OTA's post-response reboot requires the bus owner's explicit upload-quiescent
signal: no active waveform or follower ACK. Commands and queued host requests
are intentionally retained, not drained into transmissions during the upload.

### HTTP ownership, SSE and log capture (phase 2b)

`EspHttpServer` listens on port 80 with `max_open_sockets=4`,
`lru_purge_enable=true`, a 16384-byte httpd stack and 1024-byte maximum request
headers. Its exact method/path table is `aether_logic::http::route`; wildcard
handlers return JSON `not_found` for unknown paths. The embedded `index.html`
(≤ 64 KiB, the console designed in `docs/design/aether-ui/`) includes its
CSS/vanilla JS; no assets, dependencies or build step are needed. It renders all
device- and bus-derived strings through `textContent`/DOM attributes (OSD names
are attacker-controlled bytes from the bus), parses SSE incrementally with LF and
CRLF framing, holds one stream with bounded reconnect backoff, sends config as a
changed-field patch without `node`, and persists an OTA attempt record before the
upload so a lost response is reconciled from `/healthz` and `/api/log` after the
reboot. The UI uses authenticated fetch streams (no token in URLs), storing the
token in localStorage. WebCrypto SHA-256 is preferred; because port-80 LAN
origins are not secure contexts in most browsers, a local JS SHA-256 fallback
keeps upload functional there.

Normal JSON HTTP bodies are bounded to 2048 bytes. A four-entry command mailbox
hands operations to main, which calls `commands::plan` and the same executor
`apply_plan` as MQTT. Config is persisted before cfg publication and response;
TX uses the same asynchronous validated bus-task submission as MQTT, returning HTTP 202.
Read-only snapshots are copied from main at most once per second, never by
sharing the mutable Bridge.
HTTP replies use proto structs; failures map through the pure error-status policy.
Main unconditionally owns the restart/reset shutdown deadline: reboot after
two seconds or earlier on offline PUBACK, regardless of HTTP waiter timeout or
response delivery. The handler's post-response 500-ms grace can speed up reboot
but never gates it. OTA keeps its own post-response 500-ms grace.

SSE requires a native IDF async request because esp-idf-svc unconditionally
completes responses when its normal handler returns. The exact `/api/events`
handler authenticates on httpd, calls `httpd_req_async_handler_begin`, and returns
immediately. At most three 4096-byte-stack SSE tasks own copied requests, leaving
the fourth socket for API/UI use. Each client receives a bounded 16-entry mailbox
of shared immutable SSE strings; a full mailbox disconnects that client rather
than blocking main, MQTT or other subscribers. Workers send an initial comment,
then topic-suffix `event:` / exact serialized JSON `data:` chunks and a keep-alive
comment every 15 seconds. IDF's bounded send timeout handles stuck sockets.
Workers complete the async request and trigger socket closure on disconnect or
overflow. A shared alive flag reclaims subscriptions even during an idle bus.
Main fans bus events directly to the hub before buffering for MQTT, so replay
never duplicates SSE. `MqttLink::publish` fans out status/ota; without a configured
MQTT link, main uses the identical hub directly. The MQTT callback never invokes
SSE or blocks on a connection.

The application `log::Log` wrapper forwards only scrubbed application targets
to EspLogger, and captures info/warn/error into `aether_logic::http::LogRing`
(oldest-first, newest 64). Dependency records are excluded before formatting,
preserving phase-2 secret redaction. The ring is independent of `cfg.log`;
MQTT log publication remains deferred. Token material is never passed to log:
32 bytes from `esp_fill_random`, bracketed by `bootloader_random_enable()` and
`bootloader_random_disable()` before Wi-Fi/ADC startup, become 64 lowercase hex
digits on first boot, persisted separately in `secrets/token`.
`show token` is the sole print operation; `set token` validates/normalizes and persists
before swapping the
active bearer value. Both JSON and native SSE handlers call the shared pure
`authorized(Option<Route>, Option<&str>, &str) -> bool` policy; only GET Index
and Health are exempt, and every other route uses the full-token XOR comparison.

### Push OTA and boot confirmation

The HTTP upload handler runs synchronously on httpd, not main. It checks the
running partition's PENDING_VERIFY state before reserving the shared atomic busy
flag, rejecting with `409 busy` before allocating an id. The same flag suspends
CEC transmission/ACK for the whole upload and, on success, until reboot; failure
clears it. A second streaming upload waits in httpd's accept backlog; busy covers
an accepted upload awaiting reboot.
The handler validates nonzero Content-Length and X-Sha256 against the actual
inactive slot size, obtains a durable monotonic `o-N` id, and streams through
`EspOta` in at most 4-KiB chunks while hashing with `sha2` (default features
disabled). A bounded 16-entry progress mailbox feeds main for MQTT/SSE fan-out.
`aether_logic::http::UploadProgress` owns percent deduplication and the emitted
receiving → verifying → rebooting/failed payloads:
`new(RequestId, usize) -> Self`, `received(&mut self, usize) -> Option<OtaProgress>`,
`verifying(&self) -> OtaProgress`, `finished(&self, Result<(), &'static str>) -> OtaProgress`.
Digest mismatch, short body, flash errors or a project name other than `aether` abort the writer
without activating its slot. The Rust app descriptor explicitly names `aether`;
`complete()` performs IDF's final image validation and sets the boot partition.
The pending `{id,slot}` record is persisted in NVS namespace `ota` before selecting
the partition, so a power cut never produces an untracked update. A failed
complete clears that record. After full verification the handler sends 202
`OtaAccepted`; main reboots at least 500 ms later, after upload quiescence,
without requiring deliberately suspended queues to empty.

`Health::new()` queries `esp_ota_get_state_partition(esp_ota_get_running_partition())`
before anything fallible. Only PENDING_VERIFY sets `BootKind::Pending`; the NVS
record supplies the event id and detects rollback when a nonpending image runs a
different slot. A same-slot record on a valid image is stale and is removed, never
used to re-arm confirmation or a reboot loop. Missing/unreadable record data cannot
turn a pending image into a provisioning boot.
`aether_logic::http::boot_kind(bool, u32, Option<u32>) -> BootKind` exposes that
classification; BootKind is now Valid/Pending/RolledBack (Provisioning is removed).
`boot_action` retains its signature but removes ConfirmProvisioning:
pending + online status PUBACK → mark valid, cancel timer, publish confirmed and
clear pending when an id exists; pending without that proof by 60 s → reset;
old image + successful status publication → publish rolled_back and clear pending;
valid image → no confirmation. Failed report enqueue/removal is retried without
reclassifying an already confirmed image as pending.
A retained `EspTimer` one-shot independently checks bootloader state at 60 s and
calls `esp_restart()` if still pending, even when main or startup deadlocks.
Failure to arm that timer resets immediately, never running pending without a
deadline. IDF's rollback-enabled bootloader marks a still-PENDING_VERIFY slot
ABORTED on reset and boots the previous valid slot; no explicit invalid mark is
necessary. Main/recovery polling retains the same deadline as a second path.
`shutdown_due(u128, bool) -> bool` is the pure unconditional two-second/PUBACK
shutdown policy. First installation of phase 2b by USB is needed before rollback
reporting can be bench-tested: the older phase-2 image does not know this record.

Protocol cutover: removed `OtaRequest`, `Command::Ota` and `Verb::Ota`; renamed
OTA state Downloading to Receiving. This deliberately removes the obsolete
pull-by-URL command rather than retaining a misleading compatibility alias.
Added `Healthz`, `HttpError`, `OtaAccepted`; existing `LogLine` and `OtaProgress`
remain the wire definitions. Host tests cover bearer mismatch positions, handler
auth exemptions including native SSE, exact routes/reasons, header bounds, log
eviction, SSE framing, bootloader-authoritative boot transitions, unconditional
shutdown deadlines, upload progress success/failure sequences and §9/§11 golden
wire objects. NVS effects, entropy quality, independent timer execution,
firmware/network/rollback evidence remain separate from those pure tests and
require the lead's board exercise.
