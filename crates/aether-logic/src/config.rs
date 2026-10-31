//! Persisted configuration validation and atomic patches.
use aether_proto::{
    CecVersion, Config, ConfigCommand, HpdConfig, LogicalAddress, PhysicalAddressConfig,
};
use cec_core::{bridge, rx::AckSet};
// Classic ESP32 output-capable pads, excluding flash pins and UART0.
const CEC_PINS: &[u8] = &[
    2, 4, 5, 12, 13, 14, 15, 16, 17, 18, 19, 21, 22, 23, 25, 26, 27, 32, 33,
];

/// Factory configuration (no credentials).
///
/// # Panics
/// Only if the compiled-in factory JSON no longer matches the protocol types.
pub fn defaults() -> Config {
    serde_json::from_str(r#"{"node":"aether","la":0,"type":"tv","pa":"0.0.0.0","osd":"Aether","vendor":"012345","cec_version":"1.4","replies":["83","8C","8F","9F","46","91"],"ack":[0],"telemetry_s":60,"allow_from":false,"log":"off","gpio":{"cec":4,"hpd":null}}"#).expect("factory config is valid")
}

/// Validate node syntax before storing it or constructing MQTT topics.
pub fn valid_node(node: &str) -> bool {
    !node.is_empty()
        && node.len() <= 16
        && node
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Convert wire configuration into the core contract, rejecting unsupported settings.
pub fn core_config(cfg: &Config) -> Result<bridge::Config, &'static str> {
    if !valid_node(&cfg.node) {
        return Err("bad_node");
    }
    let la = match cfg.la {
        LogicalAddress::Fixed(la) if la <= 14 => Some(la),
        LogicalAddress::Auto => None,
        LogicalAddress::Fixed(_) => return Err("bad_la"),
    };
    let physical_address = match cfg.pa {
        PhysicalAddressConfig::Fixed(pa) => pa.0,
        PhysicalAddressConfig::Edid => return Err("edid_unsupported"),
    };
    if !CEC_PINS.contains(&cfg.gpio.cec) {
        return Err("bad_cec_gpio");
    }
    if let Some(hpd) = cfg.gpio.hpd {
        if !CEC_PINS.contains(&hpd) && ![34, 35, 36, 39].contains(&hpd) {
            return Err("bad_hpd_gpio");
        }
        if hpd == cfg.gpio.cec {
            return Err("gpio_conflict");
        }
    }
    if cfg.ack.iter().any(|&la| la > 14) {
        return Err("bad_ack");
    }
    let mut replies = bridge::ReplySet::empty();
    for opcode in &cfg.replies {
        replies.insert(opcode.0).map_err(|_| "forbidden_reply")?;
    }
    Ok(bridge::Config {
        la,
        device_type: match cfg.device_type {
            aether_proto::DeviceType::Tv => bridge::DeviceType::Tv,
            aether_proto::DeviceType::Playback => bridge::DeviceType::Playback,
            aether_proto::DeviceType::Audio => bridge::DeviceType::Audio,
            aether_proto::DeviceType::Recorder => bridge::DeviceType::Recorder,
            aether_proto::DeviceType::Tuner => bridge::DeviceType::Tuner,
        },
        physical_address,
        osd: cfg.osd.clone(),
        vendor: cfg.vendor,
        cec_version: match cfg.cec_version {
            CecVersion::V1_3a => 4,
            CecVersion::V1_4 => 5,
            CecVersion::V2_0 => 6,
        },
        replies,
        ack: AckSet::new(&cfg.ack),
        allow_from: cfg.allow_from,
    })
}

/// Merge a partial update without mutating the current value on error.
pub fn merge(current: &Config, patch: ConfigCommand) -> Result<Config, &'static str> {
    let mut cfg = current.clone();
    macro_rules! replace { ($($field:ident),*) => { $(if let Some(value) = patch.$field { cfg.$field = value; })* }; }
    replace!(
        node,
        la,
        device_type,
        pa,
        osd,
        vendor,
        cec_version,
        replies,
        ack,
        telemetry_s,
        allow_from,
        log
    );
    if let Some(gpio) = patch.gpio {
        if let Some(cec) = gpio.cec {
            cfg.gpio.cec = cec;
        }
        if let Some(hpd) = gpio.hpd {
            cfg.gpio.hpd = match hpd {
                HpdConfig::Disabled => None,
                HpdConfig::Pin(pin) => Some(pin),
            };
        }
    }
    core_config(&cfg)?;
    Ok(cfg)
}
