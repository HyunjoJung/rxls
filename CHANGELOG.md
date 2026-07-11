# Changelog

All notable changes to `rxls` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **MSRV raised 1.74 → 1.85 (breaking).** `cfb`'s `uuid` dependency and
  `zip`'s `indexmap`/`hashbrown` dependency both declare `rust-version =
  "1.85"`, so the previously-stated 1.74 floor was already false for any
  build that resolves the current lockfile — this makes `Cargo.toml` match
  reality. No other resolved dependency requires a newer toolchain.

### Added
- **Package-preserving `.xlsx`/`.xlsm` editing.** `Spreadsheet::open` wraps a
  `Workbook` together with its original OOXML package bytes and reports a
  typed `EditCapability` — `ReadWrite`, or `ReadOnly(EditReadOnlyReason)` with
  the reason spelled out (`LegacyBiff`, `BinaryPackage`, `OpenDocument`,
  `PackageMetadataLoss`). `.xls`, `.xlsb`, and `.ods` are read-only through
  this API; only `.xlsx`/`.xlsm` support in-place edits. Edit methods
  (`set_cell_value`, `set_cell_formula`, `append_row`, `clear_range`,
  `set_document_properties`, `set_defined_name`, `rename_sheet`,
  `set_sheet_visibility`, `set_active_sheet`, `set_sheet_tab_color`) mutate
  worksheet/workbook XML through a new arena-based `XmlTree` edit engine
  (`src/xmltree.rs`) instead of raw string-splicing, so every part the edit
  didn't touch is preserved byte-for-byte. New text is written as inline
  `<is>` strings rather than growing the shared string table. `Package`
  tracks two integrity flags surfaced through `EditCapability` —
  `is_complete()` (every original ZIP entry, including symlinks, was
  retained) and `is_meta_lossy()` (content-types/relationships couldn't be
  round-tripped losslessly enough for editing) — implementing a
  lenient-read/strict-edit split: a package that opens fine for reading can
  still report `ReadOnly(PackageMetadataLoss)` for editing if it can't be
  saved back losslessly. `XmlTree` itself enforces parse/mutation budgets
  (max depth, node count, attributes per element) and rejects XML-1.0-illegal
  characters and malformed input rather than silently repairing it.
- **Deterministic formula evaluation.** `Workbook::evaluate_cell(sheet, row,
  col) -> FormulaEvaluation` evaluates a bounded, safe MVP grammar
  (arithmetic, comparison, concatenation, ranges, and a fixed function set)
  and returns `FormulaEvaluation::Computed(Cell)` on success. Anything
  outside the MVP — volatile functions, external references, circular
  references, unresolved names, array/dynamic-array semantics, oversized
  ranges, missing sheets, or expressions past the bounded recursion depth —
  falls back to `FormulaEvaluation::Fallback { cached, reason }` with a typed
  `FormulaUnsupportedReason` explaining why, instead of guessing or panicking.
- **CSV / HTML / Markdown exporters.** `Sheet::to_csv` /
  `to_csv_with_delimiter` / `to_html` / `to_markdown`, plus
  `Workbook::to_csv(sheet_index)` / `to_csv_with_delimiter` / `to_html` /
  `to_markdown` convenience wrappers that resolve a sheet index and return
  `None` for out-of-range or non-worksheet indices.
- **`WorkbookReport` diagnose JSON.** `WorkbookReport::from_workbook` (and,
  behind the `xlsx` feature, `from_workbook_with_package`, which adds
  retained-package part-inventory counters) builds a compact, deterministic
  report — sheet/cell/formula counts, document properties, feature
  inventory, and derived warnings — serialized without an external
  dependency via `to_json()`. Exposed on the CLI as `rxls diagnose <file>`.
  A new `rxls csv <file>` subcommand exports a sheet as CSV directly.
- **Wasm adapter.** `src/wasm.rs` exposes `extract_text_bytes`,
  `to_csv_bytes`, `to_html_bytes`, and `report_json_bytes` (reusing the
  native parser/exporter/report code), plus `wasm-bindgen` JS wrappers
  compiled only for `wasm32`. The crate now builds a `cdylib` in addition to
  `rlib`. The `rxls` CLI binary is gated behind a new `cli` Cargo feature,
  on by default so `cargo build`/`cargo run --bin rxls` are unaffected for
  existing native consumers; a `--no-default-features` build (as already
  used for the dependency-light `.xls`-only configuration) drops the bin
  target, which is what a `cdylib`-only `wasm32-unknown-unknown` build needs
  to avoid both targets colliding on the same output filename.

## [0.1.0] — 2026-06-22

First release. `rxls` is a native-Rust spreadsheet library — no JVM, no Apache POI,
no subprocess.

- **Readers:** legacy `.xls` (BIFF8 & BIFF5/7, OLE2/CFB), `.xlsx` (SpreadsheetML),
  `.xlsb` (BIFF12, `xlsb` feature), and `.ods` (OpenDocument, `ods` feature). Typed
  cells (number, date, boolean, error, text, formula); `.xls`/`.xlsb` formula tokens
  and `.xlsx` shared-formula followers are decompiled to source where supported;
  merged ranges; Korean cp949 and other BIFF5/7 codepages.
- **Writer (`.xlsx`):** styled cells, formulas, merges, hyperlinks, rich strings,
  page setup, sheet views (gridlines/zoom/freeze), hidden sheets, row/column outline
  grouping, print options, autofit, data validation, conditional formatting, images,
  charts, tables, cell comments/notes, defined names, and document properties. Plus a
  validating `Workbook::to_xlsx_checked() -> Result<_, WriteError>`.
- **Safety:** `#![forbid(unsafe_code)]`, panic-free / bounds-checked, bounded
  allocation on adversarial input. Validated against `xlrd` / `openpyxl` / `pyxlsb` /
  `odfpy` oracles and a public `.xls`/`.xlsx` corpus head-to-head vs `calamine` (see
  the README).

The detailed development history that produced this release follows.

### Fixed
- **`.xls` LABEL/RSTRING/STRING text spanning CONTINUE records is now reassembled.**
  A long inline label or cached formula-string whose characters overflowed the
  ~8224-byte BIFF record cap into a CONTINUE record was silently truncated (only
  the SST handled that boundary). The reader now gathers a record plus its
  following CONTINUE bodies and decodes across them (BIFF8 re-reads the
  compression flag per chunk; BIFF5/7 continues the codepage byte run). A
  truncated *single* record now yields best-effort partial text rather than being
  dropped. Regression test covers compressed and uncompressed (Korean) splits.

### Changed
- **Reproducible, CI-gated validation.** CI now runs the openpyxl strict-consumer
  gate for real (`RXLS_REQUIRE_OPENPYXL=1` turns a skip into a failure), builds and
  tests the `.xls`-only configuration (`--no-default-features`), and adds a
  `public-parity` job that fetches calamine's MIT `.xls` suite and gates xlrd
  parity at ≥0.95 (currently 100%). New black-box `tests/integration.rs` exercises
  the public read+authoring API. README now labels every validation figure by
  reproducibility tier and documents a one-command `Reproduce` path.

### Added
- **Reader surfaces merged ranges and `.xlsx` formulas.** `Sheet::merged_ranges()`
  returns merged cell regions read from `.xls` `MERGECELLS` and `.xlsx`
  `<mergeCells>` (0-based, inclusive). `.xlsx` cells carrying an `<f>` are now read
  as `Cell::Formula { formula, cached }` — the formula source plus its cached
  value (which remains the display text). (`.xls` formula-token decompilation is
  still out of scope; the cached value is read.) Read-discovered merges are kept
  separate from authoring merges, so exposing them never causes the writer to drop
  a source file's cells on a read→write — extraction stays full-fidelity.
- **`.xlsx` authoring — generate styled spreadsheets from data.** Beyond reading
  and round-tripping, `rxls` now builds rich `.xlsx`: `Workbook::new()`,
  `Sheet::write` / `write_styled` / `write_url` / `merge` / `set_col_width` /
  `set_row_height` / `freeze_panes` / `autofilter`, with an inline `CellStyle`
  (font family/size/color/bold/italic, fill, borders, number formats such as
  `₩#,##0` and `yyyy-mm-dd`, alignment + wrap). The writer interns styles into
  deduped OOXML resource tables (`<fonts>`/`<fills>`/`<borders>`/`<numFmts>` →
  `<cellXfs>`). Validated with openpyxl reading back a styled bid-comparison
  report. `Workbook` now carries `date1904` (preserved on round-trip) and `Cell`
  gains a `Formula` variant.

### Security
- **Bounded memory on malformed `.xls` (tolerant CFB path).** The tolerant CFB
  fallback pre-reserved a `Vec` sized by a directory entry's attacker-controlled
  stream size, so a ~2.5 KB crafted file could demand a multi-GiB allocation and
  `abort()` the process — uncatchable by `catch_unwind`. The declared size is now
  rejected when it exceeds the file, and the buffer grows only as the actual
  (minifat-chain-bounded) data is appended. Regression test added.
- **Unbounded allocation from shared-string reference amplification.** A small
  crafted `.xlsx` (or `.xls`) — one large pooled string referenced from very many
  cells, cloned into each — could drive a multi-gigabyte allocation and an OOM
  *abort* (which `catch_unwind` cannot trap), a denial-of-service on untrusted
  input. Both paths now enforce a per-workbook accumulated-text budget
  (`MAX_TEXT_BYTES`, 256 MiB); once spent, further cells are dropped. Found by the
  Apache POI `poc-shared-strings.xlsx` regression file (~8 GiB before the fix).

### Fixed
- **`.xlsx` cells/rows without an `r` attribute were dropped.** The `r` reference
  is optional in [ISO/IEC 29500]; when omitted, position is implicit (cells fill
  left-to-right, rows top-to-bottom). `rxls` required `r` and discarded every
  `r`-less cell, losing whole sheets from files written by LibreOffice, EPPlus,
  and similar tools. `parse_sheet` now tracks the implicit row/column position
  (and resyncs to any explicit `r`). Lifted the affected POI files from ~0–8% to
  100% parity vs `openpyxl`.

- **Embedded-substream sheet desync.** Worksheets are matched to their cell
  substreams by tracking BOF/EOF nesting depth, not by a running BOF count. A
  worksheet may embed nested substreams (charts, pivot tables) as `BOF … EOF`;
  the old running count treated each nested `BOF` as a new sheet, so every sheet
  after the first embedded chart was silently dropped (its cells indexed past the
  end and discarded), and the chart substream's own records were misread as cells
  of the preceding sheet. Now only a top-level (depth-0) `BOF` advances the sheet
  index, and cell records are decoded only at depth 1 — so nested chart/pivot
  records are never mistaken for cells. On the public corpus this recovered
  ~350k previously-dropped cells (GovDocs1) and lifted affected files from
  ~20–40% to 98–100% parity vs `xlrd`, with no change to files that never embed
  substreams (the Korean government corpus stays at 99.998%).

### Added
- **`.xlsx` (OOXML / SpreadsheetML) support** behind the default `xlsx` feature:
  `Workbook::open` now auto-detects the format (OLE2 `.xls` vs ZIP `.xlsx`) and
  reads modern Excel files — `workbook.xml` (sheets, 1900/1904), `sharedStrings`,
  `styles` (number formats), and the worksheet cells — into the same typed
  [`Cell`] model and text output. Dates/percentages reuse the `.xls` number-format
  logic, so output is identical across formats. Deps `zip` + `quick-xml` are
  optional; `default-features = false` keeps the dependency-light `.xls`-only
  build. Validated against `openpyxl` on the Apache POI `.xlsx` suite (311/332
  comparable files ≥99%; residual is number-format rendering).

- **Tolerant OLE2/CFB fallback** for legacy `.xls` containers the strict `cfb`
  crate refuses. When `cfb::CompoundFile::open` fails — most often a directory
  whose red-black-tree sibling ordering violates [MS-CFB] though the streams are
  intact — `rxls` falls back to a minimal, bounds-checked reader (still
  `#![forbid(unsafe_code)]`) that walks the directory entries *linearly* and
  recovers the `Workbook`/`Book` stream (header → DIFAT → FAT → directory, with
  both regular- and mini-FAT stream reads). This opens real-world files that the
  strict `cfb` reader and `xlrd` cannot — at 100% cell parity vs `xlrd` on the
  recovered files — and never changes the output of files the fast path reads.

- `Error::LegacyBiff` distinguishes a raw, pre-OLE2 Excel 2.0/3.0/4.0 stream
  (BIFF2–4, `BOF` `0x0009`/`0x0209`/`0x0409`) from arbitrary non-OLE2 input
  (`Error::NotOle2`). BIFF2–4 predate the OLE2-wrapped [MS-XLS] format and are
  out of scope; the precise error documents that boundary.

- Initial public release of `rxls`.
- `extract_text(&[u8]) -> Result<String>` convenience entry point.
- `Workbook` / `Sheet` API: parse the BIFF8 record stream, decode the shared
  string table (SST, including `CONTINUE`-spanning strings), and flatten cell
  records (`LABELSST`, `LABEL`, `RK`, `MULRK`, `NUMBER`, `FORMULA` + `STRING`)
  to per-sheet text.
- BIFF5/7 support: detect the BIFF generation from the first `BOF`, read the
  `CODEPAGE` record, and decode 8-bit strings in the workbook's ANSI codepage
  (Korean cp949, Japanese cp932, …) via `encoding_rs`. `Workbook::open_with_codepage`
  forces a codepage for files with a missing/wrong `CODEPAGE`.
- **Typed reader API**: a `Cell` enum (`Text`/`Number`/`Date`/`Bool`/`Error`)
  with `Sheet::cell(row, col)`, `Sheet::cells()`, and `Sheet::dimensions()` — so
  `rxls` is a typed `.xls` reader, not only a text extractor. `to_text` is built
  on the same typed cells.
- `BOOLERR` (0x0205) and the `FORMULA` boolean/error result branches: booleans
  render `TRUE`/`FALSE`, error cells render their code (`#DIV/0!`, `#N/A`, …).
- `RSTRING` (rich-text) cells decoded like `LABEL`.
- `FILEPASS` (password-protected) workbooks reported as `Error::Encrypted`
  instead of emitting ciphertext.
- `MULRK` count clamped to the record length (libxls-style robustness guard).
- **Number-format aware rendering** via `XF` (0x00E0) + `FORMAT` (0x041E) +
  `DATEMODE` (0x0022): date/time serials render as ISO `yyyy-mm-dd[ hh:mm:ss]`
  and percentages as `N%`, instead of raw IEEE-754 serials. Built-in and custom
  format codes are classified per [MS-XLS]/[MS-OSHARED] (incl. East-Asian and
  Thai locale sub-ranges, literal-aware percent, elapsed-time `[h]`). Excel 1900
  phantom-leap-day and 1904 epoch handled; edges unit-tested.
- Typed [`Error`] enum; panic-free, bounds-checked parsing.
- Validated against [`xlrd`](https://pypi.org/project/xlrd/) (Python reference)
  at ~99.99% parity over the readable files of a 409-file BIFF8 Korean-government
  corpus; rxls
  also extracts the files xlrd cannot open. The **BIFF5/7 record path is
  additionally validated on real BIFF5 files** from calamine's test suite —
  100% parity vs xlrd across 13 reference files (3 genuine BIFF5 + date/time/
  percent/SST-CONTINUE edge cases). A 3 200-run mutation fuzz produced zero
  panics. Harnesses: `scripts/{xls-xlrd-parity,xls-fuzz}.py`,
  `scripts/fetch-xls-reference.sh`.

[Unreleased]: https://github.com/HyunjoJung/rxls/commits/main
