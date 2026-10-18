# Iroh service channels

`unibus_iroh::transport::IrohTransport::connect_session` uses the existing
signed session admission, endpoint identity proofs, relay allowlist and local
access policy. It returns the concrete session so an adapter can open an
independent bidirectional `ServiceChannel` for each request/response exchange.
The ordinary `PeerTransport::connect` interface remains available unchanged.

Channels carry length-delimited Unibus frames, not unauthenticated raw socket
bytes. Each frame is authorized against the session's signed message kinds,
payload limit and expiry, with identity and live access-policy revalidation.
The receiver bounds the frame length before allocating. The sender waits for
QUIC backpressure; there is no unbounded producer queue. Interrupted or failed
frame operations poison the channel rather than reusing a partially consumed
frame. Dropping it cancels that stream without closing unrelated channels.

An adapter sends a fresh envelope per chunk, retaining its own protocol kind,
request correlation and terminal-response semantics. `finish()` closes only
the sending direction: a client can finish its request and continue receiving
the response. Transport EOF is not proof of successful application completion.
Sessions must remain alive for their channels' lifetimes. Expired or revoked
sessions fail closed; reconnecting requires fresh authorized admission.

This API does not issue assertions, grant discovered devices access, publish
endpoints, choose public relays, or couple adapters to Mycelium. Those remain
the existing deployment, identity-provider, directory and policy boundaries.

## Signed production permits

The optional `fleet-identity` feature includes `grant::GrantVerifier`. It uses
the existing Mycelium `AccessRecord` signature and explicitly pinned access
issuers, not a new CA or a JSON fixture. Its retained `TrustDocument` contains
an authority-signed grant, signed revocations, and a `Permit`. The grant has
exactly one `unibus-session:SHA256` scope. SHA256 covers compact JSON of the
permit with recursively sorted object keys, as produced by `permit_scope`.
This fits the shared access system's bounded selector format.

The permit binds the installation, audience, two producer/endpoint keys,
message kinds and payload budget. These are access-policy fields, **not**
permanent endpoint identity bindings. Local configuration maps each authorized
service to its fixed local socket. No SSH account, sudo permission or issuer
delegation is implied by this grant.

Session parameters can be generated locally from this signed authorization;
the verifier rejects any mismatch, widening, invalid lifetime or untrusted
issuer. Iroh proves possession of both endpoint keys. Session JSON is therefore
not a bearer credential and cannot grant discovered endpoints access. Sessions
last at most five minutes, bounded by grant expiry; the compatibility client
requests four-minute sessions and reconnects after expiration.

Signatures refresh at most once per second from a bounded local document.
Validity and operation scope are checked on every frame. An unreadable,
invalid, revoked or expired document fails closed. This is a consumer of
retained signed grants. Issuance and distribution remain separate adapters.

### Automatic renewal and gossip reconciliation

The issuer's Mycelium authority can explicitly schedule bounded grant renewal
with `mycelium access renewal enable --grant ID --authorization-ttl 90d
--credential-ttl 7d --renew-before 1d --write`. Only the original issuer key may
renew; permissions and grant ID remain unchanged. Ordinary signed revocations
stop renewal. Gossip transports the signed records, not issuer credentials.

Build the optional receiving adapter with `cargo build --release -p
unibus-mycelium --features grant-sync --bin unibus-mycelium-grants`.
`unibus-mycelium-grants CONFIG.json` queries the local public `access_list` RPC,
independently verifies pinned issuers, and atomically reconciles its retained
trust document. Lower serials cannot roll it back; conflicting serials or
changed permissions fail loudly. Signed revocations are retained across missing
or older observations, including while a grant is expired or revoked.

Use the provided systemd service/timer, a root-owned configuration, and a
root-owned `/var/lib/unibus-iroh-hdhomerun` directory (0755). The JSON config
contains `socket` (local Mycelium socket), `trust_file` (that directory's
`trust.json`), and `issuers` (explicit public signing keys). Seed `trust.json`
with the already verified signed document, mode 0644. Set the bridge's
`trust_file` to that persistent path, **not** a LoadCredential snapshot.
The bridge needs one restart to adopt this path; later trust changes refresh
within one second without a playback restart. Private endpoint keys remain
LoadCredential-protected. Neither the bridge user nor a gossip peer can write
the trust file. The adapter needs privileged local IPC access, not an issuer key.

Manual runbook: run the adapter once, inspect its exit status, compare the
retained grant's serial/expiry, then enable the timer. The timer checks every
30–40 seconds without overlap. RPC failure preserves the last good document
and reports failure; it does not extend expiry. Revocation delivery requires
connectivity and a successful reconciliation; the lease expiry bounds offline
access. The service does not promise instantaneous revocation during partition.

Qualified on two Linux hosts on 2026-10-17: an automatic issuer scheduler pass
advanced the serial, gossip delivered that record to both local daemons, and
both receiver timers updated their trust files without changing either bridge
PID. A live TV request remained HTTP 200 and delivered 72.8 MB over 90 seconds
through that change. The transfer was intentionally time-bounded (curl exit
28), not a completed finite download. Stock FireTV UI playback remains a
separate visual check. Qualification also passed 17 Iroh tests, seven adapter
tests, three issuer-renewal tests and the isolated offline SSH-renewal gossip
integration test.

## UDP compatibility for existing adapters

`unibus-iroh-udp CONFIG.json` carries bounded loopback UDP packets on authorized
service channels. This lets existing Unibus QUIC/mTLS adapters use Iroh NAT
traversal without replacing their existing authentication or protocol handlers.
It intentionally retains inner mTLS for compatibility; native adapters may use
service channels directly without this extra encryption/framing layer.

Both roles require a private endpoint key, retained trust document, producer,
explicit relay allowlist (empty disables relays), and explicit loopback `udp`
socket. The client also needs remote endpoint hints. `iroh_bind` optionally sets
the endpoint bind address; default is `0.0.0.0:0`. The server forwards only to
its configured loopback service, never a destination supplied by a client.
Each instance serves one admitted consumer and one local UDP producer; a local
producer restart reopens the session instead of pinning a dead source port.
Empty one-second keepalives do not reach the UDP service. There is no WAN route,
IP forwarding, multicast bridge, or public tuner listener.

Generate a key **on its host**, without overwriting an existing identity:

```sh
unibus-iroh-udp keygen /private/service/endpoint.key
```

`deploy/systemd/unibus-iroh-hdhomerun.service` uses a dynamic user, protected
credentials, no capabilities, and bounded memory/tasks/file descriptors. Install
host configuration and grant documents under `/etc/unibus-iroh-hdhomerun` as
root-only files. Keys and live grants never belong in the repository.

For HDHomeRun, the intended path is local proxy → loopback bridge → Iroh →
owner loopback bridge → existing tuner owner → physical tuner. Stock LAN
discovery and playback URLs remain unchanged. Before retiring an older path,
verify fresh catalog, decoded media, cancellation, sustained transfer across
session renewal, and restart recovery. Keep the prior configuration and binaries
for rollback; record grant expiry and whether the path is direct or relayed.
