# Contributing to rxls

Thanks for your interest in improving `rxls` — a native Rust spreadsheet
library that reads `.xls` (BIFF8/5/7), `.xlsx`, `.xlsb`, and `.ods`, writes
and package-preservingly edits `.xlsx`/`.xlsm`, evaluates a deterministic
formula subset, and exports CSV/HTML/Markdown.

## Ground rules

- **No `unsafe`.** The crate is `#![forbid(unsafe_code)]`. Parsing untrusted
  files must never crash a host process — every byte access is bounds-checked
  and malformed input must surface as an [`Error`], not a panic.
- **Document every public item.** The crate denies `missing_docs`.
- **Keep dependencies minimal.** The default build depends only on `cfb`,
  `encoding_rs`, `thiserror`, `zip`, and `quick-xml` (the latter two behind
  the default-on `xlsx` feature); `--no-default-features` drops to the
  `.xls`-only trio. New dependencies need a strong justification.
- **Follow the spec.** Behaviour should be traceable to [MS-XLS] / [MS-XLSB] /
  [MS-CFB] / ECMA-376 (SpreadsheetML) / ODF 1.2. Cite the relevant section in
  comments when implementing record or element details.
- **Bounded everything.** Adversarial input is a first-class concern: depth,
  node-count, part-size, and total-allocation budgets must hold on both the
  read and write/edit paths, and edits must be preflighted so a failed edit
  never leaves a half-mutated package.

## Before opening a PR

Run the full local gate (all must pass clean):

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --no-default-features
cargo doc --no-deps
```

## Tests

- Unit tests build minimal in-memory structures (BIFF records, OOXML zip
  packages, XML trees) so parsers and editors are exercised without large
  binary fixtures; small committed fixtures under `tests/fixtures/` cover
  each container format end to end.
- The trickiest reader area is the SST: shared strings that span `CONTINUE`
  records and re-specify their compression flag at the boundary. Any change
  there must keep the split-string test green.
- The trickiest edit-path invariant is byte preservation: a no-op
  open → save must reproduce every untouched part byte-for-byte, and an edit
  may only rewrite the parts it actually touched.
- Fuzz targets (`fuzz/`) cover parsing, authoring, and the package-preserving
  edit surface: `cargo +nightly fuzz run parse|author|edit`.

## Scope

Reading targets faithful typed-cell extraction with display-formatted text;
formula evaluation is limited to the deterministic subset exposed by
`Workbook::evaluate_cell` (everything else falls back to the cached value
with a typed reason). Editing is `.xlsx`/`.xlsm`-only and package-preserving.
Full custom number-format rendering, styling semantics, macro execution, and
pivot-table semantics are out of scope; unmodeled parts are preserved, not
interpreted. Larger features are welcome — open an issue first.

[MS-XLS]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/
[MS-XLSB]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xlsb/
[MS-CFB]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cfb/
