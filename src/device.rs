use crate::config::DeviceConfig;
use crate::mbap::{self, MbapError};
use anyhow::{Context, Result};
use metrics::{counter, gauge, histogram};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{error, info, warn};

/// Shared upstream connection for one device. Requests are serialized via the mutex.
struct Upstream {
    device: String,
    remote: String,
    stream: Option<TcpStream>,
    /// True after invalidate when a connection previously existed.
    reconnect_pending: bool,
}

impl Upstream {
    fn new(device: String, remote: String) -> Self {
        Self {
            device,
            remote,
            stream: None,
            reconnect_pending: false,
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
            gauge!("modbus_proxy_upstream_connected", "device" => self.device.clone()).set(1.0);
            if self.reconnect_pending {
                counter!(
                    "modbus_proxy_upstream_reconnects_total",
                    "device" => self.device.clone()
                )
                .increment(1);
                self.reconnect_pending = false;
            }
        }
        Ok(self.stream.as_mut().unwrap())
    }

    fn invalidate(&mut self) {
        if self.stream.is_some() {
            self.reconnect_pending = true;
        }
        self.stream = None;
        gauge!("modbus_proxy_upstream_connected", "device" => self.device.clone()).set(0.0);
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

struct ClientGuard {
    device: String,
}

impl ClientGuard {
    fn new(device: &str) -> Self {
        gauge!("modbus_proxy_clients", "device" => device.to_string()).increment(1.0);
        Self {
            device: device.to_string(),
        }
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        gauge!("modbus_proxy_clients", "device" => self.device.clone()).decrement(1.0);
    }
}

fn record_error(device: &str, reason: &str) {
    counter!(
        "modbus_proxy_request_errors_total",
        "device" => device.to_string(),
        "reason" => reason.to_string()
    )
    .increment(1);
}

fn error_reason(err: &MbapError) -> &'static str {
    match err {
        MbapError::InvalidProtocolId(_)
        | MbapError::InvalidLength(_)
        | MbapError::TooLarge(_) => "protocol",
        MbapError::Closed | MbapError::Io(_) => "upstream",
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

    gauge!("modbus_proxy_upstream_connected", "device" => device.name.clone()).set(0.0);
    gauge!("modbus_proxy_clients", "device" => device.name.clone()).set(0.0);

    let upstream = Arc::new(Mutex::new(Upstream::new(
        device.name.clone(),
        device.remote.clone(),
    )));
    let timeout_dur = Duration::from_millis(timeout_ms);
    let device_name = device.name.clone();

    loop {
        let (client, peer) = listener.accept().await?;
        client.set_nodelay(true)?;
        let upstream = Arc::clone(&upstream);
        let device_name = device_name.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_client(client, peer, upstream, timeout_dur, &device_name).await {
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
    let _client_guard = ClientGuard::new(device_name);
    info!(device = %device_name, peer = %peer, "client connected");

    let (reader, writer) = client.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    loop {
        let request = match mbap::read_adu(&mut reader).await {
            Ok(frame) => frame,
            Err(MbapError::Closed) => return Ok(()),
            Err(e) => {
                record_error(device_name, error_reason(&e));
                return Err(e.into());
            }
        };

        let started = Instant::now();
        let response = {
            let mut up = upstream.lock().await;
            match timeout(timeout_dur, up.exchange(&request)).await {
                Ok(Ok(resp)) => {
                    let elapsed = started.elapsed().as_secs_f64();
                    histogram!(
                        "modbus_proxy_request_duration_seconds",
                        "device" => device_name.to_string()
                    )
                    .record(elapsed);
                    counter!(
                        "modbus_proxy_requests_total",
                        "device" => device_name.to_string()
                    )
                    .increment(1);
                    resp
                }
                Ok(Err(e)) => {
                    record_error(device_name, error_reason(&e));
                    return Err(e.into());
                }
                Err(_) => {
                    up.invalidate();
                    record_error(device_name, "timeout");
                    return Err(anyhow::anyhow!(
                        "upstream request timed out after {:?}",
                        timeout_dur
                    ));
                }
            }
        };

        if let Err(e) = mbap::write_adu(&mut writer, &response).await {
            record_error(device_name, error_reason(&e));
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
