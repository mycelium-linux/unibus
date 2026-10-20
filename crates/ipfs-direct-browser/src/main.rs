#[cfg(not(target_arch = "wasm32"))]
fn main() {}

#[cfg(target_arch = "wasm32")]
fn main() {
    wasm_bindgen_futures::spawn_local(async {
        if let Err(error) = run().await {
            let text = error.as_string().unwrap_or_else(|| format!("{error:?}"));
            render("failed", &serde_json::json!({"error":text}));
        }
    });
}

#[cfg(target_arch = "wasm32")]
fn render(state: &str, result: &serde_json::Value) {
    let document = web_sys::window().unwrap().document().unwrap();
    document
        .get_element_by_id("status")
        .unwrap()
        .set_text_content(Some(state));
    let output = document.get_element_by_id("result").unwrap();
    output.set_text_content(Some(&serde_json::to_string_pretty(result).unwrap()));
    output.set_attribute("data-state", state).unwrap();
}

#[cfg(target_arch = "wasm32")]
async fn run() -> Result<(), wasm_bindgen::JsValue> {
    use ipfs_direct_browser::{
        ECHO_ALPN,
        browser::{DirectClient, DirectPreference},
    };
    use sha2::{Digest, Sha256};
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    let response: web_sys::Response = JsFuture::from(
        web_sys::window()
            .unwrap()
            .fetch_with_str("/echo-endpoint.json"),
    )
    .await?
    .dyn_into()?;
    if !response.ok() {
        return Err(JsValue::from_str("echo descriptor unavailable"));
    }
    let text = JsFuture::from(response.text()?)
        .await?
        .as_string()
        .unwrap_or_default();
    let config: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let remote = serde_json::from_value(config["endpoint"].clone())
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    let relay = config["relay"]
        .as_str()
        .ok_or_else(|| JsValue::from_str("missing approved relay"))?;
    let stun_urls = config["stun_urls"]
        .as_array()
        .map(|urls| {
            urls.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let preference = match config["mode"].as_str().unwrap_or("webrtc_only") {
        "relay_only" => DirectPreference::RelayOnly,
        "prefer_webrtc" => DirectPreference::PreferWebRtc,
        "webrtc_only" => DirectPreference::WebRtcOnly,
        _ => return Err(JsValue::from_str("invalid echo route preference")),
    };
    let client = DirectClient::spawn(relay.to_owned(), stun_urls).await?;
    render(
        "connecting",
        &serde_json::json!({"browser_endpoint":client.endpoint_id()}),
    );
    let connection = client.dial(&remote, ECHO_ALPN, preference).await?;
    let payload: Vec<u8> = (0..32785).map(|n| (n % 251) as u8).collect();
    let stream = connection.open_request(&payload).await?;
    let mut received = Vec::new();
    while let Some(chunk) = stream.read_chunk().await? {
        if received.len() + chunk.len() > payload.len() {
            return Err(JsValue::from_str("echo exceeded expected length"));
        }
        received.extend_from_slice(&chunk);
    }
    if received != payload {
        return Err(JsValue::from_str("echo bytes differ"));
    }
    let paths = connection.diagnostics().await?;
    let result = serde_json::json!({"browser_endpoint":client.endpoint_id(),
        "bytes":received.len(),"sha256":format!("{:x}",Sha256::digest(&received)),
        "requested_mode":config["mode"],"diagnostics":paths});
    render("passed", &result);
    let _ = connection.close().await;
    let _ = client.close().await;
    Ok(())
}
