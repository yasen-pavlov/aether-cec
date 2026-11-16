//! Map decoding avoids serde's allocating internally-tagged enum buffer.

use crate::{
    BusEvent, BusState, ErrorKind, Hex, Origin, Reason, ReplyTo, RequestId, TxData, TxStatus,
};
use core::fmt;
use serde::{Deserialize, Deserializer, de};

impl<'de> Deserialize<'de> for TxData {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = TxData;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("hex data or rejected raw input, at most 30 bytes")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                if let Ok(bytes) =
                    Hex::deserialize(de::value::BorrowedStrDeserializer::<E>::new(text))
                {
                    return Ok(TxData::Hex(bytes));
                }
                heapless::String::try_from(text)
                    .map(TxData::Rejected)
                    .map_err(|_| E::custom("rejected data exceeds 30 bytes"))
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum EventKind {
    Rx,
    Tx,
    Err,
    Line,
}

// Deserialize the object in one pass regardless of the position of `ev`. Every
// buffer is bounded and unknown keys are consumed by serde's IgnoredAny visitor.
#[derive(Deserialize)]
struct Object {
    ev: EventKind,
    seq: u32,
    t_us: u64,
    #[serde(default)]
    from: Option<u8>,
    #[serde(default)]
    to: Option<u8>,
    #[serde(default)]
    data: Option<TxData>,
    #[serde(default)]
    ack: Option<bool>,
    #[serde(default)]
    id: Option<RequestId>,
    #[serde(default)]
    origin: Option<Origin>,
    #[serde(default)]
    status: Option<TxStatus>,
    #[serde(default)]
    attempts: Option<u8>,
    #[serde(default)]
    reason: Option<Reason>,
    #[serde(default)]
    reply_to: Option<ReplyTo>,
    #[serde(default)]
    kind: Option<ErrorKind>,
    #[serde(default)]
    bits: Option<u8>,
    #[serde(default)]
    dropped: Option<u32>,
    #[serde(default)]
    level: Option<crate::Level>,
    #[serde(default)]
    same_level: Option<bool>,
    #[serde(default)]
    dur_us: Option<u32>,
    #[serde(default)]
    gap_us: Option<u32>,
    #[serde(default)]
    bit: Option<u8>,
    #[serde(default)]
    hist: Option<heapless::Vec<u32, 8>>,
    #[serde(default)]
    pin: Option<crate::Level>,
    #[serde(default)]
    pin_high_age_us: Option<u32>,
    #[serde(default)]
    ack_age_us: Option<u32>,
    #[serde(default)]
    ack_held_us: Option<u32>,
    #[serde(default)]
    bus: Option<BusState>,
    #[serde(default)]
    hpd: Option<bool>,
    #[serde(default)]
    la: Option<u8>,
}

fn required<T, E: de::Error>(value: Option<T>, name: &'static str) -> Result<T, E> {
    value.ok_or_else(|| E::missing_field(name))
}

impl<'de> Deserialize<'de> for BusEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let object = Object::deserialize(deserializer)?;
        let Object { seq, t_us, .. } = object;
        if object.from.is_some_and(|address| address > 15)
            || object.to.is_some_and(|address| address > 15)
        {
            return Err(de::Error::custom("from and to must be in 0..15"));
        }
        match object.ev {
            EventKind::Rx => {
                let TxData::Hex(data) = required(object.data, "data")? else {
                    return Err(de::Error::custom("rx data must be hexadecimal"));
                };
                Ok(Self::Rx {
                    seq,
                    t_us,
                    from: required(object.from, "from")?,
                    to: required(object.to, "to")?,
                    data,
                    ack: required(object.ack, "ack")?,
                })
            }
            EventKind::Tx => {
                let status = required(object.status, "status")?;
                if status != TxStatus::Rejected {
                    required(object.to, "to")?;
                    if !matches!(object.data, Some(TxData::Hex(_))) {
                        return Err(de::Error::custom(
                            "non-rejected tx data must be hexadecimal",
                        ));
                    }
                }
                Ok(Self::Tx {
                    seq,
                    t_us,
                    id: object.id,
                    origin: required(object.origin, "origin")?,
                    from: object.from,
                    to: object.to,
                    data: object.data,
                    status,
                    attempts: object.attempts,
                    reason: object.reason,
                    reply_to: object.reply_to,
                })
            }
            EventKind::Err => Ok(Self::Err {
                seq,
                t_us,
                kind: required(object.kind, "kind")?,
                from: object.from,
                to: object.to,
                bits: object.bits,
                dropped: object.dropped,
                level: object.level,
                same_level: object.same_level,
                dur_us: object.dur_us,
                gap_us: object.gap_us,
                bit: object.bit,
                pin: object.pin,
                pin_high_age_us: object.pin_high_age_us,
                ack_age_us: object.ack_age_us,
                ack_held_us: object.ack_held_us,
                hist: object.hist,
            }),
            EventKind::Line => Ok(Self::Line {
                seq,
                t_us,
                bus: object.bus,
                hpd: object.hpd,
                la: object.la,
            }),
        }
    }
}
