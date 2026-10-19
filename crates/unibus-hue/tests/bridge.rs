use serde_json::{Value, json};
use std::collections::BTreeSet;
use unibus_hue::{Config, Hue, validate_request};
const ID: &str = "00000000-0000-0000-0000-000000000001";
fn config(url: String, key: std::path::PathBuf) -> Config {
    Config {
        bridge_url: url,
        application_key_file: key,
        bridge_ca_file: None,
        bridge_certificate_file: None,
        bridge_id: None,
        provider_token_file: "unused".into(),
        allowed_light_ids: BTreeSet::from([ID.into()]),
        listen: "127.0.0.1:18815".into(),
    }
}
fn state() -> Value {
    json!({"lights":[{"id":ID,"metadata":{"name":"Desk"},"on":{"on":true},"dimming":{"brightness":50}}],"scenes":[],"rooms":[],"zones":[]})
}
fn request(catalog: &dcp::Catalog) -> dcp::ExecuteRequest {
    dcp::ExecuteRequest {
        dcp_version: dcp::VERSION.into(),
        request_id: "request-1".into(),
        decision_id: "decision-1".into(),
        action_id: format!("light.{ID}.set"),
        arguments: serde_json::from_value(json!({"on":false})).unwrap(),
        phase: dcp::Phase::Commit,
        expected_catalog_revision: catalog.catalog_revision.clone(),
        expected_state_revision: catalog.state_revision.clone(),
        prepared_receipt_id: None,
        confirmed: false,
        evidence: dcp::Evidence {
            utterance_id: "utterance-1".into(),
            transcript_revision: 1,
            final_: true,
        },
    }
}
#[test]
fn catalog_enrollment_and_revision_guards() {
    let mut config = config("http://127.0.0.1/".into(), "unused".into());
    let hue = Hue::connect(&config).unwrap();
    let catalog = hue.catalog_for(state()).unwrap();
    assert_eq!(catalog.actions.len(), 1);
    assert!(catalog.actions[0].description.contains("Desk"));
    let mut req = request(&catalog);
    assert!(validate_request(&req, &catalog).is_ok());
    req.evidence.final_ = false;
    assert_eq!(
        validate_request(&req, &catalog).unwrap_err().0,
        dcp::ErrorCode::FinalityRequired
    );
    req.evidence.final_ = true;
    req.expected_state_revision = "old".into();
    assert_eq!(
        validate_request(&req, &catalog).unwrap_err().0,
        dcp::ErrorCode::StaleState
    );
    req.expected_state_revision = catalog.state_revision.clone();
    req.action_id = "shell.exec".into();
    assert_eq!(
        validate_request(&req, &catalog).unwrap_err().0,
        dcp::ErrorCode::ActionNotFound
    );
    config.allowed_light_ids.clear();
    assert!(
        Hue::connect(&config)
            .unwrap()
            .catalog_for(state())
            .unwrap()
            .actions
            .is_empty()
    );
}
#[test]
fn network_origin_and_enrollment_fail_closed() {
    let mut config = config("http://192.168.10.15/".into(), "unused".into());
    assert!(Hue::connect(&config).is_err());
    config.bridge_url = "https://user:password@example.org/".into();
    assert!(Hue::connect(&config).is_err());
    config.bridge_url = "https://example.org/".into();
    config.allowed_light_ids.insert("../escape".into());
    assert!(Hue::connect(&config).is_err());
}

#[test]
fn explicit_pairing_requires_button_saves_private_key_and_never_replaces_it() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("application-key");
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let hue = Hue::connect(&config(
        format!("http://{}/", server.server_addr()),
        key.clone(),
    ))
    .unwrap();
    let peer = std::thread::spawn(move || {
        for body in [
            json!([{"error":{"type":101,"description":"link button not pressed"}}]),
            json!([{"success":{"username":"fixture-paired-key-123456789"}}]),
        ] {
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert_eq!(request.method().as_str(), "POST");
            assert_eq!(request.url(), "/api");
            assert!(
                !request
                    .headers()
                    .iter()
                    .any(|h| h.field.equiv("hue-application-key"))
            );
            let args: Value = serde_json::from_reader(request.as_reader()).unwrap();
            assert_eq!(args, json!({"devicetype":"unibus-hue#jarvis"}));
            request
                .respond(tiny_http::Response::from_string(body.to_string()))
                .unwrap();
        }
    });
    assert!(
        hue.pair()
            .unwrap_err()
            .to_string()
            .contains("physical Hue bridge button")
    );
    assert!(!key.exists());
    hue.pair().unwrap();
    assert_eq!(
        std::fs::read_to_string(&key).unwrap(),
        "fixture-paired-key-123456789"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o077,
            0
        );
    }
    peer.join().unwrap();
    assert!(
        hue.pair()
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );
    assert_eq!(
        std::fs::read_to_string(&key).unwrap(),
        "fixture-paired-key-123456789"
    );
}

#[test]
fn delegated_tools_advertise_only_supported_enrolled_writes() {
    let mut cfg = config("http://127.0.0.1/".into(), "unused".into());
    let hue = Hue::connect(&cfg).unwrap();
    let tools = unibus_hue::mcp_tools(&hue.catalog_for(state()).unwrap());
    let tools = tools["tools"].as_array().unwrap();
    assert!(
        tools
            .iter()
            .any(|t| t["name"] == "hue_set_light" && t["annotations"]["readOnlyHint"] == false)
    );
    assert!(!tools.iter().any(|t| t["name"] == "hue_activate_scene"));
    cfg.allowed_light_ids.clear();
    let hue = Hue::connect(&cfg).unwrap();
    let tools = unibus_hue::mcp_tools(&hue.catalog_for(state()).unwrap());
    assert!(
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["annotations"]["readOnlyHint"] == true)
    );
}

#[test]
fn delegated_arguments_reject_bad_targets_and_never_widen_room_scope() {
    let hue = Hue::connect(&config("http://127.0.0.1:1/".into(), "missing".into())).unwrap();
    for (tool, arguments, expected) in [
        (
            "hue_set_light",
            json!({"id":"../escape","on":false}),
            "invalid light id",
        ),
        (
            "hue_set_light",
            json!({"id":ID,"on":false,"shell":"oops"}),
            "unknown field",
        ),
        (
            "hue_set_light",
            json!({"id":ID,"on":false,"zone":"Lab"}),
            "verified place binding",
        ),
        (
            "hue_list_lights",
            json!({"zone":"Lab"}),
            "verified place binding",
        ),
        (
            "hue_set_light",
            json!({"id":ID,"brightness":101}),
            "brightness",
        ),
        ("hue_set_light", json!({"id":ID}), "empty"),
    ] {
        let error = unibus_hue::mcp_call(&hue, tool, arguments)
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{tool}: {error}");
    }
}

#[test]
fn delegated_reads_use_the_existing_bridge_contract_without_writes() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let key = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(key.path(), "test-application-key").unwrap();
    let hue = Hue::connect(&config(
        format!("http://{}/", server.server_addr()),
        key.path().into(),
    ))
    .unwrap();
    let worker = std::thread::spawn(move || {
        for _ in 0..3 {
            let req = server
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert_eq!(req.method().as_str(), "GET");
            let data = if req.url().ends_with("scene") {
                json!([{"id":ID,"metadata":{"name":"Evening"},"group":{"rid":"room","rtype":"room"}}])
            } else {
                state()["lights"].clone()
            };
            req.respond(tiny_http::Response::from_string(
                json!({"errors":[],"data":data}).to_string(),
            ))
            .unwrap();
        }
    });
    let lights = unibus_hue::mcp_call(&hue, "hue_list_lights", json!({})).unwrap();
    assert_eq!(lights["data"][0]["id"], ID);
    assert_eq!(lights["scope"]["kind"], "whole_home");
    let light = unibus_hue::mcp_call(&hue, "hue_get_light", json!({"id":ID})).unwrap();
    assert_eq!(light["data"][0]["on"]["on"], true);
    let scenes = unibus_hue::mcp_call(&hue, "hue_list_scenes", json!({})).unwrap();
    assert_eq!(scenes["scenes"][0]["name"], "Evening");
    worker.join().unwrap();
}
#[test]
fn mock_bridge_put_requires_successful_readback() {
    mock_verified_write(false);
}

#[test]
fn delegated_mcp_write_uses_the_same_enrollment_and_readback_executor() {
    mock_verified_write(true);
}

fn mock_verified_write(delegated: bool) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", server.server_addr());
    let key = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(key.path(), "test-application-key").unwrap();
    let hue = Hue::connect(&config(url, key.path().into())).unwrap();
    let worker = std::thread::spawn(move || {
        let mut on = true;
        let mut puts = 0;
        // One catalog fetch, then execute's fresh catalog, PUT/readback, resulting catalog.
        for _ in 0..14 {
            let mut req = server
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .expect("expected bridge request");
            assert_eq!(
                req.headers()
                    .iter()
                    .find(|h| h.field.equiv("hue-application-key"))
                    .unwrap()
                    .value
                    .as_str(),
                "test-application-key"
            );
            let light = json!({"id":ID,"metadata":{"name":"Desk"},"on":{"on":on},"dimming":{"brightness":50}});
            let data = if req.method().as_str() == "PUT" {
                assert_eq!(req.url(), format!("/clip/v2/resource/light/{ID}"));
                let mut body = String::new();
                req.as_reader().read_to_string(&mut body).unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&body).unwrap(),
                    json!({"on":{"on":false}})
                );
                on = false;
                puts += 1;
                json!([{"rid":ID,"rtype":"light"}])
            } else if req.url().starts_with("/clip/v2/resource/light") {
                json!([light])
            } else {
                json!([])
            };
            req.respond(
                tiny_http::Response::from_string(json!({"errors":[],"data":data}).to_string())
                    .with_header(
                        tiny_http::Header::from_bytes("content-type", "application/json").unwrap(),
                    ),
            )
            .unwrap();
        }
        assert_eq!(puts, 1);
    });
    let previous_revision = hue.catalog_for(state()).unwrap().state_revision;
    let receipt: dcp::Receipt = if delegated {
        serde_json::from_value(
            unibus_hue::mcp_call(&hue, "hue_set_light", json!({"id":ID,"on":false})).unwrap(),
        )
        .unwrap()
    } else {
        let catalog = hue.catalog().unwrap();
        hue.execute(&request(&catalog)).unwrap()
    };
    assert_eq!(receipt.outcome, dcp::Outcome::Applied);
    assert_ne!(
        receipt.resulting_state_revision.as_ref(),
        Some(&previous_revision)
    );
    worker.join().unwrap();
}
