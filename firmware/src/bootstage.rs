//! Boot-progress record in RTC noinit memory.
//!
//! RTC slow memory survives panic and watchdog resets (not power-on or
//! brownout), so a crash that happens before ESP-IDF's core dump writer is
//! initialised, or whose dump is lost, still reports the last stage the previous
//! boot reached and the uptime when it got there. The main loop re-marks `Loop`
//! every turn, so for a late crash `t_ms` is the uptime of the last loop turn.
//!
//! The record is captured and re-claimed from a global constructor, which IDF
//! runs on core 0 before its SECONDARY init list (`startup.c`
//! `do_global_ctors` → `do_secondary_init`) and before `main`. A boot that dies
//! during IDF startup therefore still overwrites the previous boot's record, so
//! a valid record always describes the immediately preceding boot.

use core::sync::atomic::{AtomicU32, Ordering};

/// Startup checkpoints in execution order.
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum Stage {
    /// Global constructor ran; IDF secondary init and `main` still ahead.
    PreMain = 1,
    /// `main` entered; nothing but the logger exists.
    Main = 2,
    /// Serial console worker running.
    Console = 3,
    /// NVS opened, configuration and credentials loaded.
    Store = 4,
    /// Wi-Fi driver started.
    Net = 5,
    /// MQTT client constructed.
    Mqtt = 6,
    /// HTTP server listening.
    Http = 7,
    /// CEC bus task and edge ISR armed.
    Bus = 8,
    /// Main loop turning.
    Loop = 9,
}

fn stage_name(id: u32) -> &'static str {
    match id {
        1 => "pre_main",
        2 => "main",
        3 => "console",
        4 => "store",
        5 => "net",
        6 => "mqtt",
        7 => "http",
        8 => "bus",
        9 => "loop",
        _ => "unknown",
    }
}

#[repr(C)]
struct Record {
    magic: AtomicU32,
    stage: AtomicU32,
    t_ms: AtomicU32,
    check: AtomicU32,
}

impl Record {
    const fn zero() -> Self {
        Self {
            magic: AtomicU32::new(0),
            stage: AtomicU32::new(0),
            t_ms: AtomicU32::new(0),
            check: AtomicU32::new(0),
        }
    }
}

const MAGIC: u32 = 0x4145_5442; // "AETB"

/// NOLOAD in the ESP-IDF linker script: the initialiser never reaches flash or
/// RAM, which is the point. Validity comes from `magic` and `check` only.
#[unsafe(link_section = ".rtc_noinit")]
static RECORD: Record = Record::zero();

/// The previous boot's record, copied out of RTC memory by the constructor.
static CAPTURED: Record = Record::zero();

/// IDF collects `.ctors` into `__init_array_start..__init_array_end` and calls
/// them from `do_global_ctors` on core 0; `#[used]` keeps the pointer alive.
#[unsafe(link_section = ".ctors")]
#[used]
static CLAIM: unsafe extern "C" fn() = claim;

/// Runs before IDF secondary init: no logger, no `esp_timer` yet.
unsafe extern "C" fn claim() {
    for (from, to) in [
        (&RECORD.magic, &CAPTURED.magic),
        (&RECORD.stage, &CAPTURED.stage),
        (&RECORD.t_ms, &CAPTURED.t_ms),
        (&RECORD.check, &CAPTURED.check),
    ] {
        to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
    }
    write(Stage::PreMain as u32, 0);
}

/// Forced first thing in `main`: formats the captured record and keeps it only
/// after a crash reset.
static PREVIOUS: std::sync::LazyLock<Option<aether_proto::LastStage>> =
    std::sync::LazyLock::new(take_previous);

fn checksum(stage: u32, t_ms: u32) -> u32 {
    MAGIC ^ stage.rotate_left(8) ^ t_ms.rotate_left(16)
}

fn read_captured() -> Option<aether_proto::LastStage> {
    let magic = CAPTURED.magic.load(Ordering::Relaxed);
    let stage = CAPTURED.stage.load(Ordering::Relaxed);
    let t_ms = CAPTURED.t_ms.load(Ordering::Relaxed);
    let check = CAPTURED.check.load(Ordering::Relaxed);
    if magic != MAGIC || check != checksum(stage, t_ms) {
        return None;
    }
    let mut text = aether_proto::LastStage::new();
    // "pre_main t_ms=4294967295" is 24 bytes; the bound is 32.
    core::fmt::write(&mut text, format_args!("{} t_ms={t_ms}", stage_name(stage))).ok()?;
    Some(text)
}

fn take_previous() -> Option<aether_proto::LastStage> {
    let previous = read_captured();
    mark(Stage::Main);
    let crashed = matches!(
        crate::ffi::reset_reason(),
        aether_proto::ResetReason::Panic
            | aether_proto::ResetReason::IntWdt
            | aether_proto::ResetReason::TaskWdt
    );
    match &previous {
        Some(text) if crashed => log::error!("last_stage {text}"),
        // Evidence that the record survives resets; absent after power-on.
        Some(text) => log::info!("prev_boot_stage {text}"),
        None => {}
    }
    // A record after a clean restart or power-on says nothing about a crash.
    previous.filter(|_| crashed)
}

/// Call once, first thing in `main` after the logger exists.
pub fn begin() {
    std::sync::LazyLock::force(&PREVIOUS);
}

/// The previous boot's last checkpoint, present only after a panic or watchdog
/// reset. Stable for the whole boot.
pub fn last_stage() -> Option<&'static aether_proto::LastStage> {
    PREVIOUS.as_ref()
}

/// Record that this boot reached `stage` now.
pub fn mark(stage: Stage) {
    write(stage as u32, (crate::ffi::now_us() / 1000) as u32);
}

fn write(stage: u32, t_ms: u32) {
    // Invalidate first so a reset between the stores never yields a stale
    // stage with a fresh checksum.
    RECORD.magic.store(0, Ordering::Relaxed);
    RECORD.stage.store(stage, Ordering::Relaxed);
    RECORD.t_ms.store(t_ms, Ordering::Relaxed);
    RECORD.check.store(checksum(stage, t_ms), Ordering::Relaxed);
    RECORD.magic.store(MAGIC, Ordering::Release);
}
