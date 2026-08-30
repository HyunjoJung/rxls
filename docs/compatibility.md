# Compatibility

[Back to README](../README.md)

This document defines the format and feature boundaries of the published
`rxls` core crate. Reading, authoring, and preservation editing are separate
capabilities; support for one does not imply support for the others.

## Format matrix

| Format | Read | Create | Preserve and edit | Cargo requirement |
|---|:---:|:---:|:---:|---|
| `.xls` (BIFF8/5/7) | Yes | No | No | Always available |
| `.xlsx` | Yes | Yes | Yes | `xlsx`, enabled by default |
| `.xlsm` | Yes | No | Yes, including retained VBA | `xlsx`, enabled by default |
| `.xlsb` | Yes | No | No | `xlsb` |
| `.ods` | Yes | No | No | `ods` |

`Workbook::open` detects the input container from bytes and returns one
`Workbook` model for every enabled read format. `Spreadsheet::open` exposes the
same parsed workbook plus edit capability; XLS, XLSB, and ODS report a typed
read-only reason. An incomplete or metadata-lossy OOXML package may remain
readable while edits are rejected.

## Reading

The common cell model includes text, numbers, Excel date serials, booleans,
errors, and formulas with cached values. Search and indexing callers can use
`extract_text` or `Workbook::to_text`, while structured callers can retain
coordinates, typed cells, dimensions, formulas, and metadata.

Date/time serials and percentages are rendered through retained number-format
metadata. Excel custom formats cover positive/negative/zero/text sections,
conditions and colors, locale and currency markers, grouping and scaling,
fractions, scientific notation, date/time and elapsed tokens, literals,
escapes, and text placeholders. ODS prefers the source display paragraph and
uses typed-value fallbacks when no display paragraph is present.

### Reader-surfaced metadata

| Surface | API | Sources |
|---|---|---|
| Merged ranges | `Sheet::merged_ranges()` | XLS `MERGECELLS`, XLSX `mergeCells` |
| Formula text and cached value | `Cell::Formula` | XLS, XLSX, XLSB, and ODS; source text is best effort |
| Defined names | `Workbook::defined_names()` | Named ranges in all read formats |
| Document properties | `Workbook::properties` | OOXML package properties, XLS OLE properties, ODS `meta.xml` |
| Sheet visibility | `Sheet::is_hidden()` | All read formats |
| Hyperlinks | `Sheet::hyperlinks()` | OOXML relationships, XLSB `BrtHLink`, BIFF `HLINK`, ODS `text:a` |
| Comments and notes | `Sheet::comments()` | OOXML comments, XLSB comment parts, BIFF `Note`/`TxO`, ODS annotations |
| Data validation | `Sheet::data_validations()` | OOXML, XLSB, BIFF, and ODS validation records |
| Tables | `Sheet::tables()` and workbook table lookup helpers | OOXML/XLSB table parts and named ODS database ranges |
| Sheet view and panes | `Sheet::sheet_view()` | OOXML, XLSB, and BIFF view records |
| Autofilter | `Sheet::autofilter_range()` | OOXML, XLSB, BIFF, and ODS filter ranges |
| Page setup | `Sheet::page_setup()` | Print area, repeat rows/columns, orientation, margins, scaling, header, footer |
| Charts | `Sheet::charts()` | Anchored OOXML worksheet charts |
| Images | `Sheet::images()` and `Workbook::pictures()` | OOXML worksheet images and ODS package images |

Reader-populated layout, style, and view data is a documented cross-format
subset. It does not promise that every authoring setter can be reconstructed as
a complete writer template. Read-discovered merges, for example, are tracked
separately from authoring merges so reading never changes write output.

### Range and typed rows

- `worksheet_range` exposes rectangular row views with absolute bounds.
- `Range::used_cells()` returns relative coordinates and
  `Range::used_cells_abs()` retains worksheet coordinates.
- Formula ranges expose rectangular lookup, relative and absolute used-cell
  iteration, and allocation-free `row_views()`.
- Workbook helpers include `worksheet_range_at`, `worksheets`,
  `worksheet_formula`, and `sheets_metadata`.
- The `serde` feature provides typed row deserialization, configurable header
  rows, typed headers, raw `Cell` rows, and numeric deserialization helpers.
- The `chrono` feature converts Excel date/time and duration serials to
  `chrono` types while retaining raw serial access.

Formula re-evaluation is a bounded deterministic subset, not a full Excel
calculation engine. See [Formula support](formulas.md).

## Creating XLSX

`Workbook::new` authors XLSX files without a template. The writer supports:

- fonts, fills, borders, number formats, alignment, wrapping, row heights, and
  column widths;
- merged ranges, frozen panes, autofilters, hyperlinks, rich strings, and
  legacy comments/notes;
- page orientation, margins, print areas, repeating rows/columns, headers, and
  footers;
- sheet and cell protection, tab colors, data validation, and conditional
  formatting;
- PNG/JPEG images, bar/line/pie/scatter charts, sparklines, and worksheet
  tables.

Styles are interned into deduplicated OOXML resource tables. Writer features
are checked by in-tree `openpyxl` gates. Pivot tables, threaded comments, macro
creation, and authoring formats other than XLSX are outside the current scope.

## Export, diagnostics, CLI, and WASM

A sheet or workbook can be exported to CSV, HTML, or Markdown. CSV export has
an explicit formula-text policy for callers that will open output in
spreadsheet software.

`WorkbookReport` provides machine-readable sheet, cell, formula, document
property, feature inventory, and parse-provenance data. The CLI exposes this as
`rxls diagnose` and also provides `info`, `csv`, `compare`, and
`corpus-report`. Successful help and command output use stdout; usage and
operational errors use stderr. Exit classifications and diagnose schema changes
are compatibility-controlled behavior.

The isolated `bindings/wasm` crate exposes the core model through generated Node
and browser entry points, TypeScript declarations, structured `RxlsError`
objects, and a synchronous 32 MiB input limit. It is built and distributed
separately from the native CLI.

The source workspace also contains an experimental renderer and
`@rxls/render-worker`. They are not included in the published core crate and do
not extend the read, write, edit, CLI, or core WASM compatibility claims.

The [public browser viewer](https://hyunjojung.github.io/rxls/) is built from
that worker and the static `viewer/` application. It provides local file and
project-sample inspection, sheet and page rendering, zoom, and SVG/PNG export.
Workbook bytes stay in the browser session; the viewer remains a separately
versioned product surface rather than part of the core crate's SemVer contract.

## Cargo features

| Feature | Default | Surface |
|---|:---:|---|
| `cli` | Yes | Builds the `rxls` binary |
| `xlsx` | Yes | XLSX/XLSM reading, XLSX writing, package-preserving editing |
| `xlsb` | No | XLSB reader; enables XLSX package support |
| `ods` | No | ODS reader |
| `serde` | No | Typed row deserialization |
| `chrono` | No | Date/time and duration conversions |
| `full` | No | All library format and typed-data features; excludes `cli` |

Features are additive. Use `default-features = false` for an XLS-only library
build or `features = ["full"]` for every reader and typed-data helper. The
minimum supported Rust version is 1.85.

Version 0.1.3 defines the current published API and semantics. Compatible
updates may add APIs and `#[non_exhaustive]` variants under the crate's SemVer
policy. Pin an exact version when the dependency graph or documented behavior
must remain exact.
