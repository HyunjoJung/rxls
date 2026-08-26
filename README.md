# rxls

[Korean README](README.ko.md)

**A native Rust spreadsheet toolkit.** Reads `.xls`, `.xlsx`, `.xlsb`, and `.ods`
into one typed cell model, writes styled `.xlsx`, and edits `.xlsx`/`.xlsm` in
place without disturbing the rest of the package.

[![Crates.io](https://img.shields.io/crates/v/rxls.svg)](https://crates.io/crates/rxls)
[![Docs.rs](https://docs.rs/rxls/badge.svg)](https://docs.rs/rxls)
[![CI](https://github.com/HyunjoJung/rxls/actions/workflows/ci.yml/badge.svg)](https://github.com/HyunjoJung/rxls/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![MSRV](https://img.shields.io/badge/MSRV-1.85-orange.svg)

No JVM, no Apache POI, no Office automation, no subprocess — the core library
calls none of them. It is built for document pipelines that must accept legacy
Korean cp949 workbooks and untrusted uploads without turning malformed input
into a panic.

```sh
cargo add rxls@0.1.3 --features full
```

## What it does

| Format | Read | Write | Edit in place | Visible-value oracle |
|---|:---:|:---:|:---:|---|
| `.xls` (BIFF8/5/7) | ✓ | — | — | 414/414 vs `xlrd` |
| `.xlsx` | ✓ | ✓ styled | ✓ untouched parts retained | 387/387 vs `openpyxl` |
| `.xlsm` | ✓ | — | ✓ VBA retained | counted in the OOXML row |
| `.xlsb` | ✓ | — | — | 18/18 vs `pyxlsb` |
| `.ods` | ✓ | — | — | 14/14 vs bounded ODF XML |

Also included: a deterministic formula-evaluation MVP, CSV/HTML/Markdown export,
machine-readable workbook diagnostics, a CLI, and a standalone WASM adapter.

### At a glance

| Release | Tests | Public corpus |
|---|---|---|
| `0.1.3` · MIT · MSRV 1.85 | 1,092 all-target/all-feature tests on the exact release source | 916 files · 868 opened · 48 exact expected rejections · 0 unexpected |

Published to [crates.io](https://crates.io/crates/rxls/0.1.3) and
[docs.rs](https://docs.rs/rxls/0.1.3/rxls/), with a
[52-asset release evidence bundle](https://github.com/HyunjoJung/rxls/releases/tag/v0.1.3)
bound to one exact revision.

## Demo and architecture

[![rxls 2026 OSS contest demo](.github/assets/rxls-demo-thumbnail.png)](https://youtu.be/_z8tUe4a1Ho)

The [2:49 exact-release demo](https://youtu.be/_z8tUe4a1Ho)
runs the real `rxls` CLI against a BIFF5/cp949 workbook, opens all four formats
through the common model, generates a styled operations report, and
reopens that report with `openpyxl 3.1.5`. Reader commands use the exact `v0.1.3`
CLI at [`e1390e5`](https://github.com/HyunjoJung/rxls/commit/e1390e5aa349fbf933c39bccda400a4a2ee1d814);
the tracked report driver calls the library from that same checkout.

[Korean captions](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/rxls-2026-oss-contest-demo.ko.srt) ·
[build receipt](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/video-verification.json) ·
[independent decode/audio/privacy QA](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/video-qa.json) ·
[media release](https://github.com/HyunjoJung/rxls/releases/tag/oss-contest-2026-demo)

![rxls architecture: untrusted bytes through bounded format parsers into one typed model and public surfaces](.github/assets/rxls-architecture.png)

The contest media release is deliberately separate from the immutable
`v0.1.3` 52-asset release-evidence bundle.

## Quick start

**Read** — plain text for search and indexing, or typed cells for structure:

```rust
// Plain text (search/indexing):
let bytes = std::fs::read("book.xls")?;
let text = rxls::extract_text(&bytes)?;

// Typed cells (structured reading):
let wb = rxls::Workbook::open(&bytes)?;
for sheet in &wb.sheets {
    if let Some(rxls::Cell::Date(serial)) = sheet.cell(0, 0) {
        println!("A1 is the Excel date serial {serial}");
    }
    for (row, col, cell) in sheet.cells() {
        // rxls::Cell::{Text(String), Number(f64), Date(f64), Bool(bool), Error(String)}
    }
}
```

`Workbook::open` auto-detects the container, so the same call handles all four
formats when their Cargo features are enabled (`features = ["full"]`).

**Write** — styled `.xlsx` from data, no template and no JVM:

```rust
use rxls::{Cell, CellStyle, HAlign, Workbook};

let mut wb = Workbook::new();
let sheet = wb.add_sheet("Operations Report");

let header = CellStyle::new().bold().fill([0xDD, 0xEB, 0xF7]).align(HAlign::Center).wrap();
sheet.write_styled(0, 0, "Item", &header);
sheet.write_styled(0, 1, "Amount", &header);

sheet.write_url(1, 0, "https://example.com/reports/2026-07", "July operations");
sheet.write_styled(1, 1, 150_000_000.0, &CellStyle::new().num_fmt("$#,##0"));

sheet.set_col_width(0, 42.0);
sheet.freeze_panes(1, 0);
sheet.autofilter(0, 0, 1, 1);

std::fs::write("report.xlsx", wb.to_xlsx())?;
```

**Inspect** — from the CLI:

```sh
cargo install rxls --version =0.1.3 --locked

rxls info book.xlsx
rxls diagnose book.xlsx
rxls csv book.xlsx --sheet 0 --max-output-bytes 1048576
rxls compare before.xlsx after.xlsx --limit 50
```

Successful `--help` and command output go to stdout. Usage and operational
errors go to stderr. Exit classifications and diagnose JSON schema evolution
are compatibility-controlled public behavior.

## Cargo features

| Feature | Default | Surface |
|---|:---:|---|
| `cli` | Yes | Builds the `rxls` binary |
| `xlsx` | Yes | XLSX/XLSM reading, XLSX writing, and package-preserving editing |
| `xlsb` | No | XLSB reader; enables `xlsx` package support |
| `ods` | No | ODS reader |
| `serde` | No | Typed row deserialization |
| `chrono` | No | Date/time and duration conversions |
| `full` | No | All library format/data features; intentionally excludes `cli` |

The legacy XLS reader is always available. Features are additive: use
`default-features = false` for an XLS-only library build, or
`features = ["full"]` for every reader and typed-data helper. The minimum
supported Rust version is 1.85.

Runnable examples:

```text
cargo run -p rxls --bin rxls -- --version
cargo run -p rxls --example extract -- book.xls
cargo run -p rxls --example metadata -- book.xlsx
cargo run -p rxls --example author_report -- report.xlsx
cargo run -p rxls --example robustness -- suspicious.xls
```

## Release contract

> Version `0.1.3` is accepted only when the crate, tagged source, GitHub Release
> bundle, SBOM, checksums, and provenance are bound by the release manifest to
> one exact revision.

> `v0.1.3` is published and immutable. The tag identifies its exact released
> source; later `main` revisions may contain documentation or unreleased work.
> Use the versioned package above for the release contract, and build the
> checked-out source when evaluating `main`.

The core 0.1.3 release is accepted only after its prepublication and
postpublication gates across crates.io, docs.rs, and GitHub pass. Those gates
cover reader and formula correctness, package-preserving XLSX/XLSM editing,
CLI, JSON, and core WASM contracts, public-corpus parity, security analysis,
fuzzing, performance budgets, SBOM/provenance, and exact-package installation.
The separately versioned render worker follows its own tag-only npm release
contract with pinned LibreOffice fidelity, hardening, and real-browser
evidence; it is not a prerequisite for the core crate release.

## Public corpus evidence

<!-- public-corpus-baseline:start -->
**Current public-corpus gate (2026-08-22).** The pinned fetch recipe selects 916
files from Apache POI and calamine at immutable upstream commits: 448 `.xls`,
413 `.xlsx`, 18 `.xlsm`, 21 `.xlsb`, and 16 `.ods`. `rxls corpus-report` opens
868; the remaining 48 are explicit expected rejections for encrypted input,
unsupported legacy BIFF, malformed containers, structurally invalid BIFF streams, or malformed OOXML package relationships.
The report records 0 unexpected failures and 0 unexpected accepts. Public visible-value checks report:

| Format | Comparable files | Result |
|---|---:|---:|
| `.xls` vs `xlrd` | 414 | 100.000% mean parity; 414/414 at least 99% |
| `.xlsx`/`.xlsm` vs `openpyxl` | 387 | 100.000% mean parity; 387/387 at least 99% |
| `.xlsb` vs `pyxlsb` plus committed residual oracles | 18 | 100.000% mean parity |
| `.ods` vs bounded ODF XML visible-text oracle | 14 | 100.000% mean recall |
<!-- public-corpus-baseline:end -->

The release claim depends only on public, reproducible fixtures and corpora.
GitHub Actions runs formatting, clippy, the feature/MSRV matrix, Rust and Python
harness tests, documentation, package checks, and the small pinned CI corpus.
The broader 916-file run is reproducible on demand — see
[Reproduce](#reproduce) below.

Each parity report records the oracle reader and installed version plus the
SHA-256 of the exact input manifest bytes. Directory-only development runs
explicitly report `input_manifest_sha256=none`; release evidence always uses the
pinned manifest.

## How it works

`.xls` is an OLE2 compound file whose `Workbook` stream is a sequence of BIFF
records. `rxls`:

1. opens the container (`cfb`) and reads the `Workbook` (BIFF8) or `Book`
   (BIFF5/7) stream;
2. walks the record stream, tracking the globals and per-sheet substreams, and
   detects the BIFF generation from the first `BOF`;
3. for BIFF8, decodes the **shared string table** (SST) — including strings that
   span `CONTINUE` records and re-specify their compression at the boundary;
4. for BIFF5/7, decodes 8-bit strings in the workbook's ANSI codepage (the
   `CODEPAGE` record) — so Korean **cp949**, Japanese cp932, etc. come out as
   real text rather than mojibake (via [encoding_rs](https://docs.rs/encoding_rs));
5. decodes cell records (`LABELSST`, `LABEL`, `RSTRING`, `RK`, `MULRK`,
   `NUMBER`, `BOOLERR`, and `FORMULA` + cached `STRING`) into **typed cells**
   (`Cell`: `Text`/`Number`/`Date`/`Bool`/`Error`), exposed per coordinate
   (`Sheet::cell`/`cells`/`dimensions`) and flattened to tab-joined rows by
   `to_text`.

For BIFF5/7, declarations `949` (Windows Korean/UHC) and `51949` (EUC-KR) share
`encoding_rs`'s Windows-949-compatible decoder. Missing or unknown codepages
fall back to Windows-1252, malformed byte sequences become U+FFFD, and
`Workbook::open_with_codepage` can override a missing or incorrect declaration.
BIFF8 strings are Unicode and do not use this fallback.

Modern **`.xlsx`** (OOXML) is read too (default `xlsx` feature): `Workbook::open`
auto-detects OLE2 `.xls` vs ZIP `.xlsx` and produces the same typed cells and
text. `xlsx` cell data, shared strings, and number formats (for dates) are
parsed via `zip` + `quick-xml`; `default-features = false` drops both deps for
an `.xls`-only build.

**Failure is typed, never a panic.** Unsupported password-protected workbooks
(`FILEPASS`) are reported as `Error::Encrypted` rather than emitting ciphertext.
Legacy XOR (Method 1) workbooks using Excel's default `VelvetSweatshop` password
are deobfuscated. Every read is bounds-checked; malformed structures are either
handled by an explicit bounded recovery path or return an `Error`. After a
successful read, `Workbook::parse_provenance` distinguishes the format's primary
container path from rxls's bounded tolerant CFB directory walk and exposes
stable typed recovery codes. Recovery is an audit signal, not a guarantee that
the original container was valid or complete, and it never bypasses the existing
strict edit/save safeguards.

Parsing, export, editing, and WASM paths enforce bounded input, allocation, and
output limits. Release gates enforce absolute performance ceilings and same-SHA
reproducibility thresholds; dependency policy is enforced by `deny.toml`,
CodeQL, fuzz smoke/scheduled jobs, and a deterministic CycloneDX dependency
manifest.

## Scope

### Reading

Targets plain-text extraction for search/indexing. Date/time serials and
percentages are rendered through the retained format metadata. Excel custom
formats support positive/negative/zero/text sections, conditions and colors,
locale/currency markers, grouping and scaling, fractions, scientific notation,
date/time and elapsed tokens, literals, escapes, and text placeholders. ODS
continues to prefer its source display paragraph, with typed-value fallbacks
when none is present.

Formula re-evaluation is limited to the deterministic MVP exposed by
`Workbook::evaluate_cell`, which returns a typed `FormulaUnsupportedReason`
(unsupported/volatile function, external reference, circular reference,
unresolved name, oversized range, missing sheet, …) instead of guessing when a
formula falls outside that MVP. Locale-specific calendars and digit
substitution remain explicit boundaries.

### Editing existing files

Package-preserving and `.xlsx`/`.xlsm`-only. `Spreadsheet` supports atomic
batches; cell/formula and range edits; document, name, sheet, layout, pane, and
print-area metadata; sheet add/rename/delete; merges; legacy notes; hyperlinks;
exact-range validations; and safe bottom-row resizing of existing tables.
Untouched declared parts round-trip byte-for-byte, including retained VBA
content. `.xls`, `.xlsb`, `.ods`, and metadata-lossy OOXML packages are
read-only through this API. The complete method-by-method atomicity,
preservation, rejection, and explicit non-goal boundary is treated as
compatibility-controlled behavior.

**rxls does not insert or delete rows or columns, and does not guess how to
repair unsafe structural dependencies.**

### Authoring `.xlsx`

Per-cell font (family/size/color/bold/italic/underline and strikethrough), fill,
borders, number formats, alignment + wrap, merged ranges, column widths/row
heights, frozen panes, autofilters, external hyperlinks, **page setup**
(orientation/margins/print-area/repeat rows/columns/headers-footers), **sheet
protection** (including cell-level `Format` protection), **tab color**, **data
validation** (dropdowns + numeric/date rules), **conditional formatting**
(cellIs / color scales / data bars), **images** (PNG/JPEG), **charts**
(bar/line/pie/scatter), **sparklines**, **worksheet tables** (including named
table header formats), **rich strings** (including cell-level `Format`), and
**legacy comments/notes**. Styles are interned into deduped OOXML resource
tables; writer features are validated by in-tree `openpyxl` gates.

Pivot tables, threaded comments, and macros are out of scope.

### Export, diagnostics, and WASM

A worksheet can be exported directly to **CSV**, **HTML**, or **Markdown**
(`Sheet`/`Workbook::to_csv`/`to_html`/`to_markdown`), and a whole workbook can be
summarized as machine-readable JSON via `WorkbookReport` — sheet/cell/formula
counts, document properties, and a feature inventory — surfaced on the CLI as
`rxls diagnose <file>` (and `rxls csv <file>` for direct CSV export).
Determinism, CSV safety options, diagnose JSON schema compatibility, CLI exit
codes, public Rust APIs, coordinate rules, feature guarantees, and error
semantics follow the crate's SemVer policy. Diagnose schema v2 adds the bounded
`provenance` object; schema v1 remains a historical frozen contract and is not
extended with new keys.

The portable adapter in `src/wasm.rs` is exposed to JavaScript by the isolated
`bindings/wasm` `cdylib`; the native `rxls` CLI binary itself lives behind the
`cli` feature (on by default, so existing native workflows are unaffected). The
WASM distribution provides generated Node and browser entry points, TypeScript
declarations, a minimal file-picker demo, structured `RxlsError` objects, and a
synchronous 32 MiB input limit. Build it with
`bash scripts/build-wasm-package.sh`; the CI release gate executes Node and
Chromium smoke tests, compares `reportJson` with `rxls diagnose`, and enforces
raw WASM, JavaScript glue, and compressed npm bundle budgets. See the
[WASM package guide](https://github.com/HyunjoJung/rxls/blob/main/bindings/wasm/npm/README.md)
for initialization and memory guidance.

### Choosing a crate

[`calamine`](https://crates.io/crates/calamine) is the established choice when
reader maturity and ecosystem adoption are the main criteria. `rxls` is aimed at
applications that also need styled `.xlsx` generation, package-preserving
`.xlsx`/`.xlsm` edits, bounded formula evaluation, or the built-in export and
diagnostic surfaces. The public corpus results above describe `rxls`; they are
not presented as a current head-to-head benchmark against another crate.

## Stability and reader-surfaced metadata

Version 0.1.3 defines the current public API and documented semantics.
Compatible updates may add APIs and `#[non_exhaustive]` variants under the
published SemVer policy; applications that require an exact dependency graph
should pin an exact version.

One deliberate design choice to be aware of: **a single model serves both
reading and authoring.** Readers populate the documented cross-format subset of
layout, style, and view metadata, but this is not a promise that every authoring
setter is reconstructed as a complete writer template. Read-discovered merges,
for example, are tracked separately from authoring merges so reading them never
alters write output.

<details>
<summary><b>Metadata the reader surfaces, by API and source record</b></summary>

| Surface | API | Sources |
|---|---|---|
| Merged ranges | `Sheet::merged_ranges()` | `.xls MERGECELLS`, `.xlsx <mergeCells>` |
| Formula text | `Cell::Formula` (cached value retained) | `.xlsx`, `.xls`, `.xlsb`, `.ods`; best effort |
| Defined names | `Workbook::defined_names()` | `.xlsx`, `.xls`, `.xlsb`, `.ods` named ranges |
| Document properties | `Workbook::properties` | `.xlsx`/`.xlsb` package properties, `.xls` OLE properties, `.ods meta.xml` |
| Sheet visibility | `Sheet::is_hidden()` | all read formats, including `.ods` table styles where `table:display="false"` |
| Hyperlinks | `Sheet::hyperlinks()` | OOXML relationships, XLSB `BrtHLink`, BIFF `HLINK`, ODS `text:a` |
| Comments | `Sheet::comments()` | OOXML comments, XLSB comments parts, BIFF `Note`/`TxO`, ODS `office:annotation` |
| Data validations | `Sheet::data_validations()` | OOXML `dataValidations`, XLSB `BrtDVal`/`BrtDValList`, BIFF `Dv`, ODS `table:content-validation` (conditions preserved as custom validation formulas) |
| Tables | `Sheet::tables()`, `Workbook::table_names()`, `table_names_in_sheet()`, `table_by_name()` | OOXML tables, XLSB binary table parts, named ODS `table:database-range` blocks |
| Sheet view and panes | `Sheet::sheet_view()` | OOXML sheet views, XLSB `BrtBeginWsView`/`BrtPane`, BIFF `WINDOW2`/`PANE` |
| Autofilter | `Sheet::autofilter_range()` | OOXML `autoFilter`, XLSB `BrtBeginAFilter`, BIFF `_FilterDatabase`, ODS `table:database-range` |
| Page setup | `Sheet::page_setup()` — `print_area`, `repeat_rows`, `repeat_cols`, orientation, margins, scaling, centering, header, footer | BIFF `Print_Area` sheet-local built-in names, ODS `table:print-ranges` / `table:table-header-rows` / `table:table-header-columns`, BIFF/XLSB page setup records |
| Charts | `Sheet::charts()` — anchored, maps to the writer chart model, including axis titles | OOXML worksheet charts |
| Images | `Sheet::images()`, `Workbook::pictures()` (calamine-style workbook aggregate of image extensions and bytes) | OOXML worksheet images, ODS `draw:image` package parts |

</details>

<details>
<summary><b>Range, typed-row, and calamine-style access</b></summary>

- The `worksheet_range` facade exposes rectangular row views with absolute row
  and column bounds.
- `Range::used_cells()` reports calamine-style relative coordinates;
  `Range::used_cells_abs()` keeps worksheet coordinates available.
- Formula ranges expose the same rectangular lookup, relative/absolute used-cell
  iteration, and allocation-free `row_views()` scan surface, with the same
  absolute row and column bounds for formula source text.
- Workbook helpers: `worksheet_range_at`, `worksheets`, `worksheet_formula`, and
  `sheets_metadata` (`SheetType` + `SheetVisible`).
- With the optional `serde` feature: typed row deserialization including
  `RangeDeserializerBuilder::with_header_row(row)`,
  `RangeDeserializerBuilder::with_deserialize_headers::<T>()`, and raw `Cell`
  rows for callers that want the exact `Text`/`Number`/`Date`/`Bool`/`Formula`
  model instead of coercing into primitive fields. Numeric `deserialize_with`
  helpers keep invalid numeric cells non-fatal during typed ingestion.
- With the optional `chrono` feature: Excel date serials convert directly to
  `chrono::NaiveDateTime` via `excel_serial_to_naive_datetime` or
  `Cell::as_naive_datetime`, with `Cell::as_naive_date` and `Cell::as_naive_time`
  when callers only need one component. Duration serials convert to
  `chrono::Duration` via `excel_serial_to_duration` or `Cell::as_duration`.
  `Cell::get_datetime()`
  exposes the raw Excel serial for date/time cells when callers want
  calamine-style typed access without choosing the workbook date system yet.

</details>

## Reproduce

Everything below runs from a clean checkout — no private data.

```bash
python3 -m pip install \
  "CairoSVG==2.9.0" "numpy==2.4.4" "openpyxl==3.1.5" \
  "Pillow==12.3.0" "pyxlsb==1.0.10" "xlrd==2.0.2" "odfpy==1.4.1"
python3 scripts/public_hygiene_audit.py
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
RXLS_REQUIRE_OPENPYXL=1 cargo test --all-targets --all-features --locked
cargo test --no-default-features --all-targets --locked
cargo test --doc --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --locked
python3 -m unittest discover -s scripts -p "test_*.py"
cargo package --locked
cargo publish --dry-run --locked
```

To test the exact packaged crate as both an external Rust dependency and a
`cargo install` CLI — entirely outside the checkout:

```sh
cargo package --locked
python3 scripts/smoke_crate_distribution.py \
  --crate target/package/rxls-0.1.3.crate \
  --fixture tests/fixtures/xlsx/reader-structural.xlsx
```

To exercise the same consumer, install, version, help, diagnose, and
invalid-usage contracts through crates.io:

```sh
python3 scripts/smoke_crate_distribution.py \
  --registry-version 0.1.3 \
  --fixture tests/fixtures/xlsx/reader-structural.xlsx
```

<details>
<summary><b>Full 916-file public corpus run</b></summary>

```bash
python3 scripts/fetch-public-corpus.py --dry-run
python3 scripts/fetch-public-corpus.py
cargo build --all-features --example extract --locked
cargo run --bin rxls --all-features --locked -- corpus-report local/public-corpus/manifest.json | tee target/release-corpus-report.txt
python3 scripts/xls-xlrd-parity.py --manifest local/public-corpus/manifest.json --bin target/debug/examples/extract --corpus-report target/release-corpus-report.txt --min 0.99 --show-worst 20 --show-skips 200 | tee target/release-xls-parity-full.txt
python3 scripts/xlsx-openpyxl-parity.py --manifest local/public-corpus/manifest.json --bin target/debug/examples/extract --corpus-report target/release-corpus-report.txt --min 0.99 --show-worst 20 --show-skips 200 | tee target/release-ooxml-parity-full.txt
python3 scripts/xlsb-pyxlsb-parity.py --manifest local/public-corpus/manifest.json --bin target/debug/examples/extract --expected-values tests/oracles/xlsb-visible-values.json --corpus-report target/release-corpus-report.txt --min 0.99 --show-skips 200 | tee target/release-xlsb-parity-full.txt
python3 scripts/ods-odfpy-parity.py --manifest local/public-corpus/manifest.json --bin target/debug/examples/extract --corpus-report target/release-corpus-report.txt --min 0.99 --show-skips 200 | tee target/release-ods-parity-full.txt
python3 scripts/verify_public_baseline.py --corpus-report target/release-corpus-report.txt --xls target/release-xls-parity-full.txt --ooxml target/release-ooxml-parity-full.txt --xlsb target/release-xlsb-parity-full.txt --ods target/release-ods-parity-full.txt --readme README.md
```

The dry run should report 916 files (`.xls` 448, `.xlsx` 413, `.xlsm` 18,
`.xlsb` 21, `.ods` 16). Files download into gitignored `local/public-corpus`;
this repo commits the pinned recipe and docs, not the corpus payloads.

</details>

<details>
<summary><b>How a release is actually cut</b></summary>

Maintainers create two clean `Release` workflow-dispatch candidates from the
same commit. The second receives the first run's `baseline_run_id`; the
fail-closed bundle comparator requires deterministic artifacts to be identical
and explains permitted test-duration and successful fuzz-log differences.
Timing, RSS, and edit-output variation must remain inside the documented
same-SHA reproducibility/noise limits; the absolute budgets remain the
performance regression guard.

Tag publication is allowed only after that report and every hosted gate pass.
The second candidate emits an immutable exact-SHA attestation that also binds
the candidate release-manifest digest. The tag-triggered job requires successful
exact-SHA CI and CodeQL push runs, downloads the attested candidate, and fails
before publishing unless its own 48-file candidate bundle compares cleanly. It
then binds the candidate manifest, two-candidate comparison, candidate
attestation, and tag comparison into the final 52-file public bundle.
Post-publication verification downloads every release asset and validates full
manifest coverage and checksums.

See the [Release workflow](.github/workflows/release.yml) for the exact hosted
sequence and [CONTRIBUTING.md](CONTRIBUTING.md) for the local gate and release
policy.

</details>

## Experimental rendering workspace

**Not part of the published `rxls 0.1.3` core contract.** Renderer workflows,
visual-fidelity baselines, and the render worker remain a separately gated
source-only track; their status is not a reader/writer/CLI/WASM release claim.

The source workspace also contains a separate `rxls-render` crate and source for
the `@rxls/render-worker` browser/WASM package. They are not bundled into the
published core crate: the renderer builds one bounded fixed-point scene and
replays it to deterministic SVG, PDF, and PNG, while the browser surface keeps
parsing and virtual sheet/tile/page rendering inside a CSP-safe worker. Imported
OOXML charts retain the effective theme/default Latin font, uniform
semantic-role text styling, role-aware axes, and source axis visibility. Visible
chart semantics outside the exact retained subset produce a typed placeholder
and warning instead of a silent approximation. See the
[renderer guide](render/README.md) and
[worker package guide](bindings/render-wasm/README.md) for source builds,
limits, font isolation, pagination, and distribution gates.

## Contributing

Issues and pull requests are welcome. [CONTRIBUTING.md](CONTRIBUTING.md)
documents the ground rules (`#![forbid(unsafe_code)]`, documented public items,
minimal dependencies, spec citations, bounded everything) and the exact local
gate to run before opening a PR — the same gate GitHub Actions enforces. See
also the [Code of Conduct](.github/CODE_OF_CONDUCT.md) and the
[security policy](.github/SECURITY.md).

## License

Licensed under the [MIT License](LICENSE). Third-party dependency licenses are
listed in [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md). This crate
implements only the publicly documented [MS-XLS], [MS-XLSB], [MS-CFB],
[ECMA-376], and [ODF] specifications and contains no Microsoft source.

Microsoft and Excel are trademarks of the Microsoft group of companies. This
project is not affiliated with, endorsed by, or sponsored by Microsoft.

[MS-XLS]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/
[MS-XLSB]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xlsb/
[MS-CFB]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cfb/
[ECMA-376]: https://ecma-international.org/publications-and-standards/standards/ecma-376/
[ODF]: https://docs.oasis-open.org/office/OpenDocument/v1.3/
