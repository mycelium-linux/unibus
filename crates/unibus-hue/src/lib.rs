//! Hue owns lighting state; DCP and MCP share this bounded executor.
use anyhow::{Context, Result, bail, ensure};
use dcp::{
    Action, Availability, Catalog, Discovery, Endpoints, ExecuteRequest, Outcome, Phase, Provider,
    Receipt, Safety,
};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::PathBuf,
    time::Duration,
};
mod tls;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub bridge_url: String,
    pub application_key_file: PathBuf,
    /// PEM trust anchor for the enrolled bridge. TLS hostname checks remain on.
    pub bridge_ca_file: Option<PathBuf>,
    /// Optional exact enrolled certificate for legacy Hue certificates without SANs.
    pub bridge_certificate_file: Option<PathBuf>,
    /// Bridge-owned ID verified during enrollment; required with a certificate pin.
    pub bridge_id: Option<String>,
    pub provider_token_file: PathBuf,
    pub allowed_light_ids: BTreeSet<String>,
    pub listen: String,
}

pub struct Hue {
    client: Client,
    base: reqwest::Url,
    key_file: PathBuf,
    allowed: BTreeSet<String>,
}

fn digest(value: &impl serde::Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

/// Provider-local memberships derived from Hue resource references, never
/// from a light's name or the machine hosting its controller.
fn location_context(state: &Value, light: &Value) -> Result<String> {
    let light_id = light["id"].as_str().context("light has no id")?;
    let owner = light.get("owner");
    let mut memberships = Vec::new();
    let mut physical_room_known = false;
    for (collection, label) in [("rooms", "Physical room"), ("zones", "Logical zone")] {
        for group in state[collection].as_array().into_iter().flatten() {
            let contains_light = group["children"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|child| {
                    (child["rtype"] == "light" && child["rid"] == light_id)
                        || owner.is_some_and(|owner| {
                            owner["rtype"] == "device"
                                && child["rtype"] == "device"
                                && child["rid"] == owner["rid"]
                                && child["rid"].as_str().is_some()
                        })
                });
            if !contains_light {
                continue;
            }
            physical_room_known |= collection == "rooms";
            let id = group["id"].as_str().context("Hue group has no id")?;
            ensure!(valid_id(id), "Hue group has invalid id");
            let name = group
                .pointer("/metadata/name")
                .and_then(Value::as_str)
                .unwrap_or(id);
            ensure!(
                !name.trim().is_empty()
                    && name.chars().count() <= 256
                    && !name.chars().any(char::is_control),
                "Hue group has invalid name"
            );
            memberships.push(format!("{label}: {name} (Hue resource {id})"));
        }
    }
    memberships.sort();
    memberships.dedup();
    if !physical_room_known {
        memberships
            .push("Physical room: unknown; do not infer from controller host or light name".into());
    }
    let text = memberships.join("; ");
    ensure!(
        text.chars().count() <= 768,
        "Hue location evidence exceeds context bound"
    );
    Ok(text)
}
fn timestamp() -> Result<String> {
    Ok(time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?)
}
fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

impl Hue {
    pub fn connect(config: &Config) -> Result<Self> {
        let base = reqwest::Url::parse(&config.bridge_url)?;
        ensure!(
            base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none()
                && base.path() == "/",
            "bridge URL must be an origin without credentials"
        );
        let loopback = base
            .host_str()
            .is_some_and(|h| h == "127.0.0.1" || h == "localhost" || h == "[::1]");
        ensure!(
            base.scheme() == "https" || (base.scheme() == "http" && loopback),
            "bridge requires HTTPS; HTTP is only allowed for loopback mocks"
        );
        ensure!(
            config.allowed_light_ids.iter().all(|id| valid_id(id)),
            "invalid enrolled light id"
        );
        let mut builder = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5));
        if let Some(path) = &config.bridge_certificate_file {
            ensure!(
                base.scheme() == "https",
                "certificate pinning requires HTTPS"
            );
            ensure!(
                config.bridge_ca_file.is_none(),
                "select PKI trust or exact enrolled certificate trust"
            );
            builder = builder.use_preconfigured_tls(tls::pinned_config(
                path,
                config
                    .bridge_id
                    .as_deref()
                    .context("pinned bridge requires its enrolled bridge_id")?,
            )?);
        } else if let Some(path) = &config.bridge_ca_file {
            builder = builder
                .add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(path)?)?);
        }
        Ok(Self {
            client: builder.build()?,
            base,
            key_file: config.application_key_file.clone(),
            allowed: config.allowed_light_ids.clone(),
        })
    }

    /// Explicit, physical-button enrollment. Never runs during startup or a
    /// normal tool call, never replaces an existing key, never logs the reply.
    pub fn pair(&self) -> Result<()> {
        use std::io::Write;
        ensure!(
            !self.key_file.try_exists()?,
            "application-key file already exists; refusing to replace enrollment"
        );
        let response = self
            .client
            .post(self.base.join("api")?)
            .json(&json!({"devicetype":"unibus-hue#jarvis"}))
            .send()
            .context("Hue pairing request failed")?;
        ensure!(
            response.status().is_success(),
            "Hue pairing HTTP {}",
            response.status()
        );
        let mut bytes = Vec::new();
        std::io::Read::take(response, 65537).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 65536, "Hue pairing response too large");
        let response: Value =
            serde_json::from_slice(&bytes).context("invalid Hue pairing reply")?;
        let records = response.as_array().context("invalid Hue pairing reply")?;
        ensure!(records.len() == 1, "unexpected Hue pairing reply count");
        if records[0].pointer("/error/type").and_then(Value::as_u64) == Some(101) {
            bail!("press the physical Hue bridge button, then run pair again within 30 seconds");
        }
        ensure!(
            records[0].get("error").is_none(),
            "Hue bridge rejected pairing"
        );
        let key = records[0]
            .pointer("/success/username")
            .and_then(Value::as_str)
            .filter(|key| {
                (16..=256).contains(&key.len())
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            })
            .context("Hue pairing returned no valid application key")?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.key_file)
            .context("pairing succeeded but new private key file could not be created; do not blindly retry enrollment")?;
        file.write_all(key.as_bytes())
            .context("pairing succeeded but key persistence failed")?;
        file.sync_all()
            .context("pairing succeeded but key persistence failed")?;
        Ok(())
    }

    fn api(&self, kind: &str, id: Option<&str>, update: Option<&Value>) -> Result<Value> {
        ensure!(
            ["light", "scene", "room", "zone"].contains(&kind),
            "unsupported resource kind"
        );
        if let Some(id) = id {
            ensure!(valid_id(id), "invalid resource id");
        }
        let path = format!(
            "clip/v2/resource/{kind}{}",
            id.map(|id| format!("/{id}")).unwrap_or_default()
        );
        let key = std::fs::read_to_string(&self.key_file)
            .context("read local Hue application key file")?;
        ensure!(!key.trim().is_empty(), "local Hue application key is empty");
        let url = self.base.join(&path)?;
        let request = match update {
            Some(body) => self.client.put(url).json(body),
            None => self.client.get(url),
        };
        let response = request
            .header("hue-application-key", key.trim())
            .send()
            .context("Hue LAN request failed")?;
        ensure!(
            response.status().is_success(),
            "Hue LAN HTTP {}",
            response.status()
        );
        let mut bytes = Vec::new();
        std::io::Read::take(response, 2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 2 * 1024 * 1024, "Hue response exceeds 2 MiB");
        let body: Value = serde_json::from_slice(&bytes).context("decode Hue response")?;
        ensure!(
            body.get("errors")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty),
            "Hue API rejected request"
        );
        ensure!(
            body.get("data").is_some_and(Value::is_array),
            "Hue response has no resource data"
        );
        Ok(body["data"].clone())
    }

    pub fn observe(&self) -> Result<Value> {
        let mut lights = self
            .api("light", None, None)?
            .as_array()
            .context("light list")?
            .clone();
        let mut scenes = self
            .api("scene", None, None)?
            .as_array()
            .context("scene list")?
            .clone();
        lights.sort_by_key(|v| v["id"].as_str().unwrap_or_default().to_owned());
        scenes.sort_by_key(|v| v["id"].as_str().unwrap_or_default().to_owned());
        Ok(
            json!({"lights":lights,"scenes":scenes,"rooms":self.api("room",None,None)?,"zones":self.api("zone",None,None)?}),
        )
    }

    pub fn discovery(&self) -> Discovery {
        Discovery {
            dcp: dcp::SPEC.into(),
            provider: self.provider(),
            versions: vec![dcp::VERSION.into()],
            endpoints: Endpoints {
                catalog: "/v1/decisions".into(),
                execute: Some("/v1/decisions/execute".into()),
                stream: None,
            },
            authorization: vec![dcp::Authorization {
                scheme: "bearer".into(),
                issuer: None,
                scopes: vec!["hue.control".into()],
            }],
        }
    }
    fn provider(&self) -> Provider {
        Provider {
            id: "philips-hue".into(),
            name: "Local Philips Hue".into(),
            instance: self.base.to_string(),
        }
    }

    pub fn catalog_for(&self, state: Value) -> Result<Catalog> {
        let mut actions = Vec::new();
        for light in state["lights"].as_array().context("missing lights")? {
            let id = light["id"].as_str().context("light has no id")?;
            ensure!(valid_id(id), "bridge returned invalid light id");
            if !self.allowed.contains(id) {
                continue;
            }
            let name = light
                .pointer("/metadata/name")
                .and_then(Value::as_str)
                .unwrap_or(id);
            let mut properties = json!({"on":{"type":"boolean"}});
            ensure!(
                !name.trim().is_empty()
                    && name.chars().count() <= 256
                    && !name.chars().any(char::is_control),
                "Hue light has invalid name"
            );
            let location = location_context(&state, light)?;
            if light.get("dimming").is_some() {
                properties["brightness"] = json!({"type":"number","minimum":0.0,"maximum":100.0});
            }
            let observed_state = json!({"on":light.pointer("/on/on"),"brightness":light.pointer("/dimming/brightness")});
            let description = format!(
                "Set on/off{} for {name}; resource {id}. {location}. Current state: {observed_state}",
                if light.get("dimming").is_some() {
                    " or brightness"
                } else {
                    ""
                }
            );
            ensure!(
                description.chars().count() <= 1024,
                "Hue action evidence exceeds decision context bound"
            );
            actions.push(Action { id: light_action_id(id), domain: "lighting".into(), title: format!("Set {name}"), description, input_schema: json!({"type":"object","properties":properties,"minProperties":1,"additionalProperties":false}), phases: vec![Phase::Commit], safety: Safety { idempotent: true, reversible: false, requires_final: true, confirmation_required: false }, availability: Availability { available: true, reason: None, reason_code: None } });
        }
        let catalog_revision = digest(&actions)?;
        let state_revision = digest(&state)?;
        Ok(Catalog {
            dcp_version: dcp::VERSION.into(),
            provider: self.provider(),
            catalog_revision,
            state_revision,
            observed_at: timestamp()?,
            expires_at: None,
            state: Some(BTreeMap::from([("resources".into(), state)])),
            actions,
            tree: vec![],
        })
    }
    pub fn catalog(&self) -> Result<Catalog> {
        self.catalog_for(self.observe()?)
    }

    pub fn execute(&self, request: &ExecuteRequest) -> Result<Receipt> {
        let catalog = self.catalog()?;
        let mut receipt = Receipt {
            dcp_version: dcp::VERSION.into(),
            receipt_id: format!("hue-{}", digest(request)?),
            request_id: request.request_id.clone(),
            provider_id: "philips-hue".into(),
            action_id: request.action_id.clone(),
            phase: request.phase.clone(),
            outcome: Outcome::Rejected,
            expected_catalog_revision: request.expected_catalog_revision.clone(),
            expected_state_revision: request.expected_state_revision.clone(),
            observed_catalog_revision: catalog.catalog_revision.clone(),
            observed_state_revision: catalog.state_revision.clone(),
            resulting_catalog_revision: None,
            resulting_state_revision: None,
            observed_at: timestamp()?,
            error: None,
        };
        if let Err((code, message)) = validate_request(request, &catalog) {
            receipt.error = Some(dcp::Problem {
                code,
                message: message.into(),
                retryable: false,
                details: None,
            });
            return Ok(receipt);
        }
        let id = request
            .action_id
            .strip_prefix("light.")
            .and_then(|id| id.strip_suffix(".set"))
            .context("invalid light action")?;
        let update = light_update(&request.arguments)?;
        // A transport failure is surfaced as uncertain, never auto-retried as a mutation.
        self.api("light", Some(id), Some(&update))?;
        let actual = self.api("light", Some(id), None)?;
        ensure!(
            actual
                .as_array()
                .is_some_and(|items| items.len() == 1 && items[0]["id"] == id),
            "unexpected Hue readback resource"
        );
        verify_update(&actual[0], &request.arguments)?;
        let resulting = self.catalog()?;
        receipt.outcome = Outcome::Applied;
        receipt.resulting_catalog_revision = Some(resulting.catalog_revision);
        receipt.resulting_state_revision = Some(resulting.state_revision);
        Ok(receipt)
    }
}

pub fn validate_request(
    request: &ExecuteRequest,
    catalog: &Catalog,
) -> std::result::Result<(), (dcp::ErrorCode, &'static str)> {
    use dcp::ErrorCode::*;
    if request.dcp_version != dcp::VERSION {
        return Err((UnsupportedVersion, "unsupported DCP version"));
    }
    if request.request_id.is_empty()
        || request.request_id.len() > 128
        || request.decision_id.is_empty()
        || request.decision_id.len() > 128
        || request.evidence.utterance_id.is_empty()
    {
        return Err((InvalidRequest, "request and decision IDs are required"));
    }
    if request.phase != Phase::Commit || request.prepared_receipt_id.is_some() {
        return Err((UnsafePhase, "only direct commit is supported"));
    }
    if !request.evidence.final_ {
        return Err((FinalityRequired, "final transcript required"));
    }
    if request.expected_catalog_revision != catalog.catalog_revision {
        return Err((StaleCatalog, "catalog revision changed"));
    }
    if request.expected_state_revision != catalog.state_revision {
        return Err((StaleState, "state revision changed"));
    }
    let Some(action) = catalog
        .actions
        .iter()
        .find(|action| action.id == request.action_id)
    else {
        return Err((ActionNotFound, "action not in current enrolled catalog"));
    };
    if light_update(&request.arguments).is_err()
        || request
            .arguments
            .keys()
            .any(|key| action.input_schema["properties"].get(key).is_none())
    {
        return Err((InvalidRequest, "invalid light arguments"));
    }
    Ok(())
}
fn light_update(args: &BTreeMap<String, Value>) -> Result<Value> {
    ensure!(
        !args.is_empty()
            && args
                .keys()
                .all(|k| ["on", "brightness"].contains(&k.as_str())),
        "unknown or empty light arguments"
    );
    let mut update = json!({});
    if let Some(on) = args.get("on") {
        ensure!(on.is_boolean(), "on must be boolean");
        update["on"] = json!({"on":on});
    }
    if let Some(brightness) = args.get("brightness") {
        let n = brightness.as_f64().context("brightness must be numeric")?;
        ensure!((0.0..=100.0).contains(&n), "brightness out of bounds");
        update["dimming"] = json!({"brightness":n});
    }
    Ok(update)
}
fn verify_update(actual: &Value, args: &BTreeMap<String, Value>) -> Result<()> {
    if let Some(on) = args.get("on") {
        ensure!(
            actual.pointer("/on/on") == Some(on),
            "Hue on/off readback mismatch; mutation outcome uncertain"
        );
    }
    if let Some(n) = args.get("brightness").and_then(Value::as_f64) {
        let observed = actual
            .pointer("/dimming/brightness")
            .and_then(Value::as_f64)
            .context("missing brightness readback")?;
        ensure!(
            (observed - n).abs() <= 1.0,
            "Hue brightness readback mismatch; mutation outcome uncertain"
        );
    }
    Ok(())
}

pub fn mcp_tools(catalog: &Catalog) -> Value {
    let mut tools = vec![
        json!({"name":"hue_status","description":"Inspect local lights, scenes, rooms and zones; does not change lighting.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
    ];
    for (name, description, properties, required) in [
        (
            "hue_list_lights",
            "List local bridge lights and state",
            json!({}),
            vec![],
        ),
        (
            "hue_get_light",
            "Read one local bridge light",
            json!({"id":{"type":"string"}}),
            vec!["id"],
        ),
        (
            "hue_list_scenes",
            "List observed local bridge scenes; does not activate scenes",
            json!({}),
            vec![],
        ),
    ] {
        tools.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":true}}));
    }
    if !catalog.actions.is_empty() {
        tools.push(json!({"name":"hue_set_light","description":"Set on/off or brightness of one explicitly enrolled local light, with verified state readback.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"on":{"type":"boolean"},"brightness":{"type":"number","minimum":0,"maximum":100}},"required":["id"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true}}));
    }
    tools.extend(catalog.actions.iter().map(|a| json!({"name":a.id,"description":a.description,"inputSchema":a.input_schema,"annotations":{"readOnlyHint":false,"idempotentHint":true}})));
    json!({"tools":tools})
}

pub fn mcp_call(hue: &Hue, name: &str, arguments: Value) -> Result<Value> {
    if name == "hue_status" {
        ensure!(
            arguments.as_object().is_some_and(|a| a.is_empty()),
            "status takes no arguments"
        );
        return Ok(hue.observe()?);
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Scope {
        zone: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Light {
        id: String,
        zone: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SetLight {
        id: String,
        on: Option<bool>,
        brightness: Option<f64>,
        zone: Option<String>,
    }
    let (action_id, arguments) = match name {
        "hue_list_lights" | "hue_list_scenes" => {
            let scope: Scope = serde_json::from_value(arguments)?;
            unscoped(scope.zone)?;
            return if name == "hue_list_lights" {
                Ok(
                    json!({"data":hue.api("light",None,None)?,"errors":[],"scope":{"kind":"whole_home"}}),
                )
            } else {
                let scenes = hue.api("scene", None, None)?;
                let scenes = scenes.as_array().context("missing scenes")?.iter().map(|scene| json!({
                    "id":scene["id"],"name":scene.pointer("/metadata/name"),"group":scene["group"]
                })).collect::<Vec<_>>();
                Ok(json!({"scenes":scenes,"errors":[],"scope":{"kind":"whole_home"}}))
            };
        }
        "hue_get_light" => {
            let light: Light = serde_json::from_value(arguments)?;
            unscoped(light.zone)?;
            return Ok(json!({"data":hue.api("light",Some(&light.id),None)?,"errors":[]}));
        }
        "hue_set_light" => {
            let light: SetLight = serde_json::from_value(arguments)?;
            unscoped(light.zone)?;
            ensure!(valid_id(&light.id), "invalid light id");
            let mut arguments = serde_json::Map::new();
            if let Some(on) = light.on {
                arguments.insert("on".into(), json!(on));
            }
            if let Some(brightness) = light.brightness {
                arguments.insert("brightness".into(), json!(brightness));
            }
            // Validate before observing or dispatching; the normal executor then
            // enforces enrollment, revisions, resource capabilities and readback.
            light_update(&serde_json::from_value(json!(arguments))?)?;
            (light_action_id(&light.id), json!(arguments))
        }
        _ => (name.to_owned(), arguments),
    };
    let catalog = hue.catalog()?;
    let request = ExecuteRequest {
        dcp_version: dcp::VERSION.into(),
        request_id: format!("mcp-{}", timestamp()?),
        decision_id: "mcp-explicit-call".into(),
        action_id,
        arguments: serde_json::from_value(arguments)?,
        phase: Phase::Commit,
        expected_catalog_revision: catalog.catalog_revision,
        expected_state_revision: catalog.state_revision,
        prepared_receipt_id: None,
        confirmed: false,
        evidence: dcp::Evidence {
            utterance_id: "mcp-explicit-call".into(),
            transcript_revision: 1,
            final_: true,
        },
    };
    let receipt = hue.execute(&request)?;
    if receipt.outcome == Outcome::Rejected {
        bail!("Hue rejected tool call: {:?}", receipt.error);
    }
    Ok(serde_json::to_value(receipt)?)
}

fn light_action_id(id: &str) -> String {
    format!("light.{id}.set")
}

fn unscoped(zone: Option<String>) -> Result<()> {
    ensure!(
        zone.is_none(),
        "local Hue room scope requires a verified place binding; refusing to widen the request"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn located_state() -> Value {
        json!({
            "lights":[
                {"id":"00000000-0000-0000-0000-000000000001","metadata":{"name":"Desk lamp"},"owner":{"rtype":"device","rid":"device-a"},"on":{"on":true}},
                {"id":"00000000-0000-0000-0000-000000000002","metadata":{"name":"Bedroom lamp"},"owner":{"rtype":"device","rid":"device-b"},"on":{"on":false}},
                {"id":"00000000-0000-0000-0000-000000000003","metadata":{"name":"Bedroom unassigned"},"owner":{"rtype":"device","rid":"unassigned"}}
            ],
            "rooms":[
                {"id":"00000000-0000-0000-0000-000000000010","metadata":{"name":"Lab"},"children":[{"rtype":"device","rid":"device-a"},{"rtype":"device","rid":"device-b"}]}
            ],
            "zones":[
                {"id":"00000000-0000-0000-0000-000000000011","metadata":{"name":"Evening lights"},"children":[{"rtype":"device","rid":"device-a"}]}
            ],"controller_location":"Living room"
        })
    }
    #[test]
    fn many_lights_share_room_without_using_names_or_controller_placement() {
        let state = located_state();
        for light in state["lights"].as_array().unwrap().iter().take(2) {
            let context = location_context(&state, light).unwrap();
            assert!(context.contains("Physical room: Lab"));
            assert!(!context.contains("Living room"));
            assert!(!context.contains("Physical room: Bedroom"));
        }
        let unknown = location_context(&state, &state["lights"][2]).unwrap();
        assert!(unknown.contains("Physical room: unknown"));
    }
    #[test]
    fn zones_are_not_rooms_and_reference_types_must_match() {
        let mut state = located_state();
        state["rooms"] = json!([]);
        let context = location_context(&state, &state["lights"][0]).unwrap();
        assert!(context.contains("Logical zone: Evening lights"));
        assert!(context.contains("Physical room: unknown"));
        state["zones"][0]["children"][0]["rtype"] = json!("scene");
        assert!(
            !location_context(&state, &state["lights"][0])
                .unwrap()
                .contains("Evening lights")
        );
    }
    #[test]
    fn catalog_preserves_allowlist_and_tracks_room_changes() {
        let hue = Hue {
            client: Client::new(),
            base: reqwest::Url::parse("https://bridge.invalid").unwrap(),
            key_file: PathBuf::from("unused-test-key"),
            allowed: ["00000000-0000-0000-0000-000000000001".into()]
                .into_iter()
                .collect(),
        };
        let mut state = located_state();
        let before = hue.catalog_for(state.clone()).unwrap();
        assert_eq!(before.actions.len(), 1);
        assert!(before.actions[0].description.contains("Physical room: Lab"));
        assert!(!before.actions[0].description.contains("device-a"));
        state["rooms"][0]["metadata"]["name"] = json!("Bedroom");
        let after = hue.catalog_for(state).unwrap();
        assert_ne!(before.catalog_revision, after.catalog_revision);
        assert_ne!(before.state_revision, after.state_revision);
        assert_eq!(before.actions[0].id, after.actions[0].id);
        assert!(
            after.actions[0]
                .description
                .contains("Physical room: Bedroom")
        );
    }
    #[test]
    fn malformed_or_oversized_location_evidence_fails() {
        let mut state = located_state();
        state["rooms"][0]["id"] = json!("../invented");
        assert!(location_context(&state, &state["lights"][0]).is_err());
        state = located_state();
        state["rooms"][0]["metadata"]["name"] = json!("x".repeat(257));
        assert!(location_context(&state, &state["lights"][0]).is_err());
    }
    #[test]
    fn bounds_and_types_are_enforced() {
        for value in [
            json!({}),
            json!({"brightness":101}),
            json!({"brightness":-1}),
            json!({"on":"false"}),
            json!({"id":"invented"}),
        ] {
            assert!(light_update(&serde_json::from_value(value).unwrap()).is_err());
        }
        assert_eq!(
            light_update(&serde_json::from_value(json!({"on":false,"brightness":42})).unwrap())
                .unwrap(),
            json!({"on":{"on":false},"dimming":{"brightness":42.0}})
        );
    }
    #[test]
    fn readback_is_not_a_success_on_mismatch() {
        assert!(
            verify_update(
                &json!({"on":{"on":true}}),
                &serde_json::from_value(json!({"on":false})).unwrap()
            )
            .is_err()
        );
    }
    #[test]
    fn path_segments_cannot_escape_resource() {
        assert!(!valid_id("../scene/anything"));
        assert!(valid_id("00000000-0000-0000-0000-000000000001"));
    }
}
