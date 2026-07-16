# rxls

[![Crates.io](https://img.shields.io/crates/v/rxls.svg)](https://crates.io/crates/rxls)
[![Docs.rs](https://docs.rs/rxls/badge.svg)](https://docs.rs/rxls)
[![CI](https://github.com/HyunjoJung/rxls/actions/workflows/ci.yml/badge.svg)](https://github.com/HyunjoJung/rxls/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![MSRV](https://img.shields.io/badge/MSRV-1.85-orange.svg)

> **Release status:** `v0.1.2` is published. The crate, tagged source, 47-file
> GitHub Release bundle, SBOM, and checksums are bound to commit
> `33b4db17f2047febf9e6550299c3dde572afd6e5`.

> **Development status:** `main` contains additive, unreleased work that is not
> part of the crates.io `v0.1.2` package. The installation commands below
> describe the published package; build from source to evaluate development
> APIs and behavior.

Native Rust spreadsheet toolkit. It reads **`.xls`** (BIFF8/5/7), **`.xlsx`**,
**`.xlsb`**, and **`.ods`** into one typed cell model; writes styled **`.xlsx`**;
and package-preservingly edits **`.xlsx`/`.xlsm`**. No JVM, Apache POI, or
subprocess is required. Malformed input returns a typed error instead of
panicking when bounded recovery is not possible.

## Install

Add the `0.1.2` library:

```sh
cargo add rxls@0.1.2
```

Install the CLI from the same exact release:

```sh
cargo install rxls --version =0.1.2 --locked
rxls --help
```

The minimum supported Rust version is 1.85. Core library use does not invoke
Java, Excel, LibreOffice, Python, or any other subprocess.

## Library quick start

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

## CLI

The installed CLI exposes bounded human-readable inspection, stable diagnose
JSON, CSV export, package inspection, and comparison commands:

```sh
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

The legacy XLS reader is always available. Features are additive. For example,
use `default-features = false` for an XLS-only library build, or
`features = ["full"]` for every reader and typed-data helper.

## Examples

```text
cargo run -p rxls --bin rxls -- --version
cargo run -p rxls --example extract -- book.xls
cargo run -p rxls --example metadata -- book.xlsx
cargo run -p rxls --example author_report -- report.xlsx
cargo run -p rxls --example robustness -- suspicious.xls
```

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
   real text rather than mojibake (via [`encoding_rs`]);
5. decodes cell records (`LABELSST`, `LABEL`, `RSTRING`, `RK`, `MULRK`,
   `NUMBER`, `BOOLERR`, and `FORMULA` + cached `STRING`) into **typed cells**
   ([`Cell`]: `Text`/`Number`/`Date`/`Bool`/`Error`), exposed per coordinate
   (`Sheet::cell`/`cells`/`dimensions`) and flattened to tab-joined rows by
   `to_text`.

For BIFF5/7, declarations `949` (Windows Korean/UHC) and `51949` (EUC-KR)
share `encoding_rs`'s Windows-949-compatible decoder. Missing or unknown
codepages fall back to Windows-1252, malformed byte sequences become U+FFFD,
and [`Workbook::open_with_codepage`] can override a missing or incorrect
declaration. BIFF8 strings are Unicode and do not use this fallback.

Modern **`.xlsx`** (OOXML) is read too (default `xlsx` feature): `Workbook::open`
auto-detects OLE2 `.xls` vs ZIP `.xlsx` and produces the same typed cells / text.
`xlsx` cell data, shared strings, and number formats (for dates) are parsed via
`zip` + `quick-xml`; `default-features = false` drops both deps for an
`.xls`-only build.

Unsupported password-protected workbooks (`FILEPASS`) are reported as
`Error::Encrypted` rather than emitting ciphertext. Legacy XOR (Method 1)
workbooks using Excel's default `VelvetSweatshop` password are deobfuscated.
Every read is bounds-checked. Malformed structures are either handled by an
explicit bounded recovery path or return an [`Error`], never a panic.
After a successful read, [`Workbook::parse_provenance`] distinguishes the
format's primary container path from rxls's bounded tolerant CFB directory
walk and exposes stable typed recovery codes. Recovery is an audit signal, not
a guarantee that the original container was valid or complete, and it never
bypasses the existing strict edit/save safeguards.

## Choosing a crate

[`calamine`](https://crates.io/crates/calamine) is the established choice when
reader maturity and ecosystem adoption are the main criteria. `rxls` is aimed at
applications that also need styled `.xlsx` generation, package-preserving
`.xlsx`/`.xlsm` edits, bounded formula evaluation, or the built-in export and
diagnostic surfaces. The public corpus results below describe `rxls`; they are
not presented as a current head-to-head benchmark against another crate.

Parsing, export, editing, and WASM paths enforce bounded input, allocation, and
output limits. Release gates enforce absolute performance ceilings and
same-SHA reproducibility thresholds; dependency policy is enforced by
`deny.toml`, CodeQL, fuzz smoke/scheduled jobs, and a deterministic CycloneDX
dependency manifest.

## Scope & parity

Targets plain-text extraction for search/indexing. Date/time serials and
percentages are rendered through the retained format metadata. Excel custom
formats support positive/negative/zero/text sections, conditions and colors,
locale/currency markers, grouping and scaling, fractions, scientific notation,
date/time and elapsed tokens, literals, escapes, and text placeholders. ODS
continues to prefer its source display paragraph, with typed-value fallbacks
when none is present. Formula re-evaluation is limited to the deterministic MVP
exposed by `Workbook::evaluate_cell`, which
returns a typed `FormulaUnsupportedReason` (unsupported/volatile function,
external reference, circular reference, unresolved name, oversized range,
missing sheet, …) instead of guessing when a formula falls outside that MVP;
locale-specific calendars and digit substitution remain explicit boundaries.

**Editing existing files** is package-preserving and `.xlsx`/`.xlsm`-only.
`Spreadsheet` supports atomic batches; cell/formula and range edits; document,
name, sheet, layout, pane, and print-area metadata; sheet add/rename/delete;
merges; legacy notes; hyperlinks; exact-range validations; and safe bottom-row
resizing of existing tables. Untouched declared parts round-trip byte-for-byte,
including retained VBA content. `.xls`, `.xlsb`, `.ods`, and metadata-lossy
OOXML packages are read-only through this API. The complete method-by-method
atomicity, preservation, rejection, and explicit non-goal boundary is treated
as compatibility-controlled behavior. Notably, rxls does not insert or delete
rows or columns or guess how to repair unsafe structural dependencies.

A worksheet can also be exported directly to **CSV**, **HTML**, or
**Markdown** (`Sheet`/`Workbook::to_csv`/`to_html`/`to_markdown`), and a whole
workbook can be summarized as machine-readable JSON via `WorkbookReport` —
sheet/cell/formula counts, document properties, and a feature inventory,
surfaced on the CLI as `rxls diagnose <file>` (and `rxls csv <file>` for
direct CSV export). The portable adapter in `src/wasm.rs` is exposed to
JavaScript by the isolated `bindings/wasm` `cdylib`; the native `rxls` CLI
binary itself lives behind the `cli` feature (on by default, so existing
native workflows are unaffected). Determinism, CSV safety options, diagnose JSON
schema compatibility, CLI exit codes, public Rust APIs, coordinate rules,
feature guarantees, and error semantics follow the crate's SemVer policy.
Diagnose schema v2 adds the bounded `provenance` object; schema v1 remains a
historical frozen contract and is not extended with new keys.

The WASM distribution provides generated Node and browser entry points,
TypeScript declarations, a minimal file-picker demo, structured `RxlsError`
objects, and a synchronous 32 MiB input limit. Build it with
`bash scripts/build-wasm-package.sh`; the CI release gate executes Node and
Chromium smoke tests, compares `reportJson` with `rxls diagnose`, and enforces
raw WASM, JavaScript glue, and compressed npm bundle budgets. See the
[WASM package guide](https://github.com/HyunjoJung/rxls/blob/main/bindings/wasm/npm/README.md)
for initialization and memory guidance.

## Rendering workspace

Development `main` also contains a separate `rxls-render` crate and
`@rxls/render-worker` browser/WASM package. They are not part of the published
core crate `v0.1.2`: the renderer builds one bounded fixed-point scene and
replays it to deterministic SVG, PDF, and PNG, while the browser surface keeps
parsing and virtual sheet/tile/page rendering inside a CSP-safe worker. See the
[renderer guide](render/README.md) and
[worker package guide](bindings/render-wasm/README.md) for source builds,
limits, font isolation, pagination, and distribution gates.

<!-- public-corpus-baseline:start -->
**Current public-corpus gate (2026-07-15).** The pinned fetch recipe selects 916
files from Apache POI and calamine at immutable upstream commits: 448 `.xls`,
413 `.xlsx`, 18 `.xlsm`, 21 `.xlsb`, and 16 `.ods`. `rxls corpus-report` opens
869; the remaining 47 are explicit expected rejections for encrypted input,
unsupported legacy BIFF, malformed containers, or structurally invalid BIFF streams.
The report records 0 unexpected failures and 0 unexpected accepts. Public visible-value checks report:

| Format | Comparable files | Result |
|---|---:|---:|
| `.xls` vs `xlrd` | 414 | 100.000% mean parity; 414/414 at least 99% |
| `.xlsx`/`.xlsm` vs `openpyxl` | 388 | 99.889% mean parity; 387/388 at least 99% |
| `.xlsb` vs `pyxlsb` plus committed residual oracles | 18 | 100.000% mean parity |
| `.ods` vs bounded ODF XML visible-text oracle | 14 | 100.000% mean recall |
<!-- public-corpus-baseline:end -->

The release claim depends only on public, reproducible fixtures and corpora.
GitHub Actions runs formatting, clippy, the feature/MSRV matrix, Rust and Python
harness tests, documentation, package checks, and the small pinned CI corpus.
The broader 916-file run is reproducible on demand with the commands below.

## Reproduce

Everything below runs from a clean checkout — no private data.

```bash
python3 -m pip install "xlrd==2.0.2" "openpyxl==3.1.5" "pyxlsb==1.0.10" "odfpy==1.4.1"
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
`cargo install` CLI—entirely outside the checkout—run:

```sh
cargo package --locked
python3 scripts/smoke_crate_distribution.py \
  --crate target/package/rxls-0.1.2.crate \
  --fixture tests/fixtures/xlsx/reader-structural.xlsx
```

After publication, exercise the same consumer, install, version, help,
diagnose, and invalid-usage contracts through crates.io with:

```sh
python3 scripts/smoke_crate_distribution.py \
  --registry-version 0.1.2 \
  --fixture tests/fixtures/xlsx/reader-structural.xlsx
```

Maintainers create two clean `Release` workflow-dispatch candidates from the
same commit. The second receives the first run's `baseline_run_id`; the
fail-closed bundle comparator requires deterministic artifacts to be identical
and explains permitted test-duration and successful fuzz-log differences.
Timing, RSS, and edit-output variation must remain inside the documented
same-SHA reproducibility/noise limits; the absolute budgets remain the
performance regression guard. Tag publication is allowed only after that report
and every hosted gate pass. The second candidate emits an immutable
exact-SHA attestation that also binds the candidate release-manifest digest.
The tag-triggered job requires successful exact-SHA CI and CodeQL push runs,
downloads the attested candidate, and fails before publishing unless its own
47-file bundle compares cleanly. Post-publication verification downloads every
release asset and validates full manifest coverage and checksums. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the exact sequence.

Pinned public spreadsheet corpus for parity work:

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

Each parity report records the oracle reader and installed version plus the
SHA-256 of the exact input manifest bytes. Directory-only development runs
explicitly report `input_manifest_sha256=none`; release evidence always uses
the pinned manifest.

The dry run should report 916 files (`.xls` 448, `.xlsx` 413, `.xlsm` 18,
`.xlsb` 21, `.ods` 16). Files download into gitignored `local/public-corpus`; this repo
commits the pinned recipe and docs, not the corpus payloads.

## Authoring (writing `.xlsx`)

Beyond reading, `rxls` builds styled `.xlsx` from data — no JVM, no template:

```rust
use rxls::{Cell, CellStyle, HAlign, Workbook};

let mut wb = Workbook::new();
let sheet = wb.add_sheet("입찰공고");

let header = CellStyle::new().bold().fill([0xDD, 0xEB, 0xF7]).align(HAlign::Center).wrap();
sheet.write_styled(0, 0, "공고명", &header);
sheet.write_styled(0, 1, "추정가격", &header);

sheet.write_url(1, 0, "https://www.g2b.go.kr/...", "뉴미디어 콘텐츠 제작");
sheet.write_styled(1, 1, 150_000_000.0, &CellStyle::new().num_fmt("₩#,##0"));

sheet.set_col_width(0, 42.0);
sheet.freeze_panes(1, 0);
sheet.autofilter(0, 0, 1, 1);

std::fs::write("report.xlsx", wb.to_xlsx())?;
```

Supports per-cell font (family/size/color/bold/italic/underline and
strikethrough), fill, borders, number formats, alignment + wrap, merged ranges,
column widths/row heights, frozen panes, autofilters, external hyperlinks,
**page setup** (orientation/margins/print-area/
repeat rows/columns/headers-footers), **sheet protection** (including cell-level
`Format` protection), **tab color**, **data validation** (dropdowns +
numeric/date rules), **conditional formatting** (cellIs / color scales / data
bars), **images** (PNG/JPEG), **charts** (bar/line/pie/scatter), **sparklines**,
**worksheet tables** (including named table header formats), **rich strings**
(including cell-level `Format`), and
**legacy comments/notes**. Styles are
interned into deduped OOXML resource tables; writer features are validated by
in-tree `openpyxl` gates. (Pivot tables, threaded comments, and macros are out
of scope.)

## Stability

Version 0.1.2 defines the current public API and documented semantics. Compatible
updates may add APIs and `#[non_exhaustive]` variants under the published SemVer
policy; applications that require an exact dependency graph should pin an exact
version. One deliberate design choice to be aware of: a single model serves
**both reading and authoring**. Readers populate the documented cross-format subset of layout,
style, and view metadata, but this is not a promise that every authoring setter
is reconstructed as a complete writer template. The reader also surfaces
**merged ranges** (`Sheet::merged_ranges()`),
from `.xls MERGECELLS` / `.xlsx <mergeCells>`) and best-effort formula text for
`.xlsx`, `.xls`, `.xlsb`, and `.ods` (`Cell::Formula`, with the cached value
retained). Read-discovered merges are tracked separately from authoring merges
so reading them never alters write output. Workbook-global user defined names
are surfaced for `.xlsx`, `.xls`, `.xlsb`, and `.ods` named ranges via
`Workbook::defined_names()`, and `.xlsx`/`.xlsb` package document properties,
`.xls` OLE properties, and `.ods` `meta.xml` populate `Workbook::properties`.
Sheet visibility is surfaced across the read formats, including `.ods` table
styles where `table:display="false"` maps to `Sheet::is_hidden()`.
Hyperlinks from OOXML relationships, XLSB `BrtHLink` records, BIFF HLINK
records, and ODS `text:a` links populate `Sheet::hyperlinks()`.
OOXML comments, XLSB comments parts, BIFF `Note` / `TxO` records, and ODS
`office:annotation` metadata populate `Sheet::comments()`.
OOXML `dataValidations`, XLSB `BrtDVal` / `BrtDValList`, BIFF `Dv` records, and
ODS `table:content-validation` metadata populate `Sheet::data_validations()`;
ODS conditions are preserved as custom validation formulas.
OOXML tables, XLSB binary table parts, and named ODS `table:database-range`
blocks populate
`Sheet::tables()` and workbook-level table lookup helpers
`Workbook::table_names()`, `Workbook::table_names_in_sheet()`, and
`Workbook::table_by_name()`.
OOXML sheet views, XLSB `BrtBeginWsView` / `BrtPane` records, and BIFF
`WINDOW2` / `PANE` records populate `Sheet::sheet_view()`.
OOXML `autoFilter`, XLSB `BrtBeginAFilter`, BIFF `_FilterDatabase`, and ODS
`table:database-range` metadata populate `Sheet::autofilter_range()`. BIFF
`Print_Area` sheet-local built-in names and ODS `table:print-ranges` metadata
populate `Sheet::page_setup().print_area`, ODS `table:table-header-rows`
metadata populates `Sheet::page_setup().repeat_rows`, ODS
`table:table-header-columns` metadata populates
`Sheet::page_setup().repeat_cols`, and BIFF/XLSB page setup records populate
orientation, margins, scaling, centering, header, and footer fields.
OOXML worksheet charts are surfaced as anchored `Sheet::charts()` metadata that
maps to the writer chart model, including axis titles.
OOXML worksheet images and ODS `draw:image` package parts are surfaced through
`Sheet::images()`, with `Workbook::pictures()` providing a calamine-style
workbook aggregate of image extensions and bytes.
The `worksheet_range` facade exposes rectangular row views with absolute row and
column bounds and, with the optional `serde` feature, typed row deserialization
including
`RangeDeserializerBuilder::with_header_row(row)`,
`RangeDeserializerBuilder::with_deserialize_headers::<T>()`, and raw `Cell`
rows for callers that want the exact `Text` / `Number` / `Date` / `Bool` /
`Formula` model instead of coercing into primitive fields.
`Range::used_cells()` reports calamine-style relative coordinates;
`Range::used_cells_abs()` keeps worksheet coordinates available. Formula ranges expose
the same rectangular lookup,
relative/absolute used-cell iteration, and allocation-free `row_views()` scan
surface with the same absolute row and column bounds for formula source text.
Numeric `deserialize_with` helpers keep invalid numeric cells non-fatal during
typed ingestion.
Calamine-style workbook helpers include `worksheet_range_at`, `worksheets`,
`worksheet_formula`, and `sheets_metadata` (`SheetType` + `SheetVisible`).
With the optional `chrono` feature, Excel date serials can also be converted
directly to `chrono::NaiveDateTime` via `excel_serial_to_naive_datetime` or
`Cell::as_naive_datetime`, with `Cell::as_naive_date` and
`Cell::as_naive_time` available when callers only need one component. Duration
serials can be converted to `chrono::Duration` via
`excel_serial_to_duration` or `Cell::as_duration`.
`Cell::get_datetime()` exposes the raw Excel serial for date/time cells when
callers want calamine-style typed access without choosing the workbook date
system yet.

## 0.1.2 release status

Version 0.1.2 is published on crates.io, docs.rs, and GitHub. Its release gates
cover reader and formula correctness, package-preserving XLSX/XLSM editing,
CLI and JSON contracts, WASM/npm and browser consumers, public-corpus parity,
security analysis, fuzzing, performance budgets, SBOM/provenance, independent
LibreOffice checks, and exact-package installation.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The local gate is
documented there and enforced by GitHub Actions.

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
