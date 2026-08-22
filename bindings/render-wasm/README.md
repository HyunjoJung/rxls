# rxls render worker

This package is the browser/WASM rendering surface for `rxls-render`.
It keeps parsing, pagination, shaping, SVG serialization, and PNG rasterization
inside a dedicated module worker and exposes one sheet, tile, or print page at a
time.

The worker protocol is `rxls.render-worker.v1`. An `open` request creates one
`RenderSession`, so later virtual tile/page requests reuse the parsed workbook
and a verified in-memory font pack. Input, font members, embedded images,
layout work, scene nodes, page count, raster pixels, and output bytes all have
hard ceilings. Requests may lower those ceilings but cannot raise them. The
worker also caps pending requests at 32 and ignores cancellation identifiers
that do not name active or queued work. Open and queued transferable resources
share a 128 MiB byte budget in both the client and worker.

```js
import { RenderWorkerClient, getRenderWorkerUrl } from "@rxls/render-worker";

const client = new RenderWorkerClient(getRenderWorkerUrl());
const opened = await client.open(workbookBytes, { documentId: "report" });
const pageMap = await client.preparePages(opened.documentId, 0);
const firstPage = await client.renderPage(opened.documentId, 0, 0);
viewer.replaceChildren(svgElement(firstPage.svg));
```

`getRenderWorkerUrl()` resolves `js/worker.mjs` relative to the installed client
module, so the worker, its JavaScript imports, and generated WASM stay in the
same published package. Do not pass a bare package specifier to `new URL()`;
the browser URL constructor does not apply package exports or import-map
resolution to its first argument. A bundler or static server must expose the
package assets at the URL from `getRenderWorkerUrl()`.

Font packs use the existing `rxls.render-font-pack.v1` manifest. The client
accepts `{ manifest, members: [{ name, bytes }] }`, copies transferable buffers,
and the worker builds a bounded `rxls.font-bundle.v1` envelope. Rust validates
the file set, canonical names, sizes, SHA-256 identities, licenses, and OpenType
faces without filesystem or host-font discovery. PNG text output requires this
verified pack; SVG remains available without one.

Cancellation uses `AbortSignal` or `client.cancel(requestId)`. Queued work is
removed before entering WASM. Rendering inside WASM is synchronous and is not
cooperatively cancellable: once that call is executing, the worker cannot
receive a soft-cancel message until the call returns. A soft-cancel still
rejects the local promise and discards any eventual output, but it does not
promise to stop active CPU work. `client.terminate()` is the active-work
hard-stop boundary. It destroys the dedicated worker, rejects every active and
queued request with `client_closed`, releases their transferable buffers, and
invalidates all open document sessions.

The package never creates blob workers, uses `eval`, injects scripts, or
discovers local paths. Applications must serve `js/worker.mjs`, generated WASM,
and its glue from an allowed `worker-src`/`script-src`; WebAssembly compilation
also requires the browser's `wasm-unsafe-eval` CSP token. SVG returned through
the worker is size-checked and rejected if it contains active elements, event
handlers, external paint resources, or non-embedded image data.

Run focused gates from this directory:

```sh
RUST_185_BIN="$(dirname "$(rustup which cargo --toolchain 1.85.0)")"
PATH="$RUST_185_BIN:$PATH" RUSTC="$RUST_185_BIN/rustc" \
  RUSTUP_TOOLCHAIN=1.85.0 "$RUST_185_BIN/cargo" test --locked
PATH="$RUST_185_BIN:$PATH" RUSTC="$RUST_185_BIN/rustc" \
  RUSTUP_TOOLCHAIN=1.85.0 "$RUST_185_BIN/cargo" clippy --locked --all-targets -- -D warnings
PATH="$RUST_185_BIN:$PATH" RUSTC="$RUST_185_BIN/rustc" \
  RUSTUP_TOOLCHAIN=1.85.0 "$RUST_185_BIN/cargo" check \
  --target wasm32-unknown-unknown --locked
PATH="$RUST_185_BIN:$PATH" RUSTC="$RUST_185_BIN/rustc" \
  RUSTUP_TOOLCHAIN=1.85.0 RUSTDOCFLAGS="-D warnings" \
  "$RUST_185_BIN/cargo" doc --locked --no-deps
npm test
rustup toolchain install 1.88.0 --profile minimal
WASM_BINDGEN_TOOL_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/rxls-wasm-bindgen-cli-0.2.126.XXXXXX")"
RUST_188_BIN="$(dirname "$(rustup which cargo --toolchain 1.88.0)")"
PATH="$RUST_188_BIN:$PATH" RUSTC="$RUST_188_BIN/rustc" \
  RUSTUP_TOOLCHAIN=1.88.0 "$RUST_188_BIN/cargo" install wasm-bindgen-cli \
  --version 0.2.126 --locked --root "$WASM_BINDGEN_TOOL_ROOT"
PATH="$WASM_BINDGEN_TOOL_ROOT/bin:$PATH" npm run build:wasm
npm run test:browser
```

`toolchain-lock.json` pins the Rust 1.85 source MSRV, the separate Rust 1.88
host toolchain needed to compile the exact wasm-bindgen CLI into a fresh,
isolated tool root, wasm-pack/wasm-bindgen, the exact Chrome for Testing archive
identity, and browser heap/retention ceilings. The release-runner Linux
process-tree RSS ceiling remains 1 GiB. macOS uses a separate 2 GiB absolute
ceiling because its summed per-process RSS includes shared Chromium pages in
each helper; both platforms retain the same 512 MiB post-baseline growth
ceiling.

The real worker smoke attaches separately to the page and every live dedicated
worker target. It fails closed if any DevTools heap field is absent, samples
combined V8, embedder, and backing-store memory, records bounded process-tree
RSS high-water evidence, and synchronizes garbage collection before baseline
and retained samples. Baseline collection completes before each uncached
fixture generation. The fixture is a project-owned deterministic 8,192-cell
OOXML workbook with a generated 64x64 PNG and verified generated OpenType font
pack. The browser renders a 2,048-cell virtual tile while budgeting the 4,096
same-row measurement candidates used by automatic row height, then renders one
print-page SVG and PNG. It pins the embedded PNG byte identity, dimensions,
color format, browser decode, and decoded RGBA SHA-256.

The CSP negative control requires one exact enforced `connect-src` violation
and proves that the blocked URL never enters the DevTools Network pipeline.
A second isolated, CSP-free DevTools target attempts a loopback off-origin
request; Fetch interception stops it before transport, Network must report the
same request identity with `net::ERR_INTERNET_DISCONNECTED`, no response may
arrive, and a bounded local sink must receive zero requests. The hard-stop
control binds a random nonce to a unique worker URL and request, pauses the
active worker on a confirmed WebAssembly frame, and requires
`Target.targetDestroyed` plus absence from `Target.getTargets` within two
seconds. Detachment, natural completion, ambiguous or wrong-nonce targets, and
JavaScript-only pauses cannot satisfy the proof. The post-GC retained-heap gate
covers the surviving page and every live render worker, so the terminated
worker cannot retain workbook, font, image, or output buffers. Fixture
provenance and exact SHA-256/size identities live beside the browser tests;
those test-only files are excluded by the package's explicit `files` allowlist.
The Node protocol tests have no third-party npm dependencies.

`THIRD_PARTY_NOTICES.txt` records the exact Cargo normal-dependency closure used
to build the WebAssembly artifact for `wasm32-unknown-unknown`, including
proc-macro support reached through those edges. It is generated from the nested
locked manifest, includes every crate's declared license and locked registry
checksum, and carries the corresponding legal files deduplicated by raw SHA-256.

## Distribution

Registry releases use the public package name `@rxls/render-worker`. Every
candidate is packed, inspected against an exact file and size contract,
publication-dry-run, installed into a clean consumer, and bound to its source
commit. The release gate independently checks the nested Rust advisory,
license, and source policy, verifies the checked notice against the production
closure, and uploads a deterministic, path-neutral CycloneDX manifest with the
candidate. A manual `Render package release` workflow run performs verification
only. Publication is restricted to an exact `render-v<package-version>` tag on
`main`. The tag gate requires same-commit CI, CodeQL, dispatched renderer
hardening, pinned-browser coverage, and a successful two-run 800-workbook
LibreOffice campaign whose absolute, repeatability, authored-print, and
reviewed-baseline ratchets all pass. Publication then passes through the
protected `npm-render-worker` environment before npm receives the verified
tarball and provenance identity.
