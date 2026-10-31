//! Configuration and verb-dispatched command payloads.

use crate::{DeviceType, LogLevel, Opcode, PhysicalAddress, Reason, RequestId, vendor};
use core::fmt;
use heapless::{String, Vec};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Fixed logical address or automatic allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalAddress {
    /// Fixed logical address.
    Fixed(u8),
    /// Allocate by device type.
    Auto,
}

impl Serialize for LogicalAddress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Fixed(address) => serializer.serialize_u8(*address),
            Self::Auto => serializer.serialize_str("auto"),
        }
    }
}

impl<'de> Deserialize<'de> for LogicalAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = LogicalAddress;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a logical address 0..14 or auto")
            }
            fn visit_u64<E: de::Error>(self, address: u64) -> Result<Self::Value, E> {
                if address > 14 {
                    return Err(E::custom("fixed logical address must be 0..14"));
                }
                Ok(LogicalAddress::Fixed(
                    u8::try_from(address).map_err(E::custom)?,
                ))
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                if text == "auto" {
                    Ok(LogicalAddress::Auto)
                } else {
                    Err(E::custom("expected auto"))
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// Fixed physical address or an explicit request to consult EDID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalAddressConfig {
    /// Use exactly this address; never consult EDID.
    Fixed(PhysicalAddress),
    /// Read the sink's EDID over configured DDC pins.
    Edid,
}

impl Serialize for PhysicalAddressConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Fixed(address) => address.serialize(serializer),
            Self::Edid => serializer.serialize_str("edid"),
        }
    }
}

impl<'de> Deserialize<'de> for PhysicalAddressConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = PhysicalAddressConfig;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a physical address a.b.c.d or edid")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                if text == "edid" {
                    return Ok(PhysicalAddressConfig::Edid);
                }
                PhysicalAddress::deserialize(de::value::StrDeserializer::<E>::new(text))
                    .map(PhysicalAddressConfig::Fixed)
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

/// CEC version reported in response to Get CEC Version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CecVersion {
    /// CEC 1.3a, byte 04.
    #[serde(rename = "1.3a")]
    V1_3a,
    /// CEC 1.4, byte 05.
    #[serde(rename = "1.4")]
    V1_4,
    /// CEC 2.0, byte 06.
    #[serde(rename = "2.0")]
    V2_0,
}

/// Effective GPIO assignments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gpio {
    /// CEC open-drain GPIO.
    pub cec: u8,
    /// Optional hot-plug-detect input.
    #[serde(default)]
    pub hpd: Option<u8>,
}

/// Effective, persisted CEC configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Topic name component.
    pub node: String<16>,
    /// Fixed or allocated logical address.
    pub la: LogicalAddress,
    /// Device role.
    #[serde(rename = "type")]
    pub device_type: DeviceType,
    /// Fixed address or EDID selection.
    pub pa: PhysicalAddressConfig,
    /// OSD name.
    pub osd: String<14>,
    /// Three-byte vendor identifier, in wire order.
    #[serde(with = "vendor")]
    pub vendor: [u8; 3],
    /// Reported CEC version.
    pub cec_version: CecVersion,
    /// Opcodes answered locally (all 256 opcode values fit).
    pub replies: Vec<Opcode, 256>,
    /// Acknowledged logical addresses (broadcast excluded).
    pub ack: Vec<u8, 15>,
    /// Telemetry period in seconds; zero disables it.
    pub telemetry_s: u16,
    /// Permit bench-only initiator overrides.
    pub allow_from: bool,
    /// Published logging threshold.
    pub log: LogLevel,
    /// GPIO assignments; changes require restart.
    pub gpio: Gpio,
}

/// Explicit HPD GPIO setting, distinct from an omitted configuration update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HpdConfig {
    /// Disable the optional HPD input (JSON null).
    Disabled,
    /// Enable HPD on this GPIO.
    Pin(u8),
}

impl Serialize for HpdConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Disabled => serializer.serialize_none(),
            Self::Pin(pin) => serializer.serialize_u8(*pin),
        }
    }
}

fn nullable<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<HpdConfig>, D::Error> {
    Option::<u8>::deserialize(deserializer)
        .map(|pin| Some(pin.map_or(HpdConfig::Disabled, HpdConfig::Pin)))
}

/// Partial GPIO update; HPD distinguishes omitted (keep) from null (disable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpioPatch {
    /// Replacement CEC GPIO.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cec: Option<u8>,
    /// Omitted means unchanged; explicit null disables HPD.
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Option::is_none"
    )]
    pub hpd: Option<HpdConfig>,
}

mod optional_vendor {
    use super::{Deserialize, Deserializer, Serialize, Serializer, vendor};
    #[derive(Serialize, Deserialize)]
    struct Vendor(#[serde(with = "vendor")] [u8; 3]);
    // serde's `with` hook requires a reference to the original field type.
    #[allow(clippy::ref_option, clippy::trivially_copy_pass_by_ref)]
    pub fn serialize<S: Serializer>(
        value: &Option<[u8; 3]>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.map(Vendor).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<[u8; 3]>, D::Error> {
        Option::<Vendor>::deserialize(deserializer).map(|value| value.map(|vendor| vendor.0))
    }
}

/// Atomic partial configuration update, carried on `cmd/cfg`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigCommand {
    /// Correlation identifier.
    pub id: RequestId,
    /// Replacement topic component (effective after restart).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String<16>>,
    /// Replacement logical-address policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub la: Option<LogicalAddress>,
    /// Replacement device role.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub device_type: Option<DeviceType>,
    /// Replacement physical-address policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pa: Option<PhysicalAddressConfig>,
    /// Replacement OSD name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub osd: Option<String<14>>,
    /// Replacement vendor identifier.
    #[serde(
        default,
        with = "optional_vendor",
        skip_serializing_if = "Option::is_none"
    )]
    pub vendor: Option<[u8; 3]>,
    /// Replacement CEC version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cec_version: Option<CecVersion>,
    /// Replacement local reply set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replies: Option<Vec<Opcode, 256>>,
    /// Replacement acknowledgement set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<Vec<u8, 15>>,
    /// Replacement telemetry interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_s: Option<u16>,
    /// Replacement bench override permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_from: Option<bool>,
    /// Replacement logging threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<LogLevel>,
    /// Partial GPIO replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpio: Option<GpioPatch>,
}

/// Host-set power status used by local Give Device Power Status replies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerStatus {
    /// Powered on.
    On,
    /// In standby.
    Standby,
    /// Transitioning to on.
    ToOn,
    /// Transitioning to standby.
    ToStandby,
}

/// Runtime power update on `cmd/state`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateRequest {
    /// Correlation identifier.
    pub id: RequestId,
    /// New reported power state.
    pub power: PowerStatus,
}

/// RTT measurement on `cmd/ping`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingCommand {
    /// Correlation identifier.
    pub id: RequestId,
    /// Opaque value echoed by the result.
    pub echo: String<32>,
}

/// Clean restart on `cmd/restart`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartCommand {
    /// Correlation identifier.
    pub id: RequestId,
}

/// Erase CEC configuration (not credentials) on `cmd/reset`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetCommand {
    /// Correlation identifier.
    pub id: RequestId,
    /// Node name confirming this destructive operation.
    pub confirm: String<16>,
}

/// Topic command verb; the verb is not duplicated inside request payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verb {
    /// Partial configuration update.
    Cfg,
    /// Runtime power update.
    State,
    /// Ping and echo.
    Ping,
    /// Clean restart.
    Restart,
    /// Reset CEC settings.
    Reset,
}

impl Verb {
    /// The exact suffix of the command topic.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cfg => "cfg",
            Self::State => "state",
            Self::Ping => "ping",
            Self::Restart => "restart",
            Self::Reset => "reset",
        }
    }
}

/// A command selected by the MQTT topic verb, not by inspecting its JSON keys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
#[expect(
    clippy::large_enum_variant,
    reason = "wire commands must remain no_std and allocation-free"
)]
pub enum Command {
    /// Atomic partial configuration change.
    Cfg(ConfigCommand),
    /// Runtime state change.
    State(StateRequest),
    /// Ping and echo.
    Ping(PingCommand),
    /// Restart.
    Restart(RestartCommand),
    /// Reset CEC configuration.
    Reset(ResetCommand),
}

impl Command {
    /// Decode according to the topic verb, using the caller's deserializer.
    ///
    /// This avoids an allocating tagged-enum buffer and lets firmware choose an
    /// allocation-free JSON parser while hosts can use `serde_json`.
    pub fn deserialize_for<'de, D: Deserializer<'de>>(
        verb: Verb,
        deserializer: D,
    ) -> Result<Self, D::Error> {
        match verb {
            Verb::Cfg => ConfigCommand::deserialize(deserializer).map(Self::Cfg),
            Verb::State => StateRequest::deserialize(deserializer).map(Self::State),
            Verb::Ping => PingCommand::deserialize(deserializer).map(Self::Ping),
            Verb::Restart => RestartCommand::deserialize(deserializer).map(Self::Restart),
            Verb::Reset => ResetCommand::deserialize(deserializer).map(Self::Reset),
        }
    }

    /// Return the verb needed for publishing this command.
    pub const fn verb(&self) -> Verb {
        match self {
            Self::Cfg(_) => Verb::Cfg,
            Self::State(_) => Verb::State,
            Self::Ping(_) => Verb::Ping,
            Self::Restart(_) => Verb::Restart,
            Self::Reset(_) => Verb::Reset,
        }
    }
}

/// Command outcome on `cmd/result`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResult {
    /// Echoed request identifier; absent when ingress JSON is malformed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    /// Request topic verb.
    pub verb: Verb,
    /// Whether the operation succeeded.
    pub ok: bool,
    /// Failure detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    /// Device time for ping responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_us: Option<u64>,
    /// Echoed ping value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub echo: Option<String<32>>,
}

/// OTA lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OtaState {
    /// Streaming image into inactive partition.
    Receiving,
    /// Checking image digest and board identity.
    Verifying,
    /// Switching boot partition and restarting.
    Rebooting,
    /// Update failed.
    Failed,
    /// New image reached MQTT and was confirmed.
    Confirmed,
    /// Bootloader restored the previous image.
    RolledBack,
}

/// OTA lifecycle notification on `ota`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtaProgress {
    /// Original OTA request identifier.
    pub id: RequestId,
    /// Current phase.
    pub state: OtaState,
    /// Download percentage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pct: Option<u8>,
    /// Target or running firmware version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fw: Option<String<24>>,
    /// Failure detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
}
