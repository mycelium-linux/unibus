# Vendored transport provenance

Source: https://github.com/SuddenlyHazel/iroh-webrtc-transport

Pinned revision: `fdb511da1650600dba5b97e04880b1b9aa2738a6`.

Package: `iroh-webrtc-transport` version `0.1.0-alpha.2`.

Original license and copyright are preserved in LICENSE.

FPL changes port the custom endpoint/sender API to Iroh 1.3.0, align
iroh-base/noq-udp/n0-watcher, and align Wasm tooling with wasm-bindgen 0.2.129.
Additional changes expose bounded diagnostics needed to verify the selected path.
The browser facade accepts bounded EndpointAddr JSON so relay and IP hints are
preserved without implicit DNS lookup. Explicit relay/STUN configuration replaces
hidden defaults in the application path. Native bootstrap sessions, signaling,
queues and retention are bounded. The claimed bootstrap sender is bound to the
authenticated endpoint key. Browser cleanup detaches event callbacks before
releasing Rust closures. An optional native ICE filter excludes Tailnet addresses
for controlled non-VPN qualification. The application uses the original MIT
license for this vendored code; the surrounding first-party code remains separate.
This is an experimental transport dependency. It does not own IPFS storage,
Mycelium identities, or application authorization.
