use aether_logic::http::LogRing;
use aether_proto::{LogLevel, LogLine};
use std::sync::{
    Arc, LazyLock, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender},
};

struct Client {
    tx: SyncSender<Arc<str>>,
    alive: Arc<AtomicBool>,
}
pub struct Subscription {
    pub rx: Receiver<Arc<str>>,
    pub alive: Arc<AtomicBool>,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}
#[derive(Default)]
pub struct Hub {
    clients: Mutex<Vec<Client>>,
}
impl Hub {
    pub fn subscribe(&self) -> Option<Subscription> {
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|client| client.alive.load(Ordering::Acquire));
        if clients.len() >= 3 {
            return None;
        }
        let (tx, rx) = mpsc::sync_channel(16);
        let alive = Arc::new(AtomicBool::new(true));
        clients.push(Client {
            tx,
            alive: alive.clone(),
        });
        Some(Subscription { rx, alive })
    }
    pub fn publish(&self, topic: &str, bytes: &[u8]) {
        let suffix = topic.rsplit('/').next().unwrap_or(topic);
        if let Ok(payload) = std::str::from_utf8(bytes) {
            if let Some(line) = aether_logic::http::sse(suffix, payload) {
                let line: Arc<str> = line.into();
                self.clients.lock().unwrap().retain(|client| {
                    let keep = client.alive.load(Ordering::Acquire)
                        && client.tx.try_send(line.clone()).is_ok();
                    if !keep {
                        client.alive.store(false, Ordering::Release);
                    }
                    keep
                });
            }
        }
    }
}
static LOGS: LazyLock<Mutex<LogRing>> = LazyLock::new(Default::default);
pub fn logs() -> &'static Mutex<LogRing> {
    &LOGS
}
struct Logger;
static LOGGER: Logger = Logger;
impl log::Log for Logger {
    fn enabled(&self, meta: &log::Metadata<'_>) -> bool {
        meta.target().split("::").next() == Some("aether") && meta.level() <= log::Level::Info
    }
    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let t_us = crate::ffi::now_us();
        println!(
            "{} {t_us} {}: {}",
            record.level(),
            record.target(),
            record.args()
        );
        let lvl = match record.level() {
            log::Level::Error => LogLevel::Error,
            log::Level::Warn => LogLevel::Warn,
            _ => LogLevel::Info,
        };
        // Only this application's scrubbed codes; dependency diagnostics never enter the ring.
        let text = record.args().to_string();
        let mut entry = LogLine {
            t_us,
            lvl,
            msg: Default::default(),
        };
        for ch in text.chars() {
            if entry.msg.push(ch).is_err() {
                break;
            }
        }
        logs().lock().unwrap().push(entry);
    }
    fn flush(&self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}
pub fn initialize() {
    log::set_logger(&LOGGER).expect("logger initialized once");
    log::set_max_level(log::LevelFilter::Info);
}
