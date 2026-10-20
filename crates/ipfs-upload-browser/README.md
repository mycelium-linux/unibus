# IPFS Library browser uploader

Rust/WebAssembly application for the existing authenticated IPFS Library chunk
API. Trunk and wasm-bindgen generate the module loader; there is no authored
JavaScript application. This standalone crate leaves the native Unibus workspace
independent of the browser toolchain.

## Build and install

Install Rust's `wasm32-unknown-unknown` target, Trunk 0.21.14, and
wasm-bindgen-cli 0.2.129. From this directory:

```sh
env RUSTC_WRAPPER= cargo check --target wasm32-unknown-unknown --locked
env -u NO_COLOR RUSTC_WRAPPER= trunk build --release --locked --offline \
  --skip-version-check --color never --dist /path/to/upload-bundle
```

Unsetting `NO_COLOR` avoids Trunk 0.21's clap boolean parsing conflict. The lockfile
pins the wasm-bindgen ABI to the installed generator. The bundle consists of
`index.html`, a hashed `.js` loader, and matching hashed `_bg.wasm`. Install all
three together in a root-owned versioned directory, then atomically switch a
`current` symlink. Set `uploads.browser_assets` to that absolute symlink in the
library configuration and restart the library. Generated assets are not committed.

The server supplies the library stylesheet and a fresh script CSP nonce. The
loader and Wasm are fetched through the same authenticated origin. No credentials
are embedded in assets; uploads use the visitor's existing SSO session and
separate signed upload grant. `/upload?basic=true` keeps native forms available.

## Behavior and verification

File and folder inputs preserve paths. Limits and part size come from server
status. SHA-256 is calculated with bounded part-sized reads; admission matches
the entire manifest, including checksums. Resume requires reselecting identical
files and uses saved server offsets. A lost part response is safe to retry;
three retries precede a saved-offset check. Pause aborts the active request.
Background save is polled separately and continues after a tab closes. Idle
staging expires after one hour. Empty folders are omitted by browser selection.

Verify in a permitted browser session: upload multiple files and a nested folder;
pause during a part, reload, select the same files, and resume; select changed
bytes and confirm rejection; interrupt a request and retry; reload during saving;
open the resulting link and compare downloaded SHA-256. Verify a reader cannot
upload, and content-origin POSTs are denied. For large transfers, use a file
above 2 GiB and check full download and a range above that offset.

The 2026-10-19 qualification executed the generated Wasm in controlled DOM/fetch
fixtures, covering initialization, checksum, accepted-part pause, reload resume,
changed-file rejection, failed-part retry, background polling, progress and the
result link. This runtime fixture check does not qualify a real browser's SSO
session. Shared-infra records the separate live Kubo transfer receipt and the
remaining owner/dad browser enrollment checks.
