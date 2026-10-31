# Working on aether-cec

Read `docs/protocol.md` first; it is the contract between the firmware and hosts
(Choregos). Record any deviation agreed in review in `docs/decisions.md` (date,
what, why).

## Layout
- `crates/cec-core` (`aether-cec-core`, lib `cec_core`): pure `no_std`, no alloc, no
  `unsafe`. Edge-level codec, block-level transmit state machine, bridge policy
  (ack set, local replies, logical-address allocation). Time is an injected `u64`
  of microseconds. Everything here is host-testable.
- `crates/aether-proto` (`aether-cec-proto`, lib `aether_proto`): serde types for
  every payload in `docs/protocol.md`. `no_std` by default, `std` feature for hosts.
  Depends on `cec-core` for `Frame`/`TxStatus`; never the other way round.
- `firmware/`: esp-idf `std` binary for the ESP32 (phase 2+). Thin executor of
  `cec-core` actions on RMT + a one-shot timer; Wi-Fi, esp-mqtt, NVS, serial CLI,
  OTA. Not a workspace member until the Xtensa toolchain is a CI prerequisite.
- `tests/fixtures/`: frames distilled from real bus traces (`from`,`to`,`data`,`ack`
  only). No timestamps, IPs, or host state. Raw traces stay in gitignored `local/`.

## Invariants
- Rust 2024 edition, stable toolchain, `cargo fmt`, `cargo clippy --all-targets
  --all-features -- -D warnings` clean, `unsafe_code = "forbid"` in the core crates.
- CEC timing constants live in one module of `cec-core` with the spec nominal and
  the accepted window side by side; nothing else hard-codes microseconds.
- The bridge never answers `<Request Active Source>` (85) or `<Report Physical
  Address>` (82), and never replies to an initiator at logical address 15.
- Polls (header-only frames) are never retransmitted by the firmware; NACK on a
  poll is evidence the host relies on.
- Secrets (Wi-Fi, MQTT) come from NVS via the serial console or build-time env;
  never from MQTT, logs, or the repo.
- Version in `Cargo.toml` stays `0.0.0`; releases are tags.
- Tests: behaviour, boundaries, transitions, error paths. No tests of wiring,
  copies, or defaults.
- Public repo from day one: lab-specific values (MACs, IPs, topics) belong in
  examples, not in code.

## Commands
```
cargo build
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo build -p aether-cec-core -p aether-cec-proto --target thumbv7em-none-eabi   # no_std check
```
Use `CARGO_TARGET_DIR=~/.cache/aether-cec/target-<yourname>` so parallel workers do
not fight over the lock.

## Workers
- Work only in the files your task names. Never commit, push, or rewrite git
  history; the lead commits (GPG-signed).
- Never flash, contact the real MQTT broker, or touch the living-room bridge; the
  lead runs bench hardware.
- Finish with: files changed, decisions + why, tests added and their results, open
  risks, anything not verified.
