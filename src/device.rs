use crate::config::DeviceConfig;
use crate::mbap::{self, MbapError};
use anyhow::{Context, Result};
use metrics::{counter, gauge, histogram};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// How long to wait for client tasks to finish after cancel before aborting them.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(7);

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

async fn apply_gap(gap_ms: u64) {
    if gap_ms > 0 {
        tokio::time::sleep(Duration::from_millis(gap_ms)).await;
    }
}

pub async fn run_device(
    device: DeviceConfig,
    timeout_ms: u64,
    gap_ms: u64,
    cancel: CancellationToken,
) -> Result<()> {
    let listener = TcpListener::bind(&device.bind)
        .await
        .with_context(|| format!("failed to bind {} for device '{}'", device.bind, device.name))?;

    info!(
        device = %device.name,
        bind = %device.bind,
        remote = %device.remote,
        timeout_ms,
        gap_ms,
        "listening"
    );

    serve_listener(listener, device, timeout_ms, gap_ms, cancel).await
}

async fn serve_listener(
    listener: TcpListener,
    device: DeviceConfig,
    timeout_ms: u64,
    gap_ms: u64,
    cancel: CancellationToken,
) -> Result<()> {
    gauge!("modbus_proxy_upstream_connected", "device" => device.name.clone()).set(0.0);
    gauge!("modbus_proxy_clients", "device" => device.name.clone()).set(0.0);

    let upstream = Arc::new(Mutex::new(Upstream::new(
        device.name.clone(),
        device.remote.clone(),
    )));
    let timeout_dur = Duration::from_millis(timeout_ms);
    let device_name = device.name.clone();
    let mut clients = JoinSet::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!(device = %device_name, "shutdown: stop accepting clients");
                break;
            }
            // Reap finished clients so JoinSet does not retain them until shutdown.
            Some(_) = clients.join_next() => {}
            accepted = listener.accept() => {
                let (client, peer) = accepted?;
                client.set_nodelay(true)?;
                let upstream = Arc::clone(&upstream);
                let device_name = device_name.clone();
                let client_cancel = cancel.child_token();

                clients.spawn(async move {
                    if let Err(e) = handle_client(
                        client,
                        peer,
                        upstream,
                        timeout_dur,
                        gap_ms,
                        &device_name,
                        client_cancel,
                    )
                    .await
                    {
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
    }

    // Drop the listener so the bind port is released while clients drain.
    drop(listener);

    if timeout(SHUTDOWN_DRAIN, async {
        while clients.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        warn!(
            device = %device_name,
            "shutdown: aborting remaining client tasks after {:?}",
            SHUTDOWN_DRAIN
        );
        clients.abort_all();
        while clients.join_next().await.is_some() {}
    }

    info!(device = %device_name, "shutdown complete");
    Ok(())
}

async fn handle_client(
    client: TcpStream,
    peer: std::net::SocketAddr,
    upstream: Arc<Mutex<Upstream>>,
    timeout_dur: Duration,
    gap_ms: u64,
    device_name: &str,
    cancel: CancellationToken,
) -> Result<()> {
    let _client_guard = ClientGuard::new(device_name);
    info!(device = %device_name, peer = %peer, "client connected");

    let (reader, writer) = client.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    loop {
        let request = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = mbap::read_adu(&mut reader) => match result {
                Ok(frame) => frame,
                Err(MbapError::Closed) => return Ok(()),
                Err(e) => {
                    record_error(device_name, error_reason(&e));
                    return Err(e.into());
                }
            },
        };

        let started = Instant::now();
        let response = {
            let mut up = upstream.lock().await;
            let result = match timeout(timeout_dur, up.exchange(&request)).await {
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
                    Ok(resp)
                }
                Ok(Err(e)) => {
                    record_error(device_name, error_reason(&e));
                    Err(anyhow::Error::from(e))
                }
                Err(_) => {
                    up.invalidate();
                    record_error(device_name, "timeout");
                    Err(anyhow::anyhow!(
                        "upstream request timed out after {:?}",
                        timeout_dur
                    ))
                }
            };
            apply_gap(gap_ms).await;
            result?
        };

        if let Err(e) = mbap::write_adu(&mut writer, &response).await {
            record_error(device_name, error_reason(&e));
            return Err(e.into());
        }
    }
}

/// Run until fatal listener error or shutdown cancel.
pub async fn spawn_device(
    device: DeviceConfig,
    timeout_ms: u64,
    gap_ms: u64,
    cancel: CancellationToken,
) {
    let name = device.name.clone();
    if let Err(e) = run_device(device, timeout_ms, gap_ms, cancel).await {
        error!(device = %name, error = %e, "device proxy stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::sleep;

    const READ_HOLDING: [u8; 12] = [
        0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x01, 0x03, 0x00, 0x00, 0x00, 0x01,
    ];

    fn request_with_tid(tid: u16) -> [u8; 12] {
        let mut req = READ_HOLDING;
        req[0..2].copy_from_slice(&tid.to_be_bytes());
        req
    }

    async fn echo_upstream(accepts: Arc<AtomicUsize>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accepts.fetch_add(1, Ordering::SeqCst);
            stream.set_nodelay(true).ok();
            let (reader, writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut writer = BufWriter::new(writer);
            loop {
                match mbap::read_adu(&mut reader).await {
                    Ok(frame) => {
                        if mbap::write_adu(&mut writer, &frame).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            // Reject extra upstream connections by dropping the listener without accept.
            drop(listener);
        });
        addr
    }

    async fn silent_upstream() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            sleep(Duration::from_secs(30)).await;
        });
        addr
    }

    async fn start_proxy(remote: SocketAddr, timeout_ms: u64) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let device = DeviceConfig {
            name: "test-1".to_string(),
            bind: addr.to_string(),
            remote: remote.to_string(),
            timeout_ms: None,
            gap_ms: None,
        };
        let cancel = CancellationToken::new();
        tokio::spawn(async move {
            let _ = serve_listener(listener, device, timeout_ms, 0, cancel).await;
        });
        addr
    }

    #[tokio::test]
    async fn two_clients_share_one_upstream() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let upstream = echo_upstream(Arc::clone(&accepts)).await;
        let proxy = start_proxy(upstream, 2000).await;

        let send = |tid: u16| async move {
            let mut client = TcpStream::connect(proxy).await.unwrap();
            client.set_nodelay(true).unwrap();
            let req = request_with_tid(tid);
            mbap::write_adu(&mut client, &req).await.unwrap();
            let resp = mbap::read_adu(&mut client).await.unwrap();
            assert_eq!(&resp[..], &req[..]);
        };

        tokio::join!(send(1), send(2));
        assert_eq!(accepts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn request_times_out_when_upstream_silent() {
        let upstream = silent_upstream().await;
        let proxy = start_proxy(upstream, 200).await;

        let mut client = TcpStream::connect(proxy).await.unwrap();
        client.set_nodelay(true).unwrap();
        mbap::write_adu(&mut client, &READ_HOLDING).await.unwrap();

        let result = timeout(Duration::from_secs(2), mbap::read_adu(&mut client)).await;
        match result {
            Ok(Err(_)) => {}
            Ok(Ok(_)) => panic!("expected timeout, got a response"),
            Err(_) => panic!("client did not see a closed connection after upstream timeout"),
        }
    }

    #[tokio::test]
    async fn cancel_stops_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let device = DeviceConfig {
            name: "shutdown-test".to_string(),
            bind: listener.local_addr().unwrap().to_string(),
            remote: "127.0.0.1:1".to_string(),
            timeout_ms: None,
            gap_ms: None,
        };
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let serve = tokio::spawn(async move {
            serve_listener(listener, device, 1000, 0, cancel_clone).await
        });

        // Let the accept loop start.
        sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        let result = timeout(Duration::from_secs(1), serve).await;
        let join = result.expect("serve_listener did not finish within 1s");
        join.expect("serve task panicked")
            .expect("serve_listener returned error");
    }
}
