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
removed before entering WASM. A synchronous render already executing in the
dedicated worker cannot receive another worker message until it returns, but
its output is discarded; `client.terminate()` is the hard-stop boundary and
invalidates open document sessions.

The package never creates blob workers, uses `eval`, injects scripts, or
discovers local paths. Applications must serve `js/worker.mjs`, generated WASM,
and its glue from an allowed `worker-src`/`script-src`; WebAssembly compilation
also requires the browser's `wasm-unsafe-eval` CSP token. SVG returned through
the worker is size-checked and rejected if it contains active elements, event
handlers, external paint resources, or non-embedded image data.

Run focused gates from this directory:

```sh
cargo +1.85.0 test --locked
cargo +1.85.0 check --target wasm32-unknown-unknown --locked
npm test
rustup toolchain install 1.88.0 --profile minimal
WASM_BINDGEN_TOOL_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/rxls-wasm-bindgen-cli-0.2.126.XXXXXX")"
cargo +1.88.0 install wasm-bindgen-cli --version 0.2.126 --locked \
  --root "$WASM_BINDGEN_TOOL_ROOT"
PATH="$WASM_BINDGEN_TOOL_ROOT/bin:$PATH" npm run build:wasm
npm run test:browser
```

`toolchain-lock.json` pins the Rust 1.85 source MSRV, the separate Rust 1.88
host toolchain needed to compile the exact wasm-bindgen CLI into a fresh,
isolated tool root, wasm-pack/wasm-bindgen, the exact Chrome for Testing archive
identity, and browser heap/retention ceilings. The real worker smoke attaches
separately to the page and dedicated worker targets, samples their combined V8,
embedder, and backing-store memory through the DevTools protocol, synchronizes
garbage collection before baseline/retained samples, and fails above those
ceilings. The Node protocol tests have no third-party npm dependencies.

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
