# Changelog

All notable changes to `rxls` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

No unreleased changes are recorded yet.

## [0.1.2] - 2026-07-16

This release combines the remaining correctness, editing, runtime, security,
and release-engineering work into one large release.

### Added

- Completed BIFF/XLSB formula-source recovery, including the audited official
  function table, reference flags, 3D/name/shared/array expressions, and
  explicit `_xlfn.RXLS_PARTIAL(...)` markers for unsupported token sequences.
- Expanded deterministic formula evaluation with range/name/date/coercion
  semantics, shared operation and formula-dependency depth budgets, and
  explicit calculated, cached, unsupported, and error outcomes.
- Added cross-format rich-text, layout, style, Unicode/code-page, and negative
  corpus coverage, including a licensed Korean CP949 BIFF5 fixture.
- Added atomic package-preserving worksheet rename/add/delete, transactions,
  merge and layout editing, legacy-note and hyperlink CRUD, exact-range data
  validations, safe existing-table bottom-row resizing, and atomic filesystem
  save support.
- Added bounded configurable CSV export, stable diagnostic JSON schema and CLI
  exit contracts, plus independent-consumer and LibreOffice smoke gates.
- Added a frozen Rust API inventory, compile-time contract tests, and complete
  read/inspect/evaluate/edit/create/export/diagnose examples.
- Added a publishable WASM/npm package with typed errors, Node and real-browser
  parity smokes, a demo, input limits, and bundle-size budgets.
- Added all-reader and formula fuzzing, ZIP resource limits, dependency/license
  policy, deterministic CycloneDX SBOM generation, and reproducible diagnose
  plus package-edit/save performance evidence with enforced resource budgets.
- Added deterministic release manifests, archive-aware public-hygiene checks,
  a fail-closed two-candidate bundle comparator with same-SHA performance
  reproducibility limits, exact-SHA publication attestations, and
  post-publication crate plus downloaded-WASM Node/browser install/execute,
  docs.rs, asset, and checksum verification.
- Added independent LibreOffice smokes for authored/edited XLSX and
  package-edited XLSM, including exact VBA/content-type preservation and an
  assertion that only the expected `MacrosPresentNotExecuted` warning appears.
- Added immutable GitHub Actions policy enforcement, fixed release and fuzz
  toolchains, and retained exact tool-version evidence.
- Added deterministic, hashed seed corpora and pre-campaign replay for every
  fuzz target, including valid XLSX/XLSM editing seeds.
- Expanded the packed WASM/npm contract across XLS, XLSX, XLSM, XLSB, and ODS
  with native parity, condition-correct Node/browser typings, executed
  TypeScript consumers, and real-browser coverage of the shipped demo.

### Changed

- Synchronized native, WASM, npm, and lockfile identities at 0.1.2 and made
  drift a CI/release failure.
- Consolidated the pinned 916-file corpus baseline at 869 successful opens and
  47 classified rejections, with zero unexpected failures or accepts; parity
  reports now bind the exact manifest digest and installed oracle versions.
- Made output, feature, MSRV, unsupported-input, and SemVer compatibility
  policies explicit and regression-tested.
- Made successful CLI help stdout-only while invalid usage remains stderr-only,
  and added an isolated exact-crate consumer plus `cargo install` smoke for
  pre-publication archives and published registry versions.
- Preserved retained BIFF and XLSB external-name tables and rendered original
  `NameX` names with explicit external-workbook provenance.
- Rejected non-finite or nested formula cell values, oversized/illegal XML
  text, invalid or colliding defined names, and invalid W3CDTF property updates
  before package mutation.

### Security

- Reject unsupported ZIP compression and over-budget entry counts, part sizes,
  aggregate expansion, and names before package parsing.
- Preserve fuzz crash artifacts and add short pull-request and extended
  scheduled fuzz gates for every reader and the formula path.
- Enforce dependency advisory, license, and source policy during CI and release.
- Reject zero-based whole-row references without arithmetic underflow; the
  regression was discovered by the formula decompilation/evaluation fuzzer.

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

[Unreleased]: https://github.com/HyunjoJung/rxls/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/HyunjoJung/rxls/releases/tag/v0.1.2
[0.1.1]: https://github.com/HyunjoJung/rxls/releases/tag/v0.1.1
[0.1.0]: https://github.com/HyunjoJung/rxls/releases/tag/v0.1.0
