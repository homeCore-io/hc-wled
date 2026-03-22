//! HomeCore MQTT client — publishes device state and receives commands.

use anyhow::{Context, Result};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::HomecoreConfig;

#[derive(Clone)]
pub struct HomecorePublisher {
    client:    AsyncClient,
    plugin_id: String,
}

impl HomecorePublisher {
    pub async fn publish_state(&self, device_id: &str, state: &Value) -> Result<()> {
        let topic   = format!("homecore/devices/{device_id}/state");
        let payload = serde_json::to_vec(state)?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("publish_state failed")
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
