//! Read-only Frigate NVR event adapter.
//!
//! Frigate owns detection; MQTT owns delivery; this process owns the narrow
//! translation into two Unibus kinds. It subscribes to the event and
//! availability topics only. Snapshot and clip topics are deliberately not
//! subscribed: media belongs on the content plane, not the control plane.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use rumqttc::{
    Client as MqttClient, Connection, Event, Incoming, MqttOptions, QoS, RecvTimeoutError,
    Transport,
};
use serde::Deserialize;
use tracing::{info, warn};
use unibus::Message;
use unibus::client::{Client, ClientConfig};
use unibus_frigate::{
    AVAILABILITY_KIND, EVENT_KIND, FRIGATE_CAPABILITY, MAX_EVENT_BYTES, availability_topic,
    event_topic, parse_availability, parse_event, valid_topic_prefix,
};

const DEFAULT_TOPIC_PREFIX: &str = "frigate";
const DEFAULT_KEEP_ALIVE_SECS: u64 = 30;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    bus: ClientConfig,
    mqtt: BrokerConfig,
    #[serde(default = "default_topic_prefix")]
    topic_prefix: String,
    /// Optional addressed delivery. Omitted means the router's normal fan-out.
    #[serde(default)]
    destination: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrokerConfig {
    host: String,
    port: u16,
    client_id: String,
    #[serde(default)]
    tls: bool,
    #[serde(default)]
    username_env: Option<String>,
    #[serde(default)]
    password_env: Option<String>,
    #[serde(default = "default_keep_alive_secs")]
    keep_alive_secs: u64,
}

fn default_topic_prefix() -> String {
    DEFAULT_TOPIC_PREFIX.to_owned()
}

fn default_keep_alive_secs() -> u64 {
    DEFAULT_KEEP_ALIVE_SECS
}

impl Config {
    fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("read Frigate bridge config {}", path.display()))?;
        let mut config: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse Frigate bridge config {}", path.display()))?;
        config.bus.resolve_token_file(path);
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            valid_topic_prefix(&self.topic_prefix),
            "invalid Frigate MQTT topic prefix"
        );
        ensure!(
            self.bus.schema_version == unibus::SCHEMA_VERSION_V2,
            "Frigate adapter requires Unibus schema_version 2"
        );
        ensure!(
            self.bus
                .capabilities
                .iter()
                .any(|value| value == FRIGATE_CAPABILITY),
            "bus capabilities must include {FRIGATE_CAPABILITY}"
        );
        if let Some(destination) = &self.destination {
            ensure!(
                unibus_mqtt::valid_bridge_identifier(destination),
                "invalid Frigate destination"
            );
        }
        ensure!(!self.mqtt.host.trim().is_empty(), "MQTT host is empty");
        ensure!(self.mqtt.port > 0, "MQTT port must be non-zero");
        ensure!(
            unibus_mqtt::valid_bridge_identifier(&self.mqtt.client_id),
            "MQTT client_id must be a bridge identifier"
        );
        ensure!(
            self.mqtt.keep_alive_secs > 0,
            "MQTT keep_alive_secs must be positive"
        );
        ensure!(
            self.mqtt.username_env.is_some() == self.mqtt.password_env.is_some(),
            "MQTT username_env and password_env must be set together"
        );
        for name in [
            self.mqtt.username_env.as_deref(),
            self.mqtt.password_env.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                valid_env_name(name),
                "invalid environment variable name {name}"
            );
        }
        Ok(())
    }
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| matches!(byte, b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

enum MqttEvent {
    Connected,
    Subscribed,
    Publish {
        topic: String,
        payload: Vec<u8>,
        retained: bool,
        duplicate: bool,
    },
}

struct FrigateSubscriber {
    _client: MqttClient,
    connection: Connection,
}

impl FrigateSubscriber {
    fn connect(config: &BrokerConfig, prefix: &str) -> Result<Self> {
        let mut options =
            MqttOptions::new(config.client_id.clone(), config.host.clone(), config.port);
        options
            .set_clean_session(true)
            .set_keep_alive(Duration::from_secs(config.keep_alive_secs))
            .set_max_packet_size(MAX_EVENT_BYTES + 1024, 1024);
        if config.tls {
            options.set_transport(Transport::tls_with_default_config());
        }
        if let (Some(username), Some(password)) = (
            config.username_env.as_deref(),
            config.password_env.as_deref(),
        ) {
            options.set_credentials(
                std::env::var(username).with_context(|| format!("read {username}"))?,
                std::env::var(password).with_context(|| format!("read {password}"))?,
            );
        }
        let (client, connection) = MqttClient::new(options, 64);
        client.subscribe(event_topic(prefix), QoS::AtLeastOnce)?;
        client.subscribe(availability_topic(prefix), QoS::AtLeastOnce)?;
        Ok(Self {
            _client: client,
            connection,
        })
    }

    fn poll(&mut self, timeout: Duration) -> Result<Option<MqttEvent>> {
        let event = match self.connection.recv_timeout(timeout) {
            Ok(event) => event.context("Frigate MQTT connection failed")?,
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                bail!("Frigate MQTT subscription disconnected")
            }
        };
        Ok(match event {
            Event::Incoming(Incoming::ConnAck(_)) => Some(MqttEvent::Connected),
            Event::Incoming(Incoming::SubAck(ack)) => {
                ensure!(
                    !ack.return_codes.is_empty()
                        && ack
                            .return_codes
                            .iter()
                            .all(|qos| !matches!(qos, rumqttc::SubscribeReasonCode::Failure)),
                    "Frigate MQTT subscription rejected"
                );
                Some(MqttEvent::Subscribed)
            }
            Event::Incoming(Incoming::Publish(publish)) => {
                ensure!(
                    publish.payload.len() <= MAX_EVENT_BYTES,
                    "Frigate MQTT payload is too large"
                );
                Some(MqttEvent::Publish {
                    topic: publish.topic,
                    payload: publish.payload.to_vec(),
                    retained: publish.retain,
                    duplicate: publish.dup,
                })
            }
            _ => None,
        })
    }
}

fn publish(
    bus: &Client,
    destination: Option<&str>,
    kind: &str,
    body: serde_json::Value,
) -> Result<()> {
    let envelope = bus.envelope(destination, Message::new(kind, body))?;
    bus.send(&envelope, &[])?;
    Ok(())
}

fn drain_replies(bus: &Client) -> Result<()> {
    while let Some((envelope, _)) = bus.try_read()? {
        if envelope.message.kind == unibus::kinds::REPLY
            || envelope.message.kind == unibus::kinds::V1_REPLY
        {
            ensure!(
                envelope
                    .message
                    .body
                    .get("success")
                    .and_then(|value| value.as_bool())
                    == Some(true),
                "hub rejected Frigate publication: {:?}",
                envelope.message.body
            );
        }
    }
    Ok(())
}

fn run(config: Config) -> Result<()> {
    let event_topic = event_topic(&config.topic_prefix);
    let availability_topic = availability_topic(&config.topic_prefix);
    let bus = Client::connect(config.bus)?;
    let mut subscriber = FrigateSubscriber::connect(&config.mqtt, &config.topic_prefix)?;
    info!(
        prefix = %config.topic_prefix,
        broker = %config.mqtt.host,
        "Frigate event adapter started; watching {event_topic} and {availability_topic}"
    );
    loop {
        match subscriber.poll(Duration::from_millis(50))? {
            Some(MqttEvent::Connected) => info!("connected to Frigate MQTT broker"),
            Some(MqttEvent::Subscribed) => info!("subscribed to Frigate events and availability"),
            Some(MqttEvent::Publish {
                topic,
                payload,
                retained,
                duplicate,
            }) => {
                let now = unibus::envelope::now_ms();
                if topic == event_topic {
                    if retained {
                        warn!("ignoring retained Frigate event; it is not a new detection");
                        continue;
                    }
                    if duplicate {
                        warn!("ignoring duplicate Frigate event delivery");
                        continue;
                    }
                    match parse_event(&payload, now) {
                        Ok(event) => {
                            let body = serde_json::to_value(event)?;
                            publish(&bus, config.destination.as_deref(), EVENT_KIND, body)?;
                        }
                        Err(error) => warn!(%error, "rejected Frigate event"),
                    }
                } else if topic == availability_topic {
                    match parse_availability(&payload, now) {
                        Ok(availability) => {
                            let body = serde_json::to_value(availability)?;
                            publish(&bus, config.destination.as_deref(), AVAILABILITY_KIND, body)?;
                        }
                        Err(error) => warn!(%error, "rejected Frigate availability"),
                    }
                } else {
                    warn!(%topic, "ignoring unexpected Frigate MQTT topic");
                }
                drain_replies(&bus)?;
            }
            None => {}
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let path = std::env::args_os()
        .nth(1)
        .context("usage: unibus-frigate <config.json>")?;
    run(Config::read(Path::new(&path))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        serde_json::from_str(
            r#"{
                "bus":{
                    "installation_id":"home","producer_id":"frigate-home",
                    "hub_endpoint":"127.0.0.1:18790",
                    "token_file":"frigate.token",
                    "zones":["home"],
                    "capabilities":["frigate.events"],
                    "schema_version":2
                },
                "mqtt":{
                    "host":"127.0.0.1","port":1883,"client_id":"frigate-home"
                },
                "topic_prefix":"home/frigate"
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn minimal_config_is_valid() {
        let mut config = config();
        config
            .bus
            .resolve_token_file(Path::new("/etc/unibus/frigate.json"));
        config.validate().unwrap();
        assert_eq!(
            config.bus.token_file.as_deref(),
            Some(Path::new("/etc/unibus/frigate.token"))
        );
    }

    #[test]
    fn bridge_requires_v2_capability() {
        let mut config = config();
        config.bus.capabilities.clear();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("frigate.events")
        );
    }

    #[test]
    fn credentials_must_be_a_pair() {
        let mut config = config();
        config.mqtt.username_env = Some("MQTT_USER".into());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("together")
        );
    }

    #[test]
    fn topic_prefix_cannot_be_a_filter() {
        let mut config = config();
        config.topic_prefix = "frigate/+".into();
        assert!(config.validate().is_err());
    }
}
