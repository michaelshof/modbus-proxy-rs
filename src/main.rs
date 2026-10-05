mod config;
mod device;
mod mbap;
mod metrics;

use anyhow::{Context, Result};
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "modbus-proxy-rs",
    about = "Modbus TCP proxy that multiplexes multiple clients onto one upstream connection per device"
)]
struct Cli {
    /// Path to YAML config file
    #[arg(
        short,
        long,
        env = "CONFIG_PATH",
        default_value = "config.yaml"
    )]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::Config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;

    let filter = EnvFilter::try_new(&cfg.log_level)
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    info!(
        devices = cfg.devices.len(),
        timeout_ms = cfg.timeout_ms,
        "starting modbus-proxy-rs"
    );

    if cfg.metrics.is_some() {
        let handle = metrics::install_recorder()?;
        let bind: SocketAddr = cfg
            .metrics
            .as_ref()
            .unwrap()
            .bind
            .parse()
            .expect("metrics.bind validated at load");
        tokio::spawn(async move {
            if let Err(e) = metrics::serve(bind, handle).await {
                error!(error = %e, "metrics server stopped");
            }
        });
    }

    let mut handles = Vec::with_capacity(cfg.devices.len());
    for device in cfg.devices {
        let timeout_ms = cfg.timeout_ms;
        handles.push(tokio::spawn(async move {
            device::spawn_device(device, timeout_ms).await;
        }));
    }

    // Wait for all device tasks (they run until fatal error).
    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}
