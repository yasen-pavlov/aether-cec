# aether MQTT protocol — v1 (approved 2026-10-31)

Native MQTT contract between the Rust ESP32 HDMI-CEC bridge ("aether") and Choregos
(`bridge.kind = "aether"`). Replaces the patched-Tasmota contract
(`tele/<t>/SENSOR` / `cmnd/<t>/HdmiSend` / `stat/<t>/RESULT` / `tele/<t>/LWT`), which stays
available in Choregos as `bridge.kind = "tasmota"`.

Status: **APPROVED 2026-10-31** (single `bus` stream QoS 1; firmware-local identity replies incl. `8F` with host-set power; vendor default `012345`; push OTA over the HTTP API; serial-only credentials). Implemented: §2, §3, §6 (counters zero until phase 3), §7–§11; deferred: §4/§5 bus, `log` topic.

---

## 0. What the bus and the current bridge actually do (evidence)

Sources: two days of Choregos bus traces (`cec-bus.2026-10-{30,31}.jsonl`, kept outside the repository;
3 344 rows: 1 531 rx frames, 765 host sends, 953 send results) and the Tasmota driver on
`ht/cec-v16` (`xdrv_70_0_hdmi_cec.ino` = engine, `xdrv_70_1_hdmi_cec.ino` = MQTT surface).

Trace facts that shape the design:

| Observation | Evidence | Consequence for v1 |
|---|---|---|
| Send results are not correlated to requests | 953 `{"HdmiSend":{"Ack":…}}` vs 765 host sends; 187 results in one 16 s burst (2026-10-30 23:33:32–48) with zero preceding `cmnd/…/HdmiSend` in the trace → Choregos' FIFO would have mis-attributed them if any send had been pending | Every tx request carries a host `id`; every outcome echoes it |
| `Ack:false` conflates NACK, arbitration loss, line error, busy bus | X0:726-727 `_transmit_ack` is false for `MSG_TRANSMITTED` NACK *and* `MSG_TRANSMIT_ABORTED`; Choregos' `standby_target_absent` counts all of them as NACK | Outcome is an enum: `ack` / `nack` / `arb_lost` / `line_error` / `busy` / `rejected` |
| RX is blind while a TX is pending | X0:739 `if (isTransmitting()) return;` in the edge ISR; during the 23:33 burst no rx frame was published for 16 s, then the Kodi box's storm appears | Firmware must receive during the pre-TX wait and after arbitration loss; protocol exposes `arb_lost` |
| Host→result latency 92 ms min / 202 ms median / 710 ms max | FIFO correlation of send→RESULT in trace | Firmware queues and reports asynchronously; host never blocks on a result |
| Two rx frames can land 1 ms apart at the host, p1 = 13 ms | consecutive SENSOR timestamps | Device-side µs timestamps; host receive time is not usable for bit-level ordering |
| 26 identical frames repeated within 150 ms (e.g. `84 22 00 01` ×6 from LA 1 at ~200 ms) | Kodi box wedge storm 23:33:49–57 | rx events carry the observed ACK bit so the host can tell NACK-driven retransmits from storms |
| Longest frame seen is 15 bytes (`47 "PlayStation 5"`, ×6) | param census: 14-byte data ×6 | Tasmota caps tx at 14 data bytes (X1:168) and rx at 16; v1 supports the full 16-block CEC frame (header + 15 data) |
| Polls on the bus: `4→0` ×19, `5→5` ×6 (Denon allocating its own LA), `8→0` ×4, `15→0` ×1 | `Data:""` rows | Polls are first-class rx events (`data:""`), with ACK bit |
| Frames from unregistered initiator LA 15 (`8C` 15→0 ×2) | Kodi box boot | `from` may be 15; firmware must never answer them (Tasmota patch X0:1180-1182) |
| Switch presence poll `8F→8` every 5.000 s, 720 ack / 8 nack; `83→8` NACKed ×20 while Switch off | send census | Per-request `retries` must default to a value that preserves "NACK = absent" evidence (see §4) |
| Bridge replies locally to `83 8C 8F 9F 46 91` (TV type), never to `85`/`82`; publishes rx only for `to ∈ {own, 15, 5}` | X0:1188-1239, X1:51 | Local reply set becomes explicit config; rx publishing is promiscuous (host filters) |
| Tasmota has no retransmission at all (dead `_xmitretry` path, X0:722) and no error-bit signalling | X0:504, 931-963 | v1 defines spec-conformant retransmission + signal-free time in firmware and reports `attempts` |

Tasmota bit timing (reference for cec-core, not part of the protocol): start bit low 3 700 µs / total
4 500 µs; data bit low 600 µs (`1`) / 1 500 µs (`0`), period 2 400 µs, sample at 1 050 µs; ACK = follower
holds low (directed) — inverted for broadcast; signal-free time 5 bit periods (new initiator), 7 (same
initiator again), 3 (retransmission); rx windows start-rise [3 400, 4 000] µs, start-fall [4 050, 4 950],
data-rise `1` [300, 900] / `0` [1 200, 1 800], next-fall [1 950, 2 850]. (X0:353-375, 785-932.)

---

## 1. Design goals

1. **Correlated, typed send outcomes.** Every request has a host id; every outcome names the id, the
   final status, and how many bus attempts it took.
2. **Complete bus picture.** One ordered stream carries every frame on the bus (rx, host tx, local
   auto-replies, errors) with device-side µs timestamps and the observed ACK bit.
3. **Boring transport.** Plain JSON over MQTT 3.1.1, inspectable with `mosquitto_sub`, replayable
   by Choregos' JSONL trace tooling. No binary framing.
4. **Bridge survives without the host.** Identity replies (physical address, vendor, OSD, version,
   power) are answered on-device so other CEC devices keep seeing a TV when Choregos is down.
5. **Operable.** Retained status + LWT, periodic telemetry, remote config, push-OTA with rollback.

Transport: MQTT over TLS (port 8883, server certificate verified against the ESP-IDF certificate
bundle, user/password auth) is the default; plain 1883 is a console-selectable fallback for brokers
without a certificate. Non-goals for v1: multi-bridge arbitration, TLS client certificates, MQTT 5
features, binary/CBOR encoding, Home Assistant discovery (Choregos owns HA).
Certificate chains and hostnames are checked, but certificate validity dates are not:
`CONFIG_MBEDTLS_HAVE_TIME_DATE` is unset. There is no SNTP client or hard NTP
dependency on this LAN device; protocol time is monotonic, not wall time.

---

## 2. Topics

`<node>` is the bridge's configured name (default `aether`, `[a-z0-9-]{1,16}`). All payloads are
UTF-8 JSON objects. Unknown keys MUST be ignored by both sides (forward compatibility); every object
from the bridge carries no version field — the schema version is in `status.v`.

| Topic | Dir | QoS | Retain | Payload |
|---|---|---|---|---|
| `aether/<node>/status` | B→H | 1 | yes | §3 status; also the LWT (`{"online":false}`) |
| `aether/<node>/bus` | B→H | 1 | no | §5 bus events: `rx`, `tx`, `err`, `line` — single ordered stream with `seq` |
| `aether/<node>/tx` | H→B | 1 | no | §4 send request |
| `aether/<node>/telemetry` | B→H | 0 | no | §6 periodic counters |
| `aether/<node>/cfg` | B→H | 1 | yes | §7 effective config (republished after every change) |
| `aether/<node>/cmd/<verb>` | H→B | 1 | no | §8 commands: `cfg`, `state`, `ping`, `restart`, `reset` |
| `aether/<node>/cmd/result` | B→H | 1 | no | §8 command outcome, echoes `id` and `verb` |
| `aether/<node>/ota` | B→H | 1 | no | §9 OTA progress |
| `aether/<node>/log` | B→H | 0 | no | §6 optional log lines (level from config, default off) |

Why one `bus` topic instead of `bus/rx` + `tx/result`: MQTT only guarantees ordering per topic. Choregos'
engine depends on the order between a tx outcome and the frames around it (e.g. "a `90 xx` from an
address under standby verification is not a role signal"). A single topic with a monotonic `seq`
preserves that order and makes gap detection trivial.

Why QoS 1 on `bus`: Wi-Fi hiccups are the realistic loss case; a lost `tx` outcome today stalls
standby verification until the 5 s FIFO expiry. With `id` + `seq` duplicates are harmless (host
dedupes on `(boot, seq)`), so at-least-once is strictly better. The esp-mqtt outbox is bounded
(8 KiB); if it overflows esp-mqtt drops the newest event.
Failed enqueues and expired outbox messages increment `outbox_dropped`; after
enqueue succeeds again, the bridge emits one ordered `err kind:"overrun"` with
the accumulated `dropped` count (including a failed overrun report itself).

MQTT session: client id `aether-<node>`, clean session, keepalive 15 s, LWT on `status`, QoS 1,
retained `{"online":false}`. On connect the bridge publishes `status`, then `cfg`, then starts `bus`.
Subscribes `aether/<node>/tx` and `aether/<node>/cmd/+`.

---

## 3. `status` (retained; LWT)

```json
{"online":true,"v":1,"fw":"0.1.0","boot":3,"reset_reason":"software","t_us":1234567,
 "la":0,"pa":"0.0.0.0","type":"tv","ip":"192.0.2.66","rssi":-58,
 "bus":"ok","hpd":null}
```

| Field | Type | Meaning |
|---|---|---|
| `online` | bool | `false` is the LWT payload (exact bytes `{"online":false}`) and is also published on clean shutdown/restart |
| `v` | u32 | protocol schema version; host refuses `v` it doesn't know |
| `fw` | string | firmware semver (+ `-dirty` / git short sha in dev builds) |
| `boot` | u32 | boot counter from NVS; changes on every reset. `t_us` and `seq` restart at 0 with it |
| `reset_reason` | string | Cause of this boot from `esp_reset_reason()`: `power_on`, `software`, `panic`, `int_wdt`, `task_wdt`, `brownout`, or `other` (all remaining IDF causes). New firmware always includes it online and in `/healthz`; older payloads may omit it. The minimal LWT remains unchanged |
| `last_crash` | string ≤ 120 UTF-8 bytes, optional | Previous boot's valid ESP-IDF ELF core dump, recovered only after `panic`, `int_wdt` or `task_wdt`: `"<decimal EXCCAUSE> pc=<8 hex> task=<name> bt=<up to 8 comma-separated 8-hex PCs>"`. Names are at most 16 printable non-whitespace ASCII bytes. PCs are symbolised on the host against the crashing image's ELF. Omitted when unavailable; stays present for the current boot after the stored dump is cleared. Requires the coredump partition installed by USB; OTA cannot add that partition. The minimal LWT remains unchanged |
| `last_stage` | string ≤ 32 bytes, optional | Previous boot's last startup checkpoint, from RTC noinit memory, present only after `panic`, `int_wdt` or `task_wdt`: `"<stage> t_ms=<uptime ms when the stage was reached>"`, stage ∈ `pre_main` (global constructor, before IDF secondary init), `main`, `console`, `store`, `net`, `mqtt`, `http`, `bus`, `loop` (`loop` is re-marked every main-loop turn, so its `t_ms` is the last turn before the crash). Independent of the core dump, so it also covers crashes before the dump writer is initialised; the record is re-claimed from a global constructor, so a valid value always describes the immediately preceding boot, and `pre_main` means that boot died in IDF startup before `main`. Absent after power-on/brownout (RTC memory is lost) and after clean restarts. The minimal LWT remains unchanged |
| `t_us` | u64 | µs since boot (`esp_timer_get_time()`), monotonic, no wall clock on device |
| `la` | u8 \| null | logical address in use; `null` while allocating |
| `pa` | string | physical address `a.b.c.d` |
| `type` | string | CEC device type: `tv`, `playback`, `audio`, `recorder`, `tuner` |
| `bus` | string \| null | `ok` \| `stuck_low` (line low > 100 ms with no valid frame) \| `no_pullup` (line reads low at boot); `null` = not measured (phase 2 publishes `null`) |
| `hpd` | bool \| null | HPD input level if an HPD GPIO is configured, else `null` |

Before MQTT is ready, main retains up to 32 bus events (oldest first) without
changing their sequence or timestamp. On each connection it queues retained
`status`, then `cfg`, then flushes this FIFO before newer bus events. SSE receives
events immediately and does not replay them on MQTT reconnect. Only FIFO overflow
(drop newest), event-channel overflow, or actual MQTT enqueue/expiry failures
count as lost events; merely being disconnected does not.

Choregos mapping: `online:true` → `Event::BridgeOnline` (triggers `resync()`); `online:false` →
`Event::BridgeOffline`. A `boot` change without an intervening `online:false` (retained message lost
or missed) is treated as offline+online. `v` mismatch → adapter error, bridge stays "offline" in
`/healthz`.

---

## 4. `tx` — send request (host → bridge)

```json
{"id":"s-1041","to":8,"data":"8F"}
{"id":"s-1042","to":15,"data":"85"}
{"id":"s-1043","to":4,"data":"4125","retries":0}
{"id":"s-1044","to":8,"data":""}
```

| Field | Type | Rules |
|---|---|---|
| `id` | string ≤ 16 bytes | Opaque, host-assigned, echoed verbatim in the outcome. Host guarantees uniqueness within the bridge's 5 s result window. Fixed buffer on device; longer → `rejected` |
| `to` | u8 0–15 | Destination LA; 15 = broadcast |
| `data` | hex string, even length, 0–30 chars | Opcode + operands, uppercase hex, no separators. `""` = polling message (header only). Max 15 bytes (full CEC 16-block frame) |
| `retries` | nonnegative integer, optional | Retransmissions after NACK, clamped to 4 (at most 5 total attempts). Default: **0 for polls (`data:""`), 1 otherwise** (libCEC's default). Arbitration loss and line errors are always retried, up to 5 total attempts, regardless of this field |
| `from` | u8 0–14, optional | Initiator override. **Test/bench only** (lets a bench bridge impersonate a playback device when replaying traces). Default = bridge's own LA. Refused unless `cfg.allow_from=true` |

Validation failures are reported as a `tx` bus event with `status:"rejected"` and `reason` — never
silently dropped. The ingress parser retains up to 32 UTF-8 bytes of `id` and 64 of `data`,
and accepts escaped JSON strings. Validation checks id length, destination, data length,
hex content, initiator override, then allocated LA, in that order. An id of 17–32 bytes gives
`id_too_long` with no echoed id; data of 31–64 bytes gives `data_too_long` with the valid id
echoed. Invalid JSON, missing required fields, wrong field types, numeric overflow, or strings
beyond the ingress bounds give `reason:"malformed"` with no id. Any explicit `from` requires
`allow_from` and a value in 0–14; otherwise it gives `from_not_allowed`.
The request queue is 16 deep; a full queue yields `status:"busy"` immediately.

Why `retries` defaults to 0 for polls and 1 otherwise: Choregos' presence/standby logic treats NACK
as evidence of absence (`standby_target_absent` needs `nacked >= sent`). Retrying polls in firmware
would hide that. The host can request NACK retransmissions within the five-attempt total cap.

Why `id` is an opaque string and not a u32: the broker is shared with humans (`mosquitto_pub`) and
with the trace replay CLI; a string namespace (`s-…` for Choregos, `m-…` manual) avoids collisions
without a registry. 16 bytes keeps it a stack buffer.

---

## 5. `bus` — ordered event stream (bridge → host)

Every object has `ev`, `seq` (u32, per boot, +1 per event, all kinds share one counter) and `t_us`
(u64, µs since boot, = time of the frame's **start bit falling edge** for `rx`/`tx`, = detection time
for `err`/`line`).

### 5.1 `rx` — a frame observed on the bus

```json
{"ev":"rx","seq":812,"t_us":48211034,"from":1,"to":0,"data":"9000","ack":true}
{"ev":"rx","seq":813,"t_us":48333410,"from":5,"to":5,"data":"","ack":false}
{"ev":"rx","seq":814,"t_us":48400017,"from":4,"to":15,"data":"84210004","ack":true}
```

| Field | Meaning |
|---|---|
| `from`,`to` | header nibbles; `to:15` broadcast; `from:15` = unregistered initiator |
| `data` | opcode + operands, uppercase hex; `""` = poll |
| `ack` | ACK sampled on the last block: directed `true` = somebody pulled low; broadcast `true` = nobody pulled low. Set membership requests a hardware ACK but never overrides the observed level; own transmissions never trigger a follower ACK |

**Promiscuous**: every valid frame is published regardless of `to`. Tasmota published only `to ∈
{own, 15, 5}` (patched); Choregos learns OSD names from `47` replies to LA 5 and benefits from seeing
`4→0 83/8F/9F/8C` bursts in full. The host filters. Worst observed rate is the Kodi box storm
(~10 frames/s); budget is 100 frames/s ≈ 12 KiB/s.

A frame is valid when its blocks have legal timing and an EOM was seen, or when a directed
initiator stops after a NACKed block: once the line stays high past `DATA_TOTAL.max` from that
block's ACK falling edge, publish `rx` with `ack:false` and the blocks received so far (even
without EOM). Other partial/garbled frames go to `err`, never `rx`. Joining mid-frame silently
discards data-width pulses; after a decoding failure emit at most one `err` until a new start.

### 5.2 `tx` — outcome of a transmission (host-requested or local)

```json
{"ev":"tx","seq":815,"t_us":48512000,"id":"s-1041","origin":"host","from":0,"to":8,"data":"8F",
 "status":"ack","attempts":1}
{"ev":"tx","seq":816,"t_us":48540220,"id":"s-1043","origin":"host","from":0,"to":4,"data":"4125",
 "status":"nack","attempts":2}
{"ev":"tx","seq":817,"t_us":48590900,"origin":"local","from":0,"to":15,"data":"84000000",
 "status":"ack","attempts":1,"reply_to":{"seq":811}}
{"ev":"tx","seq":818,"t_us":48600001,"id":"s-1050","origin":"host","to":4,"data":"ZZ",
 "status":"rejected","reason":"data_not_hex"}
```

| Field | Meaning |
|---|---|
| `id` | echoed request id; absent for `origin:"local"` and when unavailable (`id_too_long` / `malformed`) |
| `origin` | `host` (from `tx` topic) or `local` (auto-reply, §7 `replies`) |
| `from`,`to`,`data` | the frame as actually put on the bus; rejected events instead retain validated/bounded request fields: `from` may be absent, `to` is absent if invalid/unavailable, and `data` is absent if longer than 30 UTF-8 bytes or unavailable. Non-rejected events require `to` and `data` |
| `status` | see table below |
| `attempts` | number of times the start bit was sent (1 = first try succeeded). Absent for `rejected`/`busy` |
| `reason` | only for `rejected`: `id_too_long`, `bad_to`, `data_not_hex`, `data_too_long`, `from_not_allowed`, `no_la` (bridge has no logical address yet), `malformed` (ingress deserialization failed) |
| `reply_to.seq` | only for `origin:"local"`: `seq` of the `rx` event that triggered it |

| `status` | Meaning | Choregos `SendResult` |
|---|---|---|
| `ack` | last block acknowledged (directed: target pulled low; broadcast: nobody pulled low) | `ack:true` |
| `nack` | a block was not acknowledged on the final attempt; earlier blocks of that attempt may have been acked | `ack:false`, counts toward `standby_target_absent` |
| `arb_lost` | lost arbitration in the header on the final attempt (another initiator won; its frame appears as `rx`) | `ack:false`, does **not** count as NACK |
| `line_error` | expected line level or sample deadline missed on the final attempt (collision / bus fault / executor delay) | `ack:false`, does not count as NACK |
| `busy` | request queue full, bus continuously busy for > 2 s, or an active request interrupted by an OTA upload (§9) | `ack:false`, does not count as NACK |
| `rejected` | request validation failed | adapter logs error; no `SendResult` |

One `tx` event per request, after the final attempt. Intermediate attempts are not individually
published (the `attempts` counter and the `telemetry` counters cover diagnosis). `t_us` is the start
of the final attempt, taken from TX itself, never a decoder echo. Status always classifies the
last attempt, even after mixed failures. NACK retries are counted separately from arbitration/
line-error retries: `retries` permits that many retransmissions after NACK, subject to the shared
five-attempt cap. The executor drains receive edges and decoder timeouts before publishing `tx`,
so the final attempt's completed (including NACK-terminated) `rx` precedes its `tx` event.

Bus-level timing of a host send (for the host's own timeouts): queue → signal-free wait (5 or 7 bit
periods = 12–17 ms) → frame (≈ 4.5 ms + 10 × 2.4 ms per block) → result. A 2-block frame with no
retries completes in ≈ 70 ms; a 16-block frame with 5 attempts worst-case ≈ 2.2 s. Choregos' 5 s
result expiry stays valid.

### 5.3 `err` — receive error / overrun

```json
{"ev":"err","seq":819,"t_us":48700100,"kind":"start_bit","from":null,"to":null,"bits":0}
{"ev":"err","seq":820,"t_us":48711000,"kind":"overrun","dropped":4}
```

`kind`: `start_bit` (bad start-bit timing), `bit_timing` (data bit outside window), `eom_missing`
(16 blocks without EOM), `line_low` (line held low past the maximum start interval, a frame-level
error, **not** the 100 ms bus-health threshold), `overrun` (device ring or MQTT outbox overflow;
`dropped` events lost). `from`/`to` are filled when decoded. At most one decoder error is emitted
per frame; other events are not rate-limited beyond the outbox limit. The host counts errors for
the "bus degraded" warning. A continuously low line past 100 ms separately emits `line` with
`bus:"stuck_low"` once; recovery emits `bus:"ok"` once.

Decoder errors (`start_bit`, `bit_timing`, `eom_missing`, `line_low`) additionally carry
edge-level evidence (added 2026-11-01 after the live bench showed ~15 % `bit_timing` losses on
frames from some initiators; all keys optional, older payloads omit them):

```json
{"ev":"err","seq":826,"t_us":48900100,"kind":"bit_timing","from":4,"to":0,"bits":40,
 "level":"low","same_level":false,"dur_us":1900,"gap_us":1170,"bit":8,
 "hist":[1790,1500,900,620,1780,1510,890,730]}
```

This falling edge ends a 1 900 µs data period (below the 1 950 µs minimum):
the last accepted low pulse in `hist` was 730 µs, followed by the failing
1 170 µs high interval. For a high-level, non-same-level failure,
`dur_us == gap_us` because the previous edge was the last fall.

| Key | Meaning |
|---|---|
| `level` | `high` \| `low`: level reported by the edge that failed; absent for timeouts detected without an edge |
| `same_level` | the failing edge reported the same level as the previous edge (glitch pair or merged edge) |
| `dur_us` | the value that missed its window: fall→fall period on a falling edge, low-pulse width on a rising edge, time since the last fall for a timeout; saturated to u32. Same-level and out-of-order failures instead report `t − last fall` (saturating at 0 and u32::MAX); no timing window was compared |
| `gap_us` | µs since the previous edge of any level, saturated to u32 (0 for timeouts or out-of-order edges) |
| `bit` | the bit whose low phase is in progress or whose falling edge is due: 0–7 data, 8 EOM, 9 ACK; absent during the start bit. For `level:"low"` failures and high-phase timeouts the failed interval belongs to `bit−1` (`bit:0` ⇒ previous block's ACK) |
| `hist` | up to eight intervals (µs) between preceding edges, oldest first; resets at the start fall. `hist[0]` may be the idle gap before the frame (up to u32::MAX). The failing interval appears only in `gap_us` |

Firmware-side evidence (added 2026-11-16; optional, older payloads omit every key). The decoder
keys above are derived from the edge stream alone: `line_low` means "the last edge the decoder
saw was a fall and no edge followed in time", which a line that is really low and a rising edge
that never reached the decoder produce identically. These keys are measured outside the decoder
and are attached to every decoder error (`start_bit`, `bit_timing`, `eom_missing`, `line_low`):

```json
{"ev":"err","seq":830,"t_us":49000000,"kind":"line_low","from":8,"to":0,"bits":16,
 "same_level":false,"dur_us":5042,"gap_us":0,"bit":9,"hist":[604,1796,1504,896,604,1796,1507,893],
 "pin":"low","pin_high_age_us":5310,"ack_age_us":5042,
 "tmr_int":true,"tmr_en":false,"tmr_alarm_in_us":-3540,
 "tmr_isr_in_us":-3538,"tmr_task_in_us":20000,"ack_due_in_us":-3542}
```

| Key | Meaning |
|---|---|
| `pin` | `high` \| `low`: the GPIO input level read by the bus task when it emitted the error. For timeouts this is the line at detection; for edge-triggered errors it is read after the failing edge was drained (µs to ms later) |
| `ack_age_us` | µs from the bridge's most recent follower-ACK assertion (the edge ISR drove the line low) to this error's `t_us`, saturating at 0. Present only when the coherent record agrees at both reads and the assertion is at most 100 000 µs (`STUCK_LOW`) old at the event; a delayed capture does not change this existing cutoff |
| `ack_held_us` | how long that ACK held the line: assertion to the release ISR. Present only with `ack_age_us`, and only once the release has run; absent with an ACK age = still asserted at capture |
| `pin_high_age_us` | µs from the last time the bus task itself read the pin High to this error's `t_us`, saturating at 0 and at u32::MAX; absent until the first High read of the boot. The task reads the pin once per loop pass (about once per millisecond), independent of the edge interrupt and of the decoder. For edge-triggered errors `t_us` is the failing edge's time, so 0 can mean the High read came after `t_us` |

Reading a `line_low` at `bit:9` on a frame the bridge acknowledges. The ACK-slot fall is both the
decoder's last fall and the assertion timestamp, so for the failing slot `ack_age_us` equals
`dur_us` exactly:

| Evidence | Meaning |
|---|---|
| `ack_age_us` absent, or different from `dur_us` (an earlier block's ACK is larger by multiples of 24 000) | the bridge asserted no ACK in the failing slot |
| `ack_age_us` = `dur_us`; `ack_held_us` absent or above 1 800 | the bridge's own ACK was still held at detection, or was released late (telemetry then shows `ack_late` + 1 and `ack_held_max_us` ≥ `dur_us`) |
| `ack_age_us` = `dur_us`; `ack_held_us` near 1 500; `pin_high_age_us` ≥ `dur_us` | released on time, yet the task never read the line High after the fall: another device held it |
| `ack_age_us` = `dur_us`; `ack_held_us` near 1 500; `pin_high_age_us` < `dur_us` | the line went High after the fall and no edge reached the decoder: edges were lost |

`pin` alone separates the last two rows only on a final block (EOM low phase near 600): after a
good ACK on any other block the initiator keeps sending, so the line can be low again at
detection. The task's High reads are about a millisecond apart; a High read proves the line
rose, while all-low reads over one detection interval make a held line likely, not certain.
The ACK evidence is captured coherently under ESP-IDF's timer hardware lock,
not the timer-list lock. One critical section reads, in order: ACK state and
assertion time; `now` (the same counter reader used by `esp_timer_get_time`);
masked LACT interrupt status; `ALARM_EN`; hardware alarm LO then HI;
cached TASK deadline (`timestamp_id[0]`); cached ISR deadline (`timestamp_id[1]`);
then ACK state and assertion time again, and the completed hold duration.
The state and assertion must agree at both reads; the ages and timer evidence
therefore describe one ACK, never a mixture of two assertions.

Timer evidence (added 2026-11-22; optional, omitted by older firmware):

From the 2026-11-27 dedicated-release firmware onward, a separate GPTimer
schedules follower-ACK release. The `tmr_*` group still describes `esp_timer`
(TX sampling and bus-task wake), not the release deadline; the reading table
below applies to older shared-release firmware. `ack_due_in_us`, `ack_age_us`,
`ack_held_us`, `ack_late` and `ack_held_max_us` keep their meaning.

| Key | Type | Meaning |
|---|---|---|
| `tmr_int` | bool | LACT masked interrupt status bit set at capture; presence marks the captured group |
| `tmr_en` | bool | LACT `ALARM_EN` set at capture |
| `tmr_alarm_in_us` | i32 | hardware alarm register minus counter, in µs, saturating; negative = already passed. Register ticks are converted at 2 ticks/µs, truncating to µs like IDF's counter |
| `tmr_isr_in_us` | i32 | IDF's cached next ISR-dispatch deadline (`timestamp_id[1]`) minus capture `now`, saturating; absent when the deadline is `UINT64_MAX` |
| `tmr_task_in_us` | i32 | same for the TASK-dispatch deadline (`timestamp_id[0]`) |
| `ack_due_in_us` | i32 | ACK assertion time + `ACK_LOW` (1 500 µs) minus capture `now`, saturating; negative = overdue |

`tmr_int`, `tmr_en`, `tmr_alarm_in_us` and `ack_due_in_us` are present together,
only on decoder errors and only if the ACK state was held at both reads with
the same assertion time, at most `STUCK_LOW` old at capture. Cached ISR/TASK keys occur
only inside that group and only when their individual deadlines exist.
All `*_in_us` values use capture `now`, not the event's `t_us`.
No timer-list lock, logging or allocation occurs inside the section. Its only
register write is the counter latch used by IDF's time reader; it does not
acknowledge an interrupt, reprogram an alarm or change ACK scheduling.

Reading the timer group (a timer interrupt is shared by **every** deadline):

| Evidence | Interpretation |
|---|---|
| `tmr_int:false`, `ack_due_in_us < 0`, ISR deadline absent or later than ACK due by more than tolerance | ACK release deadline is missing from IDF's bookkeeping |
| `tmr_int:false`, ISR deadline equals ACK due within tolerance, and hardware alarm does not match the release deadline | Bookkeeping intact, hardware not programmed for that deadline |
| `tmr_int:true`, ISR deadline equals ACK due within tolerance and is negative | Consistent with the ACK's alarm raised and not yet delivered; not conclusive alone |
| `tmr_int:true` otherwise | A timer interrupt was pending for some deadline; no conclusion about the ACK |

`tmr_en:false` alone does not establish missing hardware programming. If the alarm
value matches the release deadline and has passed, the enable bit may merely have
cleared itself when the alarm fired; read it together with `tmr_int`. Self-clearing
is documented for the timer group's general-purpose timers, but is **unverified
for LACT** (IDF v5.4.2 `soc/timer_group_struct.h`: general timers at lines 20–25,
LACT's reserved description at lines 402–405).

Use **10 µs** as a reading aid for comparisons: the timer's stored deadline is
taken a few µs after the edge ISR's assertion timestamp, plus possible lock wait.
This is not a bound; firmware publishes raw values without classification.
Larger differences need context. An earlier TASK
deadline can legitimately own the hardware alarm, so compare both cached deadlines
before attributing a mismatch to a fault. An absent `tmr_task_in_us` while
TASK-dispatch timers are armed is itself notable.

### 5.4 `line` — bus line state changes

```json
{"ev":"line","seq":821,"t_us":48800000,"bus":"stuck_low"}
{"ev":"line","seq":822,"t_us":49800000,"bus":"ok"}
{"ev":"line","seq":823,"t_us":49900000,"hpd":true}
{"ev":"line","seq":824,"t_us":49910000,"la":4}
```

Emitted on change only; current values are mirrored in `status`.

`la` is emitted when logical-address allocation completes; 15 = none free.
`status` is republished too.

---

## 6. `telemetry` and `log`

`telemetry` every `cfg.telemetry_s` (default 60 s; Tasmota used 300 s and Choregos' "stale" rule is
15 min = 3 × period — keep 3× and move the rule to 3 min):

```json
{"t_us":60000123,"boot":3,"rssi":-58,"link_count":1,"heap_free":168000,"heap_min":150000,
 "rx":{"frames":1531,"errors":2,"acked_by_us":303,"ack_late":0,"ack_held_max_us":1512,"edges_lost":0},
 "tx":{"host":765,"local":120,"ack":740,"nack":37,"arb_lost":3,"line_error":0,"busy":0,"rejected":1,
       "retransmits":12,"origin_missing":0,"sample_late":0,"launch_rejected":0,
       "echo_error":0,"edge_overrun":0,"driven_low_mismatch":0},
 "queue":{"depth":0,"max":16},"outbox_dropped":0}
```

Counters are per boot, monotonic. Choregos maps `rssi` → `signal_dbm`, `link_count` (MQTT connect
count this boot) → `link_count`, `t_us/1e6` → `uptime_s`; the `rx.errors` / `tx.arb_lost` deltas feed
the dashboard bus-health view.

`heap_largest` is the largest contiguous `MALLOC_CAP_8BIT` free block, in bytes
(optional; older telemetry omits it). Compare it with `heap_free` to distinguish
fragmentation from total exhaustion. New firmware always includes it.
`rx.ack_late`, `rx.ack_held_max_us` and `rx.edges_lost` (added 2026-11-16; optional, older
telemetry omits them) are measured in the interrupt handlers, independent of the decoder:
`ack_held_max_us` is the longest follower-ACK hold this boot (assertion to release ISR, nominal
1 500), `ack_late` counts holds longer than `DATA_LOW_0.max` (1 800 µs), and `edges_lost` counts
edges the ISR dropped because its ring was full (each such batch also resets the decoder and is
included in `rx.errors`).
At 60 seconds after boot, firmware emits one `stack_hwm task=<name> hwm=<bytes>`
record per live FreeRTOS task into the serial log and `/api/log`; the measurement
includes startup's deepest path, not just the current stack depth. Tasks that
have already exited or start after that snapshot are not represented.
MQTT reconnect failures emit numeric `mqtt_error` type/ESP/TLS/verification/
CONNACK/socket codes once per disconnected retry series (reset on connection),
plus `mqtt_handshake_no_memory` on the first allocator failure in that series.
No broker address, certificate subject, credentials or payload is logged.

`tx.arb_lost` and `tx.line_error` count **failed attempts**, including failures
hidden by a later successful attempt, not just the final outcome. The six
additional counters partition `tx.line_error`: `origin_missing` (no block-origin
edge within 300 µs of RMT submission for a fresh header, or the predicted chained
start), `sample_late` (late/missing sample), `launch_rejected` (bus-acquisition,
chaining or RMT submission rejected), `echo_error` (malformed own echo),
`edge_overrun` (lost GPIO edges during TX), and `driven_low_mismatch` (driven zero
sampled High). Count only the first cause per aborted attempt. Older payloads
without these detail fields decode with zero values. `ack`, `nack`, and `busy`
continue to count final request outcomes.

Firmware also keeps rate-limited application info records in `/api/log`:
`tx_attempt_failed <code>`, at most one per second, after releasing the failed
waveform. Launch detail codes distinguish `detached_busy`, `launch_line_low`,
`launch_edges_pending`, `chain_late`, `chain_inactive`, and `rmt_rejected`;
the other codes match their counters. Counters are not rate-limited.

`log` (only when `cfg.log` ≠ `"off"`): `{"t_us":…,"lvl":"warn","msg":"…"}`. Never contains
credentials. Off by default; debugging aid for bench work.

---

## 7. `cfg` — effective configuration (retained) and `cmd/cfg`

```json
{"node":"aether","la":0,"type":"tv","pa":"0.0.0.0","osd":"Aether","vendor":"012345",
 "cec_version":"1.4","replies":["83","8C","8F","9F","46","91"],"ack":[0],
 "telemetry_s":60,"allow_from":false,"log":"off","gpio":{"cec":4,"hpd":null}}
```

| Field | Type | Meaning / default |
|---|---|---|
| `node` | string | topic name component. Changing it takes effect after restart |
| `la` | u8 0–14 \| `"auto"` | Fixed logical address, or allocate by polling the candidates for `type` (TV: 0, 14; playback: 4, 8, 11; audio: 5; …) and fall back to 15. Default `0` |
| `type` | string | device type reported in `84` and used for allocation. Default `tv` |
| `pa` | string \| `"edid"` | Physical address reported in `84`. `"edid"` = read from the sink's EDID over DDC (I²C, pins 15/16) — only if those GPIOs are configured; otherwise error. Default `"0.0.0.0"` (the bridge *is* the TV). Never consult EDID when fixed (Tasmota forced-PA lesson) |
| `osd` | string ≤ 14 | OSD name for `47`. Default `Aether` |
| `vendor` | 6 hex | vendor id bytes for `87`, in wire order. Default `012345`. Note: Tasmota stored `0x012345` but emitted it little-endian (X0:1198 `(uint8_t)vendor, >>8, >>16`), so the sources have seen `87 45 23 01` until now; cutover changes the vendor they see (decided 2026-10-31: fix it, watch vendor-specific traffic such as Apple `A6` on the bench) |
| `cec_version` | `"1.3a"` \| `"1.4"` \| `"2.0"` | value for `9E`. Default `1.4` |
| `replies` | list of opcodes | directed opcodes the bridge answers **locally** (§7.1). Any opcode not listed is left to the host. Default as shown. `85` and `82` are refused (Choregos PR #12 lesson: only the active source answers `85`; answering stale `82` made Kodi go Inactive Source) |
| `ack` | list of LA | logical addresses the bridge acknowledges in hardware. Default `[la]`. Broadcasts are always "acked" (not NACKed). Lets a bench bridge ack an impersonated LA |
| `telemetry_s` | u16 | period; 0 disables |
| `allow_from` | bool | permit `from` in `tx`. Default false |
| `log` | `off`\|`error`\|`warn`\|`info`\|`debug` | `log` topic level |
| `gpio` | object | `cec` (required), `hpd` (optional). Changing requires restart |
| `power` | — | **not** in `cfg`; runtime state via `cmd/state` (§8). Boots as `on` |

Persisted in NVS. `cmd/cfg` takes a partial object, validates the whole merged result atomically,
persists, republishes `cfg`, then answers on `cmd/result`. Invalid → nothing changes, `ok:false` with
`reason`. Wi-Fi SSID/password and MQTT host/credentials are **not** settable over MQTT (see §10).

### 7.1 Local replies

For a directed frame `to == la` (never `from == 15`, never broadcast), opcode in `replies`:

| Opcode in | Reply | To | Notes |
|---|---|---|---|
| `83` Give Physical Address | `84 <pa> <type>` | broadcast | |
| `8C` Give Device Vendor ID | `87 <vendor>` | broadcast | bytes exactly as configured (wire order) |
| `8F` Give Device Power Status | `90 <power>` | initiator | `power` from `cmd/state`: `on`=00, `standby`=01, `to_on`=02, `to_standby`=03 |
| `9F` Get CEC Version | `9E <ver>` | initiator | `1.3a`=04, `1.4`=05, `2.0`=06 |
| `46` Give OSD Name | `47 <osd>` | initiator | |
| `91` Get Menu Language | `32 "eng"` | broadcast | |

Replies are transmitted with `retries` = 1 and published as `tx` events with `origin:"local"` and
`reply_to.seq`, so Choregos' bus log shows them (today it never sees frames from LA 0 at all).
Exactly one side answers each opcode: the Choregos aether adapter reads `cfg.replies` on connect and
logs a warning if its own `[fake_tv]` set overlaps. No automatic `Feature Abort` is sent for
unhandled opcodes (the host may send `00 <op> 00` itself); that keeps the host free to answer anything
not in `replies` within the 1 s CEC response window.

Open question for the reviewer: `8F` could also be left to Choregos, which knows the real room state.
Local answer with host-set `power` is proposed because a device querying power while Choregos is
restarting should still get an answer.

---

## 8. `cmd/<verb>` and `cmd/result`

Every command object has an `id` (same rules as `tx.id`); the result echoes `id` and `verb`:
Malformed command JSON receives `ok:false, reason:"malformed"` with `id` omitted.

```json
{"id":"c-7","verb":"ping","ok":true,"t_us":51000000,"echo":"abc"}
{"id":"c-8","verb":"cfg","ok":false,"reason":"forbidden_reply"}
```

| Verb | Request | Effect |
|---|---|---|
| `cfg` | partial §7 object | merge + persist + republish `cfg` |
| `state` | `{"power":"on"\|"standby"\|"to_on"\|"to_standby"}` | sets the value for local `8F` replies; not persisted |
| `ping` | `{"id":…,"echo":"…"}` | result carries device `t_us`; host uses RTT/2 to map `t_us` onto wall time (sub-10 ms absolute, µs relative) |
| `restart` | `{"id":…}` | publishes `status {"online":false}` then reboots |
| `reset` | `{"id":…,"confirm":"<node>"}` | restore default CEC config in NVS, preserving provisioned `node` and Wi-Fi/MQTT secrets; restart |

Commands are processed in order of arrival; `cmd/result` is QoS 1.
Serial `factory-reset` is deliberately stronger: erase the entire default NVS
partition, including config, node, secrets and boot counter, then reboot.

---

## 9. OTA (push, over the HTTP API)

Push model (decided 2026-10-31, replacing the earlier pull-by-URL draft): the bridge sits on an IoT
VLAN that may not reach any image host, while operators and CI on the LAN can always reach the
bridge. An operator uploads the image to the bridge's HTTP API (§11, `POST /api/ota`); the bridge
streams it into the inactive OTA partition, verifies SHA-256, reboots, and confirms the new image
only after MQTT connects and the retained online `status` receives PUBACK. The bootloader's running
partition state, not the NVS record, determines whether an image is pending verification. An
independent one-shot timer resets a still-pending image at 60 s, causing bootloader rollback even
if main stalls or startup fails. Non-OTA/USB boots are already valid and need no application
confirmation; the bootloader initializes blank otadata as VALID.

Progress is published on MQTT `ota` and on the HTTP event stream:

```json
{"id":"o-1","state":"receiving","pct":37}
{"id":"o-1","state":"verifying"}
{"id":"o-1","state":"rebooting"}
{"id":"o-1","state":"failed","reason":"sha256_mismatch"}
// after reboot, from the new image
{"id":"o-1","state":"confirmed","fw":"0.2.0"}   // or "rolled_back" from the old one
```

`id` (bridge-assigned, `o-<n>`) and the target slot survive the reboot in NVS so confirmation
can be attributed and the old image can report `rolled_back` when the recorded slot differs
from its running slot. This record never decides whether the running image is pending.
Rules: `X-Sha256` header mandatory and checked before the slot is activated; image must be an
esp-idf app image for the same project name. OTA is refused (`409 busy`) while the running image
is still pending verification or an accepted upload is awaiting reboot, before allocating a
new id. There is no MQTT `cmd/ota`.

During an upload the bridge does not transmit or acknowledge; frames addressed
to it are NACKed for roughly 15 s (the upload duration, not a fixed timeout).
Any in-progress `tx` is aborted/released and receives its correlated `busy`
outcome, with `t_us` at interruption and no `attempts`; unstarted requests stay
queued. Receive edges are still drained/decoded, but flash cache stalls may lose
edges/frames; this is not a gap-free capture mode. A failed upload resumes normal
CEC operation. A successful upload stays passive through the response/reboot
grace; reboot waits for the released hardware, not for suspended queues to empty.

## 10. Provisioning (out of band — recorded here so the contract is complete)

Wi-Fi SSID/password, MQTT host/port/user/password/TLS, `node`, and the HTTP API token are
provisioned over the USB serial console (`aether> set wifi …`, stored in NVS, never echoed back).
They are not readable or writable over MQTT or HTTP. Tasmota's captive portal is dropped for v1;
the bridge is a fixed-IP device with a DHCP reservation and a broker account tied to its MAC.

---

## 11. HTTP API and web UI

Machine-readable HTTP contract: [`docs/openapi.yaml`](openapi.yaml) (OpenAPI 3.1).

One HTTP server on port 80, LAN-only (the IoT VLAN firewall is the perimeter; TLS is not offered
because the device has no certificate). Bodies are the exact same JSON objects as the MQTT payloads;
every handler calls the same logic as the MQTT verb, so the web UI, Choregos, `curl` and
`mosquitto_pub` speak one dialect.

Authentication: `Authorization: Bearer <token>` on every endpoint except `GET /` (the page embeds its assets) and
`GET /healthz`. The token is 32 random bytes (hex), generated before Wi-Fi/ADC startup with the
bootloader entropy source enabled and stored separately in NVS `secrets/token`; loading invalid
credentials never overwrites the credentials blob. Console `show token` prints it;
`set token <hex>` replaces only that key. Missing/wrong token → `401`.

| Method | Path | Body / response |
|---|---|---|
| `GET` | `/` | web UI (one HTML, one CSS, vanilla JS; no build step) |
| `GET` | `/healthz` | `{"online":true,"fw":…,"boot":…,"reset_reason":"software"}` — unauthenticated, for monitoring; reset cause uses §3's vocabulary |
| `GET` | `/api/status` | §3 object |
| `GET` | `/api/cfg` | §7 object |
| `PUT` | `/api/cfg` | partial §7 object → `cmd/result`-shaped reply, same validation as `cmd/cfg` |
| `POST` | `/api/cmd/<verb>` | `ping` \| `state` \| `restart` \| `reset`: same payload and result as §8 |
| `POST` | `/api/tx` | §4 request → `202`; the outcome is a `bus` event (SSE and MQTT) |
| `GET` | `/api/events` | `text/event-stream`: every `bus` event, `status` change, `ota` progress and `telemetry` sample, as they are published to MQTT (telemetry is also emitted here while MQTT is down); `event:` = topic suffix, `data:` = payload |
| `GET` | `/api/log` | last 64 `log` lines as a JSON array, regardless of `cfg.log` |
| `GET` | `/api/telemetry` | the latest §6 telemetry object, sampled once per second for HTTP; `404 not_found` only during the first second after the bus starts |
| `POST` | `/api/ota` | raw image body, `Content-Length` set, `X-Sha256: <64 hex>` → `202 {"id":"o-1"}`; `400` bad digest/header, `409` busy, `413` too big for the slot |

HTTP errors use `{"ok":false,"reason":"…"}` with the §8 reason vocabulary plus `unauthorized`,
`not_found`, `too_large`, `busy`. At most 4 concurrent connections; SSE clients count against that. `/api/events` is additionally admitted only while free heap is ≥ 60 000 bytes (`409 busy` otherwise): a console session costs ≈ 27 KB and main needs ≈ 30 KB of burst headroom, so on the 4 MB WROOM this means one live console at a time.
The single httpd task streams uploads synchronously: a second upload while one is streaming waits
in the accept backlog; `409` covers an accepted-but-not-yet-rebooted upload (and a running image
still pending verification). Other requests, including `/healthz`, can also wait behind an upload.
HTTP status snapshots refresh at most once per second. Restart/reset always retain main's
two-second shutdown deadline, or reboot earlier on offline PUBACK; the HTTP response's 500-ms
grace is only a speed-up, not a condition for reboot.
The web UI shows status, the live bus log, the cfg form and the OTA upload; it never shows secrets.

## 12. Mapping onto Choregos (`bridge.kind = "aether"`)

| Protocol | Choregos today | Change |
|---|---|---|
| `status.online` / LWT | `tele/<t>/LWT` → `Event::BridgeOnline/Offline` | same events; add `boot` tracking → treat `boot` change as offline+online |
| `bus.rx` | `tele/<t>/SENSOR` → `Frame::parse` → `Event::Frame` | `Frame` gains `ack: Option<bool>` and `t_us: Option<u64>` (None for Tasmota); filter stays in the bus model (`from == bridge` ignored for learning) |
| `bus.tx origin:host` | `stat/<t>/RESULT` + FIFO → `Event::SendResult{to,data,ack}` | id-correlated; `SendResult` gains `status: SendStatus` + `attempts`; only `nack` increments `nacked` for `standby_target_absent` |
| `bus.tx origin:local` | not visible | pushed to the UI ring as `FrameDirection::Sent` with origin tag; not an engine event |
| `bus.err` / `bus.line` | n/a | new `Input::BridgeHealth`; dashboard bus-degraded warning + HA binary sensor (the proposed CEC-wedge follow-up) |
| `telemetry` | `tele/<t>/STATE` → `BridgeTelemetry` | same struct; stale threshold = 3 × `telemetry_s` |
| `tx` | `cmnd/<t>/HdmiSend {"To","Data"}` | adapter publishes `{"id","to","data"}`; `OutstandingSends` becomes an id map with the same 5 s expiry and reset points; raw-send cap 14 → 15 for aether |
| `cfg` | `[bridge] logical_address` | adapter reads `la`/`pa`/`replies` from the retained `cfg`; `[bridge] logical_address` stays as the configured expectation and mismatches are logged |
| `cmd/state power` | n/a | engine already knows room power; adapter sends `state` on room on/off transitions |
| trace JSONL | keyed on Tasmota topic names | `trace.rs` learns the `aether/<node>/…` topics; replay CLI correlates by `id` |

`BridgeConfig` becomes `#[serde(tag = "kind", rename_all = "snake_case")]` with `Tasmota { topic,
logical_address }` and `Aether { node, logical_address }` (precedent: `WakeConfig`/`TvOffConfig`).
Both adapters implement the same internal `Bridge` surface (`send(to, data) -> Option<SendId>`, event
stream), which does not exist as a trait today and is introduced with the second implementation.

---

## 13. Tradeoffs decided in this draft (for approval)

| # | Decision | Alternative | Why |
|---|---|---|---|
| 1 | JSON, readable keys | CBOR / packed binary | inspectable with `mosquitto_sub`, zero new tooling, replay-friendly; payload ~110 B/frame is nothing on a LAN |
| 2 | One `bus` topic with `seq`, QoS 1 | `bus/rx` + `tx/result` split, QoS 0 | ordering between tx outcome and neighbouring rx is semantically load-bearing; at-least-once + dedupe beats "lost result = 5 s stall" |
| 3 | Host-assigned opaque `id` ≤ 16 B | bridge-assigned counter; no id (today) | correlation without a round trip; multiple publishers can coexist; fixed buffer on device |
| 4 | Outcome enum + `attempts` | `Ack:bool` | arbitration loss / line error must not count as "device absent"; retransmit counts are the bus-health signal |
| 5 | Promiscuous rx | own/15/5 filter (today) | host learns from directed traffic between other devices; cost is trivial |
| 6 | Device µs monotonic time + `boot`, no SNTP | SNTP wall clock on device | monotonic is unambiguous; host maps via `ping` RTT; no clock steps mid-trace |
| 7 | Firmware retransmits (spec: ≤ 5), host chooses `retries`, polls default 0 | all retries at the host | a retransmission must happen 3 bit periods after the failure — only the firmware can; but NACK-as-evidence must stay visible to the host |
| 8 | Identity replies on-device with host-set `power`; `85`/`82` refused; no auto Feature Abort | all replies at the host (Choregos fake TV) | devices must see a TV even when Choregos is restarting; 1 s CEC response limit vs MQTT RTT |
| 9 | Push OTA to the device's HTTP API + sha256 + rollback (revised 2026-10-31) | pull from an HTTPS URL; push over MQTT | the IoT VLAN cannot reach image hosts; operators can always reach the bridge; MQTT chunking is more firmware for no gain |
| 10 | Credentials only via serial/NVS | MQTT-settable creds / captive portal | smaller attack surface, no secrets on the broker |
| 11 | `tx` validation errors are bus events | silent drop / separate error topic | one place to look; replay sees them |

Things explicitly **not** in v1 and why: per-attempt events (noise; `attempts` suffices), raw bit
timing dumps (bench-only, serial), multiple logical addresses per bridge beyond the `ack` list,
HA discovery from the bridge (Choregos owns HA), MQTT 5 response topics.
