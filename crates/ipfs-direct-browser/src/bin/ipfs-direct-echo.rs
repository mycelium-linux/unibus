#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use ipfs_direct_browser::{
        ECHO_ALPN, MAX_REQUEST_BYTES, NativeWebRtcIrohNode, WebRtcNodeConfig,
    };
    use iroh::{Endpoint, RelayMap, RelayMode, SecretKey};
    use std::time::Duration;

    let relay = std::env::args()
        .nth(1)
        .context("usage: ipfs-direct-echo APPROVED_RELAY_URL [ASSETS_DIR] [127.0.0.1:PORT]")?
        .parse::<iroh::RelayUrl>()?;
    ensure!(
        relay.as_str().starts_with("https://"),
        "relay requires HTTPS"
    );
    let secret = SecretKey::generate();
    let builder = Endpoint::builder(iroh::endpoint::presets::Minimal)
        .clear_address_lookup()
        .relay_mode(RelayMode::Custom(RelayMap::from_iter([relay.clone()])));
    let mut config = WebRtcNodeConfig::default();
    config.session.ice = iroh_webrtc_transport::config::WebRtcIceConfig::no_ice_servers();
    let node = NativeWebRtcIrohNode::builder(config, secret)
        .endpoint_builder(builder)
        .accept_facade(ECHO_ALPN)
        .spawn()
        .await?;
    tokio::time::timeout(Duration::from_secs(20), node.endpoint().online()).await?;
    let descriptor = serde_json::json!({"endpoint":node.endpoint_addr(),
        "alpn":String::from_utf8_lossy(ECHO_ALPN),"relay":relay.as_str(),"stun_urls":[],"mode":"webrtc_only"});
    println!("{descriptor}");
    if let Some(assets) = std::env::args().nth(2) {
        let listen = std::env::args()
            .nth(3)
            .unwrap_or_else(|| "127.0.0.1:18860".into())
            .parse::<std::net::SocketAddr>()?;
        ensure!(
            listen.ip().is_loopback(),
            "echo HTTP fixture must bind loopback"
        );
        let app = axum::Router::new()
            .route(
                "/echo-endpoint.json",
                axum::routing::get({
                    let descriptor = descriptor.clone();
                    move || async move { axum::Json(descriptor) }
                }),
            )
            .fallback_service(tower_http::services::ServeDir::new(assets));
        let listener = tokio::net::TcpListener::bind(listen).await?;
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                eprintln!("echo HTTP fixture failed: {e}");
            }
        });
    }
    loop {
        let connection = node.accept(ECHO_ALPN).await?;
        let result = tokio::time::timeout(Duration::from_secs(20), async {
            let (mut send, mut recv) = connection.accept_bi().await?;
            let bytes = recv.read_to_end(MAX_REQUEST_BYTES).await?;
            ensure!(!bytes.is_empty(), "empty echo request");
            send.write_all(&bytes).await?;
            send.finish()?;
            let paths: Vec<_> = connection.paths().iter().map(|p| serde_json::json!({
                "address":format!("{:?}",p.remote_addr()),"selected":p.is_selected(),
                "custom":matches!(p.remote_addr(),iroh::TransportAddr::Custom(_))})).collect();
            eprintln!("{}",serde_json::json!({"remote":connection.remote_id().to_string(),"bytes":bytes.len(),"paths":paths}));
            connection.closed().await;
            anyhow::Ok(())
        }).await;
        if !matches!(result, Ok(Ok(()))) {
            eprintln!("echo failed: {result:?}");
        }
        connection.close(0u32.into(), b"echo complete");
    }
}
