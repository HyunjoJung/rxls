# rxls editor embed

This private development package embeds the existing spreadsheet editor. Build it from a
checkout with its matching viewer and WASM runtime prepared:

```sh
node viewer/scripts/build-editor-embed.mjs --output-dir target/editor-embed-build
npm pack --ignore-scripts ./target/editor-embed-build/package --pack-destination ./target/editor-embed-build
```

Choose a fresh output directory for each build. The script leaves source package files intact,
checks the cumulative file/byte budget before copying and rejects symlinks. Without an output
argument it chooses a fresh directory under `target/editor-embed/`. Review `build-result.json`
and the asset manifest, then install the produced tarball in the consumer application.

Install that tarball in a separate consumer. Copy the complete `assets/` directory without changing
its filenames to a versioned HTTP(S) directory such as `/consumer/nested/editor-v1/`. Preserve
`LICENSE.txt`, `THIRD_PARTY_NOTICES.txt` and `embed-manifest.json`. Serve `.wasm` as `application/wasm`
and modules with JavaScript MIME types. Keep package/client and asset versions together. Consumer
CSP must admit the asset origin in `frame-src`; the embedded page retains its packaged CSP.
Cross-origin hosting must admit its own runtime module/worker fetches. Avoid opaque/file origins.

```js
import { createEditor } from "@rxls/editor-embed";

const editor = createEditor(document.querySelector("#sheet"), {
  assetsUrl: "/consumer/nested/editor-v1/",
});
await editor.ready; // Starts empty; no sample or workbook is fetched.
editor.onChange((state) => console.log(state.dirty, state.draft));
await editor.load(new Uint8Array(await file.arrayBuffer()), { fileName: file.name });
const saved = await editor.save();
// Persist saved.bytes using the consumer's own save destination.
await editor.dispose();
```

Use a container with explicit width/height. Each iframe owns its editor state, drafts, worker and
MessagePort. These trusted package assets share normal browser execution privileges; an iframe
does not make arbitrary untrusted package code safe. Each instance uses an exact-origin/window
handshake followed by its own port. Parent input buffers remain attached; one owned copy transfers.

Call load/save only after `ready`. One operation is admitted at a time; additional operations reject
with `busy`. Input/output workbooks are capped at 32 MiB. Startup is bounded to 30 seconds and
operations to 60 seconds. A timeout closes that instance; create another. Unexpected frame reloads
are terminal. `dispose()` rejects in-flight work, requests synchronous worker teardown and removes
only its own iframe within one second; repeated disposal returns the same completion promise.

Replacement rejects dirty edits, active grid drafts, unapplied dialogs and pending paste previews.
`load(bytes, { fileName, replace: "discard" })` explicitly discards such state, after rejecting any
active mutation. Load succeeds after current full/page output or the first current visible tile covers the actual
viewport; empty visible geometry is valid. A collapsed container with positive geometry waits for
a visible viewport. Failure is recoverable as an empty editor with a diagnostic. Save commits the grid draft through the existing controller,
refuses unapplied dialog/paste state and returns same-format XLSX/XLSM bytes. Saving a copy does
not reset `dirty`; it does not assert that a consumer wrote the bytes to disk.

Editing is the existing viewer's supported cell/range/property/history path. Read-only formats
remain viewable with their current capability/reason. XLSM macros are preserved as package parts;
the browser never executes them. This adapter does not promise complete Excel authoring parity.

`ready` acknowledges the empty editor bootstrap. `load()` awaits the initial current view;
`state.loaded` reports source presence and can be true while that view is busy. Large tiled views
are read-only displays of the same package: supported XLSX/XLSM save-copy remains available, and
bounded full-sheet views retain the existing editing/history controls.
