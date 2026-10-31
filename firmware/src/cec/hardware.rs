//! Unsafe ESP-IDF boundary, imported as ffi::cec. All contexts are retained
//! for the lifetime of the bus task; callbacks never borrow movable task data.
use aether_logic::cec::{Block, EdgeRing, Schedule, ack_in_window};
use cec_core::{
    rx::{Edge, Level},
    timing,
};
use esp_idf_svc::sys;
use std::{
    mem::ManuallyDrop,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

pub struct Interrupts {
    pub edges: EdgeRing<256>,
    pub samples: EdgeRing<8>,
    pin: i32,
    task: usize,
    ack_at_low: AtomicU32,
    ack_at_high: AtomicU32,
    in_flight: AtomicU32,
    ack_armed: AtomicBool,
    ack_active: AtomicBool,
    ack_count: AtomicU32,
    ack_release: sys::esp_timer_handle_t,
    sample_timer: sys::esp_timer_handle_t,
    wake_timer: sys::esp_timer_handle_t,
}
// Handles belong to ESP-IDF and remain valid for the firmware lifetime.
unsafe impl Sync for Interrupts {}

#[inline(always)]
unsafe fn notify(context: &Interrupts, timer: bool) {
    let mut wake = 0;
    unsafe {
        sys::xTaskGenericNotifyFromISR(
            context.task as sys::TaskHandle_t,
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
        if ack_in_window(now, at) {
            unsafe {
                sys::gpio_set_level(context.pin, 0);
            }
            context.ack_active.store(true, Ordering::Release);
            if unsafe { sys::esp_timer_start_once(context.ack_release, u64::from(timing::ACK_LOW)) }
                != sys::ESP_OK
            {
                unsafe {
                    sys::gpio_set_level(context.pin, 1);
                }
                context.ack_active.store(false, Ordering::Release);
            } else {
                context.ack_count.fetch_add(1, Ordering::Relaxed);
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
        let mut previous = sys::esp_pthread_get_default_config();
        let _ = sys::esp_pthread_get_cfg(&mut previous);
        let mut cfg = previous;
        cfg.stack_size = 12288;
        cfg.prio = 22;
        cfg.pin_to_core = 1;
        cfg.inherit_cfg = false;
        cfg.thread_name = c"cec-bus".as_ptr();
        sys::esp!(sys::esp_pthread_set_cfg(&cfg))?;
        let spawned = std::thread::Builder::new()
            .name("cec-bus".into())
            .stack_size(12288)
            .spawn(run);
        let restored = sys::esp_pthread_set_cfg(&previous);
        spawned?;
        sys::esp!(restored)?;
    }
    Ok(())
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
impl Hardware {
    /// Called on core 1, so GPIO and RMT interrupt allocation is on core 1.
    pub fn new(pin: u8) -> crate::config::Result<Self> {
        let pin = i32::from(pin);
        let mut hw = Self {
            interrupts: ManuallyDrop::new(Box::new(Interrupts {
                edges: EdgeRing::default(),
                samples: EdgeRing::default(),
                pin,
                task: unsafe { sys::xTaskGetCurrentTaskHandle() as usize },
                ack_at_low: AtomicU32::new(0),
                ack_at_high: AtomicU32::new(0),
                in_flight: AtomicU32::new(0),
                ack_armed: AtomicBool::new(false),
                ack_active: AtomicBool::new(false),
                ack_count: AtomicU32::new(0),
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
                intr_priority: 3,
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
            sys::esp!(sys::gpio_install_isr_service(
                sys::ESP_INTR_FLAG_IRAM as i32 | sys::ESP_INTR_FLAG_LEVEL3 as i32
            ))?;
            hw.service_owned = true;
            sys::esp!(sys::gpio_set_intr_type(
                pin,
                sys::gpio_int_type_t_GPIO_INTR_ANYEDGE
            ))?;
            sys::esp!(sys::gpio_isr_handler_add(pin, Some(edge_isr), arg))?;
        }
        Ok(hw)
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
    pub fn sample(&self, at_us: u64) -> bool {
        let now = crate::ffi::now_us();
        if at_us <= now {
            return false;
        }
        unsafe {
            sys::esp_timer_start_once(self.interrupts.sample_timer, at_us - now) == sys::ESP_OK
        }
    }
    pub fn send(&mut self, block: &Block, block_start_us: &u64) -> bool {
        // A detached aborted waveform must finish before RMT can own the pad
        // again. Do not overwrite buffers still consumed by the copy encoder.
        if !self.routed && !self.finished() {
            return false;
        }
        self.interrupts.ack_armed.store(false, Ordering::Release);
        let buffer = &mut self.buffers[self.slot];
        for (out, symbol) in buffer.iter_mut().zip(&block.symbols[..block.len]) {
            *out = sys::rmt_symbol_word_t {
                val: symbol.low | (symbol.high << 16) | (1 << 31),
            };
        }
        let mut cfg = sys::rmt_transmit_config_t::default();
        cfg.flags.set_eot_level(1);
        cfg.flags.set_queue_nonblocking(1);
        // Check immediately before launch, not from a stale decoder snapshot.
        if unsafe { sys::gpio_get_level(self.interrupts.pin) } == 0
            || !self.interrupts.edges.is_empty()
            || (block.len == 10 && !Schedule::can_chain(crate::ffi::now_us(), *block_start_us))
        {
            return false;
        }
        unsafe {
            if !self.routed {
                core::ptr::write_volatile(
                    (sys::GPIO_FUNC0_OUT_SEL_CFG_REG + 4 * self.interrupts.pin as u32) as *mut u32,
                    self.matrix,
                );
                self.routed = true;
            }
            self.interrupts.in_flight.fetch_add(1, Ordering::AcqRel);
            if sys::rmt_transmit(
                self.channel,
                self.encoder,
                buffer.as_ptr().cast(),
                block.len * core::mem::size_of::<sys::rmt_symbol_word_t>(),
                &cfg,
            ) != sys::ESP_OK
            {
                self.interrupts.in_flight.fetch_sub(1, Ordering::Release);
                return false;
            }
        }
        self.slot = (self.slot + 1) % 4;
        true
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
