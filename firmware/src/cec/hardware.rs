//! Unsafe ESP-IDF boundary, imported as ffi::cec. All contexts are retained
//! for the lifetime of the bus task; callbacks never borrow movable task data.
use aether_logic::cec::{
    AckCapture, AckEvidence, Block, EdgeRing, ack_evidence, ack_in_window, ack_late,
};
use cec_core::{
    rx::{Edge, Level},
    timing,
};
use esp_idf_svc::sys;
use std::{
    mem::ManuallyDrop,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

// IDF v5.4.2: components/esp_timer/include/esp_private/esp_timer_private.h:50,55;
// src/esp_timer_impl_common.c:14,20,22-33 (relative to components/esp_timer).
// These private symbols are not exposed by esp-idf-sys's generated bindings.
unsafe extern "C" {
    fn esp_timer_private_lock();
    fn esp_timer_private_unlock();
    static mut timestamp_id: [u64; 2];
}

// ESP32-only LACT module 0, 2 ticks/us:
// components/esp_hw_support/include/esp_private/systimer.h:14,26.
// Base: components/soc/esp32/register/soc/reg_base.h:43;
// offsets/bits: components/soc/esp32/register/soc/timer_group_reg.h:
// CONFIG:396, ALARM_EN:435, ALARMLO:490, ALARMHI:498, INT_ST:582, LACT_INT_ST:585.
const LACT_CONFIG: *const u32 = 0x3ff5_f070 as *const u32;
const LACT_ALARM_LO: *const u32 = 0x3ff5_f084 as *const u32;
const LACT_ALARM_HI: *const u32 = 0x3ff5_f088 as *const u32;
const LACT_INT_ST: *const u32 = 0x3ff5_f0a0 as *const u32;
const LACT_ALARM_EN: u32 = 1 << 10;
const LACT_INT: u32 = 1 << 3;

#[cfg(not(all(
    esp_idf_gpio_ctrl_func_in_iram,
    esp_idf_rmt_isr_iram_safe,
    esp_idf_esp_timer_supports_isr_dispatch_method,
    esp_idf_esp_timer_isr_affinity_cpu1
)))]
compile_error!("CEC interrupts require GPIO/RMT IRAM safety and esp_timer ISR dispatch on core 1");

pub struct Interrupts {
    // ESP32-WROOM has no PSRAM: this boxed context and both rings are internal
    // DRAM. ISR helpers inline; timing constants are immediates, not flash data.
    pub edges: EdgeRing<256>,
    pub samples: EdgeRing<8>,
    pin: i32,
    task: core::sync::atomic::AtomicUsize,
    ack_at_low: AtomicU32,
    ack_at_high: AtomicU32,
    in_flight: AtomicU32,
    ota_busy: Arc<AtomicBool>,
    ack_armed: AtomicBool,
    ack_active: AtomicBool,
    ack_count: AtomicU32,
    // Diagnostic words: 0 unavailable, 1 held, 2 released. The task captures
    // them under the timer hardware lock, masking their same-core ISR writers.
    ack_evidence_state: AtomicU32,
    ack_assert_low: AtomicU32,
    ack_assert_high: AtomicU32,
    ack_held_us: AtomicU32,
    ack_held_max_us: AtomicU32,
    ack_late: AtomicU32,
    ack_release: sys::esp_timer_handle_t,
    sample_timer: sys::esp_timer_handle_t,
    wake_timer: sys::esp_timer_handle_t,
}
// Handles belong to ESP-IDF and remain valid for the firmware lifetime.
unsafe impl Sync for Interrupts {}

#[inline(always)]
unsafe fn notify(context: &Interrupts, timer: bool) {
    let mut wake = 0;
    if context.task.load(Ordering::Relaxed) == 0 {
        return;
    }
    unsafe {
        sys::xTaskGenericNotifyFromISR(
            context.task.load(Ordering::Relaxed) as sys::TaskHandle_t,
            0,
            0,
            sys::eNotifyAction_eIncrement,
            core::ptr::null_mut(),
            &mut wake,
        );
        if wake != 0 {
            if timer {
                sys::esp_timer_isr_dispatch_need_yield();
            } else {
                sys::_frxt_setup_switch();
            }
        }
    }
}
#[unsafe(link_section = ".iram1.text")]
unsafe extern "C" fn edge_isr(arg: *mut core::ffi::c_void) {
    let context = unsafe { &*arg.cast::<Interrupts>() };
    let now = unsafe { sys::esp_timer_get_time() as u64 };
    let level = if unsafe { sys::gpio_get_level(context.pin) } == 0 {
        Level::Low
    } else {
        Level::High
    };
    // A prediction is only a window guard. The actual falling edge triggers ACK.
    // Consume even a stale arm on the first fall. Publish/read both timestamp
    // words through ack_armed; never alias a window after the low-word wrap.
    if level == Level::Low && context.ack_armed.swap(false, Ordering::AcqRel) {
        let at = u64::from(context.ack_at_low.load(Ordering::Relaxed))
            | (u64::from(context.ack_at_high.load(Ordering::Relaxed)) << 32);
        if !context.ota_busy.load(Ordering::Acquire) && ack_in_window(now, at) {
            unsafe {
                sys::gpio_set_level(context.pin, 0);
            }
            context.ack_active.store(true, Ordering::Release);
            // IDF 5.4.2 esp_timer.c:180/294/376 places start_once, insertion
            // and locking in IRAM unconditionally; no ESP_TIMER_IN_IRAM exists.
            if unsafe { sys::esp_timer_start_once(context.ack_release, u64::from(timing::ACK_LOW)) }
                != sys::ESP_OK
            {
                unsafe {
                    sys::gpio_set_level(context.pin, 1);
                }
                context.ack_active.store(false, Ordering::Release);
            } else {
                context.ack_count.fetch_add(1, Ordering::Relaxed);
                // Only observable by a reader on another core or above level 3;
                // same-core task/timer readers cannot interrupt this publication.
                context.ack_evidence_state.store(0, Ordering::Release);
                context.ack_assert_low.store(now as u32, Ordering::Relaxed);
                context
                    .ack_assert_high
                    .store((now >> 32) as u32, Ordering::Relaxed);
                context.ack_evidence_state.store(1, Ordering::Release);
            }
        }
    }
    context.edges.push(Edge { level, t_us: now });
    unsafe {
        notify(context, false);
    }
}
#[unsafe(link_section = ".iram1.text")]
unsafe extern "C" fn release_isr(arg: *mut core::ffi::c_void) {
    let context = unsafe { &*arg.cast::<Interrupts>() };
    unsafe {
        sys::gpio_set_level(context.pin, 1);
    }
    context.ack_active.store(false, Ordering::Release);
    // Release the pad before any diagnostic work, even if the timer ran late.
    let now = unsafe { sys::esp_timer_get_time() as u64 };
    if context.ack_evidence_state.load(Ordering::Acquire) == 1 {
        let assertion = u64::from(context.ack_assert_low.load(Ordering::Relaxed))
            | (u64::from(context.ack_assert_high.load(Ordering::Relaxed)) << 32);
        let held = now.saturating_sub(assertion).min(u64::from(u32::MAX)) as u32;
        context.ack_held_us.store(held, Ordering::Relaxed);
        context.ack_evidence_state.store(2, Ordering::Release);
        // Only this ISR writes the maximum; avoid a compare/exchange loop.
        if held > context.ack_held_max_us.load(Ordering::Relaxed) {
            context.ack_held_max_us.store(held, Ordering::Relaxed);
        }
        if ack_late(held) {
            context.ack_late.fetch_add(1, Ordering::Relaxed);
        }
    }
}
#[unsafe(link_section = ".iram1.text")]
unsafe extern "C" fn sample_isr(arg: *mut core::ffi::c_void) {
    let context = unsafe { &*arg.cast::<Interrupts>() };
    context.samples.push(Edge {
        level: if unsafe { sys::gpio_get_level(context.pin) } == 0 {
            Level::Low
        } else {
            Level::High
        },
        t_us: unsafe { sys::esp_timer_get_time() as u64 },
    });
    unsafe {
        notify(context, true);
    }
}
#[unsafe(link_section = ".iram1.text")]
unsafe extern "C" fn wake_isr(arg: *mut core::ffi::c_void) {
    unsafe {
        notify(&*arg.cast::<Interrupts>(), true);
    }
}

#[unsafe(link_section = ".iram1.text")]
unsafe extern "C" fn tx_done_isr(
    _channel: sys::rmt_channel_handle_t,
    _event: *const sys::rmt_tx_done_event_data_t,
    arg: *mut core::ffi::c_void,
) -> bool {
    let context = unsafe { &*arg.cast::<Interrupts>() };
    context.in_flight.fetch_sub(1, Ordering::Release);
    unsafe {
        notify(context, false);
    }
    false
}

/// Configure only the next pthread, restoring the caller's configuration after
/// spawn. Large SDK config aggregates are passed by reference across the ABI.
pub fn spawn_bus(run: impl FnOnce() + Send + 'static) -> crate::config::Result<()> {
    unsafe {
        // Bench (o-12): priority 22 (= esp_timer task) never got scheduled on core 1,
        // and restoring a cfg read with esp_pthread_get_cfg on a thread without one
        // returned ESP_ERR_INVALID_ARG. Use 20 and reset to the SDK default after.
        let mut cfg = sys::esp_pthread_get_default_config();
        cfg.stack_size = 12288;
        cfg.prio = 20;
        cfg.pin_to_core = 1;
        cfg.inherit_cfg = false;
        cfg.thread_name = c"cec-bus".as_ptr();
        sys::esp!(sys::esp_pthread_set_cfg(&cfg))?;
        let spawned = std::thread::Builder::new()
            .name("cec-bus".into())
            .stack_size(12288)
            .spawn(run);
        let default = sys::esp_pthread_get_default_config();
        let restored = sys::esp_pthread_set_cfg(&default);
        spawned?;
        sys::esp!(restored)?;
    }
    Ok(())
}

/// Protect the bus before removing only core 1's idle watchdog requirement.
/// Reconfigure also removes IDF's idle hook; deleting IDLE1 alone leaves it
/// repeatedly trying to reset an unsubscribed task.
pub fn watch_bus() -> crate::config::Result<()> {
    unsafe {
        sys::esp!(sys::esp_task_wdt_add(core::ptr::null_mut()))?;
        let cfg = sys::esp_task_wdt_config_t {
            timeout_ms: 5000,
            idle_core_mask: 1,
            trigger_panic: true,
        };
        sys::esp!(sys::esp_task_wdt_reconfigure(&cfg))?;
    }
    Ok(())
}

pub fn feed_bus() {
    unsafe { sys::esp!(sys::esp_task_wdt_reset()) }.expect("cec_watchdog_reset_failed");
}

pub struct Hardware {
    pub interrupts: ManuallyDrop<Box<Interrupts>>,
    channel: sys::rmt_channel_handle_t,
    encoder: sys::rmt_encoder_handle_t,
    buffers: ManuallyDrop<Box<[[sys::rmt_symbol_word_t; 11]; 4]>>,
    slot: usize,
    matrix: u32,
    routed: bool,
    enabled: bool,
    service_owned: bool,
}
// SAFETY: the handles are owned exclusively by the bus task after construction
// on main; esp-idf driver handles are not core-affine.
unsafe impl Send for Hardware {}
impl Hardware {
    /// Construct on main/core 0; move the handles into the core-1 bus task.
    pub fn new(pin: u8, ota_busy: Arc<AtomicBool>) -> crate::config::Result<Self> {
        let pin = i32::from(pin);
        let mut hw = Self {
            interrupts: ManuallyDrop::new(Box::new(Interrupts {
                edges: EdgeRing::default(),
                samples: EdgeRing::default(),
                pin,
                task: core::sync::atomic::AtomicUsize::new(0),
                ack_at_low: AtomicU32::new(0),
                ack_at_high: AtomicU32::new(0),
                in_flight: AtomicU32::new(0),
                ota_busy,
                ack_armed: AtomicBool::new(false),
                ack_active: AtomicBool::new(false),
                ack_count: AtomicU32::new(0),
                ack_evidence_state: AtomicU32::new(0),
                ack_assert_low: AtomicU32::new(0),
                ack_assert_high: AtomicU32::new(0),
                ack_held_us: AtomicU32::new(0),
                ack_held_max_us: AtomicU32::new(0),
                ack_late: AtomicU32::new(0),
                ack_release: core::ptr::null_mut(),
                sample_timer: core::ptr::null_mut(),
                wake_timer: core::ptr::null_mut(),
            })),
            channel: core::ptr::null_mut(),
            encoder: core::ptr::null_mut(),
            buffers: ManuallyDrop::new(Box::new([[sys::rmt_symbol_word_t { val: 0 }; 11]; 4])),
            slot: 0,
            matrix: 0,
            routed: false,
            service_owned: false,
            enabled: false,
        };
        unsafe {
            sys::esp!(sys::gpio_set_level(pin, 1))?;
            sys::esp!(sys::gpio_set_direction(
                pin,
                sys::gpio_mode_t_GPIO_MODE_INPUT_OUTPUT_OD
            ))?;
            sys::esp!(sys::gpio_set_pull_mode(
                pin,
                sys::gpio_pull_mode_t_GPIO_FLOATING
            ))?;
            // IDF initializes RMT idle LOW. Hold the already-released pad while
            // allocating and priming idle HIGH, so listener boot never pulses low.
            sys::esp!(sys::gpio_hold_en(pin))?;
            let mut cfg = sys::rmt_tx_channel_config_t {
                gpio_num: pin,
                clk_src: sys::soc_periph_rmt_clk_src_t_RMT_CLK_SRC_DEFAULT,
                resolution_hz: 1_000_000,
                mem_block_symbols: 64,
                trans_queue_depth: 4,
                intr_priority: 0,
                ..Default::default()
            };
            cfg.flags.set_io_od_mode(1);
            cfg.flags.set_io_loop_back(1);
            sys::esp!(sys::rmt_new_tx_channel(&cfg, &mut hw.channel))?;
            sys::esp!(sys::rmt_new_copy_encoder(
                &Default::default(),
                &mut hw.encoder
            ))?;
            sys::esp!(sys::rmt_enable(hw.channel))?;
            hw.enabled = true;
            let released = sys::rmt_symbol_word_t {
                val: timing::DATA_LOW_1.nominal
                    | (1 << 15)
                    | (timing::DATA_LOW_1.nominal << 16)
                    | (1 << 31),
            };
            let mut idle_cfg = sys::rmt_transmit_config_t::default();
            idle_cfg.flags.set_eot_level(1);
            sys::esp!(sys::rmt_transmit(
                hw.channel,
                hw.encoder,
                (&released as *const sys::rmt_symbol_word_t).cast(),
                core::mem::size_of_val(&released),
                &idle_cfg
            ))?;
            sys::esp!(sys::rmt_tx_wait_all_done(hw.channel, 10))?;
            let callbacks = sys::rmt_tx_event_callbacks_t {
                on_trans_done: Some(tx_done_isr),
            };
            sys::esp!(sys::rmt_tx_register_event_callbacks(
                hw.channel,
                &callbacks,
                (&mut **hw.interrupts as *mut Interrupts).cast()
            ))?;
            // Restore the allocated RMT matrix signal only during initiator TX;
            // follower ACK uses the released GPIO latch, not the RMT idle output.
            hw.matrix = core::ptr::read_volatile(
                (sys::GPIO_FUNC0_OUT_SEL_CFG_REG + 4 * pin as u32) as *const u32,
            );
            sys::esp_rom_gpio_connect_out_signal(pin as u32, sys::SIG_GPIO_OUT_IDX, false, false);
            sys::esp!(sys::gpio_set_direction(
                pin,
                sys::gpio_mode_t_GPIO_MODE_INPUT_OUTPUT_OD
            ))?;
            sys::esp!(sys::gpio_set_pull_mode(
                pin,
                sys::gpio_pull_mode_t_GPIO_FLOATING
            ))?;
            sys::esp!(sys::gpio_hold_dis(pin))?;
            let arg = (&mut **hw.interrupts as *mut Interrupts).cast();
            let interrupts = &mut **hw.interrupts;
            for (callback, handle) in [
                (
                    release_isr as unsafe extern "C" fn(*mut core::ffi::c_void),
                    &mut interrupts.ack_release,
                ),
                (sample_isr, &mut interrupts.sample_timer),
                (wake_isr, &mut interrupts.wake_timer),
            ] {
                let args = sys::esp_timer_create_args_t {
                    callback: Some(callback),
                    arg,
                    dispatch_method: sys::esp_timer_dispatch_t_ESP_TIMER_ISR,
                    name: c"cec".as_ptr(),
                    ..Default::default()
                };
                sys::esp!(sys::esp_timer_create(&args, handle))?;
            }
            sys::esp!(sys::gpio_set_intr_type(
                pin,
                sys::gpio_int_type_t_GPIO_INTR_ANYEDGE
            ))?;
        }
        Ok(hw)
    }
    /// Bind ISR notifications to the calling (bus) task and arm the edge interrupt.
    /// Bench: binding to the constructing task (main, since init moved to core 0)
    /// left the bus task never woken and wedged boot on a live bus.
    /// The GPIO ISR service is installed here too: `gpio_install_isr_service`
    /// allocates the interrupt on the calling core, and installing it from main
    /// put the edge ISR on core 0 next to Wi-Fi/TLS, where it ran 0.3–0.9 ms
    /// late under MQTT load (phase 3.1 `err` evidence, 2026-11-01).
    pub fn arm(&mut self) -> crate::config::Result<()> {
        let interrupts = &mut **self.interrupts;
        interrupts.task.store(
            unsafe { sys::xTaskGetCurrentTaskHandle() } as usize,
            Ordering::Release,
        );
        unsafe {
            sys::esp!(sys::gpio_install_isr_service(
                sys::ESP_INTR_FLAG_IRAM as i32 | sys::ESP_INTR_FLAG_LEVEL3 as i32
            ))?;
        }
        self.service_owned = true;
        let arg = (interrupts as *mut Interrupts).cast();
        sys::esp!(unsafe { sys::gpio_isr_handler_add(interrupts.pin, Some(edge_isr), arg) })?;
        Ok(())
    }
    pub fn level(&self) -> Level {
        if unsafe { sys::gpio_get_level(self.interrupts.pin) } == 0 {
            Level::Low
        } else {
            Level::High
        }
    }
    pub fn ack(&self, fall_at_us: u64) {
        self.interrupts.ack_armed.store(false, Ordering::Release);
        self.interrupts
            .ack_at_low
            .store(fall_at_us as u32, Ordering::Relaxed);
        self.interrupts
            .ack_at_high
            .store((fall_at_us >> 32) as u32, Ordering::Relaxed);
        self.interrupts.ack_armed.store(true, Ordering::Release);
    }
    pub fn ack_count(&self) -> u32 {
        self.interrupts.ack_count.swap(0, Ordering::Relaxed)
    }
    /// Only decoder errors reach this capture via `Runner::rx`; no timer-list lock is taken.
    pub fn ack_evidence(&self, event: &mut aether_proto::BusEvent) {
        let aether_proto::BusEvent::Err {
            t_us,
            ack_age_us,
            ack_held_us,
            tmr_int,
            tmr_en,
            tmr_alarm_in_us,
            tmr_isr_in_us,
            tmr_task_in_us,
            ack_due_in_us,
            ..
        } = event
        else {
            return;
        };
        let context = &self.interrupts;
        let capture;
        // Safe on the bus core: GPIO edge and timer ISRs run on this core and
        // are masked. Other-core timestamp_id writers hold this same lock
        // (esp_timer_impl_lac.c:133-160; esp_timer_impl_common.c:41-63).
        // It is recursive for its owning core (components/esp_hw_support/
        // include/spinlock.h:104-113). Do not take a timer-list lock here:
        // timer insertion takes list -> hardware, never the reverse.
        // esp_timer_get_time aliases the counter reader, which only reads
        // registers and writes the counter latch, with no lock or allocation
        // (components/esp_timer/src/esp_timer_impl_lac.c:95-131).
        unsafe {
            esp_timer_private_lock();
            capture = AckCapture {
                state_before: context.ack_evidence_state.load(Ordering::Acquire),
                assert_before_us: u64::from(context.ack_assert_low.load(Ordering::Relaxed))
                    | (u64::from(context.ack_assert_high.load(Ordering::Relaxed)) << 32),
                now_us: sys::esp_timer_get_time() as u64,
                interrupt: LACT_INT_ST.read_volatile() & LACT_INT != 0,
                alarm_enabled: LACT_CONFIG.read_volatile() & LACT_ALARM_EN != 0,
                alarm_ticks: u64::from(LACT_ALARM_LO.read_volatile())
                    | (u64::from(LACT_ALARM_HI.read_volatile()) << 32),
                task_deadline_us: (&raw const timestamp_id).cast::<u64>().read_volatile(),
                isr_deadline_us: (&raw const timestamp_id)
                    .cast::<u64>()
                    .add(1)
                    .read_volatile(),
                state_after: context.ack_evidence_state.load(Ordering::Acquire),
                assert_after_us: u64::from(context.ack_assert_low.load(Ordering::Relaxed))
                    | (u64::from(context.ack_assert_high.load(Ordering::Relaxed)) << 32),
                held_us: context.ack_held_us.load(Ordering::Relaxed),
            };
            esp_timer_private_unlock();
        }
        let mut evidence = AckEvidence::default();
        ack_evidence(*t_us, &capture, &mut evidence);
        *ack_age_us = evidence.ack_age_us;
        *ack_held_us = evidence.ack_held_us;
        *tmr_int = evidence.tmr_int;
        *tmr_en = evidence.tmr_en;
        *tmr_alarm_in_us = evidence.tmr_alarm_in_us;
        *tmr_isr_in_us = evidence.tmr_isr_in_us;
        *tmr_task_in_us = evidence.tmr_task_in_us;
        *ack_due_in_us = evidence.ack_due_in_us;
    }
    /// Drain a 32-bit ISR delta into the bus task's per-boot u64 counter.
    pub fn take_ack_late(&self) -> u32 {
        self.interrupts.ack_late.swap(0, Ordering::Relaxed)
    }
    pub fn ack_held_max_us(&self) -> u32 {
        self.interrupts.ack_held_max_us.load(Ordering::Relaxed)
    }
    pub fn sample(&self, at_us: u64) -> bool {
        let now = crate::ffi::now_us();
        if at_us <= now {
            return false;
        }
        unsafe {
            sys::esp_timer_start_once(self.interrupts.sample_timer, at_us - now) == sys::ESP_OK
        }
    }
    pub fn send(&mut self, block: &Block, block_start_us: &u64) -> Result<u64, &'static str> {
        // A detached aborted waveform must finish before RMT can own the pad
        // again. Do not overwrite buffers still consumed by the copy encoder.
        if !self.routed && !self.finished() {
            return Err("detached_busy");
        }
        self.interrupts.ack_armed.store(false, Ordering::Release);
        for (out, symbol) in self.buffers[self.slot]
            .iter_mut()
            .zip(&block.symbols[..block.len])
        {
            *out = sys::rmt_symbol_word_t {
                val: symbol.low | (symbol.high << 16) | (1 << 31),
            };
        }
        let mut cfg = sys::rmt_transmit_config_t::default();
        cfg.flags.set_eot_level(1);
        cfg.flags.set_queue_nonblocking(1);
        // Only a fresh header acquires the bus now. A chained data block is
        // queued at the ACK sample, when a directed follower legitimately holds
        // LOW; requiring HIGH here aborts every acknowledged directed header.
        // The active RMT transaction owns the pad until the queued start.
        let header = block.len == 11;
        if !block.can_launch(crate::ffi::now_us(), *block_start_us, self.level()) {
            return Err(if header {
                "launch_line_low"
            } else {
                "chain_late"
            });
        }
        if header && !self.interrupts.edges.is_empty() {
            return Err("launch_edges_pending");
        }
        if !header && (!self.routed || self.finished()) {
            return Err("chain_inactive");
        }
        let submitted_us;
        unsafe {
            if !self.routed {
                core::ptr::write_volatile(
                    (sys::GPIO_FUNC0_OUT_SEL_CFG_REG + 4 * self.interrupts.pin as u32) as *mut u32,
                    self.matrix,
                );
                self.routed = true;
            }
            self.interrupts.in_flight.fetch_add(1, Ordering::AcqRel);
            submitted_us = crate::ffi::now_us();
            if sys::rmt_transmit(
                self.channel,
                self.encoder,
                self.buffers[self.slot].as_ptr().cast(),
                block.len * core::mem::size_of::<sys::rmt_symbol_word_t>(),
                &cfg,
            ) != sys::ESP_OK
            {
                self.interrupts.in_flight.fetch_sub(1, Ordering::Release);
                return Err("rmt_rejected");
            }
        }
        self.slot = (self.slot + 1) % 4;
        Ok(submitted_us)
    }
    /// Release immediately. RMT keeps running detached; never stop ESP32 RMT.
    pub fn abort(&mut self) {
        self.release();
        unsafe {
            sys::esp_timer_stop(self.interrupts.sample_timer);
            sys::esp_timer_stop(self.interrupts.wake_timer);
        }
        while self.interrupts.samples.pop().is_some() {}
    }
    /// OTA is passive, including an ACK armed before the upload began.
    pub fn suspend(&mut self) {
        self.interrupts.ack_armed.store(false, Ordering::Release);
        self.abort();
        unsafe {
            sys::esp_timer_stop(self.interrupts.ack_release);
            sys::gpio_set_level(self.interrupts.pin, 1);
        }
        self.interrupts.ack_active.store(false, Ordering::Release);
        // Do not report a hand-released state-1 ACK as still held. This also
        // drops a completed state-2 record before Runner's suspend drain.
        self.interrupts
            .ack_evidence_state
            .store(0, Ordering::Release);
    }

    pub fn upload_wait(&self) {
        unsafe { sys::vTaskDelay(2) };
    }
    fn release(&mut self) {
        if self.routed {
            unsafe {
                sys::gpio_set_level(self.interrupts.pin, 1);
                sys::esp_rom_gpio_connect_out_signal(
                    self.interrupts.pin as u32,
                    sys::SIG_GPIO_OUT_IDX,
                    false,
                    false,
                );
            }
            self.routed = false;
        }
    }
    /// Callback accounting plus the driver's nonblocking completion check.
    pub fn finished(&self) -> bool {
        self.interrupts.in_flight.load(Ordering::Acquire) == 0
            && unsafe { sys::rmt_tx_wait_all_done(self.channel, 0) } == sys::ESP_OK
    }
    /// Normal end is only a matrix handoff, never an abort or RMT stop.
    pub fn finish(&mut self) -> bool {
        if self.routed && self.finished() {
            self.release();
            true
        } else {
            false
        }
    }
    /// A real one-tick block shares the core during quiet periods even when
    /// notifications would return immediately. Never delay ACK arming/TX samples.
    pub fn idle_wait(&self) {
        unsafe { sys::vTaskDelay(1) };
    }
    /// Sleep on ISR notifications; a 1-ms timeout guarantees decoder polling at
    /// >=200 Hz even on an entirely quiet bus. Short TX waits use an ISR timer.
    pub fn wait(&self, until_us: u64) {
        let now = crate::ffi::now_us();
        unsafe {
            sys::esp_timer_stop(self.interrupts.wake_timer);
            if until_us > now && until_us - now < 1000 {
                if sys::esp_timer_start_once(self.interrupts.wake_timer, until_us - now)
                    != sys::ESP_OK
                {
                    return;
                }
            } else if until_us <= now {
                return;
            }
            sys::ulTaskGenericNotifyTake(0, 1, 1);
        }
    }
    pub fn ack_active(&self) -> bool {
        self.interrupts.ack_active.load(Ordering::Acquire)
    }
}

impl Drop for Hardware {
    fn drop(&mut self) {
        unsafe {
            if self.service_owned {
                sys::gpio_isr_handler_remove(self.interrupts.pin);
                sys::gpio_uninstall_isr_service();
            }
            for timer in [
                self.interrupts.ack_release,
                self.interrupts.sample_timer,
                self.interrupts.wake_timer,
            ] {
                if !timer.is_null() {
                    sys::esp_timer_stop(timer);
                    sys::esp_timer_delete(timer);
                }
            }
            // Enabled ESP32 RMT cannot be safely stopped. Retain its channel,
            // callback context and waveform buffers even on initialization error.
            if !self.enabled {
                if !self.channel.is_null() {
                    sys::rmt_del_channel(self.channel);
                }
                if !self.encoder.is_null() {
                    sys::rmt_del_encoder(self.encoder);
                }
            }
            sys::gpio_set_level(self.interrupts.pin, 1);
            sys::esp_rom_gpio_connect_out_signal(
                self.interrupts.pin as u32,
                sys::SIG_GPIO_OUT_IDX,
                false,
                false,
            );
            sys::gpio_hold_dis(self.interrupts.pin);
            if !self.enabled {
                ManuallyDrop::drop(&mut self.interrupts);
                ManuallyDrop::drop(&mut self.buffers);
            }
        }
    }
}
