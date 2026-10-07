use anyhow::Result;
use tokio::signal;

/// Wait until SIGINT (Ctrl+C) or, on Unix, SIGTERM.
pub async fn wait_for_shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        signal::ctrl_c().await?;
    }

    Ok(())
}
