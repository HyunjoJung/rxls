# Preservation and editing

[Back to README](../README.md)

`Spreadsheet` combines the parsed workbook with the original package bytes
needed for preservation-aware XLSX/XLSM edits. Its contract is fail-closed:
reading can be tolerant, but a mutation is accepted only when the retained
package can be saved without silently discarding unknown content.

## Edit capability

| Input | Capability |
|---|---|
| Complete XLSX/XLSM package with lossless metadata | `EditCapability::ReadWrite` |
| XLS | `ReadOnly(EditReadOnlyReason::LegacyBiff)` |
| XLSB | `ReadOnly(EditReadOnlyReason::BinaryPackage)` |
| ODS | `ReadOnly(EditReadOnlyReason::OpenDocument)` |
| Incomplete or metadata-lossy OOXML package | `ReadOnly(EditReadOnlyReason::PackageMetadataLoss)` |

Call `edit_capability()` before presenting an edit workflow. A read-only
`Spreadsheet` still exposes `workbook()`. A no-op save of a retained OOXML
package does not imply that mutations are safe; every mutating method checks
capability before touching package state.

## Preservation contract

OOXML parts start as retained raw bytes. Only a part that an edit promotes and
changes is serialized again. Every other declared part round-trips
byte-for-byte. `edited_parts()` returns changed part names in deterministic
order so callers can audit the save.

For XLSM, untouched VBA content, macro content types, and relationships are
retained. Editing does not execute VBA or external relationships.

A save can update the minimum dependent parts required by the requested
operation. For example, a formula edit invalidates the calculation chain, and
a sheet lifecycle operation may update workbook metadata, relationships, and
content types together. This is an explicit coordinated edit, not an
unannounced package rewrite.

## Atomic updates

Individual operations that touch several parts use clone-and-swap mutation.
`Spreadsheet::transaction` exposes the same rule for caller-defined batches:

```rust
use rxls::{Cell, Spreadsheet};

let bytes = std::fs::read("book.xlsx")?;
let mut spreadsheet = Spreadsheet::open(&bytes)?;

spreadsheet.transaction(|candidate| {
    candidate.set_cell_value("Data", 0, 0, Cell::Text("Approved".into()))?;
    candidate.set_cell_formula("Data", 0, 1, "SUM(B2:B10)", 0.0)?;
    Ok(())
})?;

std::fs::write("book-edited.xlsx", spreadsheet.save()?)?;
```

The closure operates on an isolated clone. The clone is serialized and
validated before it replaces the original. If the closure or final
serialization fails, the workbook, retained package bytes, and edited-part list
remain unchanged. The transaction is in memory; the caller chooses how to
persist the committed bytes.

## Supported edits

The current package-preserving surface covers:

- cell values, formulas with cached values, and rectangular range updates;
- document properties, defined names, active sheet, and calculation metadata;
- sheet add, rename, delete, visibility, active-sheet, and tab-color operations;
- row heights, column widths and visibility, panes, view state, page setup, and
  print areas;
- merged ranges, legacy comments/notes, and hyperlinks;
- exact-range data-validation create/update/delete operations;
- safe bottom-row resizing of an existing worksheet table.

Coordinate limits follow XLSX: rows are zero-based through 1,048,575 and columns
through 16,383. Public methods return typed errors for invalid sheets,
coordinates, relationships, or unsupported package state.

## Explicit non-goals

- XLS, XLSB, and ODS mutation or conversion through `Spreadsheet`.
- Creating a new XLSM or adding a VBA project.
- Inserting or deleting worksheet rows or columns.
- Guessing how to repair formulas, names, tables, charts, drawings, or other
  structural dependencies after an unsafe shape change.
- Editing a package whose content types or relationships could not be retained
  losslessly.
- Executing macros, external links, or embedded objects.

These boundaries prevent an apparently successful save from becoming a lossy
rewrite. Open a feature request with a representative public fixture when an
additional operation can be implemented with a bounded dependency update and a
testable preservation contract.
