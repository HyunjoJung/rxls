# Preservation and editing

[Back to README](../README.md)

`Spreadsheet` combines the parsed workbook with the original package bytes
needed for preservation-aware XLSX/XLSM edits. Its contract is fail-closed:
reading can be tolerant, but a mutation is accepted only when the retained
package can be saved without silently discarding unknown content.

## Version scope

The released `rxls` core **0.1.4** includes the formula-cache batch API below,
`Spreadsheet::set_formula_cached_values`, along with `Workbook::evaluate_cells`
and `FormulaUnsupportedReason::TextLimitExceeded`. These APIs are absent from
registry core 0.1.3. Use `rxls = "=0.1.4"` or a reviewed source checkout for
the batch example.
The npm `rxls-wasm` core adapter (0.1.4) and `@rxls/render-worker` (0.3.0)
have separate public interfaces and release boundaries. See
[Compatibility](compatibility.md) and [Formula support](formulas.md).

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
`Spreadsheet::transaction` exposes the same rule for caller-defined batches.
The following example works with core 0.1.3 and 0.1.4; it requires an
editable `book.xlsx` containing a worksheet named `Data`:

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

## Formula-cache updates (core 0.1.4)

This API writes caller-supplied cached results; it does not evaluate formulas:

```text
Spreadsheet::set_formula_cached_values(
    &mut self,
    updates: &[(&str, u32, u16, Cell)],
) -> rxls::Result<()>
```

A batch accepts at most **10,000 distinct existing formula cells**. Each tuple
contains a sheet name, a zero-based row/column, and a scalar cache value. Before
mutation it checks edit capability, the batch length, worksheet resolution,
unambiguous workbook sheet names (ASCII case-insensitive duplicate names are
rejected), coordinates, duplicate targets, and the original formula nodes.
Rows must be at most 1,048,575 and columns at most 16,383. A missing cell or a
cell without an existing `<f>` is rejected; this API does not create formulas.
An empty slice is a no-op only after the editable-package gate succeeds.

Caches may be `Cell::Text`, `Number`, `Date`, `Bool`, or `Error`, but not another
`Cell::Formula`. Numbers and date serials must be finite. Text caches must
contain valid XML characters and fit **32,767 UTF-16 units**; error strings are
also XML-validated. Normal XML node/attribute, package, and final-serialization
limits still apply. These checks return `rxls::Error`, not an evaluator
`FormulaUnsupportedReason`. Passing the evaluator's 1 MiB/8 MiB UTF-8 budgets
does not guarantee that a result is acceptable as an Excel cache value.

Updates use one clone-and-swap transaction. They replace the cache value and its
cell type encoding while preserving the original formula node, source text,
and shared/array attributes. This is not permission to evaluate unsupported
array semantics. The update also invalidates any existing calculation chain,
including its required relationship/content-type wiring. Untouched unrelated
parts remain byte-for-byte; the touched worksheet is reserialized. Any failed
preflight, mutation, or final serialization leaves the original retained
package and `edited_parts()` unchanged.

`spreadsheet.workbook()` is the parsed snapshot, not a live view of package
edits. **Save and reopen before evaluating after input edits**, and reopen the
final saved bytes to inspect refreshed caches. `save()` does not update the
existing parsed snapshot or write a filesystem path by itself.

This self-contained core 0.1.4 example evaluates fresh edited input, writes
only a computed result, verifies the reopened cache, and checks duplicate-target
rollback. In a mixed batch, callers can omit `Fallback` results to retain those
caches; an outer evaluation `Err` provides no partial batch to save.

```rust
use rxls::{Cell, FormulaEvaluation, Spreadsheet, Workbook};

fn main() -> rxls::Result<()> {
    let mut source = Workbook::new();
    let sheet = source.add_sheet("Data");
    sheet.write(0, 0, 2.0);
    sheet.write_formula(0, 1, "A1*3", 0.0);
    let mut spreadsheet = Spreadsheet::open(&source.to_xlsx())?;

    spreadsheet.set_cell_value("Data", 0, 0, Cell::Number(7.0))?;
    let edited_input = Workbook::open(&spreadsheet.save()?)?;
    let results = edited_input
        .evaluate_cells(&[("Data", 0, 1)])
        .expect("small, bounded evaluation");
    let value = match &results[0] {
        FormulaEvaluation::Computed(value) => value.clone(),
        other => panic!("expected computed result: {other:?}"),
    };
    spreadsheet.set_formula_cached_values(&[("Data", 0, 1, value)])?;

    // The existing parsed snapshot still contains the original cache.
    match spreadsheet.workbook().sheets[0].cell(0, 1) {
        Some(Cell::Formula { cached, .. }) => assert_eq!(cached.as_ref(), &Cell::Number(0.0)),
        other => panic!("unexpected original cell: {other:?}"),
    }
    let saved = spreadsheet.save()?;
    let reopened = Workbook::open(&saved)?;
    match reopened.sheets[0].cell(0, 1) {
        Some(Cell::Formula { formula, cached }) => {
            assert_eq!(formula, "A1*3");
            assert_eq!(cached.as_ref(), &Cell::Number(21.0));
        }
        other => panic!("unexpected saved cell: {other:?}"),
    }

    let edited_before = spreadsheet.edited_parts().to_vec();
    assert!(spreadsheet.set_formula_cached_values(&[
        ("Data", 0, 1, Cell::Number(99.0)),
        ("Data", 0, 1, Cell::Number(100.0)),
    ]).is_err());
    assert_eq!(spreadsheet.save()?, saved);
    assert_eq!(spreadsheet.edited_parts(), edited_before.as_slice());
    Ok(())
}
```

## Supported edits

The current package-preserving surface covers:

- cell values, formulas with cached values, and rectangular range updates;
- cache-only formula batches in core 0.1.4, as described above;
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

Cell replacement and clearing must cover an entire shared or array formula
group. Use `set_cell_range_values` or `clear_range` for a group spanning several
cells; a single-cell replacement is accepted only for a one-cell group.
Malformed, incomplete, ambiguous, or namespace-prefixed group metadata is
rejected before package mutation. Unknown namespace children are preserved.
Cache-only updates preserve formula nodes and may refresh individual members.

## Browser edit boundary

This is the separately versioned **`@rxls/render-worker` 0.3.0** surface, not a
claim about the synchronous `rxls-wasm` package. Its
recalculating edits combine evaluation and cache refresh into one undoable
operation. Unsupported formulas retain their caches; a resource-limit or
cache-write failure rejects the entire edit. See the
[worker README](../bindings/render-wasm/README.md) for the JavaScript methods.

`@rxls/render-worker` owns the retained `Spreadsheet` and every edit snapshot
inside one dedicated worker. The public protocol exposes cell inspection,
typed cell/formula replacement, recalculating cell/range edits, complete
document-property replacement, undo/redo, and serialization. Each mutation edits a clone, serializes and
reopens it, checks the same 32 MiB workbook limit, and commits the candidate
only after every step succeeds. History is limited to 20 entries and 32 MiB in
total; older snapshots are discarded deterministically.

The public viewer downloads a new `.xlsx` or `.xlsm` rather than overwriting
the selected local file. Its pinned Apache POI XLSM fixture contains a real OLE
compound VBA project. The browser test checks VBA, content types, workbook
relationships, styles, and theme parts byte-for-byte before and after an edit,
then reopens the result with `openpyxl 3.1.5`. The same UI presents XLS, XLSB,
and ODS with their typed read-only reason and does not offer a conversion path.

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
