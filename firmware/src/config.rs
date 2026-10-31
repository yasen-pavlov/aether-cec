use aether_proto::Config;
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T, E = Error> = std::result::Result<T, E>;

pub struct Store {
    pub cfg: EspDefaultNvs,
    pub secrets: EspDefaultNvs,
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
        Ok(Self {
            cfg: EspDefaultNvs::new(partition.clone(), "cfg", true)?,
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
