# Package-preserving editing contract

This document freezes the supported `Spreadsheet` editing surface for rxls
0.1.2 and the intended compatibility baseline for 1.x. The contract is
deliberately narrower than arbitrary Excel authoring: rxls edits only package
structures it can validate and preserve without guessing.

The API is available with the `xlsx` feature. Public row and column coordinates
are zero-based, ranges are inclusive, and all coordinates must fit Excel's
1,048,576-row by 16,384-column grid.

## Format and state model

`Spreadsheet::open` parses all enabled reader formats, but package-preserving
edits are supported only for complete, non-lossy `.xlsx` and `.xlsm` OOXML
packages. `edit_capability` reports `EditCapability::ReadWrite` or a typed
`EditCapability::ReadOnly(EditReadOnlyReason)`:

- `.xls`: `LegacyBiff`
- `.xlsb`: `BinaryPackage`
- `.ods`: `OpenDocument`
- incomplete or metadata-lossy OOXML: `PackageMetadataLoss`

Read-only inputs remain readable through `workbook`, but mutating methods fail
before editing a package part. `save` is available for a retained OOXML package;
it is not a format converter for `.xls`, `.xlsb`, or `.ods`.

The value returned by `workbook` is the parsed view captured at open time. It is
not incrementally rewritten after package edits. Reopen the bytes from `save`
to inspect edited values and metadata through the read APIs. `edited_parts`
returns the deterministic list of package parts touched or removed since open;
it is diagnostic metadata, not a semantic diff.

## Atomicity

`transaction` is the public whole-object atomicity boundary. The closure edits
an isolated clone, then serializes and validates it before commit. If the
closure or validation fails, the original `Spreadsheet`, retained package, and
`edited_parts` list remain unchanged. Use one transaction whenever several
calls must succeed or fail as a unit.

The following individual methods also use the same clone, serialize, validate,
and swap path because they coordinate package parts or structural metadata:

- `set_document_properties`
- `rename_sheet`, `add_sheet`, and `delete_sheet`
- `merge_cells` and `unmerge_cells`
- `set_row_height`, `set_row_hidden`, `set_column_width`, and
  `set_column_hidden`
- `set_freeze_panes`, `clear_freeze_panes`, and `set_print_area`
- `set_comment` and `delete_comment`
- `set_external_hyperlink`, `set_internal_hyperlink`, and `delete_hyperlink`
- `set_data_validation`, `delete_data_validation`, and `set_table_range`

`set_cell_value`, `set_cell_formula`, `append_row`, `clear_range`,
`set_defined_name`, `set_sheet_visibility`, `set_active_sheet`, and
`set_sheet_tab_color` edit the retained package directly and do not create a
whole-`Spreadsheet` clone themselves. Their documented input and target checks
still apply, but only `transaction` provides the contract that every possible
failure leaves the exact whole object unchanged. Wrap these calls in a
transaction whenever exact rollback is required.

`save` serializes and validates the complete in-memory candidate before
returning bytes. `save_to_path` serializes first, then creates a unique sibling
temporary file, writes and `fsync`s it, and atomically renames it over the
destination. A pre-rename failure removes the temporary file and leaves an
existing destination intact. Filesystem durability and rename semantics still
depend on the host filesystem and operating system.

## Preservation and deterministic output

- Unedited ZIP parts, including VBA, images, charts, pivot content, custom XML,
  unknown relationships, and signature-adjacent content, retain their original
  bytes where the package declares them safely preservable.
- An edited XML part is parsed and serialized through the editing tree. Unknown
  attributes and child elements in supported target structures are preserved
  where safe, but byte identity is not promised for an edited part.
- Existing part names and relationship identifiers are not renumbered. New part
  names, relationship identifiers, content-type entries, and ZIP entries are
  allocated deterministically.
- `.xlsm` content types and VBA payloads are retained. rxls does not edit VBA.
- Cell and formula edits invalidate the calculation chain instead of retaining
  stale dependency metadata. New or changed text cells use inline strings and
  do not grow the shared string table.
- An operation is rejected when preserving or repairing a dependency would
  require inference beyond the supported OOXML structures.

## Supported operations

### Lifecycle, inspection, and persistence

| Method | Contract |
| --- | --- |
| `open` | Opens enabled spreadsheet formats for reading and retains editable OOXML packages when preservation metadata is complete. |
| `workbook` | Returns the immutable parsed-at-open view; reopen saved bytes to observe edits. |
| `edit_capability` | Reports the typed read/write capability before an edit is attempted. |
| `edited_parts` | Returns touched or removed package part names in deterministic order. |
| `transaction` | Provides clone-and-swap batch atomicity with pre-commit serialization and validation. |
| `save` | Returns a validated OOXML package as bytes; it does not convert read-only formats. |
| `save_to_path` | Persists through a serialized sibling temporary file and atomic rename. |

### Cells and ranges

| Method | Supported behavior and rejection boundary |
| --- | --- |
| `set_cell_value` | Creates or replaces one in-grid cell using a `Cell` value. Text is stored inline. Rejects out-of-grid coordinates, non-finite numbers/dates, nested formula caches, illegal XML text, and text over 32,767 UTF-16 units; use `clear_range` to remove cells. |
| `set_cell_formula` | Stores a formula without a leading `=` plus the caller-supplied cached value. It does not calculate the formula and applies the same value/XML/cache validation as `set_cell_value`. |
| `append_row` | Appends after the worksheet's highest materialized row and returns the zero-based row index. Rejects more than 16,384 values, a worksheet already beyond the last Excel row, or any value rejected by `set_cell_value`. |
| `clear_range` | Clears every cell in an inclusive rectangle. Reversed endpoints are normalized. Rejects an out-of-grid range or an area greater than 10,000 cells. |

These methods edit cells, not worksheet structure. They do not insert or delete
rows or columns, move surrounding cells, expand tables, or rewrite references
as if a structural insertion had occurred.

### Workbook and worksheet metadata

| Method | Supported behavior and rejection boundary |
| --- | --- |
| `set_document_properties` | Writes modeled core properties whose values are `Some`, removes modeled properties whose values are `None`, and updates the extended company property when that part exists. Illegal XML text and invalid W3CDTF timestamps are rejected before mutation. Unmodeled content is preserved. The multi-part update is atomic. |
| `set_defined_name` | Creates or replaces one workbook-global name. Sheet-local and built-in `_xlnm.*` names are not editable; invalid Excel names, case-insensitive collisions, malformed existing names, and illegal XML formula text are rejected. |
| `set_sheet_visibility` | Sets `Visible`, `Hidden`, or `VeryHidden`. Rejects hiding the last visible sheet. |
| `set_active_sheet` | Selects an existing sheet by name as the active tab. Missing sheets are rejected. |
| `set_sheet_tab_color` | Creates or replaces the target worksheet's tab color using the public `Color` model. Invalid or missing targets are rejected. |

### Sheet structure, layout, and print area

| Method | Supported behavior and rejection boundary |
| --- | --- |
| `rename_sheet` | Renames one worksheet and rewrites supported direct sheet-qualified references in workbook/worksheet formulas, names, print names, charts and other known formula-bearing parts, internal hyperlink locations, and pivot-cache worksheet sources. Invalid, duplicate, malformed, or unsafe targets are rejected. External-workbook qualifiers are left unchanged. |
| `add_sheet` | Appends an empty worksheet with deterministic sheet, relationship, and part identifiers. Sheet names must be valid and unique case-insensitively. Existing parts are not renumbered. |
| `delete_sheet` | Deletes one worksheet, repairs active-tab and local-name indexes, removes owned local names, changes supported direct references to the deleted sheet to `#REF!`, repairs extended-properties sheet lists, and garbage-collects exclusively owned known dependencies. It rejects deletion of the last worksheet or last visible worksheet and any ambiguous or unsupported structural dependency. |
| `merge_cells` | Adds one ordered, in-grid, inclusive merged rectangle containing at least two cells. Any overlap with an existing merge is rejected. |
| `unmerge_cells` | Removes one exact ordered, in-grid merged rectangle. Missing or partial-overlap targets are rejected. |
| `set_row_height` | Sets an explicit height for one row in points. Values must be finite and between 0 and 409.5 inclusive. It does not insert a row. |
| `set_row_hidden` | Hides or unhides one row without shifting cells or references. |
| `set_column_width` | Sets an explicit width for one column in character units. Values must be finite and between 0 and 255 inclusive. It does not insert a column. |
| `set_column_hidden` | Hides or unhides one column without shifting cells or references. |
| `set_freeze_panes` | Freezes rows above `row` and columns left of `col`; `(0, 0)` clears the freeze. Calling it intentionally replaces any existing pane state, including a split pane; creating or reconstructing split panes is not supported. |
| `clear_freeze_panes` | Removes the supported frozen-pane record; an already-unfrozen sheet is a no-op. |
| `set_print_area` | Creates, replaces, or clears the sheet-local `_xlnm.Print_Area` for one ordered, inclusive range. It does not edit print titles, page setup, margins, headers, or footers. |

Rename and delete repair only references in explicitly recognized structures.
Unknown formula payloads are preserved rather than heuristically rewritten.
Deleting a sheet rejects known dependency graphs that require unsupported
repair, including pivot table/cache graphs with worksheet sources, query
tables, OLE objects and controls, threaded-comment/person graphs,
slicer/timeline caches, connections/external links, non-external hyperlink
relationships, and malformed or ambiguous relationship or extended-properties
metadata. An unknown dependency target may remain preserved and orphaned when
its semantics cannot be identified safely; rxls does not delete it speculatively.

### Notes, hyperlinks, validations, and tables

| Method | Supported behavior and rejection boundary |
| --- | --- |
| `set_comment` | Creates or replaces one legacy cell comment, shown by modern Excel as a note, together with its comments and VML structures. Text and optional author must be valid XML text. Threaded comments are not created or edited. |
| `delete_comment` | Deletes one exact legacy note and removes now-unused owned comments/VML parts. Missing notes, duplicate note records, missing package parts, malformed VML, nested/grouped note shapes, or conflicting legacy-drawing relationships are rejected. |
| `set_external_hyperlink` | Creates or replaces one cell hyperlink backed by an external relationship. A shared relationship is split when retargeting only one cell. Empty/invalid targets, range hyperlinks, duplicate records, or malformed/ambiguous relationship identifiers are rejected. |
| `set_internal_hyperlink` | Creates or replaces one cell hyperlink with an internal workbook location. It follows the same exact-cell and ambiguity rules as external hyperlinks. |
| `delete_hyperlink` | Removes one exact external or internal cell hyperlink and removes an unshared owned relationship. Missing or ambiguous targets are rejected. |
| `set_data_validation` | Creates or replaces one modeled validation identified by an exact single inclusive `sqref`. Model rules are validated; unknown attributes and child elements on an updated record are retained. Overlapping, duplicate, malformed, or space-separated multi-range records are rejected. |
| `delete_data_validation` | Deletes the validation at one exact single inclusive range. It never edits one token inside an ambiguous multi-range `sqref`; missing, overlapping, or multi-range targets are rejected. |
| `set_table_range` | Resizes the bottom row of exactly one existing table. The header row, first/last column, width, column definitions, and existing totals-row tail must remain compatible; the table and `autoFilter` ranges are updated together. Table creation/deletion, header movement, width changes, overlap, active `sortState`, headerless tables, and malformed/ambiguous table relationships or counts are rejected. |

Legacy notes are the complete comment-writing scope for 0.1.2. Existing
threaded comments and people data are preserved when untouched, but no public
operation creates, edits, converts, or deletes them.

## Explicit exclusions

The following are intentionally outside the 0.1.2 editing contract:

- row insertion, row deletion, column insertion, and column deletion;
- shifting cells, formulas, names, merges, validations, drawings, charts,
  tables, pivots, or other dependencies as a structural side effect;
- table creation/deletion, changing table columns or headers, and arbitrary
  table moves;
- threaded-comment/person editing or legacy-note/threaded-comment conversion;
- pivot, query, slicer, timeline, connection, external-link, OLE, control,
  drawing, chart, image, macro, signature, and custom-XML authoring;
- heuristic rewriting or deletion of unknown or ambiguous structural
  dependencies;
- general page-layout authoring beyond the local print area and supported
  frozen-pane state.

These exclusions are safety boundaries, not silent best-effort behavior. An
operation touching one of them either leaves that content untouched or returns
an error before committing a structural change. The explicit replacement of an
existing pane by `set_freeze_panes` is the documented exception: the caller has
requested a new frozen-pane state.

## Verification status

Local regression coverage exercises open/edit/save/reopen behavior, exact
transaction rollback, deterministic output, reference and local-name repair,
merge overlap rules, data-validation and table boundaries, legacy note/VML and
hyperlink relationships, layout edits, macro retention, and byte preservation
for declared untouched parts. One combined unrelated-edit regression retains
VBA, image, chart, pivot, custom-XML, signature-adjacent, and unknown-
relationship parts byte-for-byte in the same package.

Invalid cells, formula caches, defined names, document-property XML, and W3CDTF
timestamps have exact no-mutation rejection regressions. The local candidate
has also passed independent open/save/reopen smoke tests with the explicitly
captured `LibreOffice 26.2.3.2` binary for an authored `.xlsx`, a
package-preserving `.xlsx` edit, and a package-preserving `.xlsm` edit. The
XLSX reports contain no diagnostic warnings. The XLSM report must contain
exactly `MacrosPresentNotExecuted`: that warning proves the VBA project remains
present and is expected because rxls never executes macros.

The release workflow records `soffice --version` and both conversion logs. Its
XLSM verifier requires the original and rxls-edited `xl/vbaProject.bin` bytes to
match exactly, requires the macro workbook and VBA content types before and
after the independent LibreOffice save, requires VBA to remain present after
that save, and accepts only the expected macro warning. The hosted runner's
captured version is authoritative for hosted evidence; no fixed hosted
LibreOffice version is claimed.

Microsoft Excel desktop validation has not been claimed. The completed hosted
and publication gates are recorded in [`ROADMAP-0.1.2.md`](ROADMAP-0.1.2.md).
