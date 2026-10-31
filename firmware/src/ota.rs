use crate::{
    config::{Result, Store, read_json},
    ffi,
};
use aether_logic::http::{BootAction, BootKind, boot_action, boot_kind};
use aether_proto::{OtaProgress, OtaState, RequestId};
use esp_idf_svc::{
    nvs::EspDefaultNvs,
    timer::{EspTaskTimerService, EspTimer},
};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
pub struct Pending {
    pub id: RequestId,
    pub slot: u32,
}
pub struct Health {
    started: Instant,
    kind: BootKind,
    pending: Option<Pending>,
    done: bool,
    report: Option<OtaState>,
    rollback_timer: Option<EspTimer<'static>>,
}
impl Health {
    pub fn new() -> Self {
        // Bootloader truth must be captured before console/NVS/network startup.
        let kind = boot_kind(ffi::running_pending(), ffi::running_slot(), None);
        let started = Instant::now();
        let rollback_timer = (kind == BootKind::Pending).then(|| {
            let timer = EspTaskTimerService::new()
                .and_then(|service| {
                    service.timer(|| {
                        // Query bootloader truth again: confirmation may race this callback.
                        if ffi::running_pending() {
                            ffi::restart();
                        }
                    })
                })
                .and_then(|timer| {
                    timer.after(Duration::from_secs(60))?;
                    Ok(timer)
                });
            // Never run a pending image without the independent rollback deadline.
            timer.unwrap_or_else(|_| ffi::restart())
        });
        Self {
            started,
            kind,
            pending: None,
            done: kind == BootKind::Valid,
            report: None,
            rollback_timer,
        }
    }
    pub fn load(&mut self, store: &Store) -> Result<()> {
        self.pending = read_json(&store.ota.lock().unwrap(), "pending")?
            .map(|bytes| serde_json::from_slice(&bytes))
            .transpose()?;
        self.kind = boot_kind(
            self.kind == BootKind::Pending,
            ffi::running_slot(),
            self.pending.as_ref().map(|pending| pending.slot),
        );
        self.done = self.kind == BootKind::Valid;
        if self.done && self.pending.is_some() {
            // A power cut after mark-valid can leave an already-confirmed record.
            store.ota.lock().unwrap().remove("pending")?;
            self.pending = None;
        }
        Ok(())
    }
    pub fn poll(
        &mut self,
        published: bool,
        store: Option<&Store>,
        mut emit: impl FnMut(&OtaProgress) -> bool,
    ) {
        if self.done {
            return;
        }
        match boot_action(self.kind, published, self.started.elapsed().as_secs()) {
            BootAction::Wait => {}
            BootAction::Rollback => ffi::restart(), // rollback-enabled bootloader rejects PENDING_VERIFY on reset
            BootAction::ConfirmUpdate => {
                if !ffi::confirm_app() {
                    return;
                }
                self.rollback_timer = None;
                self.kind = BootKind::Valid;
                self.report = Some(OtaState::Confirmed);
                // A failed/missing NVS record never prevents safe confirmation.
                if self.pending.is_none() {
                    self.done = true;
                }
            }
            BootAction::ReportRollback => self.report = Some(OtaState::RolledBack),
        }
        if let (Some(state), Some(pending), Some(store)) = (self.report, &self.pending, store) {
            if emit(&progress(&pending.id, state, None, None))
                && store.ota.lock().unwrap().remove("pending").is_ok()
            {
                self.done = true;
            }
        }
    }
}
pub fn progress(
    id: &RequestId,
    state: OtaState,
    pct: Option<u8>,
    reason: Option<&str>,
) -> OtaProgress {
    OtaProgress {
        id: id.clone(),
        state,
        pct,
        fw: matches!(state, OtaState::Confirmed | OtaState::RolledBack)
            .then(|| crate::FW.try_into().unwrap()),
        reason: reason.map(|r| r.try_into().unwrap()),
    }
}
pub type Storage = Arc<Mutex<EspDefaultNvs>>;
