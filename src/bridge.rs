//! Bridge: manages per-device state polling, WebSocket subscriptions,
//! and command execution.

use std::collections::HashMap;

use anyhow::Result;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::config::{DeviceConfig, WledConfig};
use crate::homecore::{AttributeKind, AttributeSchema, DeviceSchema, HomecorePublisher};
use crate::wled::{WledClient, WledState};

pub struct Bridge {
    cfg:       WledConfig,
    publisher: HomecorePublisher,
}

impl Bridge {
    pub fn new(cfg: WledConfig, publisher: HomecorePublisher) -> Self {
        Self { cfg, publisher }
    }

    pub async fn run(self, mut cmd_rx: mpsc::Receiver<(String, Value)>) {
        for dev in &self.cfg.devices {
            let client     = WledClient::new(&dev.host);
            let publisher  = self.publisher.clone();
            let poll_secs  = dev.poll_interval_secs.unwrap_or(self.cfg.wled.poll_interval_secs);

            // Register + initial state
            let ws_supported = startup_device(&client, dev, &publisher).await;

            // Subscribe to homeCore commands
            if let Err(e) = publisher.subscribe_commands(&dev.hc_id).await {
                warn!(hc_id = %dev.hc_id, error = %e, "Failed to subscribe commands");
            }

            // Spawn real-time listener: WebSocket if supported, else HTTP poll
            let dev_clone = dev.clone();
            if ws_supported {
                let ws_url = client.ws_url();
                tokio::spawn(run_websocket(dev_clone, ws_url, publisher, poll_secs));
            } else {
                tokio::spawn(run_poller(dev_clone, publisher, poll_secs));
            }
        }

        // Command routing map: hc_id → host
        let host_map: HashMap<String, String> = self.cfg.devices.iter()
            .map(|d| (d.hc_id.clone(), d.host.clone()))
            .collect();

        while let Some((hc_id, cmd)) = cmd_rx.recv().await {
            let host = match host_map.get(&hc_id) {
                Some(h) => h.clone(),
                None    => { warn!(hc_id, "Command for unknown device"); continue; }
            };
            let client    = WledClient::new(&host);
            let publisher = self.publisher.clone();
            let hc_id2    = hc_id.clone();
            tokio::spawn(async move {
                debug!(hc_id = %hc_id2, cmd = ?cmd, "Executing command");
                match execute_command(&client, &cmd).await {
                    Ok(()) => {
                        // Re-fetch and publish updated state
                        if let Ok(state) = client.get_state().await {
                            let j = state_to_json(&state);
                            if let Err(e) = publisher
                                .publish_state_for_command(&hc_id2, &j, &cmd, "hc-wled")
                                .await
                            {
                                warn!(hc_id = %hc_id2, error = %e, "Failed to publish state");
                            }
                        }
                    }
                    Err(e) => warn!(hc_id = %hc_id2, error = %e, "Command failed"),
                }
            });
        }
    }
}

/// Register a device and publish initial state.
/// Returns true if the device reports WebSocket support.
async fn startup_device(
    client:    &WledClient,
    dev:       &DeviceConfig,
    publisher: &HomecorePublisher,
) -> bool {
    match client.get_info().await {
        Ok(info) => {
            info!(
                hc_id    = %dev.hc_id,
                host     = %dev.host,
                ver      = %info.ver,
                leds     = info.leds.count,
                effects  = info.fxcount,
                palettes = info.palcount,
                ws       = info.ws,
                "WLED device online"
            );
            if let Err(e) = publisher.register_device(&dev.hc_id, &dev.name, dev.area.as_deref()).await {
                warn!(hc_id = %dev.hc_id, error = %e, "Registration failed");
            }
            // Publish capability schema for the UI.
            publisher.publish_device_schema(&dev.hc_id, &build_wled_schema()).await.ok();
            let _ = publisher.publish_availability(&dev.hc_id, true).await;
            if let Ok(state) = client.get_state().await {
                let _ = publisher.publish_state(&dev.hc_id, &state_to_json(&state)).await;
            }
            info.ws >= 0
        }
        Err(e) => {
            warn!(hc_id = %dev.hc_id, host = %dev.host, error = %e, "WLED unreachable at startup");
            let _ = publisher.register_device(&dev.hc_id, &dev.name, dev.area.as_deref()).await;
            publisher.publish_device_schema(&dev.hc_id, &build_wled_schema()).await.ok();
            let _ = publisher.publish_availability(&dev.hc_id, false).await;
            false
        }
    }
}

/// Drive state updates via WLED's WebSocket (`ws://{host}/ws`).
/// Falls back to polling on connection error and reconnects automatically.
async fn run_websocket(
    dev:       DeviceConfig,
    ws_url:    String,
    publisher: HomecorePublisher,
    poll_secs: u64,
) {
    let client = WledClient::new(&dev.host);
    loop {
        info!(hc_id = %dev.hc_id, url = %ws_url, "Connecting WebSocket");
        match connect_async(&ws_url).await {
            Ok((mut ws, _)) => {
                let _ = publisher.publish_availability(&dev.hc_id, true).await;
                // WLED sends the full state JSON on every change
                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            match serde_json::from_str::<WledState>(&text) {
                                Ok(state) => {
                                    let j = state_to_json(&state);
                                    if let Err(e) = publisher.publish_state(&dev.hc_id, &j).await {
                                        warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish WS state");
                                    }
                                }
                                // WLED also sends non-state JSON (e.g. liveview) — ignore parse errors
                                Err(_) => {}
                            }
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        _ => {}
                    }
                }
                warn!(hc_id = %dev.hc_id, "WebSocket closed; reconnecting");
                let _ = publisher.publish_availability(&dev.hc_id, false).await;
            }
            Err(e) => {
                warn!(hc_id = %dev.hc_id, error = %e, "WebSocket connect failed; falling back to poll");
                // Fallback: do one HTTP poll then wait before retry
                if let Ok(state) = client.get_state().await {
                    let _ = publisher.publish_state(&dev.hc_id, &state_to_json(&state)).await;
                    let _ = publisher.publish_availability(&dev.hc_id, true).await;
                } else {
                    let _ = publisher.publish_availability(&dev.hc_id, false).await;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(poll_secs)).await;
    }
}

/// Drive state updates via periodic HTTP polling.
async fn run_poller(dev: DeviceConfig, publisher: HomecorePublisher, poll_secs: u64) {
    let client       = WledClient::new(&dev.host);
    let mut ticker   = interval(Duration::from_secs(poll_secs));
    let mut online   = true;

    loop {
        ticker.tick().await;
        match client.get_state().await {
            Ok(state) => {
                if !online {
                    info!(hc_id = %dev.hc_id, "WLED device back online");
                    let _ = publisher.publish_availability(&dev.hc_id, true).await;
                    online = true;
                }
                let j = state_to_json(&state);
                if let Err(e) = publisher.publish_state(&dev.hc_id, &j).await {
                    warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish state");
                }
            }
            Err(e) => {
                if online {
                    warn!(hc_id = %dev.hc_id, host = %dev.host, error = %e, "WLED unreachable");
                    let _ = publisher.publish_availability(&dev.hc_id, false).await;
                    online = false;
                }
            }
        }
    }
}

async fn execute_command(client: &WledClient, cmd: &Value) -> Result<()> {
    let mut body = serde_json::Map::new();

    // ── Top-level state fields ─────────────────────────────────────────────
    if let Some(on) = cmd.get("on").and_then(Value::as_bool) {
        body.insert("on".into(), json!(on));
    }
    if let Some(bri) = cmd.get("brightness").and_then(Value::as_u64) {
        body.insert("bri".into(), json!((bri.min(255)) as u8));
    }
    if let Some(pct) = cmd.get("brightness_pct").and_then(Value::as_f64) {
        let bri = ((pct.clamp(0.0, 100.0) / 100.0) * 255.0).round() as u8;
        body.insert("bri".into(), json!(bri));
    }
    // transition in ms → WLED units (×100 ms), capped at 65535
    if let Some(ms) = cmd.get("transition").and_then(Value::as_u64) {
        body.insert("tt".into(), json!((ms / 100).min(65535)));
    }
    if let Some(ps) = cmd.get("preset").and_then(Value::as_i64) {
        body.insert("ps".into(), json!(ps));
    }

    // ── Segment-level fields (applied to segment 0) ────────────────────────
    let mut seg = serde_json::Map::new();
    seg.insert("id".into(), json!(0));

    if let Some(color) = cmd.get("color").and_then(Value::as_array) {
        // Wrap in outer array: WLED expects col = [[r,g,b], ...]
        seg.insert("col".into(), json!([color]));
    }
    if let Some(fx) = cmd.get("effect").and_then(Value::as_u64) {
        seg.insert("fx".into(), json!(fx));
    }
    if let Some(sx) = cmd.get("effect_speed").and_then(Value::as_u64) {
        seg.insert("sx".into(), json!((sx.min(255)) as u8));
    }
    if let Some(ix) = cmd.get("effect_intensity").and_then(Value::as_u64) {
        seg.insert("ix".into(), json!((ix.min(255)) as u8));
    }
    if let Some(pal) = cmd.get("palette").and_then(Value::as_u64) {
        seg.insert("pal".into(), json!(pal));
    }

    // Only attach seg if there are segment-level changes beyond the id sentinel
    if seg.len() > 1 {
        body.insert("seg".into(), json!([seg]));
    }

    if body.is_empty() {
        warn!(cmd = ?cmd, "No recognized fields in WLED command");
        return Ok(());
    }

    client.post_state(&Value::Object(body)).await
}

fn build_wled_schema() -> DeviceSchema {
    let mut attrs = HashMap::new();
    attrs.insert("on".into(), AttributeSchema {
        kind: AttributeKind::Bool,
        writable: true,
        display_name: Some("Power".into()),
        unit: None, min: None, max: None, step: None, options: None,
    });
    attrs.insert("brightness_pct".into(), AttributeSchema {
        kind: AttributeKind::Integer,
        writable: true,
        display_name: Some("Brightness".into()),
        unit: Some("%".into()),
        min: Some(0.0), max: Some(100.0), step: Some(1.0),
        options: None,
    });
    attrs.insert("preset".into(), AttributeSchema {
        kind: AttributeKind::Integer,
        writable: true,
        display_name: Some("Preset".into()),
        unit: None,
        min: Some(1.0), max: Some(250.0), step: Some(1.0),
        options: None,
    });
    DeviceSchema { attributes: attrs }
}

pub fn state_to_json(state: &WledState) -> Value {
    let bri_pct = (state.bri as f64 / 255.0 * 1000.0).round() / 10.0; // 1 decimal

    let mut j = json!({
        "on":             state.on,
        "brightness":     state.bri,
        "brightness_pct": bri_pct,
        "preset_id":      state.ps,
    });

    if let Some(seg) = state.seg.first() {
        // Primary color from first segment's first color slot
        if let Some(primary) = seg.col.first() {
            j["color"] = primary.clone();
        }
        j["effect_id"]        = json!(seg.fx);
        j["effect_speed"]     = json!(seg.sx);
        j["effect_intensity"] = json!(seg.ix);
        j["palette_id"]       = json!(seg.pal);
    }

    j
}
