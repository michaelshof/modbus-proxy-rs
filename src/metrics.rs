use anyhow::{Context, Result};
use axum::routing::get;
use axum::Router;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::net::SocketAddr;
use tokio_util::sync::CancellationToken;
use tracing::info;

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
        get(move || {
            let handle = handle.clone();
            async move { handle.render() }
        }),
    );

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind metrics endpoint on {bind}"))?;

    info!(%bind, "metrics endpoint listening at http://{bind}/metrics");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            cancel.cancelled().await;
        })
        .await
        .context("metrics HTTP server error")?;

    Ok(())
}
