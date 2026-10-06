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
    /// Pause after each upstream exchange before the next request. Default 0.
    #[serde(default)]
    pub gap_ms: u64,
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
    /// Overrides global `timeout_ms` when set.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Overrides global `gap_ms` when set.
    #[serde(default)]
    pub gap_ms: Option<u64>,
}

fn default_timeout_ms() -> u64 {
    3000
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Config {
    pub fn parse(yaml: &str) -> Result<Self> {
        let config: Config = serde_yaml::from_str(yaml).context("failed to parse config YAML")?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        Self::parse(&contents)
            .with_context(|| format!("failed to parse config file {}", path.display()))
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
            if matches!(device.timeout_ms, Some(0)) {
                bail!(
                    "timeout_ms must be greater than 0 for device '{}'",
                    device.name
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

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_yaml() -> &'static str {
        r#"
devices:
  - name: inverter-1
    bind: "0.0.0.0:5020"
    remote: "192.168.1.10:502"
  - name: meter-1
    bind: "127.0.0.1:5021"
    remote: "192.168.1.11:502"
"#
    }

    #[test]
    fn parses_valid_yaml_with_defaults() {
        let cfg = Config::parse(valid_yaml()).unwrap();
        assert_eq!(cfg.devices.len(), 2);
        assert_eq!(cfg.devices[0].name, "inverter-1");
        assert_eq!(cfg.timeout_ms, 3000);
        assert_eq!(cfg.gap_ms, 0);
        assert!(cfg.devices[0].timeout_ms.is_none());
        assert!(cfg.devices[0].gap_ms.is_none());
        assert_eq!(cfg.log_level, "info");
        assert!(cfg.metrics.is_none());
    }

    #[test]
    fn accepts_hostname_remote() {
        let yaml = r#"
devices:
  - name: inverter-1
    bind: "0.0.0.0:5020"
    remote: "device.example:502"
"#;
        let cfg = Config::parse(yaml).unwrap();
        assert_eq!(cfg.devices[0].remote, "device.example:502");
    }

    #[test]
    fn rejects_empty_devices() {
        let err = Config::parse("devices: []\n").unwrap_err();
        assert!(err.to_string().contains("at least one device"));
    }

    #[test]
    fn rejects_empty_device_name() {
        let yaml = r#"
devices:
  - name: "  "
    bind: "0.0.0.0:5020"
    remote: "192.168.1.10:502"
"#;
        let err = Config::parse(yaml).unwrap_err();
        assert!(err.to_string().contains("name must not be empty"));
    }

    #[test]
    fn rejects_invalid_bind() {
        let yaml = r#"
devices:
  - name: inverter-1
    bind: "not-an-address"
    remote: "192.168.1.10:502"
"#;
        let err = Config::parse(yaml).unwrap_err();
        assert!(err.to_string().contains("invalid bind address"));
    }

    #[test]
    fn rejects_remote_missing_port() {
        let yaml = r#"
devices:
  - name: inverter-1
    bind: "0.0.0.0:5020"
    remote: "192.168.1.10"
"#;
        let err = Config::parse(yaml).unwrap_err();
        assert!(err.to_string().contains("invalid remote"));
    }

    #[test]
    fn device_timeout_and_gap_override_globals() {
        let yaml = r#"
devices:
  - name: inverter-1
    bind: "0.0.0.0:5020"
    remote: "192.168.1.10:502"
    timeout_ms: 5000
    gap_ms: 100
timeout_ms: 3000
gap_ms: 50
"#;
        let cfg = Config::parse(yaml).unwrap();
        assert_eq!(cfg.devices[0].timeout_ms, Some(5000));
        assert_eq!(cfg.devices[0].gap_ms, Some(100));
        assert_eq!(cfg.devices[0].timeout_ms.unwrap_or(cfg.timeout_ms), 5000);
        assert_eq!(cfg.devices[0].gap_ms.unwrap_or(cfg.gap_ms), 100);
    }

    #[test]
    fn global_gap_applies_when_device_omits_gap() {
        let yaml = format!("{}\ngap_ms: 50\n", valid_yaml());
        let cfg = Config::parse(&yaml).unwrap();
        assert_eq!(cfg.gap_ms, 50);
        assert!(cfg.devices[0].gap_ms.is_none());
        assert_eq!(cfg.devices[0].gap_ms.unwrap_or(cfg.gap_ms), 50);
    }

    #[test]
    fn rejects_zero_device_timeout() {
        let yaml = r#"
devices:
  - name: inverter-1
    bind: "0.0.0.0:5020"
    remote: "192.168.1.10:502"
    timeout_ms: 0
"#;
        let err = Config::parse(yaml).unwrap_err();
        assert!(err.to_string().contains("timeout_ms"));
    }

    #[test]
    fn rejects_zero_timeout() {
        let yaml = format!("{}\ntimeout_ms: 0\n", valid_yaml());
        let err = Config::parse(&yaml).unwrap_err();
        assert!(err.to_string().contains("timeout_ms"));
    }

    #[test]
    fn rejects_invalid_metrics_bind() {
        let yaml = format!("{}\nmetrics:\n  bind: \"nope\"\n", valid_yaml());
        let err = Config::parse(&yaml).unwrap_err();
        assert!(err.to_string().contains("metrics.bind"));
    }

    #[test]
    fn accepts_metrics_bind() {
        let yaml = format!("{}\nmetrics:\n  bind: \"127.0.0.1:9090\"\n", valid_yaml());
        let cfg = Config::parse(&yaml).unwrap();
        assert_eq!(cfg.metrics.unwrap().bind, "127.0.0.1:9090");
    }
}
