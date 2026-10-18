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

## HDHomeRun rollout boundary

The running HDHomeRun adapter still uses its existing QUIC/mTLS transport.
Adding this API alone does **not** change its route or fix playback. Its next
integration must map existing request/reply/media operations onto channels,
retain tuner concurrency and disconnect cancellation, and obtain production
session assertions through the existing authorization boundary. Do not replace
the verifier with a JSON fixture or bypass certificate/identity checks.

The repository currently supplies the `DirectSessionVerifier` interface and
test fixtures, not a production verifier/issuer wired into the tuner daemon.
That wiring is a deployment prerequisite, not permission to trust discovered
endpoint IDs or unsigned session JSON. Fleet membership proofs authenticate a
machine; they are not themselves a grant to consume its tuner or other service.

Before changing the live proxy, verify bounded catalog requests, concurrent
media streams, cancellation releasing a tuner, expiry/revocation, and sustained
home-pi-to-Avery playback while Agora is unavailable. Retain the previous
configuration and binaries for rollback. No site route or tuner firmware change
is required for the Iroh path.
