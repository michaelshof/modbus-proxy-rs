use anyhow::{Context, Result};
use axum::routing::get;
use axum::Router;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::net::SocketAddr;
use tracing::info;

/// Install the global Prometheus recorder and return a handle for rendering.
pub fn install_recorder() -> Result<PrometheusHandle> {
    PrometheusBuilder::new()
        .install_recorder()
        .context("failed to install Prometheus metrics recorder")
}

/// Serve `GET /metrics` on `bind` until the process exits.
pub async fn serve(bind: SocketAddr, handle: PrometheusHandle) -> Result<()> {
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
        .await
        .context("metrics HTTP server error")?;

    Ok(())
}
