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
