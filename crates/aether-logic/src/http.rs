//! Transport-neutral HTTP authentication, bounds, SSE and OTA boot policy.
use aether_proto::{LogLine, OtaProgress, OtaState, RequestId, Verb};
use std::collections::VecDeque;

/// Decode exactly 32 bytes of hexadecimal (case insensitive).
pub fn digest(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut output = [0; 32];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        output[index] = u8::try_from(high * 16 + low).ok()?;
    }
    Some(output)
}

/// Compare all 64 token bytes without an early differing-byte exit.
pub fn bearer(header: Option<&str>, token: &str) -> bool {
    let supplied = header
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    let mut difference = supplied.len() ^ token.len();
    for (index, expected) in token.bytes().enumerate() {
        difference |= usize::from(expected ^ supplied.as_bytes().get(index).copied().unwrap_or(0));
    }
    difference == 0 && token.len() == 64
}

/// An exact supported method/path pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Single embedded page.
    Index,
    /// Unauthenticated liveness.
    Health,
    /// Full status.
    Status,
    /// Read effective configuration.
    Config,
    /// Apply a command through the shared dispatcher.
    Command(Verb),
    /// Submit CEC request.
    Tx,
    /// Subscribe to live events.
    Events,
    /// Read bounded diagnostic history.
    Log,
    /// Push firmware.
    Ota,
}
/// Resolve routes; unsupported verbs and paths use `not_found`, never command fallback.
pub fn route(method: &str, path: &str) -> Option<Route> {
    match (method, path) {
        ("GET", "/") => Some(Route::Index),
        ("GET", "/healthz") => Some(Route::Health),
        ("GET", "/api/status") => Some(Route::Status),
        ("GET", "/api/cfg") => Some(Route::Config),
        ("PUT", "/api/cfg") => Some(Route::Command(Verb::Cfg)),
        ("POST", "/api/cmd/ping") => Some(Route::Command(Verb::Ping)),
        ("POST", "/api/cmd/state") => Some(Route::Command(Verb::State)),
        ("POST", "/api/cmd/restart") => Some(Route::Command(Verb::Restart)),
        ("POST", "/api/cmd/reset") => Some(Route::Command(Verb::Reset)),
        ("POST", "/api/tx") => Some(Route::Tx),
        ("GET", "/api/events") => Some(Route::Events),
        ("GET", "/api/log") => Some(Route::Log),
        ("POST", "/api/ota") => Some(Route::Ota),
        _ => None,
    }
}
/// Shared handler policy, including the native SSE handler and unknown routes.
pub fn authorized(route: Option<Route>, header: Option<&str>, token: &str) -> bool {
    matches!(route, Some(Route::Index | Route::Health)) || bearer(header, token)
}
/// HTTP status for the shared reason vocabulary.
pub fn error_status(reason: &str) -> u16 {
    match reason {
        "unauthorized" => 401,
        "not_found" => 404,
        "too_large" => 413,
        "busy" => 409,
        "storage_error" => 500,
        _ => 400,
    }
}
/// Validate upload headers before opening or erasing the inactive partition.
pub fn ota_headers(
    length: Option<usize>,
    sha256: Option<&str>,
    slot: usize,
) -> Result<(usize, [u8; 32]), &'static str> {
    let length = length.filter(|length| *length > 0).ok_or("malformed")?;
    let digest = sha256.and_then(digest).ok_or("malformed")?;
    if length > slot {
        return Err("too_large");
    }
    Ok((length, digest))
}
/// Format one SSE message; JSON must be the same single-line serialized payload as MQTT.
pub fn sse(topic: &str, payload: &str) -> Option<String> {
    if !matches!(topic, "bus" | "status" | "ota") || payload.contains(['\r', '\n']) {
        return None;
    }
    Some(format!("event: {topic}\ndata: {payload}\n\n"))
}
/// Diagnostic ring with a hard 64-entry bound.
#[derive(Default)]
pub struct LogRing(VecDeque<LogLine>);
impl LogRing {
    /// Append a scrubbed application log, evicting the oldest.
    pub fn push(&mut self, line: LogLine) {
        if self.0.len() == 64 {
            self.0.pop_front();
        }
        self.0.push_back(line);
    }
    /// Oldest-first view for JSON serialization.
    pub fn entries(&self) -> &VecDeque<LogLine> {
        &self.0
    }
}
/// Boot health input: the bootloader owns pending state, never the NVS record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootKind {
    /// Already valid (including the first USB/provisioning boot).
    Valid,
    /// Running the just-uploaded image.
    Pending,
    /// Bootloader has returned to the previous slot.
    RolledBack,
}
/// Side effects selected by the pure boot policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootAction {
    /// Not healthy yet.
    Wait,
    /// Confirm pending image, publish confirmed, then clear durable record.
    ConfirmUpdate,
    /// Publish `rolled_back`, then clear durable record; do not confirm failed slot.
    ReportRollback,
    /// Reset unconfirmed pending image to trigger bootloader rollback.
    Rollback,
}
/// A missing, unreadable or stale record cannot change bootloader pending state.
pub fn boot_kind(pending_verify: bool, running_slot: u32, record_slot: Option<u32>) -> BootKind {
    if pending_verify {
        BootKind::Pending
    } else if record_slot.is_some_and(|slot| slot != running_slot) {
        BootKind::RolledBack
    } else {
        BootKind::Valid
    }
}
/// Confirmation requires status publication, not just a TCP/MQTT connection.
pub fn boot_action(kind: BootKind, status_published: bool, elapsed_s: u64) -> BootAction {
    match kind {
        BootKind::Pending if status_published => BootAction::ConfirmUpdate,
        BootKind::Pending if elapsed_s >= 60 => BootAction::Rollback,
        BootKind::RolledBack if status_published => BootAction::ReportRollback,
        _ => BootAction::Wait,
    }
}

/// Main always owns the shutdown bound, even if an HTTP response times out.
pub fn shutdown_due(elapsed_ms: u128, offline_acked: bool) -> bool {
    elapsed_ms >= 2_000 || offline_acked
}

/// Upload progress policy shared by the streaming handler and host tests.
pub struct UploadProgress {
    id: RequestId,
    length: usize,
    received: usize,
    last_pct: Option<u8>,
}
impl UploadProgress {
    /// Headers have already validated a nonzero length that fits the slot.
    ///
    /// # Panics
    /// Panics if `length` is zero.
    pub fn new(id: RequestId, length: usize) -> Self {
        assert!(length > 0);
        Self {
            id,
            length,
            received: 0,
            last_pct: None,
        }
    }
    /// Advance after a successfully written chunk; suppress duplicate percentages.
    ///
    /// # Panics
    /// Panics if the cumulative count exceeds the validated image length.
    pub fn received(&mut self, count: usize) -> Option<OtaProgress> {
        self.received += count;
        assert!(self.received <= self.length);
        let pct = u8::try_from(self.received * 100 / self.length).unwrap();
        if self.last_pct == Some(pct) {
            return None;
        }
        self.last_pct = Some(pct);
        Some(self.progress(OtaState::Receiving, Some(pct), None))
    }
    /// Full body written: digest, project and IDF validation follow.
    ///
    /// # Panics
    /// Panics if the complete body has not been written.
    pub fn verifying(&self) -> OtaProgress {
        assert_eq!(self.received, self.length);
        self.progress(OtaState::Verifying, None, None)
    }
    /// Success means the durable record and new boot partition have been written.
    ///
    /// # Panics
    /// Panics if an error reason exceeds the protocol's 32-byte bound.
    pub fn finished(&self, result: Result<(), &'static str>) -> OtaProgress {
        match result {
            Ok(()) => self.progress(OtaState::Rebooting, None, None),
            Err(reason) => self.progress(OtaState::Failed, None, Some(reason)),
        }
    }
    fn progress(&self, state: OtaState, pct: Option<u8>, reason: Option<&str>) -> OtaProgress {
        OtaProgress {
            id: self.id.clone(),
            state,
            pct,
            fw: None,
            reason: reason.map(|reason| reason.try_into().unwrap()),
        }
    }
}
