use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Child, Command, Stdio},
    time::Duration,
};
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn fixture() -> (tempfile::TempDir, std::path::PathBuf, u16) {
    let dir = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = dir.path().join("provider-token");
    std::fs::write(&token, "01234567890123456789012345678901").unwrap();
    let config = dir.path().join("config.json");
    std::fs::write(&config,json!({"bridge_url":"http://127.0.0.1:1/","application_key_file":dir.path().join("missing-application-key"),"bridge_ca_file":null,"provider_token_file":token,"allowed_light_ids":[],"listen":format!("127.0.0.1:{port}")}).to_string()).unwrap();
    (dir, config, port)
}
#[test]
fn stdio_initializes_without_network_and_reports_discovery_failure() {
    let (_dir, config, _) = fixture();
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_unibus-hue"))
            .arg("stdio")
            .arg(config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut stdin = child.0.stdin.take().unwrap();
    for (id, method) in [(1, "initialize"), (2, "tools/list"), (3, "ping")] {
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":{}})
        )
        .unwrap();
    }
    drop(stdin);
    let output = child.0.stdout.take().unwrap();
    let replies: Vec<Value> = std::io::BufRead::lines(std::io::BufReader::new(output))
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect();
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "unibus-hue");
    assert!(replies[1].get("error").is_some());
    assert_eq!(replies[2]["result"], json!({}));
    assert!(child.0.wait().unwrap().success());
}
#[test]
fn http_authentication_version_and_mcp_initialization() {
    let (_dir, config, port) = fixture();
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_unibus-hue"))
            .arg("serve")
            .arg(config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let mut ready = false;
    for _ in 0..100 {
        if client.get(format!("{base}/.well-known/dcp")).send().is_ok() {
            ready = true;
            break;
        }
        assert!(child.0.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ready, "edge did not start");
    assert_eq!(
        client
            .get(format!("{base}/.well-known/dcp"))
            .send()
            .unwrap()
            .status(),
        401
    );
    let token = "01234567890123456789012345678901";
    let response = client
        .get(format!("{base}/.well-known/dcp"))
        .bearer_auth(token)
        .send()
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["dcp-version"], "0.1");
    let discovery: dcp::Discovery = response.json().unwrap();
    assert_eq!(discovery.provider.id, "philips-hue");
    assert_eq!(
        client
            .get(format!("{base}/v1/decisions"))
            .bearer_auth(token)
            .send()
            .unwrap()
            .status(),
        400
    );
    let rpc: Value = client
        .post(format!("{base}/mcp"))
        .bearer_auth(token)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(rpc["result"]["serverInfo"]["name"], "unibus-hue");
    let denied = client
        .post(format!("{base}/mcp"))
        .bearer_auth("wrong-token")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .unwrap();
    assert_eq!(denied.status(), 401);
}
