mod bridge;
mod config;
mod homecore;
mod logging;
mod wled;

use anyhow::Result;
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
        match try_start(&cfg).await {
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

async fn try_start(cfg: &WledConfig) -> Result<()> {
    let hc_client  = homecore::HomecoreClient::connect(&cfg.homecore).await?;
    let publisher  = hc_client.publisher();
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    tokio::spawn(hc_client.run(cmd_tx));

    let bridge = bridge::Bridge::new(cfg.clone(), publisher);
    bridge.run(cmd_rx).await;
    Ok(())
}
