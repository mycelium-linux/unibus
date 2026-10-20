# Private IPFS library

The `unibus-ipfs-library` binary adds a read-only catalog, browser, and MCP view
to the existing local Kubo client. Kubo remains the block, pin, and swarm owner.
The catalog observes direct/recursive pins. Optional uploads import and pin files
on one explicitly configured node. It does not replicate, unpin, or expose Kubo
administration. MFS can be empty while these pins consume storage.

Build with `cargo build -p unibus-ipfs --bin unibus-ipfs-library`. Install
`poppler-utils` and GNU `timeout` on the scanner host for optional PDF extraction.
The deployment contract and example configuration live in shared-infra at
`projects/ipfs/library/`.

## Operation

```sh
unibus-ipfs-library refresh /etc/ipfs-library/config.json
unibus-ipfs-library serve /etc/ipfs-library/config.json
unibus-ipfs-library label /etc/ipfs-library/config.json CID 'Readable name.glb'
unibus-ipfs-library mcp http://127.0.0.1:18806 /path/to/private-token
```

The configuration requires a loopback listener, absolute state/token paths, one
to eight named loopback Kubo RPC origins, and distinct HTTPS browser/content
origins. All data routes require the same private bearer token. Bind the service
behind an authenticated proxy; never give this bearer token to browser clients.
`/healthz` returns only status. No public gateway or arbitrary URL is accepted.

`catalog.json` is an atomic observation snapshot, limited to 32 MiB and 20,000
unique pins. Separate atomic `labels.json` overrides inference without changing
CIDs, file contents, or Kubo state. Refresh and label writers use separate file
locks. Back up both files. Failed inventories retain previous node membership,
last success, and an explicit offline state; a subsequent successful inventory
removes obsolete membership. Node badges are observations, not replication or
retrieval guarantees.

Each refresh inspects at most 96 roots and downloads at most 128 MiB for
metadata. Directory links and named pins supply names. JSON manifests up to
1 MiB supply titles and names for already pinned references, including Holodeck
part/source/variant metadata. Recovered labels are identified as metadata;
they are not claims about original filenames. Metadata visibility is displayed,
not treated as an authorization grant. PDF metadata/text extraction is limited
to files up to 16 MiB and the first five pages/24,000 characters; plain text uses
a 4 KiB prefix. JSON searchable text is capped at 24,000 characters. Unsupported,
large, or pending files remain searchable by their CID and known labels.

Browser routes are `/`, `/files/{cid}`, `/api/search`, `/api/nodes`, and
`/ipfs/{cid}`. Files must be cataloged and have a known size up to 2 GiB.
Downloads stream from observed replicas with two concurrent streams and a
five-minute timeout. A single byte range supports PDF readers and resuming;
an unmatched `If-Range` returns the complete representation. Streaming failures
abort the response; they do not silently produce a truncated successful file.
Active formats download as attachments. Content uses a separate origin, a
sandbox CSP, `nosniff`, and `no-store`; the browser and API escape metadata.

## MCP

The HTTP `/mcp` endpoint and stdio adapter expose `search_files`, `list_nodes`,
`get_file`, and `read_indexed_text`. They read the same snapshot as the browser.
All tools carry read-only annotations. Text returned by files is untrusted data,
never instructions. Pin changes, replication, and arbitrary RPC are absent.

The stdio adapter accepts newline-delimited JSON-RPC with 64 KiB request and
1 MiB reply bounds. Its upstream must be bare loopback HTTP, normally an SSH
forward to the catalog. It reads credentials from a private file and writes
only protocol replies to stdout. Register it using the existing Unibus MCP
publisher rather than adding another registry.

## Uploads and folders

Set optional `uploads` configuration with a configured node name, a distinct
private write token file, and the absolute path of that node's local Kubo storage
filesystem. The reader token cannot authorize imports. The Mycelium adapter
supplies the write token and authenticated actor only after checking both the
read grant and `web:ipfs-library:upload`. It also requires the exact primary
browser origin. No write credential is supplied to the MCP provider.

`/upload` provides native HTML forms for files and folders, up to 90 MiB total
and 256 files per submission. Chromium sends relative folder paths with native
form uploads; see its [FormData implementation](https://chromium.googlesource.com/chromium/src/+/HEAD/third_party/blink/renderer/core/html/forms/form_data.cc).
Browsers that omit folder paths get an explicit error instead of flattening the
tree. Empty directories are omitted by browser selection. No first-party
JavaScript is used. Progress and larger browser submissions need an optional
client for the chunked API.

The authenticated chunk API accepts `/api/upload/begin` with
`{name,folder,files:[{path,size}]}`, then `/chunk?session=ID&file=N&offset=BYTES`,
then `/commit?session=ID`. `/status` and `/cancel?session=ID` manage the pending
upload. All use POST. Chunks are at most 8 MiB; imports are at most 1 GiB and
256 files. One active session is owned by one authenticated actor. Exact chunk
retries are checked against stored bytes; wrong offsets or different replay data
fail. Repeating begin with the same manifest resumes the saved session.
Sessions expire after one hour without writes. A subsequent begin removes stale
staging. Numeric staging filenames keep user paths out of local filesystem writes.
Traversal, duplicate paths, and file/directory conflicts are rejected.

Admission keeps 2 GiB free plus staging and imported bytes. The upload ledger
limits total logical imported bytes to 8 GiB. This is separate from Kubo's soft
GC target. Imported data streams through Kubo's own multipart
[`add` API](https://docs.ipfs.tech/reference/kubo/rpc/#api-v0-add).
Completed roots are pinned with their name; root type, file size and recursive
pin are verified before reporting success. A durable receipt records the actor,
paths and CID. A pin that succeeds before a later catalog/receipt error is kept;
recovery must not unpin user data. Cancel removes staging, not existing pins.

The catalog is updated under the scanner lock. Folder paths are searchable as
bounded metadata. Folder detail pages list descendants; opening or downloading
a child remains anchored to the cataloged root. CIDs outside the collection and
traversal paths cannot be used to reach arbitrary content. Uploaded roots are
stored on the selected node only; this is not a replication policy.

## Verification

`cargo check -p unibus-ipfs --all-targets` is the iteration gate. Finish with
`cargo test -p unibus-ipfs --all-targets -- --test-threads=1`. Tests cover actual
Kubo empty pin-class responses, retained offline observations, label persistence,
manifest recovery, escaping/authentication, search/pagination, range parsing,
read-only schemas, and the existing real Unibus retrieval boundary.
