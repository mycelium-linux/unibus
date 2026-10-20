//! Browser transport only. The serving application owns resource authorization.

pub const ECHO_ALPN: &[u8] = b"fpl/ipfs-direct-echo/1";
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub mod framing;

#[cfg(target_arch = "wasm32")]
pub mod browser;
#[cfg(target_arch = "wasm32")]
pub mod download;

#[cfg(not(target_arch = "wasm32"))]
pub use iroh_webrtc_transport::{NativeWebRtcIrohNode, WebRtcDialOptions, WebRtcNodeConfig};
