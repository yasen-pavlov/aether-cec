fn main() {
    embuild::espidf::sysenv::output();
    // The exported app-descriptor macro references both active and inactive SDK
    // cfgs. Register their names even when this particular IDF leaves them unset.
    for name in [
        "esp_idf_app_compile_time_date",
        "esp_idf_app_reproducible_build",
        "esp_idf_version_patch_at_least_5_1_7",
        "esp_idf_version_patch_at_least_5_2_3",
        "esp_idf_version_at_least_5_3_2",
        "esp_idf_version_at_least_5_4_0",
        "esp_idf_version_at_least_6_1_0",
        "esp_idf_idf_target_linux",
        "esp_idf_app_build_type_pure_ram_app",
        "esp_idf_version_patch_at_least_5_3_2",
        "esp_idf_gpio_ctrl_func_in_iram",
        "esp_idf_rmt_isr_iram_safe",
        "esp_idf_gptimer_ctrl_func_in_iram",
        "esp_idf_gptimer_isr_iram_safe",
        "esp_idf_gptimer_isr_handler_in_iram",
        "esp_idf_esp_timer_supports_isr_dispatch_method",
        "esp_idf_esp_timer_isr_affinity_cpu1",
    ] {
        println!("cargo:rustc-check-cfg=cfg({name})");
    }
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_else(|| "unknown".into());
    // Release builds set AETHER_VERSION (the tag without its `v`); every other
    // build reports the package version plus the commit it was built from.
    let fw = match std::env::var("AETHER_VERSION") {
        Ok(version) if !version.is_empty() => version,
        _ => format!("{}+{}", env!("CARGO_PKG_VERSION"), sha.trim()),
    };
    println!("cargo:rustc-env=AETHER_FW={fw}");
    println!("cargo:rerun-if-env-changed=AETHER_VERSION");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");
}
