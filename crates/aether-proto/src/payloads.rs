//! Status, ordered bus events, and diagnostics.

use crate::{DeviceType, Hex, PhysicalAddress, Reason, RequestId, TxStatus};
use heapless::String;
use serde::{Deserialize, Serialize};

/// Current electrical health of the CEC line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusState {
    /// Line is operating normally.
    Ok,
    /// Line remains low without a valid frame.
    StuckLow,
    /// Line was low at boot (pull-up missing).
    NoPullup,
}

/// Hardware reset cause, grouped independently of ESP-IDF's numeric values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetReason {
    /// Initial application of power.
    PowerOn,
    /// Explicit software restart.
    Software,
    /// CPU exception or panic.
    Panic,
    /// Interrupt watchdog.
    IntWdt,
    /// Task watchdog.
    TaskWdt,
    /// Supply voltage fell below the brownout threshold.
    Brownout,
    /// Any other or unknown reset cause.
    Other,
}

/// Bounded previous-boot exception summary; addresses are symbolised on the host.
pub type LastCrash = String<120>;

/// Previous boot's last startup checkpoint and its uptime, from RTC memory;
/// covers crashes that left no core dump.
pub type LastStage = String<32>;

/// Retained status. Only `online` is present in the offline/LWT payload.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Status {
    /// Whether the bridge is connected.
    pub online: bool,
    /// Protocol version.
    #[serde(default)]
    pub v: Option<u32>,
    /// Firmware version.
    #[serde(default)]
    pub fw: Option<String<24>>,
    /// Persistent boot counter.
    #[serde(default)]
    pub boot: Option<u32>,
    /// Cause of this boot; absent in older status payloads and the LWT.
    #[serde(default)]
    pub reset_reason: Option<ResetReason>,
    /// Previous panic/watchdog core dump, if recovered on this boot.
    #[serde(default)]
    pub last_crash: Option<LastCrash>,
    /// Previous boot's last reached startup stage, present after a panic or
    /// watchdog reset even when no core dump was written.
    #[serde(default)]
    pub last_stage: Option<LastStage>,
    /// Monotonic microseconds since boot.
    #[serde(default)]
    pub t_us: Option<u64>,
    /// Allocated logical address, null while allocation is pending.
    #[serde(default)]
    pub la: Option<u8>,
    /// Effective physical address.
    #[serde(default)]
    pub pa: Option<PhysicalAddress>,
    /// CEC device role.
    #[serde(default, rename = "type")]
    pub device_type: Option<DeviceType>,
    /// IP address, bounded for either IPv4 or IPv6 text.
    #[serde(default)]
    pub ip: Option<String<45>>,
    /// Wireless signal strength in dBm.
    #[serde(default)]
    pub rssi: Option<i16>,
    /// Bus health, null in online status until the line has been measured.
    #[serde(default)]
    pub bus: Option<BusState>,
    /// HPD level, null if the GPIO is not configured.
    #[serde(default)]
    pub hpd: Option<bool>,
}

impl Serialize for Status {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut object = serializer.serialize_map(None)?;
        object.serialize_entry("online", &self.online)?;
        if let Some(value) = self.v {
            object.serialize_entry("v", &value)?;
        }
        if let Some(value) = &self.fw {
            object.serialize_entry("fw", value)?;
        }
        if let Some(value) = self.boot {
            object.serialize_entry("boot", &value)?;
        }
        if let Some(value) = self.reset_reason {
            object.serialize_entry("reset_reason", &value)?;
        }
        if let Some(value) = &self.last_crash {
            object.serialize_entry("last_crash", value)?;
        }
        if let Some(value) = &self.last_stage {
            object.serialize_entry("last_stage", value)?;
        }
        if let Some(value) = self.t_us {
            object.serialize_entry("t_us", &value)?;
        }
        // Online status must distinguish pending allocation / absent HPD from
        // the minimal offline last-will object.
        if self.online || self.la.is_some() {
            object.serialize_entry("la", &self.la)?;
        }
        if let Some(value) = self.pa {
            object.serialize_entry("pa", &value)?;
        }
        if let Some(value) = self.device_type {
            object.serialize_entry("type", &value)?;
        }
        if let Some(value) = &self.ip {
            object.serialize_entry("ip", value)?;
        }
        if let Some(value) = self.rssi {
            object.serialize_entry("rssi", &value)?;
        }
        if self.online || self.bus.is_some() {
            object.serialize_entry("bus", &self.bus)?;
        }
        if self.online || self.hpd.is_some() {
            object.serialize_entry("hpd", &self.hpd)?;
        }
        object.end()
    }
}

/// A host request to transmit a frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxRequest {
    /// Host-assigned correlation identifier.
    pub id: RequestId,
    /// Destination logical address.
    pub to: u8,
    /// Opcode and operands, empty for a poll.
    pub data: Hex,
    /// NACK retransmissions; absent means zero for polls, one otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u8>,
    /// Bench-only initiator override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<u8>,
}

/// Lenient, allocation-free ingress for host transmit requests.
///
/// Extra capacity preserves invalid values long enough to return a correlated
/// rejection. Invalid JSON, wrong field types, and strings beyond these bounds
/// must be reported by the executor as `rejected`, no id, `reason: malformed`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct RawTxRequest {
    /// Raw correlation identifier (validated to sixteen UTF-8 bytes).
    pub id: String<32>,
    /// Raw destination; validated before narrowing.
    pub to: u64,
    /// Raw data, including malformed or oversized hex text.
    pub data: String<64>,
    /// Optional NACK retry budget, clamped to the total-attempt limit.
    #[serde(default)]
    pub retries: Option<u64>,
    /// Optional bench-only initiator override.
    #[serde(default)]
    pub from: Option<u64>,
}

impl RawTxRequest {
    /// Validate in order: id, destination, data length, hex, override, own LA.
    ///
    /// # Errors
    /// Returns the bounded id (absent only for `id_too_long`) and wire reason.
    pub fn validate(
        &self,
        cfg: &cec_core::bridge::Config,
    ) -> Result<cec_core::bridge::Submission, (Option<RequestId>, Reason)> {
        let id =
            RequestId::try_from(self.id.as_str()).map_err(|_| (None, reason("id_too_long")))?;
        let reject = |text| (Some(id.clone()), reason(text));
        let to = u8::try_from(self.to)
            .ok()
            .filter(|&to| to <= cec_core::frame::BROADCAST)
            .ok_or_else(|| reject("bad_to"))?;
        if self.data.len() > 30 {
            return Err(reject("data_too_long"));
        }
        let data = Hex::deserialize(
            serde::de::value::StrDeserializer::<serde::de::value::Error>::new(&self.data),
        )
        .map_err(|_| reject("data_not_hex"))?;
        let from = match self.from {
            Some(from) if cfg.allow_from => Some(
                u8::try_from(from)
                    .ok()
                    .filter(|&from| from < cec_core::frame::BROADCAST)
                    .ok_or_else(|| reject("from_not_allowed"))?,
            ),
            Some(_) => return Err(reject("from_not_allowed")),
            None => None,
        };
        let la = cfg
            .la
            .filter(|&la| la < cec_core::frame::BROADCAST)
            .ok_or_else(|| reject("no_la"))?;
        Ok(cec_core::bridge::Submission {
            id,
            frame: cec_core::frame::Frame {
                from: from.unwrap_or(la),
                to,
                data: data.0,
            },
            retries: self.retries.map(|retries| {
                u8::try_from(retries)
                    .unwrap_or(u8::MAX)
                    .min(cec_core::timing::MAX_ATTEMPTS - 1)
            }),
        })
    }

    /// Data for a rejected event; omit text that exceeds the wire's fixed buffer.
    pub fn rejected_data(&self) -> Option<TxData> {
        TxData::deserialize(
            serde::de::value::StrDeserializer::<serde::de::value::Error>::new(&self.data),
        )
        .ok()
    }
}

fn reason(text: &str) -> Reason {
    Reason::try_from(text).expect("static rejection reason fits")
}

/// Source of a transmit request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Submitted by a host.
    Host,
    /// Generated locally by the bridge.
    Local,
}

/// Correlation with the received frame that triggered a local reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyTo {
    /// Sequence number of the triggering receive event.
    pub seq: u32,
}

/// Sent bytes, or the original malformed text of a rejected request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TxData {
    /// Valid CEC bytes.
    Hex(Hex),
    /// Rejected raw input, which cannot be represented by `Hex`.
    Rejected(String<30>),
}

/// Receive decoding failure or event-buffer overrun.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Invalid start-bit timing.
    StartBit,
    /// Invalid data-bit timing.
    BitTiming,
    /// Missing EOM within sixteen blocks.
    EomMissing,
    /// A low pulse exceeded the frame-level timeout.
    LineLow,
    /// Events were dropped from a bounded queue.
    Overrun,
}

/// Electrical level reported by a decoder error's failing edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Bus asserted.
    Low,
    /// Bus released.
    High,
}

/// One object from the single ordered bus stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum BusEvent {
    /// Complete frame observed on the bus.
    Rx {
        /// Per-boot sequence number.
        seq: u32,
        /// Start-bit falling edge, microseconds since boot.
        t_us: u64,
        /// Initiator logical address.
        from: u8,
        /// Destination logical address.
        to: u8,
        /// Opcode and operands.
        data: Hex,
        /// Observed acknowledgement of the last block.
        ack: bool,
    },
    /// Final transmission outcome, including rejected requests.
    Tx {
        /// Per-boot sequence number.
        seq: u32,
        /// Start of the final attempt, or validation time.
        t_us: u64,
        /// Host correlation identifier; absent for local replies or unavailable input ids.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<RequestId>,
        /// Host or local reply.
        origin: Origin,
        /// Initiator; unknown if validation failed before choosing it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u8>,
        /// Requested destination; omitted for rejected invalid destinations.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<u8>,
        /// Sent bytes or malformed rejected input; omitted if rejected input is too long.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<TxData>,
        /// Final classification.
        status: TxStatus,
        /// Start bits sent; absent for rejected or busy requests.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempts: Option<u8>,
        /// Validation failure detail.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<Reason>,
        /// Triggering receive event for a local reply.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<ReplyTo>,
    },
    /// Receive failure or dropped events.
    Err {
        /// Per-boot sequence number.
        seq: u32,
        /// Detection time.
        t_us: u64,
        /// Error classification.
        kind: ErrorKind,
        /// Initiator, if decoded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u8>,
        /// Destination, if decoded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<u8>,
        /// Data bits decoded before failure.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bits: Option<u8>,
        /// Events lost to overrun.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dropped: Option<u32>,
        /// Failing edge's level; absent for poll timeouts and overruns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<Level>,
        /// Whether the failing edge repeated the previous edge's level.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        same_level: Option<bool>,
        /// Failed pulse/period or timeout in microseconds, saturated to `u32`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dur_us: Option<u32>,
        /// Time since the previous edge; zero for poll timeouts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gap_us: Option<u32>,
        /// Block position: data 0–7, EOM 8, ACK 9; absent during start.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bit: Option<u8>,
        /// Up to eight preceding inter-edge intervals, oldest first.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hist: Option<heapless::Vec<u32, 8>>,
        /// GPIO input sampled by firmware when emitting the error.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pin: Option<Level>,
        /// Age of the bus task's last High GPIO read, saturated to `u32`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pin_high_age_us: Option<u32>,
        /// Age of the most recent follower ACK, at most `STUCK_LOW`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ack_age_us: Option<u32>,
        /// Assertion-to-release duration; absent while the ACK remains held.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ack_held_us: Option<u32>,
        /// LACT masked interrupt status; presence marks a coherent timer capture.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tmr_int: Option<bool>,
        /// LACT alarm enable bit at the capture.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tmr_en: Option<bool>,
        /// Hardware alarm minus capture time, microseconds saturated to `i32`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tmr_alarm_in_us: Option<i32>,
        /// Cached ISR deadline minus capture time; absent for `UINT64_MAX`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tmr_isr_in_us: Option<i32>,
        /// Cached TASK deadline minus capture time; absent for `UINT64_MAX`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tmr_task_in_us: Option<i32>,
        /// ACK assertion plus `ACK_LOW` minus capture time, saturated to `i32`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ack_due_in_us: Option<i32>,
    },
    /// Electrical state change or completed logical-address allocation.
    Line {
        /// Per-boot sequence number.
        seq: u32,
        /// Detection time.
        t_us: u64,
        /// New line health, if changed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bus: Option<BusState>,
        /// New HPD level, if changed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hpd: Option<bool>,
        /// Allocated logical address; 15 means no address was free.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        la: Option<u8>,
    },
}

/// Per-boot receive counters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RxCounters {
    /// Complete frames received.
    pub frames: u64,
    /// Decode errors.
    pub errors: u64,
    /// Frames acknowledged by this bridge.
    pub acked_by_us: u64,
    /// Follower ACK holds longer than `DATA_LOW_0.max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_late: Option<u64>,
    /// Longest completed follower ACK hold this boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_held_max_us: Option<u32>,
    /// GPIO edges dropped because the ISR ring was full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edges_lost: Option<u64>,
}

/// Per-boot transmit counters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxCounters {
    /// Host requests.
    pub host: u64,
    /// Local replies.
    pub local: u64,
    /// Acknowledged outcomes.
    pub ack: u64,
    /// Negatively acknowledged outcomes.
    pub nack: u64,
    /// Failed arbitration attempts, including attempts followed by success.
    pub arb_lost: u64,
    /// Failed line-error attempts, including attempts followed by success.
    pub line_error: u64,
    /// Line-error attempts without an observed block origin.
    #[serde(default)]
    pub origin_missing: u64,
    /// Line-error attempts with a missing or late sample.
    #[serde(default)]
    pub sample_late: u64,
    /// Line-error attempts rejected by launch/queueing checks or RMT.
    #[serde(default)]
    pub launch_rejected: u64,
    /// Line-error attempts with a malformed own echo.
    #[serde(default)]
    pub echo_error: u64,
    /// Line-error attempts that lost GPIO edges.
    #[serde(default)]
    pub edge_overrun: u64,
    /// Line-error attempts with a driven zero sampled high.
    #[serde(default)]
    pub driven_low_mismatch: u64,
    /// Busy outcomes.
    pub busy: u64,
    /// Validation failures.
    pub rejected: u64,
    /// Additional bus attempts.
    pub retransmits: u64,
}

/// Current and maximum observed queue depth.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueCounters {
    /// Requests currently queued.
    pub depth: u8,
    /// High-water mark.
    pub max: u8,
}

/// Periodic per-boot counters and runtime health.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Telemetry {
    /// Monotonic microseconds since boot.
    pub t_us: u64,
    /// Boot counter.
    pub boot: u32,
    /// Wi-Fi signal strength in dBm.
    pub rssi: i16,
    /// MQTT connections this boot.
    pub link_count: u32,
    /// Free heap in bytes.
    pub heap_free: u32,
    /// Minimum free heap in bytes.
    pub heap_min: u32,
    /// Largest contiguous 8-bit heap block; absent in older firmware.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heap_largest: Option<u32>,
    /// Receive counters.
    pub rx: RxCounters,
    /// Transmit counters.
    pub tx: TxCounters,
    /// Request queue counters.
    pub queue: QueueCounters,
    /// Events dropped from the MQTT outbox.
    pub outbox_dropped: u64,
}

/// Configured logging threshold or the severity of a log record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Disable log publication.
    Off,
    /// Errors only.
    Error,
    /// Warnings.
    Warn,
    /// Informational messages.
    Info,
    /// Debug messages.
    Debug,
}

/// Optional bounded diagnostic record; never includes credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    /// Monotonic microseconds since boot.
    pub t_us: u64,
    /// Severity.
    pub lvl: LogLevel,
    /// Human-readable diagnostic (up to 160 UTF-8 bytes).
    pub msg: String<160>,
}

/// Unauthenticated HTTP liveness response (no provisioning information).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Healthz {
    /// Application is running.
    pub online: bool,
    /// Running firmware identifier.
    pub fw: String<24>,
    /// Persistent boot count.
    pub boot: u32,
    /// Cause of this boot; absent when reading older firmware.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_reason: Option<ResetReason>,
}

/// Uniform HTTP error body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpError {
    /// Always false.
    pub ok: bool,
    /// Static, bounded error code.
    pub reason: crate::Reason,
}

/// Accepted push update identifier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtaAccepted {
    /// Bridge-assigned identifier persisted across reboot.
    pub id: crate::RequestId,
}
