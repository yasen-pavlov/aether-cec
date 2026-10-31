# aether-cec

Rust firmware for an ESP32 HDMI-CEC bridge ("aether") that exposes the CEC bus
over a native MQTT protocol, plus the host-testable crates it is built from.

Status: pre-alpha. The host-tested CEC core and phase-2 ESP32 firmware executor
are implemented against [`docs/protocol.md`](docs/protocol.md). The CEC GPIO/RMT
driver and OTA are not implemented yet; hardware operation is not verified.

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
main stack must be 8000, flash 4 MB, custom partition table enabled and
bootloader rollback enabled.

`partitions.csv` reserves two 0x1E0000-byte OTA slots, NVS at 0x9000,
otadata at 0xF000, phy_init at 0x11000 and unused future `storage` NVS.
Pass the custom table and the **IDF-built rollback bootloader on every USB flash**;
espflash's defaults do not provide this OTA layout. Erasing all flash is necessary
once when replacing Tasmota because its NVS/otadata overlap these offsets and
otherwise leave foreign settings and secrets behind. Do not erase on routine updates.
The main task marks this phase-2 image valid on MQTT connect or after 60 s
without a broker, including console-only recovery. OTA downloading is still absent;
pending-OTA-aware status confirmation/rollback remains phase 5.

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
`ping`, `state`, config, restart and reset work; OTA returns `not_implemented`.
`cfg.log` is persisted but inert: no MQTT `log` topic is produced yet.
`pa:"edid"` returns `edid_unsupported` because the GPIO contract has no DDC pins.
No GPIO is driven in this phase, including configured HPD. Ingress is bounded
to 32 events and 2048-byte MQTT payloads; overflow is reported as a bus overrun,
and oversized command/tx payloads receive a malformed rejection.

Host logic is covered by root `cargo test --all-features`, clippy and fmt; this
does not exercise NVS, UART, Wi-Fi, TLS, MQTT delivery/LWT, or reboot on hardware.

License: MIT.
