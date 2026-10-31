//! aether firmware entry point. Phase 2 fills this in; for now it proves the
//! toolchain and links the host-tested crates.
use esp_idf_svc::sys;

fn main() {
    sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    let bridge = cec_core::bridge::Bridge::new(cec_core::bridge::Config::default(), 0);
    log::info!("aether starting, la={:?}", bridge.config().la);
}
