//! Explicit loopback UDP compatibility for legacy encrypted Unibus adapters.
//! This is not an IP router or an arbitrary-target relay.
use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, RelayMap, RelayMode, SecretKey};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{net::UdpSocket, sync::Mutex};
use unibus::{Envelope, IrohPeerEndpoint, Message, PeerCandidate};
use unibus_iroh::{
    grant::GrantVerifier,
    transport::{IrohListener, IrohSession, IrohTransport, SessionConfig},
};

const KIND: &str = "unibus.transport.udp.v1";
const PACKET_LIMIT: usize = 65507;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    role: String,
    producer: String,
    key_file: PathBuf,
    trust_file: PathBuf,
    issuer_keys: BTreeSet<String>,
    relay_urls: BTreeSet<String>,
    #[serde(default = "default_bind")]
    iroh_bind: SocketAddr,
    /// Only a local socket; the bridge cannot forward arbitrary network targets.
    udp: SocketAddr,
    #[serde(default)]
    remote: Option<IrohPeerEndpoint>,
}

fn default_bind() -> SocketAddr {
    "0.0.0.0:0".parse().expect("static bind")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_secs()
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() == 2 && args[0] == "keygen" {
        use std::io::Write;
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let key = SecretKey::generate();
        options.open(&args[1])?.write_all(&key.to_bytes())?;
        println!("{}", key.public());
        return Ok(());
    }
    ensure!(
        args.len() == 1,
        "usage: unibus-iroh-udp CONFIG.json | keygen KEY_FILE"
    );
    let bytes = std::fs::read(&args[0])?;
    ensure!(bytes.len() <= 65536, "configuration exceeds bound");
    let config: Config = serde_json::from_slice(&bytes)?;
    ensure!(
        config.udp.ip().is_loopback() && config.udp.port() != 0,
        "UDP endpoint must be explicit loopback"
    );
    ensure!(
        matches!(config.role.as_str(), "client" | "server"),
        "invalid role"
    );
    ensure!(config.relay_urls.len() <= 4, "too many explicit relays");
    let key: [u8; 32] = std::fs::read(&config.key_file)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("endpoint key must be 32 bytes"))?;
    let key = SecretKey::from_bytes(&key);
    let verifier = Arc::new(GrantVerifier::new(
        config.trust_file.clone(),
        config.issuer_keys.clone(),
    )?);
    let (permit, _, _, _) = verifier.permit(now())?;
    let local = if config.role == "client" {
        &permit.initiator
    } else {
        &permit.responder
    };
    ensure!(
        local.producer_id == config.producer
            && local.key_binding == format!("iroh:{}", key.public()),
        "local endpoint differs from signed service permit"
    );
    ensure!(
        permit.kinds == vec![KIND.to_owned()] && permit.max_payload_bytes == PACKET_LIMIT as u32,
        "unexpected packet service scope"
    );
    let mut relays: Vec<iroh::RelayUrl> = Vec::new();
    for relay in &config.relay_urls {
        let url = url::Url::parse(relay)?;
        ensure!(
            url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "unsafe relay URL"
        );
        relays.push(relay.parse()?);
    }
    let relay_mode = if relays.is_empty() {
        RelayMode::Disabled
    } else {
        RelayMode::Custom(RelayMap::from_iter(relays))
    };
    let endpoint = Endpoint::builder(iroh::endpoint::presets::Minimal)
        .secret_key(key)
        .relay_mode(relay_mode)
        .bind_addr(config.iroh_bind)?
        .alpns(vec![unibus_iroh::PEER_ALPN.as_bytes().to_vec()])
        .bind()
        .await?;
    let session_config = SessionConfig {
        verifier: verifier.clone(),
        audience: permit.audience.clone(),
        installation_id: permit.installation.clone(),
        local_producer: config.producer.clone(),
        allowed_relays: config.relay_urls.clone(),
        access_policy: Some(verifier.clone()),
        endpoint_identity: None,
    };
    eprintln!(
        "Unibus Iroh UDP {} endpoint={} local={}",
        config.role,
        endpoint.id(),
        config.udp
    );
    if config.role == "server" {
        let listener = IrohListener::new(endpoint, session_config);
        // Deliberately one admitted consumer per compatibility instance.
        loop {
            match listener.accept().await {
                Ok(session) => {
                    if let Err(error) = bridge(
                        &session,
                        &config,
                        &permit.installation,
                        &permit.initiator.producer_id,
                    )
                    .await
                    {
                        eprintln!("Unibus Iroh UDP session ended: {error:#}");
                    }
                }
                Err(error) => {
                    eprintln!("Unibus Iroh UDP admission failed: {error}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    } else {
        let remote = config
            .remote
            .clone()
            .context("client requires remote endpoint hints")?;
        ensure!(
            permit.responder.key_binding == format!("iroh:{}", remote.endpoint_id),
            "remote differs from service permit"
        );
        let transport = IrohTransport::new(endpoint, session_config);
        loop {
            let candidate = PeerCandidate {
                peer_id: permit.responder.producer_id.clone(),
                site: None,
                endpoints: vec![],
                iroh_endpoints: vec![remote.clone()],
                expires_at_unix_s: now() + 30,
            };
            let result = async {
                let assertion = verifier.assertion(now())?;
                let session = transport.connect_session(&candidate, &assertion).await?;
                bridge(
                    &session,
                    &config,
                    &permit.installation,
                    &permit.responder.producer_id,
                )
                .await
            }
            .await;
            if let Err(error) = result {
                eprintln!("Unibus Iroh UDP reconnecting: {error:#}");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn bridge(
    session: &IrohSession,
    config: &Config,
    installation: &str,
    remote: &str,
) -> Result<()> {
    let socket = if config.role == "client" {
        UdpSocket::bind(config.udp).await?
    } else {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        socket.connect(config.udp).await?;
        socket
    };
    let source = Mutex::new(None::<SocketAddr>);
    let envelope = || {
        Envelope::new(
            installation,
            &config.producer,
            Some(remote),
            "",
            Message::new(KIND, serde_json::json!({})),
        )
    };
    let transmit = async {
        let mut channel = session.open_channel().await?;
        let mut packet = vec![0; PACKET_LIMIT + 1];
        loop {
            // Empty frames keep an idle channel alive without touching UDP.
            let length =
                match tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut packet))
                    .await
                {
                    Err(_) => 0,
                    Ok(result) => {
                        let (length, address) = result?;
                        ensure!(
                            length <= PACKET_LIMIT && address.ip().is_loopback(),
                            "invalid local UDP packet"
                        );
                        if config.role == "client" {
                            let mut source = source.lock().await;
                            ensure!(
                                !source.is_some_and(|old| old != address),
                                "local UDP producer changed; reopen bounded session"
                            );
                            *source = Some(address);
                        }
                        length
                    }
                };
            let mut envelope = envelope();
            envelope.payload_bytes = length;
            channel.send(&envelope, &packet[..length]).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let receive = async {
        let mut channel = session.accept_channel().await?;
        loop {
            let (envelope, packet) = channel.receive().await?;
            ensure!(
                envelope.message.kind == KIND && packet.len() <= PACKET_LIMIT,
                "unexpected service frame"
            );
            if packet.is_empty() {
                continue;
            }
            if config.role == "server" {
                socket.send(&packet).await?;
            } else if let Some(address) = *source.lock().await {
                socket.send_to(&packet, address).await?;
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! { result = transmit => result, result = receive => result }
}
