# rxls

[![Crates.io](https://img.shields.io/crates/v/rxls.svg)](https://crates.io/crates/rxls)
[![Docs.rs](https://docs.rs/rxls/badge.svg)](https://docs.rs/rxls)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![MSRV](https://img.shields.io/badge/MSRV-1.85-orange.svg)

Native Rust reader for Microsoft Excel — legacy **`.xls`** (Excel 97–2003 / 95
binary, BIFF8 and BIFF5/7 — [MS-XLS]) and modern **`.xlsx`** (OOXML /
SpreadsheetML). No JVM, no Apache POI, no subprocess — a small, panic-free
parser. Extract **plain text**, or read **typed cells**.

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

Examples:

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

Modern **`.xlsx`** (OOXML) is read too (default `xlsx` feature): `Workbook::open`
auto-detects OLE2 `.xls` vs ZIP `.xlsx` and produces the same typed cells / text.
`xlsx` cell data, shared strings, and number formats (for dates) are parsed via
`zip` + `quick-xml`; `default-features = false` drops both deps for an
`.xls`-only build.

Unsupported password-protected workbooks (`FILEPASS`) are reported as
`Error::Encrypted` rather than emitting ciphertext. Legacy XOR (Method 1)
workbooks using Excel's default `VelvetSweatshop` password are deobfuscated.
Every read is bounds-checked: malformed input returns an [`Error`], never a
panic — safe to run untrusted files in-process.

## Why not just `calamine`?

[`calamine`](https://crates.io/crates/calamine) is excellent and far more
battle-tested — for breadth, maturity, and adoption it is the right choice. `rxls`
reads `.xls` + `.xlsx` + **`.xlsb`** + **`.ods`** (the latter two via the `xlsb` /
`ods` features) and is built for two things calamine isn't optimised for:
**robustness on adversarial legacy `.xls`** and a **lightweight, JVM-free** footprint.

**Head-to-head on real public corpora** (rxls vs **calamine 0.35**, with `xlrd` /
`openpyxl` as the value oracle — reproducible, not author-asserted):

| Corpus | rxls reads | calamine reads | rxls handles, calamine can't / vice-versa | value recall (date-fair) |
|---|---|---|---|---|
| GovDocs1, 453 `.xls` | **448** | 422 (**26 panics**) | **26 / 0** | rxls ≈ calamine |
| Apache POI, 415 `.xls` | **344** | 341 (2 panics) | 4 / 1 | rxls 94% · calamine 95% |
| public, 350 `.xlsx` | **296** | 293 | 6 / 3 | rxls **95.4%** · calamine 94.4% |

So on real-world reading `rxls` is **on par with calamine on accuracy and ahead on
robustness** — it extracts files calamine panics on (28 panics across the set;
rxls: **zero**), with comparable date-fair value recall. (On `.xls`, calamine
renders dates as raw serials while `rxls` renders ISO; the table normalizes that.)
For breadth/maturity/adoption calamine still wins; the figures above are
reproducible — see [Reproduce](#reproduce).

## Scope & parity

Targets plain-text extraction for search/indexing. Date/time serials and
percentages are rendered as Excel displays them (via `XF`/`FORMAT`/`DATEMODE`
for Excel files and ODS value-type fallbacks when no display paragraph is
present); other cached cell values are emitted as text. Formula re-evaluation is
limited to the deterministic MVP exposed by `Workbook::evaluate_cell`, which
returns a typed `FormulaUnsupportedReason` (unsupported/volatile function,
external reference, circular reference, unresolved name, oversized range,
missing sheet, …) instead of guessing when a formula falls outside that MVP;
full custom number-format rendering and styling are out of scope.

**Editing existing files** (`Spreadsheet::open`/`set_cell_value`/
`set_cell_formula`/`append_row`/`clear_range`/document- and sheet-metadata
setters/`save`) is package-preserving and `.xlsx`/`.xlsm`-only: edits rewrite
worksheet/workbook XML in place through an arena-based `XmlTree` engine, so
every untouched part round-trips byte-for-byte, and new/changed text is
written as inline `<is>` strings rather than growing the shared string table.
`.xls`, `.xlsb`, and `.ods` are read-only through this API — `Spreadsheet`
reports a typed `EditCapability::ReadOnly(EditReadOnlyReason)` (`LegacyBiff`,
`BinaryPackage`, `OpenDocument`, or `PackageMetadataLoss` for an `.xlsx`
package that can't be round-tripped losslessly enough to edit) rather than
attempting a lossy write.

A worksheet can also be exported directly to **CSV**, **HTML**, or
**Markdown** (`Sheet`/`Workbook::to_csv`/`to_html`/`to_markdown`), and a whole
workbook can be summarized as machine-readable JSON via `WorkbookReport` —
sheet/cell/formula counts, document properties, and a feature inventory,
surfaced on the CLI as `rxls diagnose <file>` (and `rxls csv <file>` for
direct CSV export). A `cdylib` + `wasm-bindgen` adapter (`src/wasm.rs`)
exposes the read/export/report surface to JavaScript for `wasm32` targets;
the native `rxls` CLI binary itself lives behind the `cli` feature (on by
default, so existing native workflows are unaffected).

> **Validation — three tiers, by reproducibility.**
> 🟢 **Local gate / workflow-ready** (`cargo test`, enforced openpyxl gate, and
>    public parity scripts that fetch calamine's MIT corpus and run xlrd parity —
>    see [Reproduce](#reproduce)). GitHub Actions workflows are currently disabled
>    to avoid runner cost. 🟡 **Public, reproducible on demand** (GovDocs1
>    + Apache POI: anyone can fetch and re-run the parity scripts). 🔴 **Author-run,
>    corpus not redistributable** (the 409 Korean-gov files — government data).
>
> 🔴 The 409-file Korean government corpus is 100% BIFF8 (validated
> vs xlrd at ~99.99%). 🟡 Beyond it, `rxls` is validated on an **868-file public corpus**:
> the [GovDocs1](https://digitalcorpora.org/corpora/files/govdocs1/) US-government
> set (453 `.xls`, ~33 M cells) and the **Apache POI** `.xls` test suite (415
> files, incl. deliberate fuzzer/crash cases). **Robustness:** every file plus a
> **22 000-run mutation fuzz** (`scripts/xls-fuzz.py`) and the panic-free sweep
> (`examples/robustness.rs`) — **zero panics** across all ~1 300 files (encrypted,
> truncated, and corrupt inputs return a clean [`Error`]). **Parity vs xlrd**
> (`scripts/xls-xlrd-parity.py`): 99.1% on POI (357/367 files ≥99%) and ≥97.6% on
> GovDocs1 (286/298 files ≥99%; the mean is understated by difflib's autojunk
> heuristic on large sheets — the residual sub-99% files are number-format
> rendering, e.g. scientific vs decimal, not missing data). The **BIFF5/7 record
> path** is additionally validated on 13 real BIFF5 reference files from
> calamine's suite (xlrd match **100%**). cp949 is covered by `resolve_encoding` +
> a synthetic cp949 BIFF5 test; `cp1361`/Johab falls back to cp949. Date math is
> unit-tested at the edges (1900 leap-bug, 1904 epoch, rounding carry, range
> bounds).
>
> **`.xlsx`** is validated against `openpyxl` on the **Apache POI `.xlsx` test
> suite** (363 files incl. fuzzer/crash cases): 0 panics, and **311/332** comparable
> files ≥99% parity (`scripts/xlsx-openpyxl-parity.py`; residual is number-format
> rendering, plus 27 files `openpyxl` cannot read that `rxls` extracts). This
> surfaced and fixed two bugs: an **8 GiB shared-string-amplification OOM** (now
> bounded by `MAX_TEXT_BYTES`) and **`r`-less cells/rows** (implicit positions,
> emitted by LibreOffice/EPPlus) that were being dropped.

## Reproduce

Everything below runs from a clean checkout — no private data.

```bash
# 🟢 In-tree tests + the strict openpyxl consumer gate.
#    RXLS_REQUIRE_OPENPYXL makes the gate FAIL (not skip) if openpyxl is missing,
#    so the .xlsx authoring conformance is genuinely enforced.
python3 -m pip install openpyxl
RXLS_REQUIRE_OPENPYXL=1 cargo test --all-targets

# 🟢 The .xls-only build (dependency-light, no zip/quick-xml).
cargo test --no-default-features

# 🟢 Public-corpus parity vs xlrd on calamine's MIT .xls suite (BIFF5/7 + date +
#    SST-CONTINUE). The local parity gate targets --min 0.95 (currently 100%).
python3 -m pip install "xlrd>=2.0"
cargo build --example extract
bash scripts/fetch-xls-reference.sh
python3 scripts/xls-xlrd-parity.py --corpus local/xls-poc/cal --bin target/debug/examples/extract --min 0.95

# 🟡 GovDocs1 / Apache POI public corpora (large; fetch your own copy, then):
#    python3 scripts/xls-xlrd-parity.py     --corpus <dir-of-xls>  --bin target/debug/examples/extract
#    python3 scripts/xlsx-openpyxl-parity.py --corpus <dir-of-xlsx> --bin target/debug/examples/extract
```

Pinned public spreadsheet corpus for parity work:

```bash
python3 scripts/fetch-public-corpus.py --dry-run
python3 scripts/fetch-public-corpus.py
cargo build --features full --example extract
python3 scripts/xls-xlrd-parity.py --manifest local/public-corpus/manifest.json --bin target/debug/examples/extract --limit 50
python3 scripts/xlsx-openpyxl-parity.py --manifest local/public-corpus/manifest.json --bin target/debug/examples/extract --limit 50
python3 scripts/xlsb-pyxlsb-parity.py --manifest local/public-corpus/manifest.json --bin target/debug/examples/extract --limit 50
python3 scripts/ods-odfpy-parity.py --manifest local/public-corpus/manifest.json --bin target/debug/examples/extract --limit 50
```

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

Pre-1.0: the API may change in minor releases until it settles; pin a version if
that matters to you. One deliberate design choice to be aware of: a single model
serves **both reading and authoring**. Most layout setters (`freeze_panes`,
`set_col_width`, styles) are *authoring inputs* the reader does not populate.
The reader does surface **merged ranges** (`Sheet::merged_ranges()`),
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

## Roadmap

- [x] BIFF5/7 (`Book` stream) codepage strings (cp949 etc.) via `CODEPAGE`
- [x] `RSTRING` rich-text cells; `FILEPASS` encryption detection
- [x] Number-format aware rendering (dates `yyyy-mm-dd`, percentages) via
      `XF` + `FORMAT` + `DATEMODE`
- [x] ODS percentage/time fallback display text when no `<text:p>` display text
      is present
- [x] BIFF5/7 record path validated on real reference files (xlrd as oracle)
- [x] Embedded chart/pivot substreams handled by BOF/EOF depth (no sheet desync)
- [x] Tolerant CFB fallback for non-spec OLE2 directories the `cfb` crate rejects
- [x] `.xlsx` implicit cell positions (`r`-less cells/rows); shared-string OOM cap
- [x] Mutation fuzzing (no panics on adversarial input) — 868-file public corpus
- [x] `LABEL`/`RSTRING`/`STRING` records that span `CONTINUE` (no truncation)
- [x] Merged-range read (`.xls MERGECELLS` / `.xlsx <mergeCells>`) via
      `Sheet::merged_ranges()`; `.xlsx` formula text via `Cell::Formula`
- [ ] A real Korean (cp949) BIFF5 corpus file to validate that path directly
- [x] Elapsed-time formats (`[h]:mm`) rendered as total hours; `BOOLERR` cells
- [x] XOR (Method 1) decryption for default-password workbooks
- [x] `.xlsb` (BIFF12 binary) reader via the `xlsb` feature (validated vs `pyxlsb`)
- [x] `.ods` (OpenDocument) reader via the `ods` feature (validated vs `odfpy`)
- [x] `.xls` formula-token (Ptg) decompilation → `Cell::Formula` source text
- [x] `.xlsb` 1904 date-system detection via `BrtWbProp`
- [x] Writer rich strings / comments; `.xls` formula-string follower records
- [x] `.xlsb` `BrtFmlaString` cached string formula records
- [x] Signal partial extraction via `Workbook::is_partial()` /
      `Workbook::text_truncated` when the `MAX_TEXT_BYTES` cap is hit
- [x] Optional `serde` helpers for typed row deserialization from worksheet ranges
- [x] Optional `chrono` helpers for Excel date serials and date cells

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The local gate is
`cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test && cargo doc --no-deps`.

## License

Licensed under the [MIT License](LICENSE). Third-party dependency licenses are
listed in [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md). This crate
implements only the publicly documented [MS-XLS] / [MS-CFB] specifications and
contains no Microsoft source.

[MS-XLS]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/
[MS-CFB]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cfb/
