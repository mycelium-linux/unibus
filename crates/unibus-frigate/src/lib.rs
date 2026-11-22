//! Frigate NVR event contracts. The adapter owns Frigate's MQTT schema; the
//! bus only carries a bounded, normalized JSON object.
//!
//! Frigate publishes `frigate/events` as
//! `{"type":"new|update|end","before":{...},"after":{...}}`. The generic
//! `unibus-mqtt` bridge preserves that JSON in the binary payload, which the
//! bounded Unibus SDK deliberately drops. This adapter lifts the event into
//! the envelope body so a Soil Spore can observe it without a raw MQTT socket.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Normalized event kind. One kind for new/update/end keeps the Soil poll
/// filter stable; the body's `type` field is what automation switches on.
pub const EVENT_KIND: &str = "frigate.event.v1";
/// Latest-value availability kind. Frigate publishes `online` / `offline`.
pub const AVAILABILITY_KIND: &str = "frigate.availability.v1";
/// Router capability that permits this adapter to publish the two kinds.
pub const FRIGATE_CAPABILITY: &str = "frigate.events";

/// Matches the Unibus control-plane/SDK body ceiling.
pub const MAX_EVENT_BYTES: usize = 64 * 1024;
pub const MAX_TEXT_BYTES: usize = 128;
pub const MAX_ZONES: usize = 32;
pub const MAX_ZONE_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    New,
    Update,
    End,
}

impl EventType {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "new" => Ok(Self::New),
            "update" => Ok(Self::Update),
            "end" => Ok(Self::End),
            other => anyhow::bail!("unknown Frigate event type {other:?}"),
        }
    }
}

/// A normalized Frigate detection event. Media is represented by the two
/// `has_*` booleans, never by snapshot or clip bytes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrigateEvent {
    pub event_id: String,
    #[serde(rename = "type")]
    pub update_type: EventType,
    pub camera: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_label: Option<String>,
    pub score: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_score: Option<f64>,
    #[serde(default)]
    pub zones: Vec<String>,
    #[serde(default)]
    pub entered_zones: Vec<String>,
    #[serde(default)]
    pub has_clip: bool,
    #[serde(default)]
    pub has_snapshot: bool,
    pub start_time: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_time: Option<f64>,
    #[serde(default)]
    pub stationary: bool,
    pub observed_at_ms: u64,
}

/// Frigate's retained `frigate/available` message, normalized into JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrigateAvailability {
    pub status: String,
    pub online: bool,
    pub observed_at_ms: u64,
}

/// Parse `frigate/events` payload bytes.
///
/// `observed_at_ms` is the adapter's receive time. Frigate's own timestamps
/// remain in `start_time`/`end_time`; the receive time is what freshness and
/// dedupe policy can compare against.
pub fn parse_event(payload: &[u8], observed_at_ms: u64) -> Result<FrigateEvent> {
    ensure!(
        payload.len() <= MAX_EVENT_BYTES,
        "Frigate event exceeds {MAX_EVENT_BYTES} bytes"
    );
    let root: Value = serde_json::from_slice(payload).context("Frigate event is not JSON")?;
    let root = root
        .as_object()
        .context("Frigate event must be a JSON object")?;
    let update_type = root
        .get("type")
        .and_then(Value::as_str)
        .context("Frigate event has no type")?;
    let update_type = EventType::parse(update_type)?;
    let after = root
        .get("after")
        .and_then(Value::as_object)
        .context("Frigate event has no after object")?;

    let event_id = required_text(after, "id", MAX_TEXT_BYTES)?;
    ensure!(valid_event_id(&event_id), "invalid Frigate event id");
    let camera = required_text(after, "camera", MAX_TEXT_BYTES)?;
    let label = required_text(after, "label", MAX_TEXT_BYTES)?;
    let sub_label = optional_text(after, "sub_label", MAX_TEXT_BYTES)?;
    let score = required_score(after, "score")?;
    let top_score = optional_score(after, "top_score")?;
    let zones = optional_zones(after, "current_zones")?;
    let entered_zones = optional_zones(after, "entered_zones")?;
    let has_clip = optional_bool(after, "has_clip")?.unwrap_or(false);
    let has_snapshot = optional_bool(after, "has_snapshot")?.unwrap_or(false);
    let start_time = required_time(after, "start_time")?;
    let end_time = optional_time(after, "end_time")?;
    if let Some(end_time) = end_time {
        ensure!(
            end_time >= start_time,
            "Frigate end_time predates start_time"
        );
    }
    let snapshot_time = optional_time(after, "snapshot_time")?;
    let stationary = optional_bool(after, "stationary")?.unwrap_or(false);

    Ok(FrigateEvent {
        event_id,
        update_type,
        camera,
        label,
        sub_label,
        score,
        top_score,
        zones,
        entered_zones,
        has_clip,
        has_snapshot,
        start_time,
        end_time,
        snapshot_time,
        stationary,
        observed_at_ms,
    })
}

/// Parse the retained `frigate/available` payload (`online` or `offline`).
pub fn parse_availability(payload: &[u8], observed_at_ms: u64) -> Result<FrigateAvailability> {
    ensure!(
        payload.len() <= MAX_TEXT_BYTES,
        "Frigate availability payload is too large"
    );
    let status = std::str::from_utf8(payload)
        .context("Frigate availability is not UTF-8")?
        .trim()
        .to_ascii_lowercase();
    let online = match status.as_str() {
        "online" => true,
        "offline" => false,
        other => anyhow::bail!("unknown Frigate availability status {other:?}"),
    };
    Ok(FrigateAvailability {
        status,
        online,
        observed_at_ms,
    })
}

/// An MQTT topic prefix is one or more `/`-separated Frigate segments. It may
/// not contain wildcards, NUL, empty segments, or non-topic punctuation.
pub fn valid_topic_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix.len() <= MAX_TEXT_BYTES
        && !prefix.starts_with('/')
        && !prefix.ends_with('/')
        && !prefix.contains("//")
        && !prefix.contains(['#', '+', '\0'])
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
}

pub fn event_topic(prefix: &str) -> String {
    format!("{prefix}/events")
}

pub fn availability_topic(prefix: &str) -> String {
    format!("{prefix}/available")
}

fn valid_event_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TEXT_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
}

fn required_text(map: &Map<String, Value>, key: &str, max: usize) -> Result<String> {
    let value = map
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Frigate event has no string {key}"))?;
    ensure!(!value.is_empty(), "Frigate {key} is empty");
    ensure!(value.len() <= max, "Frigate {key} exceeds {max} bytes");
    Ok(value.to_owned())
}

fn optional_text(map: &Map<String, Value>, key: &str, max: usize) -> Result<Option<String>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let value = value
                .as_str()
                .with_context(|| format!("Frigate {key} is not a string"))?;
            ensure!(value.len() <= max, "Frigate {key} exceeds {max} bytes");
            Ok((!value.is_empty()).then(|| value.to_owned()))
        }
    }
}

fn required_score(map: &Map<String, Value>, key: &str) -> Result<f64> {
    let value = map
        .get(key)
        .and_then(Value::as_f64)
        .with_context(|| format!("Frigate event has no numeric {key}"))?;
    ensure!(
        value.is_finite() && (0.0..=1.0).contains(&value),
        "Frigate {key} must be between 0 and 1"
    );
    Ok(value)
}

fn optional_score(map: &Map<String, Value>, key: &str) -> Result<Option<f64>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => required_score(map, key).map(Some),
    }
}

fn required_time(map: &Map<String, Value>, key: &str) -> Result<f64> {
    let value = map
        .get(key)
        .and_then(Value::as_f64)
        .with_context(|| format!("Frigate event has no numeric {key}"))?;
    ensure!(
        value.is_finite() && value > 0.0,
        "Frigate {key} must be a positive finite timestamp"
    );
    Ok(value)
}

fn optional_time(map: &Map<String, Value>, key: &str) -> Result<Option<f64>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => required_time(map, key).map(Some),
    }
}

fn optional_bool(map: &Map<String, Value>, key: &str) -> Result<Option<bool>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .with_context(|| format!("Frigate {key} is not a boolean")),
    }
}

fn optional_zones(map: &Map<String, Value>, key: &str) -> Result<Vec<String>> {
    let Some(value) = map.get(key) else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let items = value
        .as_array()
        .with_context(|| format!("Frigate {key} must be an array"))?;
    ensure!(items.len() <= MAX_ZONES, "Frigate {key} has too many zones");
    items
        .iter()
        .map(|item| {
            let zone = item
                .as_str()
                .with_context(|| format!("Frigate {key} contains a non-string zone"))?;
            ensure!(
                !zone.is_empty() && zone.len() <= MAX_ZONE_BYTES,
                "Frigate {key} contains an invalid zone"
            );
            Ok(zone.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_payload(update_type: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "type": update_type,
            "before": {},
            "after": {
                "id": "1712345678.123456-abc123",
                "camera": "front_door",
                "label": "person",
                "sub_label": null,
                "score": 0.87,
                "top_score": 0.91,
                "current_zones": ["porch"],
                "entered_zones": ["porch", "walkway"],
                "has_clip": true,
                "has_snapshot": true,
                "start_time": 1712345678.1,
                "end_time": 1712345682.4,
                "snapshot_time": 1712345679.2,
                "stationary": false
            }
        }))
        .unwrap()
    }

    #[test]
    fn a_new_event_is_normalized() {
        let event = parse_event(&event_payload("new"), 1_000).unwrap();
        assert_eq!(event.event_id, "1712345678.123456-abc123");
        assert_eq!(event.update_type, EventType::New);
        assert_eq!(event.camera, "front_door");
        assert_eq!(event.label, "person");
        assert_eq!(event.score, 0.87);
        assert_eq!(event.top_score, Some(0.91));
        assert_eq!(event.zones, ["porch"]);
        assert_eq!(event.entered_zones, ["porch", "walkway"]);
        assert!(event.has_clip && event.has_snapshot);
        assert_eq!(event.start_time, 1712345678.1);
        assert_eq!(event.end_time, Some(1712345682.4));
        assert_eq!(event.observed_at_ms, 1_000);
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "new");
        assert_eq!(json["event_id"], "1712345678.123456-abc123");
    }

    #[test]
    fn update_and_end_share_the_kind() {
        assert_eq!(
            parse_event(&event_payload("update"), 0)
                .unwrap()
                .update_type,
            EventType::Update
        );
        assert_eq!(
            parse_event(&event_payload("end"), 0).unwrap().update_type,
            EventType::End
        );
    }

    #[test]
    fn malformed_events_are_rejected() {
        assert!(parse_event(b"{}", 0).is_err());
        assert!(parse_event(b"not-json", 0).is_err());
        let mut value: Value = serde_json::from_slice(&event_payload("new")).unwrap();
        value["after"]["score"] = serde_json::json!(2.0);
        let payload = serde_json::to_vec(&value).unwrap();
        assert!(parse_event(&payload, 0).is_err());
        let mut value: Value = serde_json::from_slice(&event_payload("new")).unwrap();
        value["after"]["id"] = serde_json::json!("bad id");
        let payload = serde_json::to_vec(&value).unwrap();
        assert!(parse_event(&payload, 0).is_err());
    }

    #[test]
    fn oversized_events_are_rejected() {
        let payload = vec![b'x'; MAX_EVENT_BYTES + 1];
        assert!(parse_event(&payload, 0).is_err());
    }

    #[test]
    fn availability_is_normalized() {
        let online = parse_availability(b" online\n", 7).unwrap();
        assert_eq!(online.status, "online");
        assert!(online.online);
        assert_eq!(online.observed_at_ms, 7);
        let offline = parse_availability(b"offline", 8).unwrap();
        assert!(!offline.online);
        assert!(parse_availability(b"unknown", 0).is_err());
    }

    #[test]
    fn topic_prefix_is_bounded_to_mqtt_topic_segments() {
        assert!(valid_topic_prefix("frigate"));
        assert!(valid_topic_prefix("home/frigate"));
        assert!(!valid_topic_prefix(""));
        assert!(!valid_topic_prefix("/frigate"));
        assert!(!valid_topic_prefix("frigate/"));
        assert!(!valid_topic_prefix("frigate//nvr"));
        assert!(!valid_topic_prefix("frigate/+"));
        assert!(!valid_topic_prefix("frigate/#"));
        assert!(!valid_topic_prefix("frigate nvr"));
        assert_eq!(event_topic("home/frigate"), "home/frigate/events");
        assert_eq!(availability_topic("home/frigate"), "home/frigate/available");
    }
}
