use aether_proto::Config;
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T, E = Error> = std::result::Result<T, E>;

pub struct Store {
    pub cfg: EspDefaultNvs,
    pub secrets: EspDefaultNvs,
    pub ota: std::sync::Arc<std::sync::Mutex<EspDefaultNvs>>,
    pub boot: u32,
}
impl Store {
    pub fn open() -> Result<Self> {
        let partition = match EspDefaultNvsPartition::take_with(false) {
            Ok(partition) => partition,
            Err(error)
                if matches!(
                    error.code(),
                    esp_idf_svc::sys::ESP_ERR_NVS_NO_FREE_PAGES
                        | esp_idf_svc::sys::ESP_ERR_NVS_NEW_VERSION_FOUND
                ) =>
            {
                log::warn!("nvs_reinitialize");
                crate::ffi::erase_nvs()?;
                EspDefaultNvsPartition::take_with(false)?
            }
            Err(error) => return Err(error.into()),
        };
        let system = EspDefaultNvs::new(partition.clone(), "system", true)?;
        let boot = system.get_u32("boot")?.unwrap_or(0).wrapping_add(1);
        system.set_u32("boot", boot)?;
        install_panic_record(system)?;
        Ok(Self {
            cfg: EspDefaultNvs::new(partition.clone(), "cfg", true)?,
            ota: std::sync::Arc::new(std::sync::Mutex::new(EspDefaultNvs::new(
                partition.clone(),
                "ota",
                true,
            )?)),
            secrets: EspDefaultNvs::new(partition, "secrets", true)?,
            boot,
        })
    }
    pub fn load_config(&self) -> Result<Config> {
        let loaded = (|| -> Result<Config> {
            let cfg = match read_json(&self.cfg, "json")? {
                Some(bytes) => serde_json::from_slice(&bytes)?,
                None => aether_logic::config::defaults(),
            };
            aether_logic::config::core_config(&cfg)?;
            Ok(cfg)
        })();
        Ok(loaded.unwrap_or_else(|_| {
            log::warn!("stored_cfg_invalid");
            aether_logic::config::defaults()
        }))
    }
    pub fn save_config(&self, cfg: &Config) -> Result<()> {
        self.cfg.set_blob("json", &serde_json::to_vec(cfg)?)?;
        Ok(())
    }
}
pub fn read_json(nvs: &EspDefaultNvs, key: &str) -> Result<Option<Vec<u8>>> {
    let Some(len) = nvs.blob_len(key)? else {
        return Ok(None);
    };
    if len > 4096 {
        return Err("NVS object exceeds bound".into());
    }
    let mut buf = vec![0; len];
    nvs.get_blob(key, &mut buf)?.ok_or("NVS key disappeared")?;
    Ok(Some(buf))
}

/// The previous boot's panic message, if any, is logged once and cleared; the
/// hook stores the next one. Lets a rolled-back OTA image be diagnosed from
/// the surviving image's `/api/log` without a serial cable.
fn install_panic_record(system: EspDefaultNvs) -> Result<()> {
    let mut buf = [0_u8; 160];
    if let Ok(Some(previous)) = system.get_str("panic", &mut buf) {
        log::error!("last_panic {previous}");
        system.remove("panic")?;
    }
    let system = std::sync::Mutex::new(system);
    std::panic::set_hook(Box::new(move |info| {
        let text = info.to_string();
        let cut = text.char_indices().nth(150).map_or(text.len(), |(i, _)| i);
        if let Ok(system) = system.try_lock() {
            let _ = system.set_str("panic", &text[..cut]);
        }
        println!("{text}");
    }));
    Ok(())
}
