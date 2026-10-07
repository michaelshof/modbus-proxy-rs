mod config;
mod device;
mod mbap;
mod metrics;
mod shutdown;

use anyhow::{Context, Result};
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
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
        gap_ms = cfg.gap_ms,
        "starting modbus-proxy-rs"
    );

    let cancel = CancellationToken::new();

    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        match shutdown::wait_for_shutdown().await {
            Ok(()) => info!("shutdown signal received"),
            Err(e) => error!(error = %e, "failed to listen for shutdown signals"),
        }
        signal_cancel.cancel();
    });

    let mut handles = Vec::new();

    if cfg.metrics.is_some() {
        let handle = metrics::install_recorder()?;
        let bind: SocketAddr = cfg
            .metrics
            .as_ref()
            .unwrap()
            .bind
            .parse()
            .expect("metrics.bind validated at load");
        let metrics_cancel = cancel.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = metrics::serve(bind, handle, metrics_cancel).await {
                error!(error = %e, "metrics server stopped");
            }
        }));
    }

    for device in cfg.devices {
        let timeout_ms = device.timeout_ms.unwrap_or(cfg.timeout_ms);
        let gap_ms = device.gap_ms.unwrap_or(cfg.gap_ms);
        let device_cancel = cancel.clone();
        handles.push(tokio::spawn(async move {
            device::spawn_device(device, timeout_ms, gap_ms, device_cancel).await;
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }

    info!("modbus-proxy-rs stopped");
    Ok(())
}
