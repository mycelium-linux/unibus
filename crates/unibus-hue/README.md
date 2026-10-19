# Local Hue edge

Standalone Rust provider for the Hue bridge's LAN CLIP v2 API. No cloud OAuth,
no mcp-bridge, and no Hue-specific behavior in Jarvis. DCP and MCP use the same
resource-derived bounded executor. This initial slice supports light on/off and
brightness; scenes/rooms/zones are observed, not mutated.

## Resource location evidence

Light action descriptions now include room and zone memberships derived from
the bridge's resource references: room children identify devices, and lights
identify their owning device. A room can contain many enrolled lights. Direct
light references are also accepted, but equal names alone never establish
membership. Unassigned lights have an explicitly unknown physical room.

Rooms are advertised as physical-room evidence; zones are separately labeled
logical groups. Controller/host placement never becomes the controlled light's
location. Thus a living-room host may control a bedroom light without relocating
that light. Labels and IDs are bridge-owned, provider-local observations, not
automatically canonical Presence PlaceIds. Joining a Hue room to the shared
place registry needs an explicit verified binding; duplicate room names are not
identity. Membership evidence is bounded and malformed/oversized evidence fails
instead of being silently truncated.

JARVIS already consumes these descriptions through its generic DCP candidate
projection. Allowlist, action identity, finality and execution checks are
unchanged. Room/name changes alter catalog and state revisions. No room-wide
write action is added: controlling all lights in Lab still requires an
authorized multi-resource task or provider-owned group action. This change is
not deployed and does not claim live lighting control. JARVIS optional-argument
binding remains a separate tracked gap (`jarvis-ajdo`).

## Enroll and run

Mycelium identified the home bridge at `192.168.10.15`, MAC
`ec:b5:fa:13:4f:06`. Read-only `/api/config` independently verified bridge ID
`ECB5FAFFFE134F06`, model BSB002. These are observations, not compiled defaults.

1. Reuse an existing local application-key file when available. If none exists,
   prepare the TLS enrollment/config below, press the physical bridge button,
   then run `unibus-hue pair <config.json>` within 30 seconds. This explicit
   command makes one HTTPS registration request, saves the returned key in a
   newly created 0600 file, and refuses to replace an existing file. A closed
   pairing window creates no file. It never changes lights or runs at startup.
2. Enroll TLS trust explicitly. The default mode uses normal certificate-chain
   and hostname verification, with optional `bridge_ca_file`. Legacy Hue bridges
   without DNS/IP subject alternative names can instead use
   `bridge_certificate_file` plus the independently verified `bridge_id`.
   This pins exactly one leaf certificate, checks its bridge-ID common name and
   validity period on every handshake, and uses standard rustls TLS signature
   verification. Confirm the certificate's fingerprint through a trusted path
   during enrollment. A changed/expired certificate fails; it is never silently
   replaced. These two trust modes are mutually exclusive. The example selects
   explicit leaf enrollment. HTTP is permitted only for loopback mock bridges.
3. Create a separate random provider bearer token, also in a private file.
4. Copy `config/hue-local.example.json` to private configuration. An empty
   `allowed_light_ids` permits observation but exposes no write actions. Inspect
   resources and enroll only the intended light UUIDs.
5. Build with `lockout run -t hue-build -- cargo build --manifest-path
   crates/unibus-hue/Cargo.toml`. Run `unibus-hue inspect <config.json>` then
   `unibus-hue serve <config.json>`.

Endpoints (bearer authentication required):

- `GET /.well-known/dcp`
- `GET /v1/decisions` (`DCP-Version: 0.1`)
- `POST /v1/decisions/execute` (`DCP-Version: 0.1`)
- `POST /mcp` (stateless Streamable HTTP JSON-RPC)

DCP writes require final evidence, commit phase, and exact current revisions.
Each action binds one enrolled light; callers cannot supply an arbitrary resource
ID or URL. Arguments reject unknown fields and out-of-range brightness. A PUT
must have no Hue API errors and a matching readback before an applied receipt.
Request IDs are replay-protected within the process (4096 entries, fail closed
when full). Network/readback failures may follow an applied mutation: inspect
state before retrying. Restart loses the replay ledger; do not replay old requests
across restarts. Hue has no atomic revision/CAS API, so external writers can race
the observe/PUT window; this is not an atomic transaction guarantee.

## Publish via existing UniBus edges

Keep this HTTP edge loopback-only. Add a `philips-hue` provider to the existing
`unibus-dcp` publisher with endpoint `http://127.0.0.1:18815`, the provider token
file, node/producer identities, and owner email. Add the `/mcp` endpoint to the
existing `unibus-mcp` publisher when it supports provider bearer credentials.
For now use its existing `stdio` transport: absolute `command` pointing to
`unibus-hue`, `args` equal to `["stdio", "/absolute/private/hue.json"]`.
The local child inherits the launching user's authority and reads the private
application-key file itself; no key is passed on the command line. DCP publication
already supports bearer-token files. Do not claim live UniBus discovery until a
publisher lease is observed.

Manual runbook: inspect bridge resources, select an enrolled light, fetch catalog,
submit exact advertised action and revisions with final evidence, then verify the
returned receipt/current state. Nothing here performs unattended pairing,
changes lights during startup, or deploys itself.

## Delegated MCP compatibility

Both MCP transports advertise `hue_list_lights`, `hue_get_light` and
`hue_list_scenes` as read-only tools. `hue_set_light` is advertised only when the
catalog contains enrolled write actions and dispatches through the same bounded
executor and readback as per-light DCP. Unknown arguments, invalid resource IDs,
empty updates and out-of-range brightness fail before dispatch. An explicit room
scope fails until a verified place binding is implemented; it never expands to
all lights. Scene activation is not advertised or supported.

JARVIS can select this owner directly over stdio or select its namespaced tools
through the existing UniBus MCP aggregate. UniBus still requires its own scoped
mutation capability; provider enrollment is not that authorization. The
compatibility tools do not bypass enrolled-light restrictions, finality or
readback checks. The HTTP DCP replay ledger does not provide durable or MCP
exactly-once semantics; callers must not retry an uncertain MCP mutation.

Hue references: https://developers.meethue.com/new-hue-api/ and
https://developers.meethue.com/develop/get-started-2/.
