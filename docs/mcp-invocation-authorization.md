# Existing-consent exchange for MCP invocations

This is an interface proposal, not an installed exchange or a new authorization
policy. Automatic exact-call issuance is not qualified by the TV service-grant
renewal tests.

The application currently checks its existing `botMcpServer` assignment on each
execution against the user, space, bot, enabled server and allowed tool. Its MCP
transport then sends only the tool name and arguments. The enrolled desktop
authenticates its device credential and forwards that request. The local Unibus
boundary requires a separate five-minute exact-call capability for a mutation.
An FPL Auth owner session or a desktop device credential cannot reconstruct the
application's bot assignment or approved invocation.

Keep assignment and invocation approval with their existing application owner.
Keep device enrollment and session renewal with FPL Auth. Keep signed peer and
service grants with Mycelium, and exact-call policy issuance with Unibus. An
exchange must call these owners rather than copy their policy into the desktop.

Source interfaces inspected for this proposal:

| Owner | Source boundary |
| --- | --- |
| Agent application | `packages/adapters/src/mcp-connector.ts`: `McpConnector.execute` checks the assignment; `mcp-transport.ts`: `McpSession.callTool` sends name/arguments |
| FPL desktop services | `rust/fpl-desktop-bridge/src/proxy.rs`: `proxy_request` and `send_upstream` authenticate the device and forward the body |
| Hosted MCP gateway | shared-infra `batty/librechat/fpl-mcp-bridge/server.mjs`: `checkFplGrant` and `resolveDesktopCapability` resolve owner and device |
| Exact-call issuer | Unibus `crates/unibus-mcp/src/bin/unibus-mcp-authorize.rs`: `request` locks, bounds and atomically updates the private policy |
| Desktop session renewal | desktop-services `rust/fpl-enrollment/src/renewal.rs`: `send_authenticated`; Auth `src/pg.rs`: `renew_desktop_session` |

These source observations identify the missing contract; they do not establish
that every installed binary matches a clean committed checkout.

## Proposed narrow interface

After its ordinary assignment and invocation-approval checks, the application
authority can issue a short-lived opaque reference for that invocation. The
reference is transport metadata; the model cannot create it by supplying user,
bot or space fields. It carries no provider credential and creates no new
assignment, permission, scope or approval interval.

The receiving Rust issuer asks that same application authority to authorize the
reference with the exact invocation identity, enrolled destination device,
provider, tool and JSON arguments. The issuer authenticates through the existing
host-held service identity. No additional signing key or trust root is implied.
An authority endpoint or service identity suitable for this exchange has not
yet been established; a client-supplied URL or caller-supplied identity is not a
substitute.

The authority rechecks the current owner, bot and space assignment, server
revision, tool permission, invocation approval, revocation and original expiry.
It returns a decision bound to the requested invocation and destination, with
an expiry no later than those existing approvals. Missing authority data or an
unreachable authority denies issuance. A positive response is a decision about
this call, not a reusable provider-wide write grant.

Only that validated decision may enter the existing private-policy issuer. The
issuer generates fresh `_meta.unibus_authorization` for the exact provider,
tool and arguments, capped by both the decision expiry and the existing
five-minute maximum. The bridge and publisher keep their independent exact-call
verification. The opaque reference and capability stay out of model prompts,
logs and tool arguments.

The authority must define atomic consumption or idempotency for the invocation
reference before implementation. A lost issuance response must not extend its
approval, and a lost tool response must not cause an automatic mutation retry.
Removing an assignment or revoking approval must prevent subsequent issuance;
existing capabilities retain their established explicit-removal/expiry behavior.

## Required validation cases

These are exchange acceptance criteria, not tests claimed to pass today.

| Input or transition | Required result |
| --- | --- |
| Current exact assignment and approved invocation | One bounded capability for the exact invocation |
| Changed user, space, bot, server revision or destination device | Deny before policy mutation |
| Different provider, tool or arguments | Deny before policy mutation |
| Removed assignment, disabled server or revoked approval | Deny fresh issuance |
| Future, expired or malformed reference | Deny; no expiry extension |
| Device bearer plus invented metadata, without an authority reference | Deny fresh issuance |
| Missing or failed authenticated introspection | Fail visibly; preserve the policy |
| Concurrent or repeated reference consumption | Follow defined atomic/idempotent semantics without extending approval |
| Lost tool response after dispatch | Report unknown outcome; do not retry automatically |
| Exact capability at its expiry or after explicit policy removal | Existing bridge and publisher reject it |
| Mycelium TV grant or upstream OAuth refresh succeeds | No implication of MCP mutation authority |
