mod bridge;
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

const MAX_ATTEMPTS:    u32 = 3;
const RETRY_DELAY_SECS: u64 = 30;

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let (_log_guard, log_level_handle) = init_logging(&config_path);

    let cfg = match WledConfig::load(&config_path) {
        Ok(c)  => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-wled plugin");
        match try_start(&cfg, &config_path, log_level_handle.clone()).await {
            Ok(())  => return,
            Err(e)  => {
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

fn init_logging(config_path: &str) -> (tracing_appender::non_blocking::WorkerGuard, hc_logging::LogLevelHandle) {
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

async fn try_start(cfg: &WledConfig, config_path: &str, log_level_handle: hc_logging::LogLevelHandle) -> Result<()> {
    let sdk_config = PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id:   cfg.homecore.plugin_id.clone(),
        password:    cfg.homecore.password.clone(),
    };

    let client = PluginClient::connect(sdk_config).await?;
    let publisher = client.device_publisher();
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    // Enable management protocol (heartbeat + remote config/log commands).
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?;

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

    // Clean up stale devices from previous config.
    let current_ids: Vec<String> = cfg.devices.iter().map(|d| d.hc_id.clone()).collect();
    let cache_path = published_ids_cache_path(config_path);
    let previous_ids = load_published_ids(&cache_path);
    let plugin_id = &cfg.homecore.plugin_id;
    for stale_id in previous_ids
        .into_iter()
        .filter(|id| !current_ids.contains(id))
    {
        if let Err(e) = publisher.unregister_device(plugin_id, &stale_id).await {
            error!(device_id = %stale_id, error = %e, "Failed to unregister stale device");
        } else {
            info!(device_id = %stale_id, "Unregistered stale configured device");
        }
    }
    save_published_ids(&cache_path, &current_ids)?;

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

fn published_ids_cache_path(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".published-device-ids.json")
}

fn load_published_ids(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .unwrap_or_default()
}

fn save_published_ids(path: &Path, device_ids: &[String]) -> Result<()> {
    let payload = serde_json::to_vec_pretty(device_ids)?;
    std::fs::write(path, payload)?;
    Ok(())
}
