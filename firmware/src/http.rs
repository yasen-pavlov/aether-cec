use crate::{config::Result, events::Hub, ota};
use aether_logic::http::{self as policy, Route};
use aether_proto::{Healthz, HttpError, OtaAccepted, OtaProgress, Verb};
use esp_idf_svc::{
    handle::RawHandle,
    http::{
        Method,
        server::{Configuration, EspHttpConnection, EspHttpServer, Request},
    },
    io::{EspIOError, Write},
    ota::EspOta,
};
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

pub struct Operation {
    pub verb: Option<Verb>,
    pub data: Vec<u8>,
    pub reply: SyncSender<(u16, Vec<u8>)>,
}
pub struct Shared {
    pub token: Mutex<String>,
    pub status: Mutex<aether_proto::Status>,
    pub cfg: Mutex<aether_proto::Config>,
    pub hub: Arc<Hub>,
    pub busy: AtomicBool,
    pub reboot: Mutex<Option<Instant>>,
    pub progress: SyncSender<OtaProgress>,
    pub storage: ota::Storage,
    pub boot: u32,
    pub operations: SyncSender<Operation>,
}
pub struct Http {
    pub shared: Arc<Shared>,
    pub operations: Receiver<Operation>,
    pub progress: Receiver<OtaProgress>,
    _sse: crate::ffi::SseRegistration,
    _server: EspHttpServer<'static>,
}
/// Constructor inputs. One struct argument: on the bench, passing these
/// six values as separate by-value parameters delivered a garbage `hub`
/// pointer (0xa5a5a5a5 stack fill) with the esp Rust 1.97 toolchain, i.e. a
/// caller/callee stack-argument mismatch on Xtensa. Keep call signatures small.
pub struct Init<'a> {
    pub token: &'a str,
    pub cfg: &'a aether_proto::Config,
    pub initial_status: aether_proto::Status,
    pub boot: u32,
    pub storage: ota::Storage,
    pub hub: Arc<Hub>,
}
impl Http {
    pub fn new(init: Init<'_>) -> Result<Self> {
        let Init {
            token,
            cfg,
            initial_status,
            boot,
            storage,
            hub,
        } = init;
        let (operations, requests) = mpsc::sync_channel(4);
        let (progress, events) = mpsc::sync_channel(16);
        let shared = Arc::new(Shared {
            token: Mutex::new(token.into()),
            status: Mutex::new(initial_status),
            cfg: Mutex::new(cfg.clone()),
            hub,
            busy: AtomicBool::new(false),
            reboot: Mutex::new(None),
            progress,
            storage,
            boot,
            operations,
        });
        let mut server = EspHttpServer::new(&Configuration {
            http_port: 80,
            max_open_sockets: 4,
            lru_purge_enable: true,
            uri_match_wildcard: true,
            max_uri_handlers: 16,
            stack_size: 12288,
            ..Default::default()
        })?;
        let sse = crate::ffi::register_sse(server.handle(), shared.clone())?;
        for method in [
            Method::Get,
            Method::Post,
            Method::Put,
            Method::Delete,
            Method::Patch,
            Method::Options,
            Method::Head,
        ] {
            let state = shared.clone();
            server.fn_handler("/*", method, move |request| handle(request, method, &state))?;
        }
        Ok(Self {
            shared,
            operations: requests,
            progress: events,
            _sse: sse,
            _server: server,
        })
    }
    pub fn snapshot(&self, status: &aether_proto::Status, cfg: &aether_proto::Config) {
        *self.shared.status.lock().unwrap() = status.clone();
        let mut current = self.shared.cfg.lock().unwrap();
        if *current != *cfg {
            *current = cfg.clone();
        }
    }
}
type Req<'a, 'b> = Request<&'a mut EspHttpConnection<'b>>;
fn response(
    req: Req<'_, '_>,
    code: u16,
    content: &str,
    bytes: &[u8],
) -> std::result::Result<(), EspIOError> {
    req.into_response(
        code,
        None,
        &[("Content-Type", content), ("Cache-Control", "no-store")],
    )?
    .write_all(bytes)
}
fn error(req: Req<'_, '_>, reason: &str) -> std::result::Result<(), EspIOError> {
    let body = HttpError {
        ok: false,
        reason: reason.try_into().unwrap(),
    };
    response(
        req,
        policy::error_status(reason),
        "application/json",
        &serde_json::to_vec(&body).unwrap(),
    )
}
fn handle(
    req: Req<'_, '_>,
    method: Method,
    state: &Arc<Shared>,
) -> std::result::Result<(), EspIOError> {
    let name = match method {
        Method::Get => "GET",
        Method::Put => "PUT",
        Method::Post => "POST",
        _ => "",
    };
    let route = policy::route(name, req.uri().split('?').next().unwrap_or(""));
    if !policy::authorized(
        route,
        req.header("Authorization"),
        &state.token.lock().unwrap(),
    ) {
        return error(req, "unauthorized");
    }
    let Some(route) = route else {
        return error(req, "not_found");
    };
    match route {
        Route::Index => response(
            req,
            200,
            "text/html; charset=utf-8",
            include_str!("index.html").as_bytes(),
        ),
        Route::Health => response(
            req,
            200,
            "application/json",
            &serde_json::to_vec(&Healthz {
                online: true,
                fw: crate::FW.try_into().unwrap(),
                boot: state.boot,
            })
            .unwrap(),
        ),
        Route::Status => response(
            req,
            200,
            "application/json",
            &serde_json::to_vec(&*state.status.lock().unwrap()).unwrap(),
        ),
        Route::Config => response(
            req,
            200,
            "application/json",
            &serde_json::to_vec(&*state.cfg.lock().unwrap()).unwrap(),
        ),
        Route::Log => response(
            req,
            200,
            "application/json",
            &serde_json::to_vec(crate::events::logs().lock().unwrap().entries()).unwrap(),
        ),
        Route::Ota => upload(req, state),
        Route::Command(verb) => dispatch(req, state, Some(verb)),
        Route::Tx => dispatch(req, state, None),
        Route::Events => error(req, "not_found"), // exact route is registered by the native asynchronous handler
    }
}
fn dispatch(
    mut req: Req<'_, '_>,
    state: &Shared,
    verb: Option<Verb>,
) -> std::result::Result<(), EspIOError> {
    let Some(length) = req
        .header("Content-Length")
        .and_then(|h| h.parse::<usize>().ok())
    else {
        return error(req, "malformed");
    };
    if length > 2048 {
        return error(req, "too_large");
    }
    let mut data = vec![0; length];
    let mut received = 0;
    while received < length {
        let count = req.read(&mut data[received..])?;
        if count == 0 {
            return error(req, "malformed");
        }
        received += count;
    }
    let (reply, rx) = mpsc::sync_channel(1);
    if state
        .operations
        .try_send(Operation { verb, data, reply })
        .is_err()
    {
        return error(req, "busy");
    }
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok((code, bytes)) => {
            let result = response(req, code, "application/json", &bytes);
            if code == 200 && matches!(verb, Some(Verb::Restart | Verb::Reset)) {
                *state.reboot.lock().unwrap() = Some(Instant::now());
            }
            result
        }
        Err(_) => error(req, "busy"),
    }
}
fn upload(mut req: Req<'_, '_>, state: &Shared) -> std::result::Result<(), EspIOError> {
    if crate::ffi::running_pending() || state.busy.swap(true, Ordering::AcqRel) {
        return error(req, "busy");
    }
    let result = receive(&mut req, state);
    match result {
        Ok(id) => {
            log::info!("ota_accepted");
            let result = response(
                req,
                202,
                "application/json",
                &serde_json::to_vec(&OtaAccepted { id }).unwrap(),
            );
            // The full image and reply have been sent; main waits another 500 ms.
            *state.reboot.lock().unwrap() = Some(Instant::now());
            result
        }
        Err(reason) => {
            log::info!("ota_failed");
            state.busy.store(false, Ordering::Release);
            error(req, reason)
        }
    }
}
fn receive(
    req: &mut Req<'_, '_>,
    state: &Shared,
) -> std::result::Result<aether_proto::RequestId, &'static str> {
    let (slot, size) = crate::ffi::update_slot().ok_or("storage_error")?;
    let (length, expected) = policy::ota_headers(
        req.header("Content-Length").and_then(|h| h.parse().ok()),
        req.header("X-Sha256"),
        size,
    )?;
    let id: aether_proto::RequestId = {
        let nvs = state.storage.lock().unwrap();
        let next = nvs
            .get_u32("next")
            .map_err(|_| "storage_error")?
            .unwrap_or(0)
            .wrapping_add(1);
        nvs.set_u32("next", next).map_err(|_| "storage_error")?;
        format!("o-{next}").as_str().try_into().unwrap()
    };
    let mut progress = policy::UploadProgress::new(id.clone(), length);
    let emit = |event| {
        let _ = state.progress.send(event);
    };
    emit(progress.received(0).unwrap());
    let result = (|| {
        let mut ota = EspOta::new().map_err(|_| "busy")?;
        let mut update = ota.initiate_update().map_err(|_| "storage_error")?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 4096];
        let mut received = 0;
        while received < length {
            let want = buffer.len().min(length - received);
            let count = req.read(&mut buffer[..want]).map_err(|_| "malformed")?;
            if count == 0 {
                return Err("malformed");
            }
            update
                .write(&buffer[..count])
                .map_err(|_| "storage_error")?;
            hasher.update(&buffer[..count]);
            received += count;
            if let Some(event) = progress.received(count) {
                emit(event);
            }
        }
        emit(progress.verifying());
        if hasher.finalize().as_slice() != expected {
            return Err("sha256_mismatch");
        }
        if !crate::ffi::update_project() {
            return Err("project_mismatch");
        }
        // Persist before selecting the boot slot: loss of power can never leave an untracked update.
        let pending = ota::Pending {
            id: id.clone(),
            slot,
        };
        let nvs = state.storage.lock().unwrap();
        nvs.set_blob(
            "pending",
            &serde_json::to_vec(&pending).map_err(|_| "storage_error")?,
        )
        .map_err(|_| "storage_error")?;
        if update.complete().is_err() {
            let _ = nvs.remove("pending");
            return Err("malformed");
        }
        drop(nvs);
        log::info!("ota_verified");
        Ok(())
    })();
    emit(progress.finished(result));
    result?;
    Ok(id)
}
