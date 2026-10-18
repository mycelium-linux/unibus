#![cfg(feature = "fleet-identity")]
use mycelium_peer_protocol::{AccessRecord, AccessStatement};
use serde_json::json;
use std::{
    net::UdpSocket,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use unibus::PeerBinding;
use unibus_iroh::grant::{Permit, TrustDocument, permit_scope};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn port() -> std::net::SocketAddr {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

#[test]
fn authorized_loopback_udp_round_trip_without_relay() {
    let dir = tempfile::tempdir().unwrap();
    let a = iroh::SecretKey::generate();
    let b = iroh::SecretKey::generate();
    let service = UdpSocket::bind("127.0.0.1:0").unwrap();
    service
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let local = port();
    let server_endpoint = port();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let permit = Permit {
        audience: "test".into(),
        installation: "test".into(),
        initiator: PeerBinding {
            producer_id: "client".into(),
            key_binding: format!("iroh:{}", a.public()),
            candidates: vec![],
        },
        responder: PeerBinding {
            producer_id: "server".into(),
            key_binding: format!("iroh:{}", b.public()),
            candidates: vec![],
        },
        kinds: vec!["unibus.transport.udp.v1".into()],
        max_payload_bytes: 65507,
    };
    let statement: AccessStatement = serde_json::from_value(json!({"kind":"grant","grant_id":"udp-test","principal":"test","serial":1,
        "roles":[],"scopes":[permit_scope(&permit).unwrap()],"not_before":now-1,"not_after":now+600})).unwrap();
    let issuer = ed25519_dalek::SigningKey::from_bytes(&[91; 32]);
    let grant = AccessRecord::sign(&issuer, statement).unwrap();
    let signer = grant.signer.clone();
    let trust = dir.path().join("trust.json");
    std::fs::write(
        &trust,
        serde_json::to_vec(&TrustDocument {
            grant,
            revocations: vec![],
            permit: permit.clone(),
        })
        .unwrap(),
    )
    .unwrap();
    let mut processes = Vec::new();
    for (role, producer, key, udp, bind) in [
        (
            "server",
            "server",
            b,
            service.local_addr().unwrap(),
            server_endpoint,
        ),
        ("client", "client", a, local, port()),
    ] {
        let key_path = dir.path().join(format!("{role}.key"));
        std::fs::write(&key_path, key.to_bytes()).unwrap();
        let path = dir.path().join(format!("{role}.json"));
        let remote = if role == "client" {
            json!({"endpoint_id":permit.responder.key_binding.strip_prefix("iroh:").unwrap(),"direct_addresses":[server_endpoint],"relay_urls":[]})
        } else {
            serde_json::Value::Null
        };
        std::fs::write(&path,serde_json::to_vec(&json!({"role":role,"producer":producer,"key_file":key_path,"trust_file":trust,"issuer_keys":[signer],"relay_urls":[],"udp":udp,"iroh_bind":bind,"remote":remote})).unwrap()).unwrap();
        processes.push(Process(
            Command::new(env!("CARGO_BIN_EXE_unibus-iroh-udp"))
                .arg(path)
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        ));
    }
    for _ in 0..2 {
        // A fresh source port models restarting the legacy adapter while the
        // transport service remains alive. It must recover without a four-minute
        // wait for the old service session to expire.
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(12);
        let mut packet = [0u8; 2048];
        loop {
            assert!(
                Instant::now() < deadline,
                "bridge never delivered an authorized packet"
            );
            client.send_to(b"encrypted legacy packet", local).unwrap();
            if let Ok((len, source)) = service.recv_from(&mut packet) {
                assert_eq!(&packet[..len], b"encrypted legacy packet");
                service.send_to(&packet[..len], source).unwrap();
                if let Ok((len, _)) = client.recv_from(&mut packet) {
                    assert_eq!(&packet[..len], b"encrypted legacy packet");
                    break;
                }
            }
        }
    }
    for process in &mut processes {
        assert!(process.0.try_wait().unwrap().is_none());
    }
}
