use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub devices: Vec<DeviceConfig>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Opt-in Prometheus scrape endpoint. Omit to disable.
    #[serde(default)]
    pub metrics: Option<MetricsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MetricsConfig {
    pub bind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceConfig {
    pub name: String,
    pub bind: String,
    pub remote: String,
}

fn default_timeout_ms() -> u64 {
    3000
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let config: Config = serde_yaml::from_str(&contents)
            .with_context(|| format!("failed to parse config file {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.devices.is_empty() {
            bail!("config must define at least one device");
        }
        for device in &self.devices {
            if device.name.trim().is_empty() {
                bail!("device name must not be empty");
            }
            device
                .bind
                .parse::<SocketAddr>()
                .with_context(|| format!("invalid bind address for device '{}'", device.name))?;
            // remote may be a hostname; validate port portion only loosely via last ':'
            if !device.remote.contains(':') {
                bail!(
                    "invalid remote address for device '{}': expected host:port",
                    device.name
                );
            }
            let port = device
                .remote
                .rsplit_once(':')
                .map(|(_, p)| p)
                .unwrap_or_default();
            if port.parse::<u16>().is_err() {
                bail!(
                    "invalid remote port for device '{}': '{}'",
                    device.name,
                    device.remote
                );
            }
        }
        if self.timeout_ms == 0 {
            bail!("timeout_ms must be greater than 0");
        }
        if let Some(metrics) = &self.metrics {
            metrics
                .bind
                .parse::<SocketAddr>()
                .with_context(|| format!("invalid metrics.bind address '{}'", metrics.bind))?;
        }
        Ok(())
    }
}
