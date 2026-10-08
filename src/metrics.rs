use anyhow::{Context, Result};
use axum::routing::get;
use axum::Router;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::net::SocketAddr;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Matches `metrics-exporter-prometheus` default upkeep interval.
const UPKEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Install the global Prometheus recorder and return a handle for rendering.
pub fn install_recorder() -> Result<PrometheusHandle> {
    PrometheusBuilder::new()
        .install_recorder()
        .context("failed to install Prometheus metrics recorder")
}

/// Serve `GET /metrics` on `bind` until `cancel` is cancelled.
pub async fn serve(
    bind: SocketAddr,
    handle: PrometheusHandle,
    cancel: CancellationToken,
) -> Result<()> {
    let app = Router::new().route(
        "/metrics",
        get({
            let handle = handle.clone();
            move || {
                let handle = handle.clone();
                async move { handle.render() }
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind metrics endpoint on {bind}"))?;

    info!(%bind, "metrics endpoint listening at http://{bind}/metrics");

    // `install_recorder` does not spawn upkeep; drain histogram samples periodically.
    let upkeep_handle = handle.clone();
    let upkeep_cancel = cancel.clone();
    let upkeep = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = upkeep_cancel.cancelled() => break,
                _ = tokio::time::sleep(UPKEEP_INTERVAL) => {
                    upkeep_handle.run_upkeep();
                }
            }
        }
    });

    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            cancel.cancelled().await;
        })
        .await
        .context("metrics HTTP server error");

    upkeep.abort();
    let _ = upkeep.await;

    result?;
    Ok(())
}
