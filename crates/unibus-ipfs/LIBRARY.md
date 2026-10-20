# Private IPFS library

The `unibus-ipfs-library` binary adds a read-only catalog, browser, and MCP view
to the existing local Kubo client. Kubo remains the block, pin, and swarm owner.
The catalog observes direct/recursive pins; it does not replicate, unpin, upload,
or expose Kubo administration. MFS can be empty while these pins consume storage.

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

## Verification

`cargo check -p unibus-ipfs --all-targets` is the iteration gate. Finish with
`cargo test -p unibus-ipfs --all-targets -- --test-threads=1`. Tests cover actual
Kubo empty pin-class responses, retained offline observations, label persistence,
manifest recovery, escaping/authentication, search/pagination, range parsing,
read-only schemas, and the existing real Unibus retrieval boundary.
