# =============================================================================
# hc-wled — HomeCore WLED Plugin
# Alpine Linux — minimal, static-friendly runtime
# =============================================================================
#
# Build:
#   docker build -t hc-wled:latest .
#
# Run:
#   docker run -d \
#     -v ./config/config.toml:/opt/hc-wled/config/config.toml:ro \
#     -v hc-wled-logs:/opt/hc-wled/logs \
#     hc-wled:latest
#
# Volumes:
#   /opt/hc-wled/config   config.toml (WLED device IPs, credentials)
#   /opt/hc-wled/logs     rolling log files
# =============================================================================

# -----------------------------------------------------------------------------
# Stage 1 — Build
# -----------------------------------------------------------------------------
FROM rust:alpine AS builder

RUN apk upgrade --no-cache && apk add --no-cache musl-dev openssl-dev pkgconfig

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/

RUN cargo build --release --bin hc-wled

# -----------------------------------------------------------------------------
# Stage 2 — Runtime
# -----------------------------------------------------------------------------
FROM alpine:3

# `apk upgrade` first pulls CVE patches for packages baked into the
# alpine:3 base since the upstream image was last rebuilt. Defense
# in depth — without this, `apk add --no-cache` only refreshes the
# named packages, leaving busybox/musl/etc. on the base's frozen
# versions.
RUN apk upgrade --no-cache && \
    apk add --no-cache \
        ca-certificates \
        libssl3 \
        tzdata

RUN adduser -D -h /opt/hc-wled hcwled

COPY --from=builder /build/target/release/hc-wled /usr/local/bin/hc-wled
RUN chmod 755 /usr/local/bin/hc-wled

RUN mkdir -p /opt/hc-wled/config /opt/hc-wled/logs

COPY config/config.toml.example /opt/hc-wled/config/config.toml.example

RUN chown -R hcwled:hcwled /opt/hc-wled

USER hcwled
WORKDIR /opt/hc-wled

VOLUME ["/opt/hc-wled/config", "/opt/hc-wled/logs"]

ENV RUST_LOG=info

ENTRYPOINT ["hc-wled"]
