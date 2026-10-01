# Multi-stage build for modbus-proxy-rs
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/modbus-proxy-rs /usr/local/bin/modbus-proxy-rs
ENV CONFIG_PATH=/config/config.yaml
ENTRYPOINT ["modbus-proxy-rs"]
