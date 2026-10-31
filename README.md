# aether-cec

Rust firmware for an ESP32 HDMI-CEC bridge ("aether") that exposes the CEC bus
over a native MQTT protocol, plus the host-testable crates it is built from.

Status: pre-alpha. The host-tested CEC core, phase-2 ESP32 firmware executor
and phase-2b HTTP API/web UI/push OTA are implemented against
[`docs/protocol.md`](docs/protocol.md). The CEC GPIO/RMT driver is not implemented;
HTTP/OTA hardware operation still requires bench verification.

| Crate | Purpose |
|---|---|
| `crates/cec-core` | `no_std` CEC codec, transmit state machine, bridge policy |
| `crates/aether-proto` | serde types for the MQTT protocol, shared with hosts |
| `crates/aether-logic` | host-tested firmware config, console, commands and JSON |
| `firmware/` | esp-idf application for the ESP32 (phase 2) |

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
otadata at 0xF000, phy_init at 0x11000 and unused future `storage` NVS.
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

Phase 2 publishes online status with `la:null`, `bus:null` (unmeasured),
`hpd:null`, retained configuration, and real heap/RSSI/link/outbox telemetry.
Deferred bus counters stay zero; queue.max is the capacity, 16.
Valid `tx` requests receive an ordered `rejected/no_la` event, never a fabricated send.
`ping`, `state`, config, restart and reset work; MQTT `cmd/ota` is not supported.
`cfg.log` is persisted but inert: no MQTT `log` topic is produced yet.
`pa:"edid"` returns `edid_unsupported` because the GPIO contract has no DDC pins.
No GPIO is driven in this phase, including configured HPD. Ingress is bounded
to 32 events and 2048-byte MQTT payloads; overflow is reported as a bus overrun,
and oversized command/tx payloads receive a malformed rejection.

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
  -d '{"id":"h-tx","to":8,"data":"8F"}' "$BASE/api/tx"  # 202; phase 2 no_la on bus
curl -N -H "Authorization: Bearer $TOKEN" "$BASE/api/events"  # separate terminal
curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE/api/log"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-restart"}' "$BASE/api/cmd/restart"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-reset","confirm":"bench"}' "$BASE/api/cmd/reset"
```

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
