# Compatibility

[Back to README](../README.md)

This document distinguishes the published `rxls` core **0.1.4**, source
checkouts, and separately versioned adapters. Reading,
authoring, and preservation editing are separate capabilities; support for one
does not imply support for the others.

## Release and source boundaries

| Surface | Version boundary | Formula/cache contract |
|---|---|---|
| Native `rxls` core from the registry | 0.1.4 (`v0.1.4`) | Single-cell and shared-budget batch evaluation, atomic formula-cache batches, and bounded UTF-8 text production |
| Historical native core | 0.1.3 (`v0.1.3`) | Single-cell `Workbook::evaluate_cell`; no `evaluate_cells`, `set_formula_cached_values`, or `TextLimitExceeded` |
| Current native source | Pin a reviewed revision | Check that revision's API and verification evidence |
| npm `rxls-wasm` | Core adapter 0.1.4 | Its own synchronous JavaScript API, not an automatic export of every native Rust method |
| npm `@rxls/render-worker` | Worker 0.3.0, versioned independently | Interactive rendering and atomic recalculating edits through the worker API |

Installing registry `rxls = "=0.1.3"` does not select the APIs added in 0.1.4.
The batch examples in [Formula support](formulas.md) and
[Preservation and editing](preservation.md) work with `rxls = "=0.1.4"` or an
appropriate source checkout. Their evaluator budgets and edit limits
are separate contracts; successful evaluation is not proof that a cache value
can be written to XLSX.

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

In the source checkout, equal adjacent column layouts are written as compact
ranges, including imported full-width formats. Unspecified gaps remain
distinct. This authoring optimization does not rewrite the retained column
intervals of package-preserving `Spreadsheet` edits.

## Export, diagnostics, CLI, WASM, MCP, and VS Code

A sheet or workbook can be exported to CSV, HTML, or Markdown. CSV export has
an explicit formula-text policy for callers that will open output in
spreadsheet software.

`WorkbookReport` provides machine-readable sheet, cell, formula, document
property, feature inventory, and parse-provenance data. The CLI exposes this as
`rxls diagnose` and also provides `info`, `csv`, `compare`, and
`corpus-report`. Successful help and command output use stdout; usage and
operational errors use stderr. Exit classifications and diagnose schema changes
are compatibility-controlled behavior.

The isolated `bindings/wasm` crate is distributed as npm **`rxls-wasm` 0.1.4**.
It exposes the core model through generated Node and browser entry points,
TypeScript declarations, structured `RxlsError` objects, and a synchronous
32 MiB input limit. It is built and distributed separately from the native CLI
and is not the `@rxls/render-worker` package.

The isolated `bindings/mcp` crate exposes nine local stdio tools for opening,
inspecting, reading, exporting, preservation-editing, save-copying, and closing
workbook sessions. It accepts paths rather than workbook bytes, canonicalizes
them below one or more configured roots, and opens no network listener. One
workbook is capped at 32 MiB, four sessions at 128 MiB of current package bytes,
one range at 10,000 cells, one edit batch at 100 cells, and each JSON-RPC line
and structured result at 1 MiB. XLS, XLSB, and ODS are read-only; only retained
XLSX/XLSM sessions with `EditCapability::ReadWrite` can mutate or save. The MCP
crate is currently built from the source workspace and is not independently
published.

The source workspace also contains the renderer and local MCP server. The
browser facade is distributed separately as **`@rxls/render-worker` 0.3.0**;
its version and protocol do not change the published native core or
`rxls-wasm` compatibility claims. See its [README](../bindings/render-wasm/README.md)
for the worker's interactive rendering and recalculating-edit methods.
The worker keeps retained workbook bytes inside its dedicated session. For
editable XLSX/XLSM packages it exposes typed cell inspection, cell value or
formula replacement, document-property replacement, undo/redo, and preserved
package serialization. Its history is capped at 20 entries and 32 MiB. XLS,
XLSB, ODS, and metadata-lossy OOXML return stable read-only reasons before any
mutation is attempted. An editable session reserves that full history budget
inside the worker's 128 MiB aggregate open-resource ceiling before it opens.

The [public browser viewer](https://hyunjojung.github.io/rxls/) is built from
that worker and the static `viewer/` application. It provides local file and
project-sample inspection, sheet and page rendering, zoom, SVG/PNG export, and
the bounded XLSX/XLSM edit operations above. Downloads preserve the source
`.xlsm` extension and untouched VBA/package parts. Workbook bytes stay in the
browser session; the viewer remains a separately versioned product surface
rather than part of the core crate's SemVer contract.

The separately packaged `extensions/vscode` custom editor provides read-only
previews for XLS, XLSX, XLSM, XLSB, and ODS through the same worker. It supports
sheet and page navigation, zoom, explicit and file-change reload, and SVG/PNG
export. Inputs are capped at 32 MiB, at most four active previews retain 128 MiB
in aggregate, and export messages are capped at 16 MiB. Workbook bytes remain
inside the extension host and isolated webview worker. The extension opens no
external network connection, collects no telemetry, uses a packaged-resource
CSP, and supports both Restricted Mode and virtual workspaces. It is versioned
and verified independently from the core crate.

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

Version 0.1.4 defines the current published API and semantics. CI and release
verification compare all,
default, and no-default feature APIs against the published 0.1.3 baseline.
Compatible updates may add APIs and `#[non_exhaustive]` variants under the
crate's SemVer policy. Pin an exact version when the dependency graph or
documented behavior must remain exact.
