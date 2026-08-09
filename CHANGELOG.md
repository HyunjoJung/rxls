# Changelog

All notable changes to `rxls` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

_Nothing yet._

## [0.1.3] - 2026-08-09

### Added

- Added deterministic PDF Type0 composite font embedding for verified
  TrueType and CFF faces whose OS/2 permissions explicitly allow installable
  embedding and subsetting. The renderer carries shaped glyph and face
  identities through the scene, subsets each used font program once per
  document, and retains the Type3 outline path as a fail-safe fallback for
  restricted, synthetic, or unsupported faces.
- Added deterministic synthetic-font coverage for the permitted TrueType and
  CFF Type0 paths and the conservative Type3 fallback gate.
- Added checked HTML and Markdown export APIs with typed format, output-size,
  and work-limit errors. Sparse column gaps are encoded without materializing
  worksheet-width rows, while CSV and WASM exports preflight their encoded
  output ceilings before emission.
- Added pinned render and browser dependency locks, reviewed third-party
  notices, complete license-text inputs, and immutable package inspection to
  the render-worker supply-chain evidence.
- Added a hosted crates.io publication dry-run receipt and bound the preserved
  candidate manifest, two-candidate comparison, candidate attestation, and tag
  comparison into the public release manifest.
- Kept a row that is script-mixed only across internally-uniform cells on
  Calc's pattern row height, so a western/Asian or western/complex heading row
  no longer renders 3.34 pt taller than Calc while every other cohort agreed.
  Text that mixes script classes inside one cell, a cell re-resolved by an
  active conditional format, and multi-line runs deliberately keep the
  per-line metric accumulation.
- Ratcheted the hosted OOXML row diagnostic over all 34 cohorts. Automatic
  height previously sat outside the tracked baseline, so an automatic-height
  residual of any size still reported success; an over-threshold residual now
  fails closed.
- Split soft-wrapped PDF `ActualText` spans at asymmetric-size visual line
  breaks, using the bound the layout actually guarantees
  (`baseline_delta >= right.ascent - left.descent`) instead of the larger of
  the two adjacent clusters' nominal heights, which under-split whenever a
  whitespace-free wrap coincided with a rich-text size change. Strongly
  right-to-left pairs keep the original symmetric bound so bidi-reordered
  same-line runs are not split.
- Gave ODS sheets Calc's native 0.5 cm no-information default row height on
  both the single-page and ordinary row paths, which previously disagreed with
  each other and drifted 0.85 pt per undeclared row against the oracle.
- Retained exact integral XLSB font-size provenance, resolved only when the
  binary Fonts, CellStyleXFs, CellXFs, and Styles collections are structurally
  complete and their default font sources agree, and drove renderer axis
  geometry from it.
- Proved the browser render package's typed input/scene/page/output limits,
  tiered cancellation, monotonic progress, and per-page virtualization with
  dedicated tests, including the Rust-boundary input ceiling and a render-time
  limit breach mapping to a typed facade error.

- Added typed, bounded parse provenance for primary and tolerant CFB container
  paths, surfaced consistently through `Workbook`, diagnose JSON, CLI, and WASM.
- Added a deterministic Korean BIFF8 exact-oracle fixture covering an
  SST/CONTINUE compression transition, Unicode sheet/text, numeric adjacency,
  and a cached-string formula, plus malformed CP949 panic-free coverage.
- Added a bounded deterministic Excel custom-number-format engine covering
  section selection, conditions, locale/currency markers, fractions,
  scientific notation, date/time and elapsed tokens, literals, and text
  placeholders across XLS, XLSX, and XLSB display paths.
- Added standalone `rxls-render` with one backend-neutral fixed-point scene,
  deterministic SVG/PDF/PNG replay, authored and single-page pagination,
  verified caller/OFL font packs, CJK/complex-script shaping, rich text,
  cross-format style and conditional-format resolution, images, drawings,
  charts, sparklines, path-free bundle identities, and explicit resource and
  fidelity warnings.
- Added the worker-based `@rxls/render-worker` browser/WASM distribution with
  cancellation, progress, virtual sheet/tile/page output, strict CSP and
  memory limits, installed-package Chromium coverage, immutable package
  inspection, third-party notices, SBOM evidence, and a protected tag-only npm
  publication workflow.
- Added a license-reviewed, semantically deduplicated rendering corpus plus a
  deterministic four-format 800-workbook lattice, pinned LibreOffice/Poppler
  container oracle, multi-metric visual and semantic gates, authored-print
  parity, same-SHA full-campaign comparison, and reviewed per-format/feature
  baseline ratcheting.

### Changed

- Made public package-preserving edits transactional for cell values, appended
  rows, cleared ranges, defined names, sheet visibility, the active sheet, and
  sheet tab colors. Each edit validates a cloned package and replaces live
  state only after the complete result passes validation.
- Normalized every formula-authoring entry point by removing all leading `=`
  characters, and made checked writes reject formula-valued cached results
  with a typed row-and-column error while retaining unchecked compatibility.
- Bound rendered scene identity and replay metadata to shaped glyph, face, and
  cluster fields. Pagination transforms now move glyph origins and cluster
  metrics together, and merged glyph runs retain their actual cluster index.
- Defaulted cells without an authored vertical alignment to the bottom of their
  row and matched Calc's 1 pt top and bottom in-cell offsets while preserving
  middle alignment and clipping behavior.
- Matched Calc 26.2.3 by preserving the source print-gridline setting and
  caller flag in the `SinglePageSheets` fidelity override as well as authored
  print, while continuing to replace authored page geometry.
- Distinguished exact authored page geometry from Calc's bounded imported-file
  quantization with a 15,000-micropoint acceptance envelope, and extended
  hosted oracle evidence to attest Type0 TrueType, Type0 CFF, and Type3
  fallback inventories together with font embedding, subsetting, Unicode
  extraction, and page geometry.
- Enforced SemVer compatibility across default, no-default, and all-feature API
  surfaces, and expanded lint, test, and package gates across the CLI feature
  combinations and both WASM bindings.
- Made browser-render CI react to changes in the committed full-parity
  baseline, ensuring a baseline update produces fresh same-revision browser
  evidence for the hosted release gates.
- Advanced the stable diagnose JSON contract to schema v2 for the new
  provenance object; schema v1 remains frozen rather than receiving new keys.
- Preserved format-native imported row and column geometry through Calc-style
  cumulative print bounds, while distinguishing XLSX automatic cached rows
  from manual overrides and honoring XLSB `fUnsynced` row-height authority.
- Aligned repeated print-title pagination with Calc for middle title bands,
  protected breaks, sparse pages, title-only boundaries, and single-page
  sheet bounds.
- Retained OOXML chart-space fills, major-gridline presence, and series line
  widths (including visible zero-width hairlines) for consistent SVG, PDF, and
  PNG replay.
- Made ODS General and inherited number-style display resolution typed and
  deterministic, including literal currency/text tokens and repeated cells
  that cross differently formatted columns.
- Hardened PDF Type3 semantic extraction at adjacent-cell, clipping, RTL,
  rotation, and mixed-script boundaries without adding visible paint.

### Fixed

- Retained OOXML chart theme/default Latin fonts, uniform text styling by
  semantic role, axis roles and visibility, and exact supported plot/legend
  semantics. Unsupported visible chart constructs now fail closed to a typed
  placeholder instead of inheriting an unrelated worksheet font or silently
  approximating the source chart.
- Hardened OOXML relationship selection and theme/chart parsing against
  duplicate identifiers, mismatched relationship types, malformed targets,
  incomplete themes, unsupported markup, and workbook-wide chart resource
  amplification.
- Rejected Pie and Doughnut charts whose nonnegative value total is not finite,
  and normalized slice ratios before multiplying by a full turn so valid
  extreme finite values render without intermediate floating-point overflow.
- Evaluated numeric and duplicate conditional-format rules across their full
  authored ranges, including sparse cells beyond the rendered subset.
- Prevented sparse CSV, HTML, Markdown, and WASM exports from performing
  unbounded dense-gap work or allocating output beyond their declared limits;
  legacy string-returning helpers now produce an explicit bounded fallback.
- Hardened PDF syntax attestation to recognize only line-delimited stream
  objects with validated direct lengths and terminators. Binary font and image
  payloads can no longer fabricate, hide, or satisfy textual PDF evidence such
  as `ActualText`.
- Rejected invalid glyph identifiers and conflicting TrueType/CFF font
  programs before Type0 serialization, bounded retained font bytes, generated
  deterministic PDF identifiers, and corrected composite-font width mapping.
- Extended public-hygiene auditing to reject Markdown task-list syntax in
  public release inputs and converted the public contribution prompt to plain
  verification bullets.

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

[Unreleased]: https://github.com/HyunjoJung/rxls/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/HyunjoJung/rxls/releases/tag/v0.1.3
[0.1.2]: https://github.com/HyunjoJung/rxls/releases/tag/v0.1.2
[0.1.1]: https://github.com/HyunjoJung/rxls/releases/tag/v0.1.1
[0.1.0]: https://github.com/HyunjoJung/rxls/releases/tag/v0.1.0
