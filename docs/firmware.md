# Firmware: build, flash, provision, diagnose

Operator reference for the `firmware/` package. The wire contract is
[`protocol.md`](protocol.md); the HTTP side is in [`http-api.md`](http-api.md).

## Build and first flash

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
Before flashing, run `scripts/package-firmware.sh <dir>` from the repository
root. It runs the release build itself and takes the ELF and the `esp-idf-sys`
output directory from Cargo's build messages, so the sdkconfig and bootloader
it uses always belong to that build even when older `esp-idf-sys-*` directories
are left in `target/`. It fails unless every line of `sdkconfig.defaults` (main
task stack, 4 MB flash, custom partition table, bootloader rollback, core dump,
...) is present in the generated sdkconfig and the application fits an OTA
slot, then writes `aether-ota.bin` (for `POST /api/ota`), `aether-factory.bin`
(bootloader, partition table and application merged, for offset 0x0) and
`aether.elf`. CI runs the same script on every change.

### Released images

A signed `vX.Y.Z` tag makes CI build the firmware with `status.fw` set to
`X.Y.Z` (any other build reports `0.0.0+<commit>`) and attach
`aether-vX.Y.Z-ota.bin`, `aether-vX.Y.Z-factory.bin`, `aether-vX.Y.Z.elf` and
`SHA256SUMS` to the GitHub release, with a build provenance attestation
(`gh attestation verify <file> -R yasen-pavlov/aether-cec`). Push the OTA image
as described in [`http-api.md`](http-api.md#push-ota); first-time USB flashing
of a board that ran other firmware is `espflash erase-flash` followed by
`espflash write-bin 0x0 aether-vX.Y.Z-factory.bin`. The merged factory image
has not been flashed on the bench yet; the build-from-source commands above are
the verified USB path. Keep the `.elf` of every image you flash: crash reports
are symbolised against it.

## Partition table and boot confirmation

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

## Serial console and provisioning

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

## MQTT session and TLS

`set mqtt` defaults to TLS when its last argument is omitted; `plain` explicitly
selects unencrypted MQTT (typically port 1883). TLS verifies certificate chains
and hostnames using the ESP-IDF certificate bundle, **not certificate validity
dates** (`CONFIG_MBEDTLS_HAVE_TIME_DATE` is unset). There is no SNTP client or
NTP availability dependency. A broker needs a certificate trusted by the bundle.
Client identity is `aether-<node>` with a 15-second keepalive, clean session and
retained offline LWT. The outbox is bounded to 16 KiB; it drops newest on full,
counts failed enqueues/expiry in `outbox_dropped`, and emits an accumulated
ordered overrun error after enqueue recovers. Failed subscriptions reconnect.

## Credentials, defaults and resets

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

## Runtime limits

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
