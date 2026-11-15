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
- `firmware/`: esp-idf `std` binary for the ESP32. Thin executor of `cec-core`
  actions on RMT + a one-shot timer; Wi-Fi, esp-mqtt, NVS, serial CLI, OTA.
  Standalone package, not a workspace member: it needs the Xtensa toolchain and
  must not break host-side `cargo test` at the root. CI builds it in its own job.
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
- Version in `Cargo.toml` stays `0.0.0`; releases are tags. A release build gets
  its `status.fw` from `AETHER_VERSION` (set by the release workflow from the tag).
- `rust-version` is the checked minimum for the host crates (CI job `MSRV`); raise
  it in the same PR as the dependency that needs it.
- Tests: behaviour, boundaries, transitions, error paths. No tests of wiring,
  copies, or defaults.
- Public repo from day one: lab-specific values (MACs, IPs, topics) belong in
  examples, not in code.
- Xtensa esp Rust 1.97/esp-clang 21 has a bench-observed stack-argument ABI bug.
  Firmware-owned functions/methods must have at most four parameters (including
  `self`); bundle lists of five or more into one struct argument. Audit by-value
  aggregates (`Status`, `Config`, `Arc`, `String`, tuples, etc.) as well as scalar
  arguments when adding calls. Keep these workarounds in firmware, not the
  host-tested logic/protocol/core crates; ESP-IDF C ABI signatures are unchanged.

## Commands
```
cargo build
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo build -p aether-cec-core -p aether-cec-proto --target thumbv7em-none-eabi   # no_std check
cargo llvm-cov --all-features --lcov --output-path lcov.info                      # what CI runs for tests
scripts/package-firmware.sh /tmp/aether   # firmware release build + checks + images; needs the esp toolchain
```
Use `CARGO_TARGET_DIR=~/.cache/aether-cec/target-<yourname>` so parallel workers do
not fight over the lock. Never set it for `firmware/`: embuild derives paths from
the in-tree target directory and can silently skip `sdkconfig.defaults` otherwise.

## Workflow
- `main` is protected: signed commits, linear history, squash-merged pull
  requests only, required checks `rust`, `MSRV`, `firmware / build`, `docs`,
  `Sonar` and `SonarCloud Code Analysis`. No direct pushes, admins included.
- Branch, open a PR, get CI green, squash-merge. The lead signs commits.
- CI (`.github/workflows/ci.yml`): fmt, clippy, tests with coverage, `no_std`
  build; MSRV check; full firmware build plus `scripts/package-firmware.sh`
  (sdkconfig defaults applied, application fits an OTA slot, images as an
  artifact); OpenAPI lint and offline Markdown link check; dependency review on
  PRs; SonarQube Cloud with the coverage report.
- Release: `git tag -s vX.Y.Z` on `main` and push the tag. `release.yml` builds
  the firmware with that version and publishes OTA and factory images, the ELF,
  checksums and a provenance attestation. A CI-built image is still bench-tested
  before it replaces the bridge in the room.
- SonarQube Cloud analyses the whole repository (workflows and scripts
  included); `sonar-project.properties` only excludes the embedded console page
  and the design canvases. Third-party actions are pinned to commit SHAs with
  the version in a comment; Dependabot keeps them current.
- When a firmware function is split, check stack frames, not only behaviour:
  compare the `entry` frame of the task's loop function plus its largest new
  helper against the previous image (decisions.md, 2026-11-15).

## Workers
- Work only in the files your task names. Never commit, push, or rewrite git
  history; the lead commits (GPG-signed).
- Never flash, contact the real MQTT broker, or touch the living-room bridge; the
  lead runs bench hardware.
- Finish with: files changed, decisions + why, tests added and their results, open
  risks, anything not verified.
