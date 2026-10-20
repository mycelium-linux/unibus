//! A bounded transport adapter. Mycelium owns access; the HTTP library owns reads.
use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, RelayMap, RelayMode, SecretKey};
use iroh_webrtc_transport::{NativeWebRtcIrohNode, WebRtcNodeConfig, config::WebRtcIceConfig};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const ALPN: &[u8] = b"fpl/ipfs-read/1";
const MAX_RANGE: u64 = 8 * 1024 * 1024;
const MAX_CONTROL: usize = 64 * 1024;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    node: String,
    key_file: PathBuf,
    endpoint_file: PathBuf,
    relay_url: String,
    #[serde(default)]
    stun_urls: Vec<String>,
    #[serde(default)]
    exclude_tailnet_candidates: bool,
    library_url: String,
    library_token_file: PathBuf,
    authority_url: String,
    authority_token_file: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    session: String,
    offset: u64,
    length: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Authorization {
    cid: String,
    path: String,
    size: u64,
    offset: u64,
    length: u64,
    expires_at: u64,
}
struct App {
    config: Config,
    http: reqwest::Client,
    authority_token: String,
    library_token: String,
    streams: tokio::sync::Semaphore,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs()
}
fn loopback(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none(),
        "upstream must be bare loopback HTTP"
    );
    Ok(url)
}
fn token(path: &Path) -> Result<String> {
    let value = std::fs::read_to_string(path)?.trim().to_string();
    ensure!(
        (32..=256).contains(&value.len())
            && value.is_ascii()
            && !value.chars().any(char::is_whitespace),
        "invalid private service credential"
    );
    Ok(value)
}
async fn bounded(mut reply: reqwest::Response) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = reply.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= MAX_CONTROL,
            "control reply exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
fn validate(request: &Request, authorization: &Authorization, at: u64) -> Result<()> {
    ensure!(
        request.session.len() == 64 && request.session.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid session"
    );
    ensure!(
        authorization.offset == request.offset
            && authorization.length == request.length
            && request.length <= MAX_RANGE
            && authorization.size <= 4 * 1024 * 1024 * 1024
            && authorization.expires_at > at
            && request
                .offset
                .checked_add(request.length)
                .is_some_and(|end| end <= authorization.size),
        "authority response exceeds request scope"
    );
    ensure!(
        !authorization.cid.is_empty()
            && authorization.cid.len() <= 128
            && authorization.cid.bytes().all(|b| b.is_ascii_alphanumeric())
            && authorization.path.len() <= 1024,
        "invalid authorized resource"
    );
    Ok(())
}
async fn resource(
    app: &App,
    peer: &str,
    request: &Request,
) -> Result<(Authorization, reqwest::Response)> {
    ensure!(
        request.session.len() == 64
            && request.length <= MAX_RANGE
            && (request.length > 0 || request.offset == 0),
        "invalid range request"
    );
    let response = app.http.post(format!("{}/verify", app.config.authority_url.trim_end_matches('/')))
        .bearer_auth(&app.authority_token).timeout(Duration::from_secs(8))
        .json(&json!({"session":request.session,"endpoint_id":peer,"offset":request.offset,"length":request.length}))
        .send().await?.error_for_status()?;
    let auth: Authorization = serde_json::from_slice(&bounded(response).await?)?;
    validate(request, &auth, now())?;
    let mut url = loopback(&app.config.library_url)?;
    url.set_path(&format!("/ipfs/{}", auth.cid));
    if !auth.path.is_empty() {
        url.query_pairs_mut().append_pair("path", &auth.path);
    }
    let mut read = app
        .http
        .get(url)
        .bearer_auth(&app.library_token)
        .header("x-library-prefer-node", &app.config.node);
    if auth.length > 0 {
        read = read.header(
            "Range",
            format!("bytes={}-{}", auth.offset, auth.offset + auth.length - 1),
        );
    }
    let reply = read.send().await?.error_for_status()?;
    ensure!(
        reply.content_length() == Some(auth.length),
        "library range size differs from authorized request"
    );
    if auth.length > 0 {
        ensure!(
            reply.status() == reqwest::StatusCode::PARTIAL_CONTENT,
            "library did not honor byte range"
        );
        let expected = format!(
            "bytes {}-{}/{}",
            auth.offset,
            auth.offset + auth.length - 1,
            auth.size
        );
        ensure!(
            reply
                .headers()
                .get("content-range")
                .and_then(|h| h.to_str().ok())
                == Some(expected.as_str()),
            "library resource changed"
        );
    }
    Ok((auth, reply))
}
async fn write_header(send: &mut iroh::endpoint::SendStream, value: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    send.write_all(&bytes).await?;
    Ok(())
}
async fn handle(app: Arc<App>, connection: iroh::endpoint::Connection) -> Result<()> {
    let peer = connection.remote_id().to_string();
    loop {
        let (mut send, mut recv) =
            match tokio::time::timeout(Duration::from_secs(15), connection.accept_bi()).await? {
                Ok(pair) => pair,
                Err(_) => return Ok(()),
            };
        let _permit = match app.streams.try_acquire() {
            Ok(permit) => permit,
            Err(_) => {
                write_header(&mut send, json!({"ok":false,"error":"Direct service busy"})).await?;
                send.finish()?;
                continue;
            }
        };
        let request =
            tokio::time::timeout(Duration::from_secs(10), recv.read_to_end(MAX_CONTROL)).await??;
        let result = async {
            let request: Request = serde_json::from_slice(&request)?;
            resource(&app, &peer, &request).await
        }
        .await;
        let (auth, mut reply) = match result {
            Ok(read) => read,
            Err(error) => {
                eprintln!("direct request denied or unavailable: {error:#}");
                write_header(
                    &mut send,
                    json!({"ok":false,"error":"Read is unavailable or session expired"}),
                )
                .await?;
                send.finish()?;
                continue;
            }
        };
        write_header(
            &mut send,
            json!({"ok":true,"size":auth.size,"offset":auth.offset,"length":auth.length}),
        )
        .await?;
        let work = async {
            let mut remaining = auth.length;
            while let Some(bytes) = reply.chunk().await? {
                ensure!(
                    bytes.len() as u64 <= remaining,
                    "library stream exceeds requested range"
                );
                send.write_all(&bytes).await?;
                remaining -= bytes.len() as u64;
            }
            ensure!(remaining == 0, "library stream ended early");
            send.finish()?;
            Ok::<_, anyhow::Error>(())
        };
        if let Err(error) = tokio::time::timeout(Duration::from_secs(120), work)
            .await
            .context("range stream timed out")?
        {
            let _ = send.reset(1u32.into());
            return Err(error);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "keygen") {
        use std::io::Write;
        ensure!(args.len() == 2, "usage: ipfs-direct-provider keygen PATH");
        let key = SecretKey::generate();
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&args[1])?;
        let encoded: String = key.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        writeln!(file, "{encoded}")?;
        file.sync_all()?;
        println!("{}", key.public());
        return Ok(());
    }
    let config_path = std::env::args()
        .nth(1)
        .context("usage: ipfs-direct-provider CONFIG")?;
    let config: Config = serde_json::from_slice(&std::fs::read(config_path)?)?;
    ensure!(
        !config.node.is_empty() && config.node.len() <= 48,
        "invalid node name"
    );
    for path in [
        &config.key_file,
        &config.endpoint_file,
        &config.library_token_file,
        &config.authority_token_file,
    ] {
        ensure!(path.is_absolute(), "provider paths must be absolute");
    }
    loopback(&config.library_url)?;
    loopback(&config.authority_url)?;
    let relay = config.relay_url.parse::<iroh::RelayUrl>()?;
    ensure!(
        relay.as_str().starts_with("https://"),
        "relay must use HTTPS"
    );
    // Provision the dedicated private endpoint key separately. Never regenerate on read failure.
    let key: SecretKey = std::fs::read_to_string(&config.key_file)?.trim().parse()?;
    let builder = Endpoint::builder(iroh::endpoint::presets::Minimal)
        .clear_address_lookup()
        .relay_mode(RelayMode::Custom(RelayMap::from_iter([relay])));
    let mut transport = WebRtcNodeConfig::default();
    transport.session.exclude_tailnet_candidates = config.exclude_tailnet_candidates;
    transport.session.ice = if config.stun_urls.is_empty() {
        WebRtcIceConfig::no_ice_servers()
    } else {
        WebRtcIceConfig::direct_only(config.stun_urls.clone())?
    };
    let node = Arc::new(
        NativeWebRtcIrohNode::builder(transport, key)
            .endpoint_builder(builder)
            .accept_facade(ALPN)
            .spawn()
            .await?,
    );
    tokio::time::timeout(Duration::from_secs(30), node.endpoint().online()).await?;
    let publisher = node.clone();
    let publish_path = config.endpoint_file.clone();
    let publish_node = config.node.clone();
    let publish_relay = config.relay_url.clone();
    let publish_stun = config.stun_urls.clone();
    let app = Arc::new(App {
        authority_token: token(&config.authority_token_file)?,
        library_token: token(&config.library_token_file)?,
        config,
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(120))
            .build()?,
        streams: tokio::sync::Semaphore::new(4),
    });
    let publication = async move {
        loop {
            let bytes = serde_json::to_vec(
                &json!({"node":publish_node,"endpoint":publisher.endpoint_addr(),"protocol":String::from_utf8_lossy(ALPN),"updated_at":now(),"relay_url":publish_relay,"stun_urls":publish_stun}),
            )?;
            let next = publish_path.with_extension("json.next");
            tokio::fs::write(&next, bytes).await?;
            tokio::fs::rename(next, &publish_path).await?;
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };
    let accept = async move {
        let connections = Arc::new(tokio::sync::Semaphore::new(8));
        loop {
            let connection = node.accept(ALPN).await?;
            let Ok(permit) = connections.clone().try_acquire_owned() else {
                connection.close(1u32.into(), b"Service busy");
                continue;
            };
            let app = app.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let result = tokio::time::timeout(
                    Duration::from_secs(1800),
                    handle(app, connection.clone()),
                )
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    eprintln!("direct connection closed: {result:?}");
                }
                connection.close(0u32.into(), b"Read session closed");
            });
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };
    tokio::try_join!(publication, accept)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authorized_resource_and_range_must_match() {
        let mut request = Request {
            session: "a".repeat(64),
            offset: 90,
            length: 10,
        };
        let mut auth = Authorization {
            cid: "bafyabc".into(),
            path: "folder/file".into(),
            size: 100,
            offset: 90,
            length: 10,
            expires_at: 50,
        };
        assert!(validate(&request, &auth, 49).is_ok());
        assert!(validate(&request, &auth, 50).is_err());
        auth.offset = 0;
        assert!(validate(&request, &auth, 49).is_err());
        auth.offset = 90;
        auth.cid = "../admin".into();
        assert!(validate(&request, &auth, 49).is_err());
        auth.cid = "bafyabc".into();
        request.offset = u64::MAX;
        assert!(validate(&request, &auth, 49).is_err());
        assert!(loopback("https://example.com").is_err());
        assert!(loopback("http://127.0.0.1:8788").is_ok());
    }
}
