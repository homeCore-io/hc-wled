use anyhow::{Context, Result};
use serde::Deserialize;

/// The operator-config JSON Schema, published on the capability manifest so the
/// hc-web editor renders a typed form. `None` when built without `schema`.
#[cfg(feature = "schema")]
pub fn config_schema() -> Option<serde_json::Value> {
    serde_json::to_value(schemars::schema_for!(WledConfig)).ok()
}

#[cfg(not(feature = "schema"))]
pub fn config_schema() -> Option<serde_json::Value> {
    None
}

/// The plugin's own **config descriptor** — how this configuration should be
/// presented, which a JSON Schema cannot express: units, live data sources, and
/// prose. Published on the capability manifest; core serves it at
/// `GET /plugins/{id}/config/descriptor` and the editor renders it directly.
///
/// Coverage note (phase 6): every `WledConfig` key is represented. The Devices
/// section binds to the **live device registry** rather than this file's
/// `[[devices]]` array — naming and room assignment belong to the registry
/// (core owns inventory, per the device-identity model), so those edits go to
/// `/devices`. The array's per-device `poll_interval_secs` override is a rare
/// advanced knob not surfaced here; the global poll interval covers the common
/// case. `homecore.plugin_id` is bootstrap identity fixed at install.
pub fn config_descriptor() -> serde_json::Value {
    use plugin_sdk_rs::config_descriptor::{Descriptor, Field, Section, Source};

    Descriptor::new("plugin.wled")
        .title("WLED")
        .section(
            Section::new("discovery", "Discovery")
                .field(
                    Field::duration("wled.poll_interval_secs")
                        .label("Poll interval")
                        .unit("secs")
                        .default(30)
                        .min(1)
                        .help("How often to poll each WLED device for state. WLED has no push API, so state is read on this cadence."),
                )
                .field(
                    Field::list("wled.discovery_hosts", "host")
                        .label("Cross-subnet hosts")
                        .default(Vec::<String>::new())
                        .help(
                            "Local WLEDs are found automatically over mDNS, which \
                             can't cross VLANs. List an IP/hostname here only to \
                             reach a WLED on another subnet — leave empty on a flat \
                             network.",
                        ),
                ),
        )
        .section(
            Section::new("devices", "Devices").field(
                Field::table("devices")
                    .label("WLED devices")
                    .render("cards")
                    .key_by("device_id")
                    .help("Every WLED found by mDNS or the Discover action — set its name and room.")
                    .source(
                        Source::core_resource("devices")
                            .item_key("device_id")
                            .labels("name", "device_id"),
                    )
                    .columns([
                        Field::text("name").label("Name"),
                        Field::select("area")
                            .label("Room")
                            .placeholder("Unassigned")
                            .allow_create()
                            .source(Source::core_resource("areas")),
                    ]),
            ),
        )
        .section(
            Section::new("logging", "Logging")
                .field(
                    Field::text("logging.level")
                        .label("Level")
                        .default("info")
                        .placeholder("info | debug | hc_wled=debug"),
                )
                .field(
                    Field::enumeration("logging.log_forward_level")
                        .label("Forward to core")
                        .render("segmented")
                        .default("info")
                        .help(
                            "Minimum level forwarded to homeCore over MQTT; \
                             anything below is written locally only.",
                        )
                        .option("off", "Off")
                        .option("error", "Error")
                        .option("warn", "Warn")
                        .option("info", "Info")
                        .option("debug", "Debug"),
                )
                .field(
                    Field::enumeration("logging.rotation")
                        .label("Rotate")
                        .render("segmented")
                        .default("daily")
                        .option("hourly", "Hourly")
                        .option("daily", "Daily")
                        .option("weekly", "Weekly")
                        .option("never", "Never"),
                )
                .field(
                    Field::int("logging.max_size_mb")
                        .label("Rotate at size")
                        .unit("MB")
                        .default(100)
                        .min(0)
                        .help("Whichever comes first, this or the schedule. 0 disables size-based rotation."),
                )
                .field(
                    Field::int("logging.prune_after_days")
                        .label("Prune after")
                        .unit("days")
                        .default(0)
                        .min(0)
                        .help("Delete rotated files older than this. 0 = never prune."),
                )
                .field(
                    Field::toggle("logging.compress")
                        .label("Compress rotated files")
                        .default(true),
                ),
        )
        .section(
            Section::new("connection", "Connection")
                .hidden()
                .field(Field::host("homecore.broker_host").label("Broker host"))
                .field(Field::port("homecore.broker_port").label("Broker port"))
                .field(Field::secret("homecore.password").label("Broker password")),
        )
        .build()
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    1883
}
fn default_plugin_id() -> String {
    "plugin.wled".into()
}

impl Default for HomecoreConfig {
    fn default() -> Self {
        Self {
            broker_host: default_host(),
            broker_port: default_port(),
            plugin_id: default_plugin_id(),
            password: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WledGlobalConfig {
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    /// OPTIONAL cross-subnet fallback for `discover_devices`. Local-subnet
    /// WLEDs are found automatically over mDNS (`_wled._tcp.local`), which is
    /// link-local and cannot cross VLANs — so list an IP/hostname here only to
    /// reach a WLED on a subnet the homeCore host can route to but mDNS can't.
    /// Each listed host is queried once via `/json/nodes` and its WLED-Sync
    /// peer list is merged into the result. Leave empty on a flat network.
    #[serde(default)]
    pub discovery_hosts: Vec<String>,
}

fn default_poll() -> u64 {
    30
}

impl Default for WledGlobalConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll(),
            discovery_hosts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading config: {path}"))?;
        toml::from_str(&content).with_context(|| format!("parsing config: {path}"))
    }
}

#[cfg(all(test, feature = "schema"))]
mod tests {
    use super::*;

    /// A published descriptor is *authoritative* — the editor renders it
    /// instead of deriving from the schema — so any config field it omits
    /// becomes uneditable (the class of bug that dropped four hc-sonos logging
    /// settings, `5bccebf`). Every schema leaf must appear in the descriptor or
    /// be a justified omission.
    #[test]
    fn descriptor_covers_every_schema_field() {
        // Bootstrap identity fixed at install, not an operator setting.
        const JUSTIFIED_OMISSIONS: &[&str] = &["homecore.plugin_id"];

        fn resolve_ref<'a>(
            node: &'a serde_json::Value,
            defs: &'a serde_json::Value,
        ) -> &'a serde_json::Value {
            // schemars wraps a struct field as `{"allOf": [{"$ref": ...}]}` and
            // a bare reference as `{"$ref": ...}`. Unwrap either.
            let reference = node.get("$ref").and_then(|r| r.as_str()).or_else(|| {
                node.get("allOf")
                    .and_then(|a| a.as_array())
                    .filter(|a| a.len() == 1)
                    .and_then(|a| a[0].get("$ref"))
                    .and_then(|r| r.as_str())
            });
            if let Some(reference) = reference {
                if let Some(name) = reference.rsplit('/').next() {
                    if let Some(target) = defs.get(name) {
                        return target;
                    }
                }
            }
            node
        }

        // Flatten schema to dotted leaf paths. Arrays are leaves — an array
        // field (`devices`) is covered as a whole by a table, we don't descend.
        fn flatten(
            schema: &serde_json::Value,
            defs: &serde_json::Value,
            prefix: &str,
            out: &mut Vec<String>,
        ) {
            let node = resolve_ref(schema, defs);
            let is_object = node.get("type").and_then(|t| t.as_str()) == Some("object")
                || node.get("properties").is_some();
            if is_object {
                if let Some(props) = node.get("properties").and_then(|p| p.as_object()) {
                    for (name, child) in props {
                        let path = if prefix.is_empty() {
                            name.clone()
                        } else {
                            format!("{prefix}.{name}")
                        };
                        flatten(child, defs, &path, out);
                    }
                }
            } else {
                out.push(prefix.to_string());
            }
        }

        fn collect_keys(
            descriptor: &serde_json::Value,
            out: &mut std::collections::HashSet<String>,
        ) {
            let Some(sections) = descriptor.get("sections").and_then(|s| s.as_array()) else {
                return;
            };
            for section in sections {
                let Some(fields) = section.get("fields").and_then(|f| f.as_array()) else {
                    continue;
                };
                for field in fields {
                    if let Some(key) = field.get("key").and_then(|k| k.as_str()) {
                        out.insert(key.to_string());
                    }
                }
            }
        }

        let schema = config_schema().expect("schema feature is on");
        let defs = schema
            .get("definitions")
            .or_else(|| schema.get("$defs"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));

        let mut schema_leaves = Vec::new();
        flatten(&schema, &defs, "", &mut schema_leaves);

        let mut descriptor_keys = std::collections::HashSet::new();
        collect_keys(&config_descriptor(), &mut descriptor_keys);

        let uncovered: Vec<&String> = schema_leaves
            .iter()
            .filter(|leaf| {
                !descriptor_keys.contains(*leaf) && !JUSTIFIED_OMISSIONS.contains(&leaf.as_str())
            })
            .collect();

        assert!(
            uncovered.is_empty(),
            "config fields missing from the descriptor (add them or justify the omission): {uncovered:?}"
        );
    }
}
