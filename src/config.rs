use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct WledConfig {
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub logging: crate::logging::LoggingConfig,
    #[serde(default)]
    pub wled: WledGlobalConfig,
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HomecoreConfig {
    #[serde(default = "default_host")]
    pub broker_host: String,
    #[serde(default = "default_port")]
    pub broker_port: u16,
    #[serde(default = "default_plugin_id")]
    pub plugin_id: String,
    #[serde(default)]
    pub password: String,
}

fn default_host()      -> String { "127.0.0.1".into() }
fn default_port()      -> u16    { 1883 }
fn default_plugin_id() -> String { "plugin.wled".into() }

impl Default for HomecoreConfig {
    fn default() -> Self {
        Self {
            broker_host: default_host(),
            broker_port: default_port(),
            plugin_id:   default_plugin_id(),
            password:    String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WledGlobalConfig {
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
}

fn default_poll() -> u64 { 30 }

impl Default for WledGlobalConfig {
    fn default() -> Self { Self { poll_interval_secs: default_poll() } }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceConfig {
    /// IP address or hostname of the WLED device.
    pub host: String,
    /// Stable homeCore device ID.
    pub hc_id: String,
    /// Human-readable display name.
    pub name: String,
    #[serde(default)]
    pub area: Option<String>,
    /// Per-device polling interval override (seconds).
    #[serde(default)]
    pub poll_interval_secs: Option<u64>,
}

impl WledConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading config: {path}"))?;
        toml::from_str(&content)
            .with_context(|| format!("parsing config: {path}"))
    }
}
