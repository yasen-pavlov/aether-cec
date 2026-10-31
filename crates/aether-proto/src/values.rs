//! Allocation-free encodings shared by the wire payloads.

use core::fmt::{self, Write};
use heapless::{String, Vec};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// A host-assigned request identifier, bounded in UTF-8 bytes.
pub type RequestId = String<16>;
/// A diagnostic or validation reason.
pub type Reason = String<32>;

const DIGITS: &[u8; 16] = b"0123456789ABCDEF";

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode<E: de::Error>(text: &str, bytes: &mut [u8]) -> Result<(), E> {
    if text.len() != bytes.len() * 2 {
        return Err(E::custom("incorrect hex length"));
    }
    for (pair, byte) in text.as_bytes().chunks_exact(2).zip(bytes) {
        let high = nibble(pair[0]).ok_or_else(|| E::custom("invalid hex digit"))?;
        let low = nibble(pair[1]).ok_or_else(|| E::custom("invalid hex digit"))?;
        *byte = high << 4 | low;
    }
    Ok(())
}

fn encode<const N: usize>(bytes: &[u8]) -> Result<String<N>, fmt::Error> {
    let mut text = String::new();
    for &byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]))
            .map_err(|_| fmt::Error)?;
        text.push(char::from(DIGITS[usize::from(byte & 15)]))
            .map_err(|_| fmt::Error)?;
    }
    Ok(text)
}

/// Opcode and operands encoded as an uppercase, separator-free hexadecimal string.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hex(pub Vec<u8, 15>);

impl Serialize for Hex {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let text = encode::<30>(&self.0).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = Hex;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("hex data containing 0..15 bytes")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<Hex, E> {
                if text.len() > 30 || text.len() % 2 != 0 {
                    return Err(E::custom("hex data must contain 0..15 bytes"));
                }
                let mut bytes = Vec::new();
                bytes
                    .resize(text.len() / 2, 0)
                    .map_err(|_| E::custom("hex data too long"))?;
                decode::<E>(text, &mut bytes)?;
                Ok(Hex(bytes))
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

/// Three-byte vendor identifiers represented in wire order as six hex digits.
pub mod vendor {
    use super::{Deserializer, Serializer, de, decode, encode, fmt};

    /// Serialize a vendor identifier in uppercase wire order.
    pub fn serialize<S: Serializer>(value: &[u8; 3], serializer: S) -> Result<S::Ok, S::Error> {
        let text = encode::<6>(value).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&text)
    }

    /// Parse a case-insensitive, exactly six-digit vendor identifier.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 3], D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = [u8; 3];
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a six-digit vendor identifier")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                let mut bytes = [0; 3];
                decode::<E>(text, &mut bytes)?;
                Ok(bytes)
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

/// A physical address, rendered as four hexadecimal nibbles separated by dots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalAddress(pub u16);

impl Serialize for PhysicalAddress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut text = String::<7>::new();
        write!(
            text,
            "{:X}.{:X}.{:X}.{:X}",
            self.0 >> 12,
            (self.0 >> 8) & 15,
            (self.0 >> 4) & 15,
            self.0 & 15
        )
        .map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for PhysicalAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = PhysicalAddress;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a physical address a.b.c.d")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                let bytes = text.as_bytes();
                if bytes.len() != 7 || bytes[1] != b'.' || bytes[3] != b'.' || bytes[5] != b'.' {
                    return Err(E::custom("physical address must be a.b.c.d"));
                }
                let mut address = 0;
                for index in [0, 2, 4, 6] {
                    address = address << 4
                        | u16::from(
                            nibble(bytes[index])
                                .ok_or_else(|| E::custom("invalid physical address nibble"))?,
                        );
                }
                Ok(PhysicalAddress(address))
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

/// One opcode rendered as two hex digits (used by the local reply list).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opcode(pub u8);

impl Serialize for Opcode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let text = encode::<2>(&[self.0]).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for Opcode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = Opcode;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a two-digit opcode")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                let mut byte = [0];
                decode::<E>(text, &mut byte)?;
                Ok(Opcode(byte[0]))
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

/// Device role used for logical-address allocation and Report Physical Address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceType {
    /// Television.
    Tv,
    /// Playback device.
    Playback,
    /// Audio system.
    Audio,
    /// Recording device.
    Recorder,
    /// Tuner.
    Tuner,
}

/// Transmit outcome, including validation failures that never reached the bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxStatus {
    /// Final block acknowledged.
    Ack,
    /// A block was negatively acknowledged.
    Nack,
    /// Arbitration lost.
    ArbLost,
    /// Collision or unexpected line level.
    LineError,
    /// Queue full or bus never became free.
    Busy,
    /// Request validation failed.
    Rejected,
}

impl From<cec_core::frame::TxStatus> for TxStatus {
    fn from(value: cec_core::frame::TxStatus) -> Self {
        match value {
            cec_core::frame::TxStatus::Ack => Self::Ack,
            cec_core::frame::TxStatus::Nack => Self::Nack,
            cec_core::frame::TxStatus::ArbLost => Self::ArbLost,
            cec_core::frame::TxStatus::LineError => Self::LineError,
            cec_core::frame::TxStatus::Busy => Self::Busy,
        }
    }
}
