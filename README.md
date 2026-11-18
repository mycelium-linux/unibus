# Unibus

## License

First-party Unibus code is dual-licensed under **MIT OR Apache-2.0**, at your
option. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
Third-party dependencies and separately licensed material retain their own
licenses; this grant does not relicense vendor code or nested checkouts.

One household bus for every surface. Display, audio, presence, sensor, and
control modules are all just grant-bearing slots on the same backplane —
heterogeneous machines (mac-studio, macbook-neo, home-pi + TV, Agora),
one narrow protocol. The name is the DEC UNIBUS bet, restated.

For MCP/DCP provider discovery and agent client setup, see the
[agent tool discovery reference](docs/reference/agent-tool-discovery.md).

This crate is **L1**: envelopes, framing, grants, QoS classification. It is
deliberately dumb — it routes by destination, screen anchor, zone +
capability, and TTL; it never reads a message body. Domain meaning lives in
L2 contract crates (`canvas.*`, `audio.*`, `presence.*`, ...) that each
domain owns.

## Status

Contract seed, lifted from JARVIS master (`jarvis-core::edge_transport`) and
kept wire-compatible with router v1. The JARVIS-side extraction and the
Canvas-side edge connector are tracked downstream (`dock-2ex7` epic).

## Build

Use current stable Rust and Git. No sibling checkout is required: external
Rust integrations use revision-pinned HTTPS Git dependencies. On Linux, full
workspace builds require `pkg-config` and `libasound2-dev` for MIDI/audio.
The private Holodeck integration is an opt-in independent workspace; see
[its runbook](docs/holodeck-bridge.md) for manifest-path builds and repository access.

For only the portable ADB bridge (no audio development libraries required):

```sh
cargo check --locked -p unibus-adb
cargo test --locked -p unibus-adb
cargo install --locked --path crates/unibus-adb
```

See the [portable ADB quickstart](docs/adb-portable-quickstart.md) for two-machine
setup, credential handoff, USB permissions, and static SSH without Mycelium.

```sh
cargo test          # unit + wire-compat golden vectors
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

## Layout

`crates/unibus` — the L1 contract:

| Path | What |
|---|---|
| `src/envelope.rs` | The frozen envelope shape, TTL/installation validation |
| `src/framing.rs` | `[u32 BE len][JSON][binary payload]` codec |
| `src/kinds.rs` | Control-plane kinds (`register`/`heartbeat`/`reply`, v1+v2) |
| `src/qos.rs` | command / stream / snapshot classification |
| `src/grants.rs` | Node grants: zones, namespaced capabilities, anchors, site, role |
| `src/auth.rs` | fpl-auth-aligned JWT claim shape + offline freshness rules |
| `tests/wire_compat.rs` | Golden JARVIS v1 frames — the cross-repo contract |

`crates/unibus-router` — the hub daemon, lifted from `jarvis-router`:

| Path | What |
|---|---|
| `src/hub.rs` | Accept loop, registration/auth, ack policy, anchor/zone/capability relay |
| `src/outbound.rs` | Bounded per-peer command/stream/snapshot scheduler and writer |
| `src/quic.rs` | Quinn/Rustls endpoint, explicit trust configuration, datagram/stream mapping |
| `src/registry.rs` | Live node registry: heartbeats (3 s / 10 s TTL), deterministic selection |
| `src/config.rs` | A v1 `router.json` loads unchanged; v2 adds routes table + site/role |
| `src/plugin.rs` | `HubPlugin`/`Sink` seam — stateful domain behavior (like presence fan-out) lives with its domain |
| `src/auth.rs` | shared-token compatibility plus verifier-backed fpl-auth sessions; credentials terminate at registration |
| `tests/hub.rs` | End-to-end over loopback: routing, echoes, anchors, plugin gating |

`crates/unibus-mqtt` — a bidirectional MQTT 3.1.1 edge adapter. Local MQTT
clients publish and subscribe through their local broker; adapters carry exact
topics and binary payloads over the reliable `mqtt.publish.v1` Unibus contract.
Broker sessions, retained state, and MQTT acknowledgements remain broker-owned.

`crates/unibus-fix` — a read-only FIX market-data edge. It decodes fragmented
FIX/TCP streams, maintains books from snapshot (`W`) and incremental (`X`)
messages, and publishes normalized `market.order_book.v1` and
`market.trade.v1` events. See [the FIX streamer runbook](docs/fix-streamer-runbook.md).

`crates/unibus-midi` — a realtime MIDI edge for CoreMIDI and ALSA. An input
edge forwards exact MIDI packets to one explicitly addressed output edge over
the reliable, ordered `midi.packet.v1` contract. See
[`docs/midi-bridge-runbook.md`](docs/midi-bridge-runbook.md).
See [`docs/mqtt-bridge-runbook.md`](docs/mqtt-bridge-runbook.md).

`crates/unibus-mcp` and `crates/unibus-dcp` are parallel L2 bindings. An
application may expose both from one provider-owned capability registry: MCP
advertises general-purpose tools, while DCP advertises the finite,
state-derived decisions legal now. UniBus leases and routes both contracts but
does not translate between them or own their semantics. DCP catalog messages
carry the canonical `dcp` crate types unchanged; execution remains bound to the
advertised catalog and state revisions. `config/add-dcp-router.jq` contains the
minimal provider/client routes.

`unibus-dcp inspect config/dcp-home.json` verifies discovery and a fresh catalog
without joining the bus. `unibus-dcp publish config/dcp-home.json` renews the
catalog lease every half-TTL. Bearer credentials remain local to that edge and
are never included in a catalog announcement.

`unibus-dcp serve config/dcp-bridge.json` is the client-facing counterpart. It
leases every routed provider catalog and exposes `GET /v1/decisions` plus a
compact `GET /capabilities` projection. `POST /v1/decisions/execute` accepts a
canonical DCP-over-UniBus `Execution`, revalidates its provider and exact
catalog/state revisions against the live lease, and returns the matching
provider receipt. It never derives DCP actions from MCP tools.

An optional `discovery` block makes the bridge consume Mycelium's existing
service catalog and adopt `kind: "dcp"` endpoints. Credential rules are keyed
by stable service ID and contain only local credential-file references; bearer
values never enter Mycelium observations, catalogs, or `/capabilities`.
Loopback HTTP is accepted only for a service owned by the local node, while
remote providers must use HTTPS.

The local `unibus-mcp serve` endpoint also exposes `GET /capabilities` for
read-only operations visibility. It derives provider ownership, lease health,
catalog digests, and mutation-class counts from live advertisements and keeps a
bounded recent invocation log containing only provider, tool, timing, and
outcome. Arguments, results, authorization grants, and credentials are never
included. Expired advertisements remain visible as `stale` but are never used
for routing. DCP providers will project into this same schema when a DCP edge is
available.

Configure local AI clients from the same endpoint with:

```sh
# Preview the Codex, Claude Code, and OpenCode changes.
unibus-mcp setup-clients

# Apply them idempotently. Select clients with repeated --client flags.
unibus-mcp setup-clients --write
```

The command registers `http://127.0.0.1:18791/mcp` as
`unibus-local-tools`. It uses the Codex and Claude Code configuration CLIs and
performs a validated, atomic update of OpenCode's JSONC configuration. Remote
endpoints must use HTTPS.

Routing is configuration: the hub maps message kinds to capabilities via a
`routes` table; unknown kinds without a plugin fail loudly. The hub parses
only `register` — every data body stays opaque.

Schema-v2 node grants must explicitly list `publishes` plus their permitted
`publish_zones`, `publish_screen_anchors`, and `publish_producers`. Frozen v1
peers with no publish policy retain compatibility. Plaintext TCP binds are
loopback-only unless `allow_insecure_lan: true` is explicitly set; use a secure
tunnel for remote TCP deployments.

Nodes with `auth_subject` are authenticated through the `OfflineVerifier`
installed with `Hub::with_offline_verifier`; subject, audience, installation,
producer, lifetime, and asserted grant scope are all bound before registration.
Nodes without `auth_subject` remain in explicit shared-token compatibility mode.

The library hub can additionally listen over QUIC/TLS with `Hub::start_quic`.
The standalone `unibus-hub` starts both TCP and QUIC when configuration includes:

```json
"quic": {
  "listen": "0.0.0.0:18791",
  "cert_file": "tls/server.pem",
  "key_file": "tls/server-key.pem"
}
```

Certificate paths resolve relative to `router.json`; invalid or missing PEM
material stops startup. See [`docs/quic-runbook.md`](docs/quic-runbook.md).

Related: `FuturePresentLabs/dock` — `docs/unibus.md` (Canvas's use of the
bus); JARVIS — `jarvis-router`, `jarvis-edge`, `distributed-topology.md`.
