# hc-wled

[![CI](https://github.com/homeCore-io/hc-wled/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-wled/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-wled/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-wled/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges WLED LED controllers into HomeCore via WebSocket with REST polling fallback.

## Published state

- `on` — boolean
- `brightness` — 0-255
- `color` — RGB hex
- `effect` — current effect name
- `speed` — effect speed
- `intensity` — effect intensity

## Supported actions

- `on` / `off`
- `set_brightness`
- `set_color`
- `set_effect`

## Setup

1. Copy `config/config.toml.example` to `config/config.toml`
2. Add device entries with the WLED controller IP/hostname
3. Add a `[[plugins]]` entry in `homecore.toml`

## Configuration

- `poll_interval_secs` — fallback polling interval (global or per-device)
- `[[devices]]` — each device needs `host`, `hc_id`, `name`, and `area`
