//! The only firmware module permitted to call unsafe ESP-IDF APIs.
use esp_idf_svc::sys;

// Override IDF's generic descriptor with Cargo package name `aether`, allowing
// push uploads to enforce the same-project check before activating a slot.
esp_idf_svc::sys::esp_app_desc!();

pub fn private_logging() {
    // SDK network diagnostics can contain SSIDs, broker names and URLs.
    // Permit only our scrubbed application records after provisioning is read.
    unsafe {
        sys::esp_log_level_set(c"*".as_ptr(), sys::esp_log_level_t_ESP_LOG_NONE);
        sys::esp_log_level_set(c"aether".as_ptr(), sys::esp_log_level_t_ESP_LOG_INFO);
    }
}
pub fn now_us() -> u64 {
    // ESP-IDF guarantees a nonnegative, monotonic microsecond clock.
    unsafe { sys::esp_timer_get_time() as u64 }
}
pub fn heap() -> (u32, u32) {
    // These functions take no pointers and may be called from any task.
    unsafe {
        (
            sys::esp_get_free_heap_size(),
            sys::esp_get_minimum_free_heap_size(),
        )
    }
}
pub fn restart() -> ! {
    unsafe { sys::esp_restart() }
}

pub fn install_console_uart() -> Result<(), sys::EspError> {
    // RX interrupts buffer a whole pasted console line; TX remains synchronous,
    // shared with EspLogger. VFS driver reads block instead of polling the FIFO.
    if !unsafe { sys::uart_is_driver_installed(sys::uart_port_t_UART_NUM_0) } {
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
        .stack_size(4096)
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
