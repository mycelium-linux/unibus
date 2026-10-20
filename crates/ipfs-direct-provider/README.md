# Authorized Iroh IPFS range provider

Standalone Rust adapter for `fpl/ipfs-read/1`. It calls the Mycelium web adapter
for authorization and the existing authenticated Unibus catalog for bytes.
It owns no pin database, login system, or signing authority.

Generate a dedicated endpoint identity once with `ipfs-direct-provider keygen
PATH`. The command creates a new mode-0600 file and refuses to overwrite one.
Run `ipfs-direct-provider CONFIG.json`; see shared-infra
`projects/ipfs/library/direct-provider.json` and its systemd unit. All service
credentials come from PID 1 credential files. Never give this process the upload
credential or the Mycelium signing key.

Each bidirectional stream carries one bounded JSON request, followed by client
FIN: `{"session":"64 hex characters","offset":0,"length":8388608}`. The
provider verifies the session against the actual authenticated Iroh remote key.
Success is a JSON line `{"ok":true,"size":N,"offset":O,"length":L}` followed
by exactly L bytes and FIN. Denial is an error JSON line and FIN with no bytes.
A disconnected partial range is discarded by the browser and retried before
writing. Bounds are four active ranges, eight connections, 8 MiB per range,
4 GiB per file, 120 seconds per range, and 30 minutes per connection.

The public descriptor is atomically refreshed every 30 seconds. It carries the
actual Iroh EndpointAddr and explicit relay/STUN configuration. The provider
uses the experimental vendored WebRTC custom transport ported to Iroh 1.3.
Relay signaling can assist connection setup while bulk bytes use a direct ICE
path. A ticket/address cannot establish access or guarantee a direct path.
Do not infer directness from candidates: inspect the selected Iroh path and
selected ICE pair in the browser diagnostics. `exclude_tailnet_candidates`
defaults to false; qualification can set it true to exclude Tailscale routes.

The preferred local Kubo may fetch/cache an already cataloged resource over the
private swarm. This does not add a pin or claim another replica. HTTPS remains
the fallback if the browser lacks a file picker or direct setup fails.

```sh
cargo check --manifest-path crates/ipfs-direct-provider/Cargo.toml
cargo test --manifest-path crates/ipfs-direct-provider/Cargo.toml
cargo build --release --manifest-path crates/ipfs-direct-provider/Cargo.toml
```
