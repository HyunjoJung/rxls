# Public output contracts

This document defines the compatibility boundary for rxls 0.1.2 output. Tests
may exercise more behavior, but releases must not weaken the guarantees below
without an explicit contract change.

## Deterministic output

For the same `Workbook`, rxls emits byte-identical XLSX, CSV, HTML, Markdown,
and diagnose JSON output when called with the same options. Worksheet and cell
ordering follows workbook order and ascending `(row, column)` order; duplicate
cell coordinates use last-write-wins semantics. XLSX package entry order and ZIP
metadata are fixed by the writer.

`Workbook::to_xlsx_checked` is the recommended authoring path. It returns a
typed `WriteError` for invalid Excel coordinates and metadata, cell strings over
32,767 UTF-16 code units, and workbooks conservatively estimated to exceed the
256 MiB uncompressed writer payload budget. `Workbook::to_xlsx` remains the
best-effort conversion path and may sanitize or omit data beyond writer limits.

## CSV

`export_csv` with `CsvOptions::default()` preserves the existing CSV contract:
UTF-8 without a BOM, comma delimiter, LF record separators, display values,
standard double-quote escaping, no trailing record separator, and formula-like
text preserved verbatim. Its default result limit is 256 MiB.

Callers can select a delimiter, LF or CRLF separators, a UTF-8 BOM, a smaller
output limit, and `CsvFormulaPolicy::Escape`. Escape mode prefixes fields that
start with `=`, `+`, `-`, `@`, tab, CR, or LF with an apostrophe before quoting.
It is opt-in because mitigation deliberately changes exported text. Quote, CR,
and LF are invalid delimiters. Embedded field newlines are preserved inside a
quoted field; the newline option controls record separators only.

The CLI exposes these settings as `--delimiter`, `--newline lf|crlf`, `--bom`,
`--formula-injection preserve|escape`, and `--max-output-bytes`.

## HTML and Markdown

`Sheet::to_html` emits one `<table>` fragment without an HTML document wrapper.
It uses formatted display text, preserves modeled merged cells with `rowspan`
and `colspan`, emits empty cells needed to keep sparse columns aligned, and
escapes `&`, `<`, `>`, and `"` in cell content.

`Sheet::to_markdown` emits a GitHub-flavored table, treats the first materialized
row as the header, escapes `|`, and converts embedded CR/LF to `<br>`. Because
GFM cannot represent merged cells losslessly, sheets with merges fall back to
the HTML fragment. Sheets wider than 256 columns use the same bounded fallback.
The workbook wrappers return `None` for an invalid or non-worksheet index.

## Diagnose JSON schema v1

`WorkbookReport::to_json` and `rxls diagnose` use `schema_version: 1`. The
compact object has this fixed top-level order and shape:

1. `schema_version` — integer
2. `format` — string
3. `stats` — object
4. `properties` — object
5. `defined_names_count` — integer
6. `local_defined_names_count` — integer
7. `features` — object
8. `evaluation` — object
9. `warnings` — array of strings

Within schema v1, existing keys, value types, ordering, and meanings are frozen.
Count corrections for parser bugs do not change the schema. Consumers should
tolerate unknown warning strings. Adding, removing, renaming, reordering, or
changing the type or meaning of a JSON field requires a schema-version bump and
a new golden fixture. The current exact serialization is locked by
`tests/golden/diagnose-v1.json`.

## CLI process contract

Successful commands return 0 and write results to stdout. Successful `--help`
also writes its usage text to stdout and leaves stderr empty. Invalid usage and
operational errors are written to stderr; they do not write usage text or a
partial CSV result to stdout. Stable exit classes are:

| Code | Meaning |
|---:|---|
| 0 | success |
| 1 | parse, comparison, or verification failure |
| 64 | invalid command line or usage |
| 65 | invalid sheet/data selection or bounded export rejection |
| 66 | input file cannot be read |
| 69 | command unavailable in the selected feature build |
| 74 | output I/O failure |

`--help` and `--version` return 0. Unknown commands and missing required
arguments return 64 with their diagnostics and usage text on stderr.

## API selection by workload

- Small and ordinary files: `Workbook::open`, then the in-memory export helpers.
- Authored XLSX: prefer `Workbook::to_xlsx_checked` and handle `WriteError`.
- Bounded CSV: use `export_csv` with an explicit `max_output_bytes` when the
  default is too large for the application.
- Streaming consumers: iterate `Sheet::rows` and write records directly. The
  convenience HTML, Markdown, CSV, and XLSX functions return complete in-memory
  buffers and are not streaming APIs.
