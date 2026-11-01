# aether-cec

Rust firmware for an ESP32 HDMI-CEC bridge ("aether") that exposes the CEC bus
over a native MQTT protocol, plus the host-testable crates it is built from.

Status: alpha, bench-verified on a real living-room bus (2026-11-01). The
host-tested CEC core, the ESP32 GPIO/RMT bus executor and the HTTP API/web
UI/push OTA are implemented against [`docs/protocol.md`](docs/protocol.md).
Verified live: promiscuous receive with ACK bits, follower ACK, local identity
replies, directed and broadcast host sends answered by the receiver and a
playback device on the first attempt, OTA upgrade and bootloader rollback.
OTA uploads on the live bus run clean (5/5) after the heap-abort bench setting
was removed; crashes are captured to a flash core dump and surfaced as `last_crash`.
No host adapter uses the protocol yet (Choregos `bridge.kind = "aether"` is next).

| Crate | Purpose |
|---|---|
| `crates/cec-core` | `no_std` CEC codec, transmit state machine, bridge policy |
| `crates/aether-proto` | serde types for the MQTT protocol, shared with hosts |
| `crates/aether-logic` | host-tested firmware config, console, commands and JSON |
| `firmware/` | esp-idf application and core-1 GPIO/RMT CEC executor (phase 3) |

Hardware target: ESP32-WROOM-32 DevKit, CEC on one open-drain GPIO with pull-up,
HDMI pin 13 → GPIO, pin 17 → GND. Nothing else is connected.

The first host is [Choregos](https://github.com/yasen-pavlov/choregos)
(`bridge.kind = "aether"`).

## Firmware

The standalone `firmware/` package targets ESP32-WROOM-32 (4 MB flash, no PSRAM),
using ESP-IDF v5.4.2 and `esp-idf-svc` 0.53. Install the ESP Rust toolchain with
`espup` and the `ldproxy` / `espflash` tools, then:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
. "$HOME/.espup-export.sh"
cd firmware
unset CARGO_TARGET_DIR
cargo build --release
# Locate the bootloader produced by this ESP-IDF build, not espflash's bundled one.
bootloader=(target/xtensa-esp32-espidf/release/build/esp-idf-sys-*/out/build/bootloader/bootloader.bin)
test "${#bootloader[@]}" -eq 1 && test -f "${bootloader[0]}"
# Offline size/layout check: app must fit ota_0 (0x1E0000 bytes).
espflash save-image --chip esp32 --flash-size 4mb \
  --partition-table partitions.csv --bootloader "${bootloader[0]}" \
  target/xtensa-esp32-espidf/release/aether /tmp/aether-esp32.bin
# Bench hardware only, disconnected from the production CEC bus.
# FIRST FLASH OVER TASMOTA ONLY: erase foreign NVS, otadata and stored credentials.
espflash erase-flash
espflash flash --flash-size 4mb --partition-table partitions.csv \
  --bootloader "${bootloader[0]}" --monitor \
  target/xtensa-esp32-espidf/release/aether
```

Use Bash for the bootloader glob above; the check must succeed before flashing.
Build **in-tree without a CARGO_TARGET_DIR override**: embuild derives workspace
paths from its output directory, so a cache target can silently skip SDK defaults.
The Cargo config also resolves `ESP_IDF_SDKCONFIG_DEFAULTS` relative to this package.
Before flashing, inspect the generated
`target/xtensa-esp32-espidf/release/build/esp-idf-sys-*/out/sdkconfig`:
main stack must be 32768, flash 4 MB, custom partition table enabled and
bootloader rollback enabled.

`partitions.csv` reserves two 0x1E0000-byte OTA slots, NVS at 0x9000,
otadata at 0xF000, phy_init at 0x11000, a 64-KiB `coredump` data partition at
0x3E0000 and unused 64-KiB `storage` NVS at 0x3F0000.
Pass the custom table and the **IDF-built rollback bootloader on every USB flash**;
espflash's defaults do not provide this OTA layout. Erasing all flash is necessary
once when replacing Tasmota because its NVS/otadata overlap these offsets and
otherwise leave foreign settings and secrets behind. Do not erase on routine updates.
First USB/provisioning boots are already VALID: the bootloader initializes blank
otadata, so no application confirmation or broker is needed. For push updates,
the bootloader's PENDING_VERIFY state is authoritative, even if NVS loading fails.
The new image must connect to MQTT and receive the retained online status PUBACK
within 60 seconds. An independent esp_timer one-shot resets a still-pending image
even if main stalls, so the rollback-enabled bootloader returns to the old slot.
Pending update identity and target partition survive in NVS; the old image reports
`rolled_back` after reconnect. A stale record cannot turn a valid image into pending.

USB UART0 provides the `aether> ` console (115200 baud, newline-terminated input).
It does not echo input; disable local echo in your serial terminal as well.
Commands have a 200-byte line limit. Double quotes preserve password whitespace;
there is no backslash-escape syntax.
Wi-Fi passwords must be empty (open), 8–63 printable ASCII characters, or exactly
64 hexadecimal digits. UART0 has a 512-byte interrupt-driven RX buffer and
blocking reads, so a pasted maximum-length command is not limited to its FIFO.

```text
set wifi "example-ssid" "example password"
set mqtt broker.example.org 8883 example-user "example password" tls
set node bench
show
show token
reboot
```

`set mqtt` defaults to TLS when its last argument is omitted; `plain` explicitly
selects unencrypted MQTT (typically port 1883). TLS verifies certificate chains
and hostnames using the ESP-IDF certificate bundle, **not certificate validity
dates** (`CONFIG_MBEDTLS_HAVE_TIME_DATE` is unset). There is no SNTP client or
NTP availability dependency. A broker needs a certificate trusted by the bundle.
Client identity is `aether-<node>` with a 15-second keepalive, clean session and
retained offline LWT. The outbox is bounded to 16 KiB; it drops newest on full,
counts failed enqueues/expiry in `outbox_dropped`, and emits an accumulated
ordered overrun error after enqueue recovers. Failed subscriptions reconnect.

Credentials live only in NVS namespace `secrets`; SDK Wi-Fi credential persistence
is disabled. No secret contents are logged, published or shown by `show` (SSID,
broker host/user and both passwords are redacted). Console writes take effect
after `reboot`; node and GPIO changes over MQTT also require restart. Without
credentials the console remains available and a provisioning hint repeats every
30 seconds. Invalid stored cfg/secrets fall back to defaults with scrubbed error
codes; NVS full/new-version initialization erases and retries once. Wi-Fi setup
or connect errors leave the console usable. Remaining startup errors retain a
console-only `show` / `factory-reset` / `reboot` loop.
`factory-reset` erases all default NVS (cfg, node, secrets, boot), then reboots;
MQTT `reset` restores default CEC cfg but preserves node, secrets and boot.

Phase 3 publishes measured bus health, effective LA, retained configuration and
real CEC/heap/RSSI/link/outbox telemetry. Queue.max is capacity 16; depth excludes
the active host request. MQTT and HTTP TX are validated on the bus task and finish
as ordered asynchronous bus events; queue saturation is busy, invalid requests
are rejected. Config/state changes are handed off without blocking main.
`ping`, `state`, config, restart and reset work; MQTT `cmd/ota` is not supported.
`cfg.log` is persisted but inert. HPD remains null; `pa:"edid"` returns
`edid_unsupported` because the GPIO contract has no DDC pins. MQTT ingress remains
bounded to 32 messages / 2048 bytes; the bus task uses bounded command/event queues
and reports ring/event/outbox loss through ordered overrun errors.

## Crash diagnostics and host symbolisation

Firmware enables ESP-IDF 5.4.2's flash **ELF core dumps with CRC32** and a
dedicated 2048-byte panic stack. On a `panic`, `int_wdt` or `task_wdt` boot,
it validates the saved image, reads its summary and emits error-level
`last_crash <text>` into `/api/log`. The same text is retained as optional
`status.last_crash` (MQTT and `/api/status`) for that boot, then the flash dump
is erased. Format: decimal Xtensa EXCCAUSE, `pc=<8 hex>`, `task=<up to 16
printable non-whitespace ASCII bytes>`, `bt=<up to 8 comma-separated 8-hex PCs>`.
An abort may show an instruction exception; symbolise its PC/backtrace to
identify the IDF abort caller. The existing `last_panic` NVS hook still reports
Rust panics, but is not a CPU-exception or IDF-abort hook.

**One USB flash with the updated `firmware/partitions.csv` is required.**
The old table has only `storage` at 0x3E0000/128 KiB. This table splits it into
`coredump` at 0x3E0000/64 KiB and unused `storage` at 0x3F0000/64 KiB without
moving/resizing NVS, otadata, phy_init or either application slot. Use the USB
command above with the custom table and matching IDF bootloader; do not erase
the whole flash or lose provisioning on this routine update. Application OTA
does **not** rewrite the partition table at 0x8000: an OTA-only installation on
the old table logs `coredump_unavailable_usb_table_required` and cannot capture
these crashes. Once the table is installed, subsequent normal OTA updates can
capture/recover dumps. The recovering image must also include this code; an
older rollback image will not publish `last_crash`.

Independently of the dump, `firmware/src/bootstage.rs` keeps a boot-progress
record in RTC noinit memory (survives panic/watchdog resets, not power-on):
checkpoints `pre_main` (global constructor, before IDF secondary init), `main`, `console`, `store`, `net`, `mqtt`, `http`, `bus`, and
`loop` (re-marked every main-loop turn) with the uptime they were reached at.
After a `panic`, `int_wdt` or `task_wdt` boot the previous boot's last
checkpoint is logged as `last_stage <stage> t_ms=<n>` and retained as optional
`status.last_stage`; any other boot logs it as `prev_boot_stage`. This covers
crashes that happen before IDF's core dump writer is initialised, which leave
no dump at all (the writer then prints "flash config is corrupted"). An
`pre_main` after a crash reset means the crash happened in IDF startup before
`main`; the constructor re-claims the record, so it always describes the
immediately preceding boot.

There is deliberately no RTC-memory fallback based on an unsupported panic hook:
IDF 5.4.2's [port panic handler](https://github.com/espressif/esp-idf/blob/v5.4.2/components/esp_system/port/panic_handler.c#L263-L267)
calls the [system handler directly](https://github.com/espressif/esp-idf/blob/v5.4.2/components/esp_system/panic.c#L287-L419),
whose `panic_print_*` functions are internal, not registered application hooks.
[Shutdown handlers](https://github.com/espressif/esp-idf/blob/v5.4.2/components/esp_system/esp_system.c#L23-L64)
run from ordinary `esp_restart()`, not panic restart.
The supported handler [calls `esp_core_dump_write`](https://github.com/espressif/esp-idf/blob/v5.4.2/components/esp_system/panic.c#L409-L419);
IDF's [flash writer switches to its non-OS flash guard](https://github.com/espressif/esp-idf/blob/v5.4.2/components/espcoredump/src/core_dump_flash.c#L89-L127)
for the panic-time write rather than calling application/NVS code with caches
disabled. The [public API](https://github.com/espressif/esp-idf/blob/v5.4.2/components/espcoredump/include/esp_core_dump.h#L93-L172)
provides image integrity checking, summary retrieval and erasure.

Keep the **ELF for the image that crashed**, not just the currently running one
(especially after OTA rollback). For source lines and inlined Rust frames build
with `CARGO_PROFILE_RELEASE_DEBUG=true`; retain that ELF alongside its image.
From the repository root, substitute the `pc` and `bt` addresses from the report
(split commas into separate arguments):

```sh
. "$HOME/.espup-export.sh"
xtensa-esp32-elf-addr2line -pfiaC \
  -e firmware/target/xtensa-esp32-espidf/release/aether \
  0x40081234 0x400d5678
```

For an external `CARGO_TARGET_DIR`, point `-e` at that target directory's
`xtensa-esp32-espidf/release/aether` instead. These addresses are illustrative.
A saved dump larger than 64 KiB is rejected by IDF; corrupted task stacks,
double faults during panic handling, power loss and reset causes outside the
panic path can leave no valid summary. Full DRAM/heap capture is disabled.
Reading the summary is one-shot: export the log/status before another reset;
this is diagnostics, not a fix or proof of resolution of the OTA crash.

## HTTP API, token and push OTA

Port 80 is LAN-only plaintext HTTP: restrict access with the VLAN firewall.
Every route except `/` and `/healthz` requires `Authorization: Bearer <token>`.
The token is generated from 32 random bytes on first boot, with the bootloader
entropy source enabled before Wi-Fi/ADC startup, and stored in its own
`secrets/token` NVS key. Credential-load fallback never rewrites `secrets/json`.
**Only `show token` on the serial console prints it**; ordinary `show`, logs,
HTTP and MQTT never do. `set token <64 hex>` replaces only the token key immediately,
without a restart. Do not paste the token into shared transcripts or shell history.

Open `http://<bridge>/` for the embedded, dependency-free UI. Enter the token
once; it is retained in this browser's localStorage until “Forget token”.
The UI provides status, live SSE bus/OTA events, editable configuration, log tail,
and firmware upload. It uses WebCrypto SHA-256 when available and a local JS
SHA-256 implementation on LAN HTTP (where browsers disable WebCrypto).
SSE uses authenticated fetch, not EventSource/query-string credentials.
HTTP status snapshots refresh at most once per second. Restart/reset always reboot
within main's two-second shutdown deadline (or earlier on offline PUBACK), even
if an HTTP waiter times out; the handler's response grace is only a speed-up.

The following are **bench checks for the operator**, not hardware verification
performed by the implementation. Set `BASE` to the bench board URL and read
`TOKEN` privately from serial; all JSON bodies use the MQTT protocol types.
Run destructive restart/reset/upload examples last. Replace `bench` in reset
with the provisioned node name.

```sh
BASE=http://bridge.example
read -rs -p 'Serial API token: ' TOKEN; printf '\n'
curl -fsS "$BASE/"                                  # GET / (public)
curl -fsS "$BASE/healthz"                            # GET /healthz (public)
curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE/api/status"
curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE/api/cfg"
curl -fsS -X PUT -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-cfg","telemetry_s":60}' "$BASE/api/cfg"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-ping","echo":"bench"}' "$BASE/api/cmd/ping"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-state","power":"standby"}' "$BASE/api/cmd/state"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-tx","to":8,"data":"8F"}' "$BASE/api/tx"  # 202; asynchronous bus outcome
curl -N -H "Authorization: Bearer $TOKEN" "$BASE/api/events"  # separate terminal
curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE/api/log"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-restart"}' "$BASE/api/cmd/restart"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-reset","confirm":"bench"}' "$BASE/api/cmd/reset"
```

### Phase-3 listener-first bench gate

Do not run two TV bridges on the same CEC bus. The operator first applies
`{"id":"listen","la":0,"replies":[],"ack":[]}` before OTA; these persisted settings
survive the image change. Fixed LA0 does not allocation-poll, and the empty ACK set
disables even the core's implicit own-address ACK entry. No unsolicited line
drive is permitted in this state; explicit host TX still works.

These are operator checks, **not executed hardware results**:

```sh
# Supply broker TLS/auth privately using your normal mosquitto client config.
mosquitto_sub -v -t 'aether/bench/status' -t 'aether/bench/bus' \
  -t 'aether/bench/telemetry'
curl -fsS -X PUT -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"listen","la":0,"replies":[],"ack":[],"telemetry_s":10}' "$BASE/api/cfg"
curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE/api/status"
curl -N -H "Authorization: Bearer $TOKEN" "$BASE/api/events"
# Only after the passive capture/timing gate, enable follower/local responses:
curl -fsS -X PUT -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"enable","ack":[0],"replies":["83","8C","8F","9F","46","91"]}' "$BASE/api/cfg"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"power-on","power":"on"}' "$BASE/api/cmd/state"
mosquitto_pub -t 'aether/bench/tx' -m '{"id":"denon-pwr","to":5,"data":"8F"}'
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"http-pwr","to":5,"data":"8F"}' "$BASE/api/tx"
# End the exercise by restoring persisted passive listener mode:
curl -fsS -X PUT -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"listen-again","la":0,"ack":[],"replies":[]}' "$BASE/api/cfg"
curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE/api/cfg"
```

Listener capture should show receiver broadcasts such as `5→15 84200005`
and Apple TV `4→0 8F` (possibly NACK-truncated while ACK is disabled), with
monotonic sequence and start timestamps and no local TX. Status becomes `bus:ok`
after a valid frame or one second of high idle; boot low with no edges for
over 100 ms is `no_pullup`, later sustained low/recovery is `stuck_low`/`ok`.
After enable, `4→0 8F` must be ACKed and answered with local `0→4 9000`;
the Denon request must yield `0→5 8F` echo, `tx` with the matching id and
`status:ack`, and receiver `5→0 90xx` (receiver response order may precede
or follow the TX result, but the send's RX echo must precede its TX result).
No reply may be generated for `85`, `82`, broadcasts or LA15 initiators.
After the final return-to-listener request, confirm effective cfg has fixed LA0,
`ack:[]` and `replies:[]`; let accepted host sends drain and stop submitting TX.
Observe addressed traffic with no local TX and no increase in
`telemetry.rx.acked_by_us`. Confirm on the scope that the bridge never pulls the
line low (including after reboot); valid incoming frames alone do not prove passivity.

With a logic analyzer, measure actual ACK-fall-to-assert below 200 us and ACK
hold near 1500 us, RMT data/start widths, seamless block chaining, per-block
first-fall origin acquisition within 300 us, and sample jitter within the core's
200-us allowance under Wi-Fi/SSE/HTTP/OTA load. On arbitration loss/collision,
verify GPIO matrix detachment releases in under 1 us and the detached RMT tail
never reaches the wire or replays during a retry. Check busy-bus start rejection.
Also check bus/console stack headroom, open-drain high=released electrically,
maximum frame lengths, NACK retry/poll behavior, arbitration recovery, config
barriers and automatic LA allocation separately. OTA must wait for TX/follower
ACK idle before reboot. Software build/tests alone do not establish any of these.


Create an **app-only** image (no bootloader/table/offset padding) with:

```sh
espflash save-image --chip esp32 \
  firmware/target/xtensa-esp32-espidf/release/aether /tmp/aether-app.bin
SHA=$(sha256sum /tmp/aether-app.bin); SHA=${SHA%% *}
curl -fsS -X POST -T /tmp/aether-app.bin \
  -H "Authorization: Bearer $TOKEN" -H "X-Sha256: $SHA" "$BASE/api/ota"
```

The upload waits while writing 4-KiB chunks and checking SHA-256, ESP-IDF image
validity and project name `aether`; it returns `202 {"id":"o-N"}` then reboots
500 ms later. Never upload a merged full-flash image. Each slot is 1,966,080 bytes.
Watch MQTT `aether/<node>/ota` or SSE for receiving/verifying/rebooting followed
by confirmed or rolled_back; reconnect SSE after reboot.
For forced rollback, the lead must block **only the bench board's** broker access
before uploading, restore it after the 60-second rollback, and observe the old
image's `rolled_back` event. This requires a previously confirmed HTTP-capable
image in the old slot; the earlier phase-2 image cannot report the new event.

Error probes: missing/wrong bearer → 401 `unauthorized`; an authenticated
unknown path → 404 `not_found`; malformed/missing OTA headers → 400 `malformed`;
wrong digest → 400 `sha256_mismatch`; wrong project → 400 `project_mismatch`;
oversize image → 413 `too_large`; a running image still pending verification or
another accepted update awaiting reboot → 409 `busy` without allocating an OTA id.
The single httpd task streams uploads synchronously: a second upload waits in the
accept backlog while one is streaming, as can `/healthz` and other HTTP requests.
Check config validation failures leave the old cfg intact and reset preserves node,
token and Wi-Fi/MQTT credentials. Up to four sockets are allowed, including SSE;
each SSE client has a 16-event queue and is disconnected on overflow.
Logs retain the newest 64 scrubbed info/warn/error application records regardless
of `cfg.log`; MQTT `log` publication remains deferred.

Host logic is covered by root `cargo test --all-features`, clippy and fmt; this
does not exercise NVS, UART, Wi-Fi, TLS, MQTT delivery/LWT, or reboot on hardware.

License: MIT.
