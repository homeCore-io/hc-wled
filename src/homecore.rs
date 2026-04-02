//! HomeCore MQTT client — publishes device state and receives commands.

use anyhow::{Context, Result};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::HomecoreConfig;

// ── Inline device schema types (mirrors hc-types to avoid workspace dep) ──

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributeKind {
    Bool,
    Integer,
    Float,
    #[serde(rename = "string")]
    Str,
    Enum,
    ColorXy,
    ColorRgb,
    ColorTemp,
    Json,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttributeSchema {
    pub kind: AttributeKind,
    #[serde(default = "schema_default_true")]
    pub writable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

fn schema_default_true() -> bool { true }

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DeviceSchema {
    pub attributes: HashMap<String, AttributeSchema>,
}

#[derive(Clone)]
pub struct HomecorePublisher {
    client:    AsyncClient,
    plugin_id: String,
}

impl HomecorePublisher {
    fn with_change(payload: &Value, change: Value) -> Value {
        let mut payload = match payload.clone() {
            Value::Object(map) => map,
            other => return other,
        };
        let mut hc = payload
            .remove("_hc")
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        hc.insert("change".to_string(), change);
        payload.insert("_hc".to_string(), Value::Object(hc));
        Value::Object(payload)
    }

    fn with_default_change(&self, payload: &Value) -> Value {
        if payload
            .get("_hc")
            .and_then(|v| v.get("change"))
            .is_some()
        {
            return payload.clone();
        }

        Self::with_change(
            payload,
            json!({
                "kind": "external",
                "source": self.plugin_id,
            }),
        )
    }

    fn change_from_command(&self, command: &Value, fallback_source: &str) -> Value {
        command
            .get("_hc")
            .and_then(|v| v.get("command"))
            .cloned()
            .unwrap_or_else(|| {
                json!({
                    "kind": "homecore",
                    "source": fallback_source,
                })
            })
    }

    async fn clear_retained_topic(&self, topic: String) -> Result<()> {
        self.client
            .publish(topic, QoS::AtLeastOnce, true, Vec::<u8>::new())
            .await
            .context("clear_retained_topic failed")
    }

    pub async fn publish_state(&self, device_id: &str, state: &Value) -> Result<()> {
        let topic   = format!("homecore/devices/{device_id}/state");
        let payload = serde_json::to_vec(&self.with_default_change(state))?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("publish_state failed")
    }

    pub async fn publish_state_for_command(
        &self,
        device_id: &str,
        state: &Value,
        command: &Value,
        fallback_source: &str,
    ) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/state");
        let payload = Self::with_change(state, self.change_from_command(command, fallback_source));
        let payload = serde_json::to_vec(&payload)?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("publish_state_for_command failed")
    }

    pub async fn publish_availability(&self, device_id: &str, online: bool) -> Result<()> {
        let topic   = format!("homecore/devices/{device_id}/availability");
        let payload = if online { "online" } else { "offline" };
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload.as_bytes())
            .await
            .context("publish_availability failed")
    }

    pub async fn register_device(
        &self,
        device_id: &str,
        name:      &str,
        area:      Option<&str>,
    ) -> Result<()> {
        let topic = format!("homecore/plugins/{}/register", self.plugin_id);
        let capabilities = json!({
            "on":               { "type": "boolean" },
            "brightness":       { "type": "integer", "minimum": 0, "maximum": 255 },
            "brightness_pct":   { "type": "number",  "minimum": 0, "maximum": 100 },
            "color":            { "type": "array", "items": { "type": "integer" }, "minItems": 3, "maxItems": 3 },
            "effect_id":        { "type": "integer", "minimum": 0 },
            "effect_speed":     { "type": "integer", "minimum": 0, "maximum": 255 },
            "effect_intensity": { "type": "integer", "minimum": 0, "maximum": 255 },
            "palette_id":       { "type": "integer", "minimum": 0 },
            "preset_id":        { "type": "integer" }
        });
        let mut payload = json!({
            "device_id":    device_id,
            "plugin_id":    self.plugin_id,
            "name":         name,
            "capabilities": capabilities,
        });
        if let Some(a) = area {
            payload["area"] = Value::String(a.to_string());
        }
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, serde_json::to_vec(&payload)?)
            .await
            .context("register_device failed")?;
        debug!(device_id, "Registered device with HomeCore");
        Ok(())
    }

    pub async fn subscribe_commands(&self, device_id: &str) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/cmd");
        self.client
            .subscribe(&topic, QoS::AtLeastOnce)
            .await
            .context("subscribe_commands failed")
    }

    /// Publish a device capability schema (retained) to HomeCore.
    pub async fn publish_device_schema(
        &self,
        device_id: &str,
        schema: &DeviceSchema,
    ) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/schema");
        let payload = serde_json::to_vec(schema)
            .context("serialising device schema")?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("publish_device_schema failed")?;
        debug!(device_id, "Device schema published");
        Ok(())
    }

    pub async fn unregister_device(&self, device_id: &str) -> Result<()> {
        self.clear_retained_topic(format!("homecore/devices/{device_id}/state"))
            .await?;
        self.clear_retained_topic(format!("homecore/devices/{device_id}/availability"))
            .await?;
        self.clear_retained_topic(format!("homecore/devices/{device_id}/schema"))
            .await?;

        let topic = format!("homecore/plugins/{}/unregister", self.plugin_id);
        let payload = json!({
            "device_id": device_id,
            "plugin_id": self.plugin_id,
        });
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, serde_json::to_vec(&payload)?)
            .await
            .context("unregister_device failed")
    }
}

pub struct HomecoreClient {
    client:    AsyncClient,
    eventloop: rumqttc::EventLoop,
    plugin_id: String,
}

impl HomecoreClient {
    pub async fn connect(cfg: &HomecoreConfig) -> Result<Self> {
        let mut opts = MqttOptions::new(&cfg.plugin_id, &cfg.broker_host, cfg.broker_port);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_clean_session(true);
        if !cfg.password.is_empty() {
            opts.set_credentials(&cfg.plugin_id, &cfg.password);
        }
        let (client, eventloop) = AsyncClient::new(opts, 64);
        info!(host = %cfg.broker_host, port = cfg.broker_port, "HomeCore MQTT client created");
        Ok(Self { client, eventloop, plugin_id: cfg.plugin_id.clone() })
    }

    pub fn publisher(&self) -> HomecorePublisher {
        HomecorePublisher { client: self.client.clone(), plugin_id: self.plugin_id.clone() }
    }

    pub async fn run(mut self, tx: mpsc::Sender<(String, Value)>) {
        info!("HomeCore MQTT event loop starting");
        loop {
            match self.eventloop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    info!("Connected to HomeCore broker");
                }
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    let parts: Vec<&str> = p.topic.splitn(4, '/').collect();
                    if parts.len() == 4
                        && parts[0] == "homecore"
                        && parts[1] == "devices"
                        && parts[3] == "cmd"
                    {
                        let device_id = parts[2].to_string();
                        match serde_json::from_slice::<Value>(&p.payload) {
                            Ok(cmd) => { let _ = tx.send((device_id, cmd)).await; }
                            Err(e)  => warn!(topic = %p.topic, error = %e, "Non-JSON cmd payload"),
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    error!(error = %e, "HomeCore MQTT error; reconnecting in 2 s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
}
