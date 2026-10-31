use crate::{config::Result, secrets::Wifi};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::modem::Modem,
    wifi::{AuthMethod, ClientConfiguration, Configuration, EspWifi},
};
use std::time::{Duration, Instant};

pub struct Net {
    pub wifi: EspWifi<'static>,
    configured: bool,
    last_connect: Instant,
}
impl Net {
    pub fn new(
        modem: Modem<'static>,
        sysloop: EspSystemEventLoop,
        credentials: Option<&Wifi>,
    ) -> Result<Self> {
        // No Wi-Fi SDK NVS: secrets exist only in our `secrets` namespace.
        let mut wifi = EspWifi::new(modem, sysloop, None)?;
        let configured = credentials.is_some_and(|credentials| {
            let configured = (|| -> Result<()> {
                let config = ClientConfiguration {
                    ssid: credentials
                        .ssid
                        .as_str()
                        .try_into()
                        .map_err(|_| "bad_ssid")?,
                    password: credentials
                        .pass
                        .as_str()
                        .try_into()
                        .map_err(|_| "bad_wifi_pass")?,
                    auth_method: if credentials.pass.is_empty() {
                        AuthMethod::None
                    } else {
                        AuthMethod::WPA2Personal
                    },
                    ..Default::default()
                };
                wifi.set_configuration(&Configuration::Client(config))?;
                wifi.start()?;
                Ok(())
            })();
            if configured.is_err() {
                log::warn!("wifi_config_failed");
                false
            } else {
                if wifi.connect().is_err() {
                    log::warn!("wifi_connect_pending");
                }
                true
            }
        });
        Ok(Self {
            wifi,
            configured,
            last_connect: Instant::now(),
        })
    }
    pub fn poll(&mut self) {
        if self.configured
            && !self.wifi.is_connected().unwrap_or(false)
            && self.last_connect.elapsed() >= Duration::from_secs(5)
        {
            // ESP_ERR_WIFI_CONN while an attempt is pending is harmless.
            let _ = self.wifi.connect();
            self.last_connect = Instant::now();
        }
    }
    pub fn ip(&self) -> String {
        self.wifi
            .sta_netif()
            .get_ip_info()
            .map_or_else(|_| "0.0.0.0".into(), |info| info.ip.to_string())
    }
    pub fn rssi(&self) -> i16 {
        self.wifi
            .get_ap_info()
            .map_or(0, |ap| i16::from(ap.signal_strength))
    }
}
