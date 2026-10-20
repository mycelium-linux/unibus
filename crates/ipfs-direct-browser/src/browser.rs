use iroh::{EndpointAddr, SecretKey};
use iroh_webrtc_transport::browser::{
    BrowserDialOptions, BrowserWebRtcConnection, BrowserWebRtcNode, BrowserWebRtcNodeConfig,
    BrowserWebRtcStream,
};
use wasm_bindgen::JsValue;

use crate::MAX_REQUEST_BYTES;

#[derive(Clone, Copy)]
pub enum DirectPreference {
    WebRtcOnly,
    PreferWebRtc,
    RelayOnly,
}

pub struct DirectClient {
    node: BrowserWebRtcNode,
}
pub struct DirectConnection {
    connection: BrowserWebRtcConnection,
}
pub struct DirectStream {
    stream: BrowserWebRtcStream,
}

impl DirectClient {
    /// Use explicit relay and STUN origins. Empty STUN means host candidates only.
    pub async fn spawn(relay_url: String, stun_urls: Vec<String>) -> Result<Self, JsValue> {
        let config = BrowserWebRtcNodeConfig {
            relay_urls: vec![relay_url],
            stun_urls,
            ..Default::default()
        };
        let node = BrowserWebRtcNode::spawn(config, SecretKey::generate()).await?;
        Ok(Self { node })
    }

    pub fn endpoint_id(&self) -> &str {
        self.node.endpoint_id()
    }

    pub async fn dial(
        &self,
        remote: &EndpointAddr,
        alpn: &[u8],
        preference: DirectPreference,
    ) -> Result<DirectConnection, JsValue> {
        let options = match preference {
            DirectPreference::WebRtcOnly => BrowserDialOptions::webrtc_only(),
            DirectPreference::PreferWebRtc => BrowserDialOptions::webrtc_preferred(),
            DirectPreference::RelayOnly => BrowserDialOptions::iroh_relay(),
        };
        let remote = serde_json::to_string(remote).map_err(js_error)?;
        let connection = self.node.dial(&remote, alpn, options).await?;
        Ok(DirectConnection { connection })
    }

    pub async fn close(&self) -> Result<(), JsValue> {
        self.node.close("client closed").await
    }
}

impl DirectConnection {
    /// The caller sends bounded control data. It must contain no fleet credentials.
    pub async fn open_request(&self, request: &[u8]) -> Result<DirectStream, JsValue> {
        if request.is_empty() || request.len() > MAX_REQUEST_BYTES {
            return Err(JsValue::from_str("request must contain 1..=65536 bytes"));
        }
        let stream = self.connection.open_bi().await?;
        stream.send_all(request).await?;
        stream.close_send().await?;
        Ok(DirectStream { stream })
    }

    pub async fn diagnostics(&self) -> Result<serde_json::Value, JsValue> {
        self.connection.diagnostics().await
    }

    pub async fn close(&self) -> Result<(), JsValue> {
        self.connection.close("request complete").await
    }
}

impl DirectStream {
    /// A single chunk, with transport backpressure. Never buffer a whole file.
    pub async fn read_chunk(&self) -> Result<Option<Vec<u8>>, JsValue> {
        self.stream.read_chunk().await
    }
}

fn js_error(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}
