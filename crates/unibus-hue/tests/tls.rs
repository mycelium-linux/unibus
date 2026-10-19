use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    net::TcpListener,
    sync::Arc,
    time::Duration,
};
use unibus_hue::{Config, Hue};

const BRIDGE: &str = "ECB5FAFFFE134F06";

fn certificate(expired: bool) -> (rcgen::Certificate, KeyPair) {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, BRIDGE.to_lowercase());
    if expired {
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        params.not_after = rcgen::date_time_ymd(2001, 1, 1);
    }
    let key = KeyPair::generate().unwrap();
    (params.self_signed(&key).unwrap(), key)
}

fn config(dir: &std::path::Path, url: String) -> Config {
    let key = dir.join("application-key");
    std::fs::write(&key, "fixture-application-key").unwrap();
    Config {
        bridge_url: url,
        application_key_file: key,
        bridge_ca_file: None,
        bridge_certificate_file: Some(dir.join("bridge.pem")),
        bridge_id: Some(BRIDGE.into()),
        provider_token_file: dir.join("unused"),
        allowed_light_ids: BTreeSet::new(),
        listen: "127.0.0.1:0".into(),
    }
}

// Returns the plaintext HTTP request, if and only if certificate and handshake
// verification allowed the client to send it. Rejection must disclose no key.
fn server(cert: &rcgen::Certificate, key: &KeyPair) -> (String, std::thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("https://{}/", listener.local_addr().unwrap());
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
    let task = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut stream = StreamOwned::new(ServerConnection::new(Arc::new(config)).unwrap(), socket);
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while request.len() < 8192 && !request.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => request.push(byte[0]),
                _ => return request,
            }
        }
        let body = r#"{"data":[],"errors":[]}"#;
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        stream.flush().unwrap();
        request
    });
    (url, task)
}

#[test]
fn enrolled_cn_only_certificate_allows_authenticated_read() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = certificate(false);
    std::fs::write(dir.path().join("bridge.pem"), cert.pem()).unwrap();
    let (url, peer) = server(&cert, &key);
    let hue = Hue::connect(&config(dir.path(), url)).unwrap();
    let result = unibus_hue::mcp_call(&hue, "hue_list_lights", serde_json::json!({})).unwrap();
    assert_eq!(result["data"], serde_json::json!([]));
    let request = String::from_utf8(peer.join().unwrap())
        .unwrap()
        .to_lowercase();
    assert!(request.starts_with("get /clip/v2/resource/light "));
    assert!(request.contains("hue-application-key: fixture-application-key"));
}

#[test]
fn different_certificate_is_rejected_before_sending_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let (enrolled, _) = certificate(false);
    let (impostor, key) = certificate(false);
    std::fs::write(dir.path().join("bridge.pem"), enrolled.pem()).unwrap();
    let (url, peer) = server(&impostor, &key);
    let hue = Hue::connect(&config(dir.path(), url)).unwrap();
    assert!(unibus_hue::mcp_call(&hue, "hue_list_lights", serde_json::json!({})).is_err());
    assert!(peer.join().unwrap().is_empty());
}

#[test]
fn default_hostname_verification_does_not_accept_a_cn_only_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = certificate(false);
    std::fs::write(dir.path().join("bridge.pem"), cert.pem()).unwrap();
    let (url, peer) = server(&cert, &key);
    let mut cfg = config(dir.path(), url);
    cfg.bridge_ca_file = cfg.bridge_certificate_file.take();
    let hue = Hue::connect(&cfg).unwrap();
    assert!(unibus_hue::mcp_call(&hue, "hue_list_lights", serde_json::json!({})).is_err());
    assert!(peer.join().unwrap().is_empty());
}

#[test]
fn enrollment_rejects_expired_wrong_identity_and_ambiguous_trust() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), "https://127.0.0.1/".into());
    let (cert, _) = certificate(true);
    std::fs::write(dir.path().join("bridge.pem"), cert.pem()).unwrap();
    assert!(
        Hue::connect(&cfg)
            .err()
            .unwrap()
            .to_string()
            .contains("not currently valid")
    );
    let (cert, _) = certificate(false);
    std::fs::write(dir.path().join("bridge.pem"), cert.pem()).unwrap();
    cfg.bridge_id = Some("0000000000000000".into());
    assert!(
        Hue::connect(&cfg)
            .err()
            .unwrap()
            .to_string()
            .contains("bridge ID")
    );
    cfg.bridge_id = None;
    assert!(Hue::connect(&cfg).is_err());
    cfg.bridge_id = Some(BRIDGE.into());
    cfg.bridge_ca_file = cfg.bridge_certificate_file.clone();
    assert!(Hue::connect(&cfg).is_err());
}
