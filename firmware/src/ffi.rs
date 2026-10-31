//! The only firmware module permitted to call unsafe ESP-IDF APIs.
use esp_idf_svc::sys;

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

pub fn confirm_app() {
    // Called once by the main task, including in console-only recovery.
    if unsafe { sys::esp_ota_mark_app_valid_cancel_rollback() } != sys::ESP_OK {
        log::warn!("ota_confirm_failed");
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
