mod bridge;
mod config;
mod homecore;
mod logging;
mod wled;

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};

use config::WledConfig;

const MAX_ATTEMPTS:    u32 = 3;
const RETRY_DELAY_SECS: u64 = 30;

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let _log_guard = init_logging(&config_path);

    let cfg = match WledConfig::load(&config_path) {
        Ok(c)  => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-wled plugin");
        match try_start(&cfg, &config_path).await {
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

fn init_logging(config_path: &str) -> tracing_appender::non_blocking::WorkerGuard {
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

async fn try_start(cfg: &WledConfig, config_path: &str) -> Result<()> {
    let hc_client  = homecore::HomecoreClient::connect(&cfg.homecore).await?;
    let publisher  = hc_client.publisher();
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let current_ids: Vec<String> = cfg.devices.iter().map(|dev| dev.hc_id.clone()).collect();
    let cache_path = published_ids_cache_path(config_path);

    tokio::spawn(hc_client.run(cmd_tx));

    let previous_ids = load_published_ids(&cache_path);
    for stale_id in previous_ids
        .into_iter()
        .filter(|device_id| !current_ids.iter().any(|current| current == device_id))
    {
        if let Err(e) = publisher.unregister_device(&stale_id).await {
            error!(device_id = %stale_id, error = %e, "Failed to unregister stale configured device");
        } else {
            info!(device_id = %stale_id, "Unregistered stale configured device");
        }
    }

    save_published_ids(&cache_path, &current_ids)?;

    let bridge = bridge::Bridge::new(cfg.clone(), publisher);
    bridge.run(cmd_rx).await;
    Ok(())
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
