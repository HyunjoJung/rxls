# rxls browser viewer

The public viewer is a static, local-only product surface for the existing
`@rxls/render-worker` WebAssembly package. It opens XLS, XLSX, XLSM, XLSB, and
ODS files in one interface without sending workbook bytes to a server.

## Development

Build the render worker first, then prepare the static inputs and start Vite:

```sh
npm ci --prefix viewer --ignore-scripts
npm --prefix bindings/render-wasm run build:wasm
npm --prefix viewer run prepare:runtime
npm --prefix viewer run dev
```

The wasm-bindgen CLI must match
`bindings/render-wasm/toolchain-lock.json`. The hosted workflows install that
exact version with its pinned Rust toolchain.

Run the deterministic checks with:

```sh
npm --prefix viewer test
RXLS_BASE_PATH=/rxls/ npm --prefix viewer run build
npm --prefix viewer run test:browser
```

Set `RXLS_CHROMIUM_EXECUTABLE` when the browser test should use a specific
Chrome or Chromium binary. Set `RXLS_VIEWER_SCREENSHOTS=1` to write desktop and
mobile captures under `target/viewer-e2e/`.

## Runtime boundaries

- Workbook parsing and rendering run in a dedicated worker with the limits
  enforced by `@rxls/render-worker`.
- The UI rejects local inputs larger than 32 MiB and PNG exports larger than
  16 million pixels.
- A strict content security policy limits scripts, workers, images, and network
  requests to the deployed origin. Generated SVG is sanitized before insertion.
- `viewer/public/runtime/` and `viewer/public/samples/` are generated inputs;
  only project-owned or repository fixture workbooks are deployed.
- Hosted builds regenerate `samples/operations-report.xlsx` from
  `examples/author_report.rs` and require an exact byte-for-byte match.
- `THIRD_PARTY_NOTICES.txt` records the bundled UI license. The preparation step
  combines it with the renderer notices in the Pages artifact.

The `viewer-pages` workflow publishes `viewer/dist/` to GitHub Pages after an
exact-source build. The `render-browser` workflow also exercises the deployed
base path in the repository's pinned Chromium runtime.
