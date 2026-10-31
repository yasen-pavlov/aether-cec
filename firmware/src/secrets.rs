use crate::config::{Result, Store, read_json};
use serde::{Deserialize, Serialize};

// No Debug: no credentials can accidentally enter a log record.
#[derive(Default, Serialize, Deserialize)]
pub struct Secrets {
    pub wifi: Option<Wifi>,
    pub mqtt: Option<Mqtt>,
}
#[derive(Serialize, Deserialize)]
pub struct Wifi {
    pub ssid: String,
    pub pass: String,
}
#[derive(Serialize, Deserialize)]
pub struct Mqtt {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    pub tls: bool,
}
impl Secrets {
    pub fn load(store: &Store) -> Result<Self> {
        let loaded = (|| -> Result<Self> {
            let value: Self = read_json(&store.secrets, "json")?.map_or_else(
                || Ok::<_, crate::config::Error>(Self::default()),
                |bytes| Ok(serde_json::from_slice(&bytes)?),
            )?;
            if value
                .wifi
                .as_ref()
                .is_some_and(|wifi| !aether_logic::console::valid_wifi(&wifi.ssid, &wifi.pass))
                || value.mqtt.as_ref().is_some_and(|mqtt| {
                    !aether_logic::console::valid_mqtt(
                        &mqtt.host, mqtt.port, &mqtt.user, &mqtt.pass,
                    )
                })
            {
                return Err("invalid_stored_secrets".into());
            }
            Ok(value)
        })();
        Ok(loaded.unwrap_or_else(|_| {
            log::warn!("stored_secrets_invalid");
            Self::default()
        }))
    }
    // Called only by the serial-console dispatcher.
    pub fn save(&self, store: &Store) -> Result<()> {
        store.secrets.set_blob("json", &serde_json::to_vec(self)?)?;
        Ok(())
    }
}
