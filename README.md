# aether-cec

Rust firmware for an ESP32 HDMI-CEC bridge ("aether") that exposes the CEC bus
over a native MQTT protocol, plus the host-testable crates it is built from.

Status: pre-alpha. The protocol is defined in [`docs/protocol.md`](docs/protocol.md);
the core crates are being built against it. Nothing here runs on hardware yet.

| Crate | Purpose |
|---|---|
| `crates/cec-core` | `no_std` CEC codec, transmit state machine, bridge policy |
| `crates/aether-proto` | serde types for the MQTT protocol, shared with hosts |
| `firmware/` | esp-idf application for the ESP32 (phase 2) |

Hardware target: ESP32-WROOM-32 DevKit, CEC on one open-drain GPIO with pull-up,
HDMI pin 13 → GPIO, pin 17 → GND. Nothing else is connected.

The first host is [Choregos](https://github.com/yasen-pavlov/choregos)
(`bridge.kind = "aether"`).

License: MIT.
