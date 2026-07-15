# Rust public API inventory and compatibility policy

This is the rxls 0.1.2 API-freeze baseline and the compatibility policy for the
path to 1.0.0. The target breaking delta is **zero**. Any exception requires an
explicit release note, migration path, and approval before 1.0.

## Classification

- **Frozen**: retained through 1.x with the same path and compatible semantics.
- **Extensible**: retained, but marked `#[non_exhaustive]`; callers must include
  a wildcard when matching or use constructors instead of struct literals.
- **Compatibility alias**: retained through 1.x, but new code should prefer the
  canonical type named below.
- **Feature-gated**: frozen whenever its documented Cargo feature is enabled.

Every publicly reachable inherent method, trait method, associated function,
and public field on a type listed below inherits that type's classification
unless a row says otherwise. No current public item is classified for removal
or privatization, and there are no planned 1.0 renames or deprecations.

## Root inventory

| Area | Public items | Classification |
|---|---|---|
| Errors | `Error`, `Result`, `WriteError` (`xlsx`), `CsvExportError` | `Error`, `WriteError`, and `CsvExportError` extensible; `Result` compatibility alias for `Result<T, Error>` |
| Cells and dates | `Cell`, `CellErrorType`, `ExcelDateTime`, `DataType`, `HeaderRow`, `excel_serial_to_datetime` | Frozen |
| Compatibility cell names | `Data`, `DataRef` | Compatibility aliases for `Cell` and `&Cell` |
| Styles | `Color`, `FormatScript`, `FormatPattern`, `Fill`, `HAlign`, `VAlign`, `Alignment`, `Font`, `TextRun`, `BorderStyle`, `Border`, `CellStyle`, `CellProtection`, `Format` | Frozen |
| Compatibility style names | `FormatAlign`, `FormatBorder` | Compatibility aliases for `HAlign` and `BorderStyle` |
| Cell ranges | `Dimensions`, `Range`, `RangeRows`, `RangeRow`, `RangeRowCells`, `RangeRowUsedCells` | Frozen |
| Formula ranges | `FormulaRange`, `FormulaRangeRows`, `FormulaRangeRow`, `FormulaRangeRowCells`, `FormulaRangeRowUsedCells` | Frozen |
| Workbook model | `Workbook`, `Sheet`, `LocalDefinedName`, `DocProperties`, `Reader` | `Workbook` and `DocProperties` extensible; remaining items frozen |
| Metadata | `WorkbookMetadata`, `WorksheetMetadata`, `SheetMetadata`, `SheetType`, `SheetVisible`, `SheetView` | Frozen |
| Worksheet features | `Comment`, `CommentAuthor`, `Table`, `Image`, `ImageFmt`, `Picture`, `Sparkline`, `SparklineKind`, `Chart`, `ChartKind`, `Series`, `CondFormat`, `CfRule`, `DataValidation`, `DvKind`, `DvOp`, `ProtectionOptions`, `PageSetup` | Frozen |
| Formula evaluation | `FormulaEvaluation`, `FormulaUnsupportedReason` | Extensible |
| CSV export | `export_csv`, `CsvOptions`, `CsvNewline`, `CsvFormulaPolicy`, `DEFAULT_EXPORT_MAX_BYTES` | Policy enums extensible; function, options fields, and constant frozen |
| Diagnostics | `WorkbookReport`, `ReportStats`, `ReportProperties`, `ReportFeatures`, `ReportEvaluation`, `REPORT_SCHEMA_VERSION` | Report structs extensible and read-oriented; constructors and schema constant frozen |
| Package editing | `Spreadsheet`, `EditCapability`, `EditReadOnlyReason` (`xlsx`) | Feature-gated and frozen |
| Serde rows | `RangeDeserializer`, `RangeDeserializerBuilder`, `DeError`, numeric deserializers (`serde`), date/time/duration deserializers (`serde` + `chrono`) | Feature-gated; `DeError` is a compatibility alias to serde's value error |
| Chrono conversions | `excel_serial_to_naive_datetime`, `excel_serial_to_duration` (`chrono`) | Feature-gated and frozen |
| Convenience functions | `Workbook::open`, `extract_text`, `Workbook::to_xlsx`/`to_xlsx_checked` (`xlsx`) | Frozen |
| Portable module | `wasm::{extract_text_bytes,to_csv_bytes,to_html_bytes,report_json_bytes}` | Frozen native adapter functions; JavaScript packaging is versioned separately |

The method-level inventory is the all-features rustdoc generated with
`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --locked`.
Adding a new method is compatible. Removing a method, narrowing accepted input,
changing coordinates, changing a return type, or moving a path is breaking.
The current machine baseline contains 112 public items and is checked under
Rust 1.85 before packaging and publication.

The method-by-method `Spreadsheet` behavior, including capability gates,
atomicity tiers, package preservation, rejection cases, and intentionally
unsupported structural edits, is frozen separately in
[`EDITING-CONTRACT.md`](EDITING-CONTRACT.md). The API inventory and that editing
contract are complementary: a signature-compatible change that narrows a
documented editing guarantee is still a compatibility change requiring review.

### Exact free-function and constant inventory

- Always available: `extract_text`, `excel_serial_to_datetime`, `export_csv`,
  `DEFAULT_EXPORT_MAX_BYTES`, and `REPORT_SCHEMA_VERSION`.
- `serde`: `deserialize_as_f64_or_none`, `deserialize_as_f64_or_string`,
  `deserialize_as_i64_or_none`, and `deserialize_as_i64_or_string`.
- `serde` + `chrono`: `deserialize_as_duration_or_none`,
  `deserialize_as_duration_or_string`, and the `deserialize_as_{date,time,datetime}_{1900,1904}_{or_none,or_string}` family.
- `chrono`: `excel_serial_to_naive_datetime` and
  `excel_serial_to_duration`.
- `wasm` module: `extract_text_bytes`, `to_csv_bytes`, `to_html_bytes`, and
  `report_json_bytes`.

The only public traits are `DataType` and `Reader`; all their current required
and provided methods are frozen. All other callable public items are inherent
methods on the classified root types above.

## Feature inventory and additive rules

| Feature | Contract |
|---|---|
| default | `xlsx` + `cli`; remains a batteries-included native build |
| `cli` | Builds the `rxls` binary; does not change library semantics |
| `xlsx` | OOXML read/write, checked writer, and package-preserving editing |
| `xlsb` | BIFF12 reader; implies `xlsx` for ZIP/package support |
| `ods` | OpenDocument reader; independent of `xlsx` |
| `serde` | Typed range deserialization helpers |
| `chrono` | Chrono conversions without enabling serde |
| `full` | All library format and typed-data features; intentionally excludes `cli` |

Features are additive: enabling one may expose additional APIs and formats but
must not change the meaning of an API already available without it. Base builds
always retain the legacy XLS reader. The supported build matrix is no-default,
each individual feature, `full`, default, and all-features. The MSRV is 1.85.

## Coordinate and range policy

- Public worksheet, evaluation, editing, and authoring coordinates are
  **zero-based**: rows are `u32`, columns are `u16`.
- Worksheet ranges are inclusive `(start_row, start_col, end_row, end_col)`.
- `Range`/`FormulaRange` constructors use `(u32, u32)` coordinates for calamine
  compatibility; absolute cell access uses the zero-based row/column policy.
- Relative row-view offsets use `usize`. File-format A1 and one-based indexes
  are converted internally and are never accepted as numeric public indexes.
- An absent cell or inapplicable optional metadata returns `Option::None`.
  Invalid input, invalid edits, malformed files, and bounded-output rejection
  return `Result::Err`.
- Editing validation is method-specific and frozen in
  [`EDITING-CONTRACT.md`](EDITING-CONTRACT.md); rejected values do not mutate the
  retained package.

## Unsupported, warnings, and diagnostics

Unsupported deterministic formula semantics are not parse failures:
`FormulaEvaluation::Fallback` returns the cached value plus a typed
`FormulaUnsupportedReason`, including expression, operation, and formula
dependency-depth limits. Partial bounded reads are reported through
`Workbook::is_partial`/metadata and diagnostics. Machine-readable report
warnings and JSON evolution follow [`OUTPUT-CONTRACTS.md`](OUTPUT-CONTRACTS.md).

`Error` is non-exhaustive. Its `Cfb` I/O variant retains the original source;
format-validation variants contain stable typed classification plus display
context. New error variants may be added compatibly. Callers should match the
cases they can recover from and keep a wildcard fallback.

## Ownership, threading, and panic policy

`Workbook`, `Sheet`, and authored feature values own their data. Metadata,
ranges, rows, and accessors borrow whenever their lifetime is tied to a
workbook. Complete XLSX/CSV/HTML/Markdown outputs are owned in-memory buffers;
large streaming consumers should iterate `Sheet::rows`.

Core owned models, reports, errors, CSV options, borrowed ranges, and
`Spreadsheet` are `Send + Sync`; compile-time assertions protect this contract.
Immutable models and ranges are unwind-safe. Parsing untrusted bytes must return
an error rather than panic; allocation exhaustion and process-level failures are
outside Rust's recoverable panic contract. No public unsafe API exists.

## Dependency exposure and evolution

The base API exposes standard-library types only. The `chrono` and `serde`
features deliberately expose their respective types and therefore pin those
major-version compatibility boundaries. ZIP, XML, CFB, encoding, and error
implementation crates are not exposed in public signatures.

For 1.x:

1. Additive methods, trait implementations, and non-exhaustive variants are
   permitted in minor releases.
2. Existing behavior may be corrected for security, malformed input, or clear
   bugs, with release notes and regression tests.
3. Renames use a retained alias or forwarding method plus `#[deprecated]` for at
   least one minor release; removal waits for the next major release.
4. Feature removal, MSRV increases, coordinate changes, alias removal, and
   dependency major changes visible in signatures are breaking changes.
5. The planned 1.0.0 breaking-change list is currently **empty**.

## API-diff automation status

This inventory is the human-readable policy baseline. The deterministic
machine-readable snapshot is
`tests/oracles/public-api-0.1.2.json`, generated and checked by
`scripts/check_public_api.py` with pinned stable Rust 1.85.0. It records every
all-feature public path, declaration, inherent callable item, and explicit trait
implementation reachable through rustdoc's public module indexes. Compiler
auto/blanket implementations and prose are intentionally excluded.

CI runs:

```text
python3 scripts/check_public_api.py
```

The gate is deliberately conservative: any path, signature, inherent item, or
explicit trait-implementation drift requires review and an intentional baseline
refresh, including compatible additions. Its declared principal-entry-point
rustdoc inventory is `Workbook::open`, `Workbook::to_xlsx_checked`,
`Workbook::evaluate_cell`, `WorkbookReport::from_workbook`, `extract_text`,
`export_csv`, and the four public `wasm` adapters. Every applicable `Errors`,
`Panics`, and/or `Examples` contract in that ten-entry inventory is enforced;
the remaining public methods are still covered by warning-free rustdoc and the
machine-readable signature inventory, but are not claimed to have per-method
section checks. The checker uses stable rustdoc HTML because rustdoc JSON remains
unstable; pinning Rust 1.85.0 keeps the extraction format reproducible without
nightly tooling.
