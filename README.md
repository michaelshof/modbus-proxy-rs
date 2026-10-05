# modbus-proxy-rs

Modbus TCP proxy that lets multiple clients share devices that only allow **one** active connection.

For each device in the config, the proxy:

1. Listens on a local `bind` address
2. Keeps a **single** TCP connection to the upstream `remote`
3. Serializes request/response pairs so concurrent clients are served one at a time

## Build & run

```bash
cp config.example.yaml config.yaml
# edit bind/remote addresses
cargo run --release -- --config config.yaml
```

Run tests with `cargo test`.

Or:

```bash
export CONFIG_PATH=./config.yaml
cargo run --release
```

## Configuration

YAML file (see `config.example.yaml`):

```yaml
devices:
  - name: inverter-1
    bind: "0.0.0.0:5020"
    remote: "192.168.1.10:502"
  - name: meter-1
    bind: "0.0.0.0:5021"
    remote: "192.168.1.11:502"

timeout_ms: 3000
log_level: info

# metrics:
#   bind: "0.0.0.0:9090"
```


| Field        | Description                                           |
| ------------ | ----------------------------------------------------- |
| `name`       | Label used in logs and as the Prometheus `device` label |
| `bind`       | Local listen address (`ip:port`)                      |
| `remote`     | Upstream Modbus TCP device (`host:port`)              |
| `timeout_ms` | Per-request upstream timeout (default `3000`)         |
| `log_level`  | Tracing filter, e.g. `info`, `debug` (default `info`) |
| `metrics`    | Optional Prometheus scrape config (see below)         |


CLI: `--config <path>` (env: `CONFIG_PATH`, default `config.yaml`).

## Metrics (Prometheus)

Opt-in. Add to the config to enable a scrape endpoint:

```yaml
metrics:
  bind: "0.0.0.0:9090"
```

Scrape URL: `http://<bind>/metrics`. Omit `metrics` entirely to disable.

Per-device series use the `device` label (config `name`):

| Metric | Type | Meaning |
|--------|------|---------|
| `modbus_proxy_requests_total` | counter | Successful upstream exchanges |
| `modbus_proxy_request_errors_total` | counter | Failed exchanges (`reason`: `timeout`, `upstream`, `protocol`) |
| `modbus_proxy_request_duration_seconds` | histogram | Upstream exchange latency |
| `modbus_proxy_clients` | gauge | Active client connections |
| `modbus_proxy_upstream_connected` | gauge | `1` if upstream socket is open |
| `modbus_proxy_upstream_reconnects_total` | counter | Upstream reconnects after disconnect |

Example Prometheus scrape config:

```yaml
scrape_configs:
  - job_name: modbus-proxy
    static_configs:
      - targets: ["127.0.0.1:9090"]
```

When using Docker, map the metrics port as well (e.g. add `9090:9090` under `ports`, or extend `HOST_PORTS` / `APP_PORTS` if you publish ranges). The systemd unit does not open ports by itself; ensure firewall rules allow the metrics bind if needed.

### Grafana dashboard

An importable dashboard is in [`grafana/modbus-proxy-rs.json`](grafana/modbus-proxy-rs.json):

1. Grafana → **Dashboards** → **New** → **Import**
2. Upload the JSON (or paste it)
3. Select your Prometheus datasource when prompted

The dashboard includes a `device` variable (multi-select / All) and panels for request rate, errors, latency, clients, upstream connectivity, and reconnects. Latency panels use summary quantiles (`quantile="0.95"`) and `_sum`/`_count` from the metrics exporter’s DDSketch output — not classic Prometheus `_bucket` histograms.

## Docker

```bash
cp .env.example .env
cp config.example.yaml config.yaml
# edit config.yaml bind/remote addresses
# edit .env port mappings to match
docker compose up --build -d
```

Port publishing is controlled by env vars in `.env` (see `.env.example`), used by `docker-compose.yml` as `${HOST_PORTS}:${APP_PORTS}`:

| Variable | Meaning |
| -------- | ------- |
| `HOST_PORTS` | Ports on the Docker host that clients connect to |
| `APP_PORTS` | Ports inside the container — must match each device `bind` port in `config.yaml` |

Example (two devices on 5020 and 5021):

```env
APP_PORTS=5020-5021
HOST_PORTS=5020-5021
```

You can map to different host ports if needed (e.g. `HOST_PORTS=15020-15021` with `APP_PORTS=5020-5021`). Keep the ranges the same length and aligned with the `bind` ports in your config.

Clients connect to the published host ports (e.g. `localhost:5020`). The container must be able to reach each upstream `remote` host. On Linux, `network_mode: host` in `docker-compose.yml` can simplify access to LAN devices (omit the `ports:` section if you use host networking).

CI builds a multi-arch image (`linux/amd64`, `linux/arm64`) and pushes it to GHCR on pushes to `main` and version tags (`v*`). Pull requests only build (no push).

Version tags (`v*`) also build release binaries (Linux amd64/arm64, macOS Intel/Apple Silicon, Windows amd64) and attach them to the GitHub Release for that tag.

## systemd

For a non-Docker install on Linux:

```bash
sudo install -m 755 target/release/modbus-proxy-rs /usr/local/bin/
# or install a release binary from GitHub Releases
sudo mkdir -p /etc/modbus-proxy-rs
sudo cp config.example.yaml /etc/modbus-proxy-rs/config.yaml
# edit /etc/modbus-proxy-rs/config.yaml
sudo useradd --system --no-create-home --shell /usr/sbin/nologin modbus-proxy
sudo cp systemd/modbus-proxy-rs.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now modbus-proxy-rs
```

Logs: `journalctl -u modbus-proxy-rs -f`

## How it works

Modbus TCP frames (MBAP + PDU) are parsed and forwarded as-is (unit ID and function codes unchanged). Only one request is in flight on the upstream socket at a time; other clients wait on a per-device mutex. If the upstream connection drops, the proxy reconnects on the next request.