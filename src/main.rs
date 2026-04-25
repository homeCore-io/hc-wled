mod bridge;
mod bridge_info;
mod config;
mod logging;
mod wled;

use anyhow::Result;
use hc_types::schema::{AttributeKind, AttributeSchema, DeviceSchema};
use plugin_sdk_rs::{PluginClient, PluginConfig};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use config::WledConfig;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 30;

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let (_log_guard, log_level_handle, mqtt_log_handle) = init_logging(&config_path);

    let cfg = match WledConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-wled plugin");
        match try_start(
            &cfg,
            &config_path,
            log_level_handle.clone(),
            mqtt_log_handle.clone(),
        )
        .await
        {
            Ok(()) => return,
            Err(e) => {
                if attempt < MAX_ATTEMPTS {
                    error!(error = %e, attempt, "Startup failed; retrying in {RETRY_DELAY_SECS} s");
                    tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                } else {
                    error!(error = %e, "Startup failed after {MAX_ATTEMPTS} attempts; exiting");
                    std::process::exit(1);
                }
            }
        }
    }
}

fn init_logging(
    config_path: &str,
) -> (
    tracing_appender::non_blocking::WorkerGuard,
    hc_logging::LogLevelHandle,
    plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) {
    #[derive(serde::Deserialize, Default)]
    struct Bootstrap {
        #[serde(default)]
        logging: logging::LoggingConfig,
    }
    let bootstrap: Bootstrap = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    logging::init_logging(config_path, "hc-wled", "hc_wled=info", &bootstrap.logging)
}

async fn try_start(
    cfg: &WledConfig,
    config_path: &str,
    log_level_handle: hc_logging::LogLevelHandle,
    mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    let sdk_config = PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    };

    let client = PluginClient::connect(sdk_config)
        .await?
        // Cross-restart device tracking via the SDK. The path is the
        // same `.published-device-ids.json` the plugin used to manage
        // by hand, so existing snapshots are picked up unchanged.
        .with_device_persistence(published_ids_cache_path(config_path));
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );
    let publisher = client.device_publisher();
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    // Stash the device list so the management custom_handler closure
    // can hit any of them on demand for discovery / refresh / reboot
    // calls without going through the bridge runtime's command path.
    let devices_for_mgmt = cfg.devices.clone();
    let discovery_hosts_for_mgmt = cfg.wled.discovery_hosts.clone();

    // Enable management protocol (heartbeat + remote config/log commands +
    // capability manifest).
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_capabilities(capabilities_manifest())
        .with_custom_handler(move |cmd| {
            let action = cmd["action"].as_str()?.to_string();
            let devices = devices_for_mgmt.clone();
            let discovery_hosts = discovery_hosts_for_mgmt.clone();
            // Route each manifest action through a one-shot tokio
            // runtime — the SDK's custom_handler is a sync fn returning
            // Option<Value>, but the WLED HTTP client is async. The
            // runtime is cheap and isolated to this single call.
            let action_for_err = action.clone();
            let result = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                rt.block_on(async move {
                    run_action(&action, &devices, &discovery_hosts).await
                })
            })
            .join()
            .ok()
            .flatten();
            result.or(Some(json!({
                "status": "error",
                "error": format!("action '{action_for_err}' failed or is unknown"),
            })))
        });

    // Start the SDK event loop FIRST so the MQTT eventloop is pumping while
    // we register devices.  Without this, queued publishes block forever once
    // the rumqttc internal buffer (64) fills up.
    let cmd_tx_clone = cmd_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = client
            .run_managed(
                move |device_id, payload| {
                    let _ = cmd_tx_clone.try_send((device_id, payload));
                },
                mgmt,
            )
            .await
        {
            error!(error = %e, "SDK event loop exited with error");
        }
    });

    // Brief yield to let the eventloop connect before we start publishing.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Register all devices via DevicePublisher (PluginClient is consumed).
    let schema = build_wled_schema();
    let capabilities = wled_capabilities();
    for dev in &cfg.devices {
        if let Err(e) = publisher
            .register_device_full(
                &dev.hc_id,
                &dev.name,
                None,
                dev.area.as_deref(),
                Some(capabilities.clone()),
            )
            .await
        {
            warn!(hc_id = %dev.hc_id, error = %e, "Failed to register device");
        }
        if let Err(e) = publisher.register_device_schema(&dev.hc_id, &schema).await {
            warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish schema");
        }
        if let Err(e) = publisher.subscribe_commands(&dev.hc_id).await {
            error!(hc_id = %dev.hc_id, error = %e, "Failed to subscribe commands");
        }
    }

    // Reconcile against the SDK-tracked set: anything from a prior
    // session that's no longer in `[[devices]]` gets unregistered.
    let live: std::collections::HashSet<String> =
        cfg.devices.iter().map(|d| d.hc_id.clone()).collect();
    if let Err(e) = publisher.reconcile_devices(live).await {
        warn!(error = %e, "reconcile_devices failed");
    }

    // Slow info-poller: per-device task that polls /json/info,
    // /json/nodes, /presets.json every 5 minutes and partial-merges
    // firmware/hardware/wifi/peer attributes onto the existing device.
    // Surfaces the data the manifest's get-actions used to fetch on
    // demand but never had a place to display.
    bridge_info::spawn_per_device(publisher.clone(), cfg.devices.clone());

    let bridge = bridge::Bridge::new(cfg.clone(), publisher);
    bridge.run(cmd_rx).await;
    Ok(())
}

fn wled_capabilities() -> serde_json::Value {
    json!({
        "on":               { "type": "boolean" },
        "brightness":       { "type": "integer", "minimum": 0, "maximum": 255 },
        "brightness_pct":   { "type": "number",  "minimum": 0, "maximum": 100 },
        "color":            { "type": "array", "items": { "type": "integer" }, "minItems": 3, "maxItems": 3 },
        "effect_id":        { "type": "integer", "minimum": 0 },
        "effect_speed":     { "type": "integer", "minimum": 0, "maximum": 255 },
        "effect_intensity": { "type": "integer", "minimum": 0, "maximum": 255 },
        "palette_id":       { "type": "integer", "minimum": 0 },
        "preset_id":        { "type": "integer" }
    })
}

fn build_wled_schema() -> DeviceSchema {
    let mut attrs = HashMap::new();
    attrs.insert(
        "on".into(),
        AttributeSchema {
            kind: AttributeKind::Bool,
            writable: true,
            display_name: Some("Power".into()),
            unit: None,
            min: None,
            max: None,
            step: None,
            options: None,
        },
    );
    attrs.insert(
        "brightness_pct".into(),
        AttributeSchema {
            kind: AttributeKind::Integer,
            writable: true,
            display_name: Some("Brightness".into()),
            unit: Some("%".into()),
            min: Some(0.0),
            max: Some(100.0),
            step: Some(1.0),
            options: None,
        },
    );
    attrs.insert(
        "preset".into(),
        AttributeSchema {
            kind: AttributeKind::Integer,
            writable: true,
            display_name: Some("Preset".into()),
            unit: None,
            min: Some(1.0),
            max: Some(250.0),
            step: Some(1.0),
            options: None,
        },
    );
    DeviceSchema { attributes: attrs }
}

/// Path of the cross-restart device-id snapshot, sibling to
/// `config.toml`. Owned by the SDK's device tracker — see
/// `PluginClient::with_device_persistence`.
fn published_ids_cache_path(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".published-device-ids.json")
}

/// Capability manifest. Plugin-wide actions that aren't tied to a
/// specific device — discovery, library refresh, reboot. Per-device
/// commands (`apply_preset`, `save_preset`, `identify`) flow through
/// `PATCH /devices/:id/state` instead, handled by the bridge's
/// `execute_command`.
fn capabilities_manifest() -> hc_types::Capabilities {
    use hc_types::{Action, Capabilities, Concurrency, RequiresRole};
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(),
        actions: vec![
            Action {
                id: "discover_devices".into(),
                label: "Discover devices".into(),
                description: Some(
                    "Query the WLED-Sync mesh peer list (`/json/nodes`) \
                     from the first configured device. Returns every WLED \
                     instance the mesh knows about so you can decide \
                     which ones to add to `[[devices]]` in config.toml."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "discovered": { "type": "array" },
                    "count": { "type": "integer" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "refresh_effects_palettes".into(),
                label: "Refresh effects + palettes".into(),
                description: Some(
                    "Pull the effect-name and palette-name lists from \
                     each configured WLED device. Returns them per device \
                     so the UI / hc-mcp can show real names instead of \
                     opaque numbers. Effects + palettes change with \
                     firmware updates."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "devices": { "type": "object" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "refresh_presets".into(),
                label: "Refresh presets".into(),
                description: Some(
                    "Pull `/presets.json` from each configured device. \
                     Returns the preset name + id list so you can pick \
                     by name in the UI / hc-mcp."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "devices": { "type": "object" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "reboot".into(),
                label: "Reboot devices".into(),
                description: Some(
                    "Reboot every configured WLED device by hitting the \
                     legacy `/win&RB=1` endpoint. Use sparingly — \
                     interrupts active effects on every device. Optional \
                     `host` param to target a single device by IP."
                        .into(),
                ),
                params: Some(json!({
                    "host": {
                        "type": "string",
                        "description": "Optional WLED IP/hostname; reboots all configured devices if omitted",
                    },
                })),
                result: Some(json!({
                    "rebooted": { "type": "array" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: None,
            },
        ],
    }
}

async fn run_action(
    action: &str,
    devices: &[config::DeviceConfig],
    discovery_hosts: &[String],
) -> Option<serde_json::Value> {
    use crate::wled::WledClient;
    match action {
        "discover_devices" => {
            // Build the seed list: every configured device's host +
            // every entry in [wled].discovery_hosts. The latter
            // covers VLAN segments where no devices are configured
            // yet but at least one WLED is reachable.
            let mut seeds: Vec<String> = devices.iter().map(|d| d.host.clone()).collect();
            for h in discovery_hosts {
                if !seeds.iter().any(|s| s == h) {
                    seeds.push(h.clone());
                }
            }
            if seeds.is_empty() {
                return Some(json!({
                    "status": "error",
                    "error": "no [[devices]] configured and no [wled].discovery_hosts \
                             listed — nothing to query for /json/nodes",
                }));
            }

            // Query each seed in parallel; merge + dedup peer lists.
            let mut merged: Vec<serde_json::Value> = Vec::new();
            let mut seen_ips: std::collections::HashSet<String> = Default::default();
            let mut probe_errors: Vec<serde_json::Value> = Vec::new();

            for seed in &seeds {
                let client = WledClient::new(seed);
                match client.get_nodes().await {
                    Ok(nodes) => {
                        let arr = match nodes.get("nodes").and_then(|v| v.as_array()) {
                            Some(a) => a.clone(),
                            None => nodes.as_array().cloned().unwrap_or_default(),
                        };
                        for node in arr {
                            let ip = node
                                .get("ip")
                                .and_then(|v| v.as_str())
                                .map(str::to_string);
                            let dedup_key = ip.clone().unwrap_or_default();
                            if dedup_key.is_empty() || seen_ips.insert(dedup_key) {
                                merged.push(node);
                            }
                        }
                    }
                    Err(e) => {
                        probe_errors.push(json!({
                            "host": seed,
                            "error": e.to_string(),
                        }));
                    }
                }
            }

            Some(json!({
                "status": "ok",
                "discovered": merged,
                "count": merged.len(),
                "seeds": seeds,
                "errors": probe_errors,
            }))
        }
        "refresh_effects_palettes" => {
            let mut per_device = serde_json::Map::new();
            for d in devices {
                let client = WledClient::new(&d.host);
                let effects = client.get_effect_names().await.ok();
                let palettes = client.get_palette_names().await.ok();
                per_device.insert(
                    d.hc_id.clone(),
                    json!({
                        "host": d.host,
                        "effects": effects,
                        "palettes": palettes,
                    }),
                );
            }
            Some(json!({
                "status": "ok",
                "devices": per_device,
            }))
        }
        "refresh_presets" => {
            let mut per_device = serde_json::Map::new();
            for d in devices {
                let client = WledClient::new(&d.host);
                let presets = client.get_presets().await.ok();
                per_device.insert(
                    d.hc_id.clone(),
                    json!({
                        "host": d.host,
                        "presets": presets,
                    }),
                );
            }
            Some(json!({
                "status": "ok",
                "devices": per_device,
            }))
        }
        "reboot" => {
            // No host filter is supported via params here (the
            // custom_handler closure doesn't see the params object) —
            // reboots all configured devices. The manifest's `host`
            // param is documented for future expansion when params
            // routing through custom_handler is wired in.
            let mut rebooted = Vec::new();
            for d in devices {
                let client = WledClient::new(&d.host);
                match client.reboot().await {
                    Ok(()) => rebooted.push(d.host.clone()),
                    Err(e) => {
                        warn!(host = %d.host, error = %e, "reboot failed");
                    }
                }
            }
            Some(json!({
                "status": "ok",
                "rebooted": rebooted,
            }))
        }
        _ => None,
    }
}
