# Canvas endpoint status

`canvas-fleet` is the local multi-endpoint companion to `canvasctl`. It calls
the existing owner inspection interface; it neither scans for services nor
changes workspaces. Gateway drivers leave remote transport to Unibus.

```sh
canvas-fleet status
canvas-fleet status desktops
canvas-fleet status neo,studio --json
canvas-fleet status --config /path/to/canvas-fleet.json
```

Without a selection, status queries every target registered in
`$XDG_CONFIG_HOME/unibus/canvas-fleet.json`, or
`$HOME/.config/unibus/canvas-fleet.json`. It reports the active workspace,
revision, and Canvas surfaces (not the operating system's entire native window
inventory). Missing fields stay unknown. Each failed endpoint remains visible;
the command exits nonzero if any endpoint fails.

The existing explicit commands remain available:

```sh
canvas-fleet CONFIG GROUP_OR_TARGETS inspect
canvas-fleet CONFIG GROUP_OR_TARGETS switch WORKSPACE
```

Command drivers default to correlated schema-v1 owner replies. For the
ADB-authorized `canvas-tvctl` helper, an operator may explicitly configure
`"reply_contract":"presentation_transaction"`. That helper performs one request
per private-token-authenticated connection and returns a presentation receipt
without wire request IDs. The local adapter validates its boolean outcome and
revision, rejects conflicting IDs/versions, and marks normalization with
`reply_provenance: single_authenticated_transaction`. It does not claim
wire-level correlation. Never select this contract for a multiplexed client;
Unibus gateway adapters always retain strict correlated replies.

TV status reads the active presentation document; desktop status reads the
owner snapshot. Revision always means the endpoint's control revision, not the
document revision.

Registration currently means the local target config;
automatic Mycelium catalog ingestion and live Iroh qualification are separate.

## Owner-local app launch

```sh
canvas-fleet launch firetv
canvas-fleet launch firetv --config /absolute/path/to/canvas-fleet.json
```

An updated `canvas-tvctl` resolves Canvas's installed Android LEANBACK launcher,
brings it forward without clearing data or force-stopping playback, and verifies
the exact foreground component. No activity class is hard-coded. ADB must already
be connected and authorized. Launch uses a correlated owner receipt, not a
renderer revision, and is refused by Unibus gateway drivers. Other owner CLIs
must explicitly implement this action; unsupported endpoints return errors.
It cannot launch arbitrary apps or install an APK. The manual equivalent is
package-manager launcher resolution, `am start -W --activity-single-top -n`, then
`dumpsys window windows` focus verification.

## Bounded document apply

```sh
canvas-fleet apply /path/to/document.json studio,firetv --dry-run
canvas-fleet apply /path/to/document.json studio,firetv
```

`apply` validates the owner document envelope (48 KiB maximum, identity,
project, positive revision), binds each target's observed runtime revision,
sends one `apply_document`, then reads the named workspace back and verifies it.
A document codec can canonicalize owner defaults before comparison, exactly as
`sync` does. The Marbles-specific API mapping stays in Marbles; this command
carries the already-materialized document.

The corresponding gateway operation is `canvas.workspace.apply` with input
`{"document": {...}, "expected_revision": N}`. It is bounded by the same
document size and identity checks and still depends on the target lease
advertising the operation.

## Explicit workspace sync

```sh
canvas-fleet sync neo project-dashboard studio,firetv --dry-run
canvas-fleet sync neo project-dashboard studio,firetv
canvas-fleet sync neo project-dashboard all --activate
```

Sync reads one named owner document and concurrently imports it into the
selected endpoints. The source is excluded from the destination group. It does
not replace catalogs or switch the source. Without `--activate`, each target
keeps its selection (including an already-active imported project). Equal
documents are skipped; newer target revisions and equal-revision differences
are conflicts, never rebased. Every write binds the preflight endpoint revision,
is not retried, and is followed by exact owner-document read-back verification.
An endpoint failure does not hide other endpoint results; partial success is
not rolled back. `--dry-run` performs reads and conflict checks but no writes.

Documents are opaque bounded JSON (48 KiB maximum); Canvas alone validates
widget/layout semantics. Source URLs and local data references are not copied
or rewritten, so destinations still need access to referenced streams/assets.
Gateway read/import is intentionally refused until its adapter advertises
those operations; existing inspection/switch leases are not broadened.

For heterogeneous Canvas JSON codecs (for example Android float encoding),
optionally set `"document_codec":"/absolute/path/to/canvas-workspace-ctl"` in
the fleet config. Deploy Canvas's independently-built owner codec on the caller.
The controller invokes `normalize` with a bounded, correlated schema-v1 request,
canonicalizes source/existing/read-back through Canvas's own typed model, verifies
identity/generation retention and normalization idempotence, then compares exact
JSON. Without this option comparison remains strict and unchanged. Unibus never
duplicates the workspace schema or applies approximate float tolerance. Codec
failure aborts source sync before writes; no alternate decoder is substituted.
