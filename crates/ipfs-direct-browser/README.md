# Iroh browser download transport

This standalone Rust workspace ports the pinned experimental WebRTC transport
to Iroh 1.3.0 and wasm-bindgen 0.2.129. Native applications use ordinary Iroh
connections. The browser client preserves explicit `EndpointAddr` relay/IP
hints and negotiates WebRTC through the approved Iroh relay. It does not publish
through implicit public DNS discovery.

The native provider owns IPFS reads. Mycelium owns authorization. This package
never receives a Kubo RPC token, a fleet key, or an authority signing key.
See `vendor/iroh-webrtc-transport/PROVENANCE.md` and its MIT license.

## Build and check

```sh
cargo check --all-targets
cargo check --target wasm32-unknown-unknown --bins --features browser-fixture
cargo test --lib
cargo test -p iroh-webrtc-transport --lib
cargo build --bin ipfs-direct-echo

# Echo fixture. Trunk generates all JavaScript glue and the bootstrap.
env -u NO_COLOR trunk build --release --locked --dist /tmp/ipfs-echo-dist

# Production. This bundle excludes the browser qualification sink.
env -u NO_COLOR trunk build download.html --release --locked \
  --public-url /direct-assets/ --dist /tmp/ipfs-download-dist

# Controlled qualification only. Never deploy this bundle as production.
env -u NO_COLOR trunk build download.html --release --locked \
  --features browser-fixture --public-url /direct-assets/ \
  --dist /tmp/ipfs-download-fixture-dist
```

Use Trunk 0.21.14 and the pinned wasm-bindgen version. No application JavaScript
is authored. Build on a host with sufficient disk; the initial WebRTC native
and Wasm dependency build uses several GiB. The implementation session used
an isolated Titan toolchain with two build jobs and debug symbols disabled.
Trunk emits `index.html` for both selected HTML targets.

## Interfaces

`browser::DirectClient::spawn(relay_url, stun_urls)` returns a browser endpoint.
Its `endpoint_id()` is the public key that the authorization broker binds to the
file session. `dial(endpoint_addr, alpn, preference)` preserves the fresh route
hints. `DirectConnection::open_request(bytes)` sends a bounded control request
and closes its send half. `DirectStream::read_chunk()` applies backpressure.
`diagnostics()` reads actual selected Iroh paths and browser ICE candidate pairs.

Native applications depend on the vendored package with `default-features =
false, features = ["native"]`. Use `NativeWebRtcIrohNode::builder(config, secret)
with a deployment-owned Minimal/custom-relay endpoint builder and
`accept_facade(alpn)`. `accept(alpn)` returns an ordinary `iroh::Connection`.
`endpoint_addr()` supplies real, current hints for publication through the
existing directory. A sync endpoint cannot substitute for a file provider.

The download application uses these existing application boundaries:

- `GET /api/download/options`: approved relay/STUN configuration and provider.
- `POST /api/download/session`: exact CID/path and this browser's endpoint key.
- `fpl/ipfs-read/1`: bounded JSON `{session, offset, length}`, then a JSON header
  line `{ok, size, offset, length}` followed by exactly that many payload bytes.

The provider and broker must reject a different key, CID/path, node, expired
session, or revoked grant. Endpoint tickets carry routing hints, not permission.
The opaque session stays in browser memory and never appears in URLs or details.

## Browser behavior

The Save file button opens the browser file picker during the trusted click.
The application streams into a writable file. It buffers at most one 8 MiB
range, validates its metadata and exact length, then writes it. A failed partial
range never reaches disk. It reconnects and retries at the last complete range
up to three attempts, and renews a short session before expiry. Reload resume
is not implemented. Cancellation aborts the writable file. Unsupported browsers
receive the existing HTTPS download link; the application does not create a
whole-file Blob.

Root-hosted assets use `/direct-assets/`. CSS is external. The host must add its
script nonce to Trunk's bootstrap and allow self, the approved WSS relay, and
Wasm compilation in CSP. There is no `eval` or `new Function` application code.

The separate `browser-fixture` build accepts `fixture=opfs` on the direct page.
Click Save file to write `ipfs-direct-qualification.bin` into the browser's real
origin-private file system. This permits byte verification in a fresh headless
browser without a desktop save dialog. The production build contains no such
sink. Qualification must still use a real browser transport and the controlled
broker; synthetic browser API fixtures do not prove a direct path.

## Bounds and path evidence

Native bootstrap admission is capped at eight active WebRTC sessions. Each
bootstrap uses bounded 64 KiB signals, at most 256 received messages, and a
45-second whole-operation deadline. The claimed sender must match the Iroh
handshake key. Unused sessions expire after 30 seconds; application close
releases the associated route, transport queue and peer. A 30-minute absolute
limit bounds retained sessions. Facade accept queues reject excess connections.
Packet queues default to 128 entries, with 1 MiB/256 KiB DataChannel high/low
water marks. These are bounded changes, not a comprehensive third-party audit.

`WebRtcSessionConfig::exclude_tailnet_candidates` defaults to false. A controlled
non-VPN qualification can set it true to filter native host candidates in
100.64.0.0/10 and fd7a:115c:a1e0::/48 without altering SSH or host networking.
Use explicit STUN configuration when testing public NAT behavior. Inspect the
selected ICE pair: a custom Iroh path over Tailnet addresses proves WebRTC over
the VPN, not an off-tailnet route. Host candidates on one LAN do not prove NAT
hole punching. A stock browser Iroh route remains relay-only when WebRTC is not
available; the client reports the actual route and retains HTTPS fallback.

Run the bounded echo fixture with an approved relay:

```sh
ipfs-direct-echo https://APPROVED-RELAY/ /tmp/ipfs-echo-dist 127.0.0.1:18860
```

The HTTP fixture binds loopback only. Its separate echo ALPN returns at most
64 KiB of caller-supplied data and never reads IPFS. It is temporary test
infrastructure, not a production file endpoint. Remove its process and SSH
forward after qualification.
