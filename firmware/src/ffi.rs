//! The only firmware module permitted to call unsafe ESP-IDF APIs.
use esp_idf_svc::sys;

#[path = "cec/hardware.rs"]
pub mod cec;

// Override IDF's generic descriptor with Cargo package name `aether`, allowing
// push uploads to enforce the same-project check before activating a slot.
esp_idf_svc::sys::esp_app_desc!();

pub fn private_logging() {
    // SDK network diagnostics can contain SSIDs, broker names and URLs.
    // Rust application records use events::Logger directly, not IDF tag filters.
    unsafe {
        sys::esp_log_level_set(c"*".as_ptr(), sys::esp_log_level_t_ESP_LOG_NONE);
    }
}
pub fn now_us() -> u64 {
    // ESP-IDF guarantees a nonnegative, monotonic microsecond clock.
    unsafe { sys::esp_timer_get_time() as u64 }
}
pub fn heap() -> (u32, u32, u32) {
    // These functions take no pointers and may be called from any task.
    unsafe {
        (
            sys::esp_get_free_heap_size(),
            sys::esp_get_minimum_free_heap_size(),
            sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_8BIT) as u32,
        )
    }
}

/// Snapshot all tasks alive at 60 s, including SDK tasks. ESP-IDF's stack depth
/// and high-water APIs use bytes (StackType_t is u8), not upstream port words.
pub fn log_stack_watermarks() {
    const MAX_TASKS: usize = 32;
    let mut tasks = [sys::TaskStatus_t::default(); MAX_TASKS];
    let count = unsafe {
        sys::uxTaskGetSystemState(tasks.as_mut_ptr(), MAX_TASKS as u32, core::ptr::null_mut())
    } as usize;
    if count == 0 {
        log::warn!("stack_hwm_unavailable capacity={MAX_TASKS}");
        return;
    }
    // Copy task names before logging: SSE tasks may exit while records are
    // emitted, so do not keep their TCB-owned name pointers across logger calls.
    let mut names = [[0_u8; 16]; MAX_TASKS];
    for (task, name) in tasks[..count].iter().zip(&mut names) {
        for (index, byte) in name.iter_mut().enumerate() {
            let ch = unsafe { *task.pcTaskName.add(index) } as u8;
            if ch == 0 {
                break;
            }
            *byte = if ch.is_ascii_graphic() { ch } else { b'?' };
        }
    }
    for (task, name) in tasks[..count].iter().zip(&names) {
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        let name = core::str::from_utf8(&name[..end]).unwrap_or("unknown");
        log::info!("stack_hwm task={name} hwm={}", task.usStackHighWaterMark);
    }
}

/// The safe wrapper currently replaces MQTT_EVENT_ERROR with a generic error.
/// Subscribe to the supported C event API instead; retain only numeric codes.
pub struct MqttDiagnostics {
    client: sys::esp_mqtt_client_handle_t,
    reported: Box<std::sync::atomic::AtomicU32>,
}
impl MqttDiagnostics {
    pub fn new(
        client: &esp_idf_svc::mqtt::client::EspMqttClient<'_>,
    ) -> Result<Self, sys::EspError> {
        use esp_idf_svc::handle::RawHandle;
        let diagnostics = Self {
            client: client.handle(),
            reported: Box::new(std::sync::atomic::AtomicU32::new(0)),
        };
        sys::esp!(unsafe {
            sys::esp_mqtt_client_register_event(
                diagnostics.client,
                sys::esp_mqtt_event_id_t_MQTT_EVENT_ANY,
                Some(mqtt_diagnostic),
                (&*diagnostics.reported as *const std::sync::atomic::AtomicU32)
                    .cast_mut()
                    .cast(),
            )
        })?;
        Ok(diagnostics)
    }
}
impl Drop for MqttDiagnostics {
    fn drop(&mut self) {
        unsafe {
            sys::esp_mqtt_client_unregister_event(
                self.client,
                sys::esp_mqtt_event_id_t_MQTT_EVENT_ANY,
                Some(mqtt_diagnostic),
            );
        }
    }
}
unsafe extern "C" fn mqtt_diagnostic(
    arg: *mut core::ffi::c_void,
    _: sys::esp_event_base_t,
    id: i32,
    data: *mut core::ffi::c_void,
) {
    use std::sync::atomic::{AtomicU32, Ordering};
    let reported = unsafe { &*arg.cast::<AtomicU32>() };
    if id == sys::esp_mqtt_event_id_t_MQTT_EVENT_CONNECTED as i32 {
        reported.store(0, Ordering::Relaxed);
        return;
    }
    if id != sys::esp_mqtt_event_id_t_MQTT_EVENT_ERROR as i32 {
        return;
    }
    let event = unsafe { &*data.cast::<sys::esp_mqtt_event_t>() };
    let Some(error) = (unsafe { event.error_handle.as_ref() }) else {
        return;
    };
    // mbedTLS can combine high-level and low-level errors. Match either part,
    // including SSL/X509/PK/PEM/ECP and MPI/ASN1 allocator failures.
    let tls = error.esp_tls_stack_err.unsigned_abs();
    let oom = error.esp_tls_last_esp_err == sys::ESP_ERR_NO_MEM
        || error.esp_transport_sock_errno == sys::ENOMEM as i32
        || matches!(
            tls & 0xff80,
            0x7f00 | 0x2880 | 0x3f80 | 0x1180 | 0x4d80 | 0x6180 | 0x5180 | 0x3400 | 0x5780
        )
        || matches!(tls & 0x7f, 0x10 | 0x6a | 0x17);
    let previous = reported.fetch_or(if oom { 3 } else { 1 }, Ordering::Relaxed);
    if previous & 1 == 0 {
        log::warn!(
            "mqtt_error type={} esp={} tls={} verify={} connack={} errno={} oom={}",
            error.error_type,
            error.esp_tls_last_esp_err,
            error.esp_tls_stack_err,
            error.esp_tls_cert_verify_flags,
            error.connect_return_code,
            error.esp_transport_sock_errno,
            u8::from(oom)
        );
    }
    // Do not hide a later OOM behind an earlier non-memory transport failure.
    if oom && previous & 2 == 0 {
        log::warn!("mqtt_handshake_no_memory tls={}", error.esp_tls_stack_err);
    }
}
pub fn restart() -> ! {
    unsafe { sys::esp_restart() }
}

pub fn reset_reason() -> aether_proto::ResetReason {
    use aether_proto::ResetReason;
    match unsafe { sys::esp_reset_reason() } {
        sys::esp_reset_reason_t_ESP_RST_POWERON => ResetReason::PowerOn,
        sys::esp_reset_reason_t_ESP_RST_SW => ResetReason::Software,
        sys::esp_reset_reason_t_ESP_RST_PANIC => ResetReason::Panic,
        sys::esp_reset_reason_t_ESP_RST_INT_WDT => ResetReason::IntWdt,
        sys::esp_reset_reason_t_ESP_RST_TASK_WDT => ResetReason::TaskWdt,
        sys::esp_reset_reason_t_ESP_RST_BROWNOUT => ResetReason::Brownout,
        _ => ResetReason::Other,
    }
}

static LAST_CRASH: std::sync::LazyLock<Option<aether_proto::LastCrash>> =
    std::sync::LazyLock::new(read_last_crash);

/// First called before console/network startup, after the logger exists. IDF
/// initializes its panic-time writer during system startup; we only read the
/// previous boot's ELF dump in normal task context.
pub fn last_crash() -> Option<&'static aether_proto::LastCrash> {
    LAST_CRASH.as_ref()
}

fn read_last_crash() -> Option<aether_proto::LastCrash> {
    let partition = unsafe {
        sys::esp_partition_find_first(
            sys::esp_partition_type_t_ESP_PARTITION_TYPE_DATA,
            sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_DATA_COREDUMP,
            core::ptr::null(),
        )
    };
    if partition.is_null() {
        // OTA cannot replace the partition table installed by the bootloader.
        log::error!("coredump_unavailable_usb_table_required");
        return None;
    }
    let checked = unsafe { sys::esp_core_dump_image_check() };
    if checked != sys::ESP_OK {
        if checked != sys::ESP_ERR_NOT_FOUND {
            log::warn!("coredump_check_failed code={checked}");
        }
        return None;
    }
    if !matches!(
        reset_reason(),
        aether_proto::ResetReason::Panic
            | aether_proto::ResetReason::IntWdt
            | aether_proto::ResetReason::TaskWdt
    ) {
        // A dump surviving a power cycle is not evidence about this reset.
        clear_core_dump();
        return None;
    }
    let mut summary = sys::esp_core_dump_summary_t::default();
    let result = unsafe { sys::esp_core_dump_get_summary(&mut summary) };
    if result != sys::ESP_OK {
        log::warn!("coredump_summary_failed code={result}");
        return None;
    }
    let task = summary.exc_task.map(|byte| byte as u8);
    let bt = &summary.exc_bt_info;
    let depth = (bt.depth as usize).min(bt.bt.len());
    let text = aether_logic::status::crash_summary(
        summary.ex_info.exc_cause,
        summary.exc_pc,
        &task,
        &bt.bt[..depth],
    );
    log::error!("last_crash {text}");
    // Keep the recovered summary in RAM for every status publication this boot.
    // The flash dump is one-shot; a later crash may replace it.
    clear_core_dump();
    Some(text)
}

fn clear_core_dump() {
    let result = unsafe { sys::esp_core_dump_image_erase() };
    if result != sys::ESP_OK {
        log::warn!("coredump_erase_failed code={result}");
    }
}

pub fn install_console_uart() -> Result<(), sys::EspError> {
    use std::io::Write;
    // Drain Rust's line buffer and newlib's early-log buffers before changing
    // VFS backends. Newlib remains unbuffered; subsequent flushes cannot replay
    // an old C stdio buffer. This runs before console/network worker startup.
    let _ = std::io::stdout().flush();
    unsafe {
        let reent = &mut *sys::__getreent();
        for stream in [reent._stdout, reent._stderr] {
            if sys::fflush(stream) != 0
                || sys::setvbuf(stream, core::ptr::null_mut(), sys::_IONBF as i32, 0) != 0
            {
                return Err(sys::EspError::from_infallible::<{ sys::ESP_FAIL }>());
            }
        }
    }
    // Bench: with a TX ring buffer the ESP32 UART0 re-sent suffixes of the last
    // line hundreds of times (4 MB in 100 s), saturating the console and starving
    // OTA uploads. Synchronous TX (no ring buffer, single FIFO writer) and a
    // drained FIFO at install time are the measured fix; the hardware cause is
    // not pinned down (see decisions.md).
    if !unsafe { sys::uart_is_driver_installed(sys::uart_port_t_UART_NUM_0) } {
        unsafe { sys::uart_wait_tx_idle_polling(sys::uart_port_t_UART_NUM_0) };
        sys::esp!(unsafe {
            sys::uart_driver_install(
                sys::uart_port_t_UART_NUM_0,
                512,
                0,
                0,
                core::ptr::null_mut(),
                0,
            )
        })?;
    }
    unsafe { sys::uart_vfs_dev_use_driver(0) };
    Ok(())
}

pub fn erase_nvs() -> Result<(), sys::EspError> {
    sys::esp!(unsafe { sys::nvs_flash_erase() })
}

pub fn confirm_app() -> bool {
    if unsafe { sys::esp_ota_mark_app_valid_cancel_rollback() } != sys::ESP_OK {
        log::warn!("ota_confirm_failed");
        false
    } else {
        true
    }
}

pub fn reconnect_mqtt(client: &esp_idf_svc::mqtt::client::EspMqttClient<'_>) {
    use esp_idf_svc::handle::RawHandle;
    // Runs on main, never inside the callback holding esp-mqtt's mutex.
    // A disconnect enters WAIT_RECONNECT; esp-mqtt's enabled auto-reconnect
    // retries after its normal delay. reconnect() here would race that transition.
    let result = unsafe { sys::esp_mqtt_client_disconnect(client.handle()) };
    if result != sys::ESP_OK {
        log::warn!("mqtt_reconnect_failed");
    }
}

pub fn new_token() -> String {
    use std::fmt::Write;
    let mut bytes = [0_u8; 32];
    // Called only while loading secrets, before Wi-Fi/ADC initialization.
    unsafe {
        sys::bootloader_random_enable();
        sys::esp_fill_random(bytes.as_mut_ptr().cast(), bytes.len());
        sys::bootloader_random_disable();
    }
    let mut token = String::with_capacity(64);
    for byte in bytes {
        write!(token, "{byte:02x}").unwrap();
    }
    token
}

pub fn running_slot() -> u32 {
    unsafe {
        sys::esp_ota_get_running_partition()
            .as_ref()
            .map_or(0, |p| p.address)
    }
}
pub fn running_pending() -> bool {
    let mut state = sys::esp_ota_img_states_t_ESP_OTA_IMG_UNDEFINED;
    unsafe {
        sys::esp_ota_get_state_partition(sys::esp_ota_get_running_partition(), &mut state)
            == sys::ESP_OK
            && state == sys::esp_ota_img_states_t_ESP_OTA_IMG_PENDING_VERIFY
    }
}
pub fn update_slot() -> Option<(u32, usize)> {
    unsafe {
        sys::esp_ota_get_next_update_partition(core::ptr::null())
            .as_ref()
            .map(|p| (p.address, p.size as usize))
    }
}
pub fn update_project() -> bool {
    let mut desc = sys::esp_app_desc_t::default();
    unsafe {
        let partition = sys::esp_ota_get_next_update_partition(core::ptr::null());
        !partition.is_null()
            && sys::esp_ota_get_partition_description(partition, &mut desc) == sys::ESP_OK
            && core::ffi::CStr::from_ptr(desc.project_name.as_ptr()).to_bytes() == b"aether"
    }
}

// Native async requests are needed because EspHttpConnection completes a response
// when its handler returns. Each copied request is owned by exactly one SSE task.
pub struct SseRegistration {
    server: sys::httpd_handle_t,
    context: Box<std::sync::Arc<crate::http::Shared>>,
}
impl Drop for SseRegistration {
    fn drop(&mut self) {
        unsafe {
            sys::httpd_unregister_uri_handler(
                self.server,
                c"/api/events".as_ptr(),
                sys::http_method_HTTP_GET,
            );
        }
    }
}
pub fn register_sse(
    server: sys::httpd_handle_t,
    state: std::sync::Arc<crate::http::Shared>,
) -> Result<SseRegistration, sys::EspError> {
    let mut registration = SseRegistration {
        server,
        context: Box::new(state),
    };
    let uri = sys::httpd_uri_t {
        uri: c"/api/events".as_ptr(),
        method: sys::http_method_HTTP_GET,
        handler: Some(sse_handler),
        user_ctx: (&mut *registration.context as *mut std::sync::Arc<crate::http::Shared>).cast(),
        ..Default::default()
    };
    sys::esp!(unsafe { sys::httpd_register_uri_handler(server, &uri) })?;
    Ok(registration)
}
unsafe extern "C" fn sse_handler(req: *mut sys::httpd_req_t) -> sys::esp_err_t {
    let state = unsafe {
        &*((*req)
            .user_ctx
            .cast::<std::sync::Arc<crate::http::Shared>>())
    };
    let mut authorization = [0_u8; 80];
    let header = if unsafe {
        sys::httpd_req_get_hdr_value_str(
            req,
            c"Authorization".as_ptr(),
            authorization.as_mut_ptr().cast(),
            authorization.len(),
        )
    } == sys::ESP_OK
    {
        core::ffi::CStr::from_bytes_until_nul(&authorization)
            .ok()
            .and_then(|s| s.to_str().ok())
    } else {
        None
    };
    if !aether_logic::http::authorized(
        Some(aether_logic::http::Route::Events),
        header,
        &state.token.lock().unwrap(),
    ) {
        return unsafe {
            native_error(
                req,
                c"401 Unauthorized",
                c"{\"ok\":false,\"reason\":\"unauthorized\"}",
            )
        };
    }
    let Some(events) = state.hub.subscribe() else {
        return unsafe {
            native_error(req, c"409 Conflict", c"{\"ok\":false,\"reason\":\"busy\"}")
        };
    };
    let mut copy = core::ptr::null_mut();
    if unsafe { sys::httpd_req_async_handler_begin(req, &mut copy) } != sys::ESP_OK {
        return unsafe {
            native_error(req, c"409 Conflict", c"{\"ok\":false,\"reason\":\"busy\"}")
        };
    }
    // Capture the address as usize; the async begin API grants independent ownership.
    let address = copy as usize;
    if std::thread::Builder::new()
        .name("http-sse".into())
        .stack_size(8192)
        .spawn(move || {
            let req = address as *mut sys::httpd_req_t;
            unsafe {
                sys::httpd_resp_set_type(req, c"text/event-stream".as_ptr());
                sys::httpd_resp_set_hdr(req, c"Cache-Control".as_ptr(), c"no-cache".as_ptr());
            }
            if unsafe { sys::httpd_resp_send_chunk(req, c": connected\n\n".as_ptr(), 13) }
                != sys::ESP_OK
            {
                unsafe {
                    sys::httpd_req_async_handler_complete(req);
                }
                return;
            }
            loop {
                if !events.alive.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                let line = match events.rx.recv_timeout(std::time::Duration::from_secs(15)) {
                    Ok(line) => line,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        std::sync::Arc::from(": keep-alive\n\n")
                    }
                    Err(_) => break,
                };
                if unsafe {
                    sys::httpd_resp_send_chunk(req, line.as_ptr().cast(), line.len() as isize)
                } != sys::ESP_OK
                {
                    break;
                }
            }
            unsafe {
                // Reclaim copied request and actively close; no stale queue/socket survives.
                let server = (*req).handle;
                let fd = sys::httpd_req_to_sockfd(req);
                sys::httpd_req_async_handler_complete(req);
                sys::httpd_sess_trigger_close(server, fd);
            }
        })
        .is_err()
    {
        unsafe {
            sys::httpd_req_async_handler_complete(copy);
        }
        return unsafe {
            native_error(req, c"409 Conflict", c"{\"ok\":false,\"reason\":\"busy\"}")
        };
    }
    sys::ESP_OK
}
unsafe fn native_error(
    req: *mut sys::httpd_req_t,
    status: &core::ffi::CStr,
    body: &core::ffi::CStr,
) -> sys::esp_err_t {
    unsafe {
        sys::httpd_resp_set_status(req, status.as_ptr());
        sys::httpd_resp_set_type(req, c"application/json".as_ptr());
        sys::httpd_resp_send(req, body.as_ptr(), body.to_bytes().len() as isize)
    }
}
