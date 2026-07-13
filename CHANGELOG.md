# Changelog

All notable changes to `rxls` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-07-13

### Fixed

- Hardened `.xls` BIFF parsing for OLE2 `Workbook`/`Book` streams that are malformed
  or semantically empty by introducing explicit header/truncation checks.
  Arbitrary bytes no longer parse as a successful (empty) workbook; they now
  return `Error::Biff`, while still allowing valid header-only BIFF payloads to
  round-trip as an empty but typed `Workbook` (and `extract_text` still reports
  `NoText`).
- Added regression tests for malformed `Workbook` streams (empty stream and random
  bytes), unsupported or misplaced `BOF` records, truncated records, unbalanced
  substreams, and valid-header, no-cell BIFF payloads.

## [0.1.0] - 2026-07-11

First public release. `rxls` is a native Rust spreadsheet toolkit with no JVM,
Apache POI, or runtime subprocess dependency.

### Added

- Readers for `.xls` (BIFF8 and BIFF5/7), `.xlsx`, `.xlsb`, and `.ods`, with
  typed cells, formulas and cached values, merged ranges, hyperlinks, comments,
  validation rules, tables, views, page setup, images, charts, defined names,
  document properties, and sheet visibility where the source format exposes them.
- A styled `.xlsx` writer covering formulas, merged cells, rich strings,
  hyperlinks, comments, images, charts, tables, validation, conditional
  formatting, protection, print settings, views, and document properties.
- Package-preserving `.xlsx` and `.xlsm` editing through `Spreadsheet`, with a
  typed `EditCapability` explaining why read-only formats or lossy packages
  cannot be saved.
- A rectangular `Range` API, optional `serde` row deserialization, optional
  `chrono` conversions, and calamine-style workbook convenience methods.
- Bounded deterministic formula evaluation with typed cached-value fallbacks for
  unsupported, volatile, circular, external, or oversized expressions.
- CSV, HTML, and Markdown export, deterministic `WorkbookReport` JSON, and CLI
  commands for extraction, conversion, diagnosis, comparison, and corpus reports.
- A portable `wasm32` adapter plus an isolated `wasm-bindgen` `cdylib` for
  extraction, export, and report generation.
- Reproducible public-corpus and oracle harnesses. The pinned 916-file recipe
  opens 876 inputs with 40 expected rejections and zero unexpected failures;
  comparable files reach 99.520% mean `.xls`, 99.889% mean `.xlsx`/`.xlsm`,
  and 100.000% mean `.xlsb` and `.ods` parity under the documented gates.
- Fuzz targets for parsing, authoring, and package-preserving editing.

### Changed

- The minimum supported Rust version is 1.85 and is enforced across no-default,
  default, and all-feature builds.
- Migrated `cfb` from 0.10 to 0.14 and `quick-xml` from 0.36 to 0.41 after unit,
  MSRV, and full public-corpus regression verification. `zip` remains on the
  compatible 2.x line because 8.x requires Rust 1.88.
- `Cargo.lock` is tracked and CI, packaging, and publication use locked
  dependency resolution.
- CI now covers public hygiene, all feature combinations, strict rustdoc,
  Python harnesses, package verification, `wasm32`, and a pinned parity corpus.
- Tag releases validate `v<package-version>` on `main`, emit checksummed release
  evidence, publish idempotently, and create or update the GitHub release.

### Security

- The crate forbids unsafe Rust and applies bounds, depth, node, part-size,
  recursion, range, and accumulated-text limits to untrusted input.
- Shared-string amplification, malformed CFB stream sizes, ZIP package metadata,
  XML entity references, and XML 1.0 character validity are checked before
  allocation or mutation.
- Edit operations preflight write-side budgets and preserve every untouched
  package part byte-for-byte; a package that cannot be preserved is read-only.
- Public release tooling scans tracked and untracked release inputs, including
  Office package member names and XML, for secrets, local paths, and internal
  project traces.

### Fixed

- Reassembled BIFF `LABEL`, `RSTRING`, cached formula strings, and shared strings
  that span `CONTINUE` records, including per-chunk compression changes.
- Recovered implicit `.xlsx` rows and cells without `r` attributes.
- Prevented nested BIFF chart or pivot substreams from desynchronizing later
  worksheets.
- Bounded shared-string reference amplification and tolerant CFB fallback reads
  that previously could request attacker-controlled allocations.
- Preserved and validated XML character references across strings, formulas,
  comments, metadata, charts, drawings, and editable package parts after the
  `quick-xml` migration.

[Unreleased]: https://github.com/HyunjoJung/rxls/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/HyunjoJung/rxls/releases/tag/v0.1.1
[0.1.0]: https://github.com/HyunjoJung/rxls/releases/tag/v0.1.0
