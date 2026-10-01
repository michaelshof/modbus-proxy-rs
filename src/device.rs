use crate::config::DeviceConfig;
use crate::mbap::{self, MbapError};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{error, info, warn};

/// Shared upstream connection for one device. Requests are serialized via the mutex.
struct Upstream {
    remote: String,
    stream: Option<TcpStream>,
}

impl Upstream {
    fn new(remote: String) -> Self {
        Self {
            remote,
            stream: None,
        }
    }

    async fn ensure_connected(&mut self) -> Result<&mut TcpStream> {
        if self.stream.is_none() {
            info!(remote = %self.remote, "connecting to upstream");
            let stream = TcpStream::connect(&self.remote)
                .await
                .with_context(|| format!("failed to connect to {}", self.remote))?;
            stream.set_nodelay(true)?;
            self.stream = Some(stream);
        }
        Ok(self.stream.as_mut().unwrap())
    }

    fn invalidate(&mut self) {
        self.stream = None;
    }

    async fn exchange(&mut self, request: &[u8]) -> Result<bytes::BytesMut, MbapError> {
        // First attempt; reconnect once on failure.
        for attempt in 0..2 {
            if attempt > 0 {
                self.invalidate();
            }
            let result = async {
                let stream = self
                    .ensure_connected()
                    .await
                    .map_err(|e| {
                        MbapError::Io(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            e.to_string(),
                        ))
                    })?;

                let (reader, writer) = stream.split();
                let mut writer = BufWriter::new(writer);
                let mut reader = BufReader::new(reader);

                mbap::write_adu(&mut writer, request).await?;
                mbap::read_adu(&mut reader).await
            }
            .await;

            match result {
                Ok(response) => return Ok(response),
                Err(e) => {
                    warn!(
                        remote = %self.remote,
                        attempt,
                        error = %e,
                        "upstream exchange failed"
                    );
                    self.invalidate();
                    if attempt == 1 {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!()
    }
}

pub async fn run_device(device: DeviceConfig, timeout_ms: u64) -> Result<()> {
    let listener = TcpListener::bind(&device.bind)
        .await
        .with_context(|| format!("failed to bind {} for device '{}'", device.bind, device.name))?;

    info!(
        device = %device.name,
        bind = %device.bind,
        remote = %device.remote,
        "listening"
    );

    let upstream = Arc::new(Mutex::new(Upstream::new(device.remote.clone())));
    let timeout_dur = Duration::from_millis(timeout_ms);
    let device_name = device.name.clone();

    loop {
        let (client, peer) = listener.accept().await?;
        client.set_nodelay(true)?;
        let upstream = Arc::clone(&upstream);
        let device_name = device_name.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_client(client, peer, upstream, timeout_dur, &device_name).await {
                // Closed connections are normal; log others at warn.
                match e.downcast_ref::<MbapError>() {
                    Some(MbapError::Closed) => {
                        tracing::debug!(device = %device_name, peer = %peer, "client disconnected");
                    }
                    _ => {
                        warn!(device = %device_name, peer = %peer, error = %e, "client session ended");
                    }
                }
            }
        });
    }
}

async fn handle_client(
    client: TcpStream,
    peer: std::net::SocketAddr,
    upstream: Arc<Mutex<Upstream>>,
    timeout_dur: Duration,
    device_name: &str,
) -> Result<()> {
    info!(device = %device_name, peer = %peer, "client connected");

    let (reader, writer) = client.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    loop {
        let request = match mbap::read_adu(&mut reader).await {
            Ok(frame) => frame,
            Err(MbapError::Closed) => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        let response = {
            let mut up = upstream.lock().await;
            match timeout(timeout_dur, up.exchange(&request)).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => {
                    // Drop lock and propagate; client will see session end.
                    return Err(e.into());
                }
                Err(_) => {
                    up.invalidate();
                    return Err(anyhow::anyhow!(
                        "upstream request timed out after {:?}",
                        timeout_dur
                    ));
                }
            }
        };

        if let Err(e) = mbap::write_adu(&mut writer, &response).await {
            return Err(e.into());
        }
    }
}

/// Run forever; on fatal listener error, log and return.
pub async fn spawn_device(device: DeviceConfig, timeout_ms: u64) {
    let name = device.name.clone();
    if let Err(e) = run_device(device, timeout_ms).await {
        error!(device = %name, error = %e, "device proxy stopped");
    }
}
