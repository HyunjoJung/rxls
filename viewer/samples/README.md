# Viewer samples

`operations-report.xlsx` is generated from the project-owned
`examples/author_report.rs` scenario. The Pages preparation script also copies
small project fixtures for the XLS, XLSB, and ODS sample entries. Generated
runtime and copied sample files under `viewer/public/` are build artifacts and
are not committed.

Regenerate the authored sample from the repository root:

```sh
cargo run --locked --example author_report -- viewer/samples/operations-report.xlsx
```

## Macro fixture provenance

`apache-poi-simple-macro.xlsm` is Apache POI's `SimpleMacro.xlsm` test
workbook, copied without modification from the immutable upstream revision
`aa268199243921dd0d9e1dc8d96cc06331280c94`:

<https://raw.githubusercontent.com/apache/poi/aa268199243921dd0d9e1dc8d96cc06331280c94/test-data/spreadsheet/SimpleMacro.xlsm>

- License: Apache-2.0
- Size: 13,796 bytes
- SHA-256: `f76c986f4ebc25c2cc57c088b2511a1269f4bd61d6223a2ab58db351da348ba6`

The viewer uses this workbook only as a deterministic macro-preservation
fixture. `scripts/prepare-runtime.mjs` verifies its size and digest before
copying it into the generated static site.
