# Formula support

[Back to README](../README.md)

rxls preserves formula source text and cached values where a reader can recover
them. It also provides a deterministic, bounded evaluation subset through
`Workbook::evaluate_cell`. This subset is useful for controlled pipelines; it
is not a complete Excel calculation engine.

## Version scope

The released native core is `rxls` **0.1.4** (`v0.1.4`). It provides
`Workbook::evaluate_cell`, `Workbook::evaluate_cells`,
`Spreadsheet::set_formula_cached_values`, and
`FormulaUnsupportedReason::TextLimitExceeded`, including the shared UTF-8 text
budgets described below. Use `rxls = "=0.1.4"` or a reviewed source checkout
for the batch examples. Core 0.1.3 supports single-cell evaluation but does not
provide these batch APIs or the text-limit variant.

The npm packages `rxls-wasm` (core adapter, 0.1.4) and
`@rxls/render-worker` (render/edit worker, 0.3.0) are separate distributions.
Worker 0.3.0 recalculating edits do not imply a new native-core release or expose
every Rust method as JavaScript. See [Compatibility](compatibility.md).

## Formula cells

A formula is represented as `Cell::Formula { formula, cached }`. The cached cell
is the value stored by the producing spreadsheet application. Formula source is
read on a best-effort basis across XLS, XLSX, XLSB, and ODS and is exposed
through cells and formula range APIs.

XLSX authoring and package-preserving editing accept formula text plus an
explicit cached value. rxls does not launch a spreadsheet application to
recalculate a workbook.

## Deterministic evaluation

`Workbook::evaluate_cell(sheet, row, col)` returns:

- `FormulaEvaluation::Computed(Cell)` when the value can be evaluated within
  the supported grammar and resource limits;
- `FormulaEvaluation::Fallback { cached, reason }` when exact evaluation is not
  supported, preserving the stored cached value and a typed reason.

```rust
let mut workbook = rxls::Workbook::new();
workbook
    .add_sheet("Data")
    .write_formula(0, 0, "1+1", 2.0);

match workbook.evaluate_cell("Data", 0, 0) {
    rxls::FormulaEvaluation::Computed(rxls::Cell::Number(value)) => {
        assert_eq!(value, 2.0);
    }
    other => panic!("unexpected evaluation: {other:?}"),
}
```

The evaluator handles literals, arithmetic and comparison expressions,
concatenation, bounded cell/range references, worksheet references, defined
names, and dependency evaluation. Supported functions are:

```text
SUM MIN MAX AVERAGE COUNT COUNTA PRODUCT
IF IFERROR IFNA
ROUND ROUNDUP ROUNDDOWN TRUNC ABS INT SIGN SQRT POWER MOD
LEN TRIM UPPER LOWER LEFT RIGHT MID CONCATENATE EXACT VALUE
AND OR NOT ISNA ISERROR ISNUMBER ISTEXT ISBLANK
```

Invalid argument counts produce Excel-compatible value errors where the
supported function contract defines them.

The source checkout includes aggregate coercion fixes for the next core
release. `SUM`, `AVERAGE`, `MIN`, `MAX` and `PRODUCT` ignore referenced text,
booleans and blanks, including single-cell ranges and defined references.
Direct numeric text and booleans contribute; invalid direct text returns
`#VALUE!`. `COUNT` ignores errors, and `COUNTA` distinguishes a missing cell
from empty text. `IF` retains a selected reference's origin; `IFERROR`, `IFNA`
and unary `+` produce values, converting a selected missing cell to zero.
`AND`/`OR` ignore referenced text and accept direct `TRUE`/`FALSE` strings,
omitting other text. They return `#VALUE!` when no logical/numeric input remains.
A formula returning a missing reference produces zero; directly evaluating a
missing target retains the empty-text result described below.
These scalar conditional and unary paths accept actual one-cell ranges;
selected multi-cell or whole-axis arrays return `ArraySemantics`, even when
only one value is stored. Direct bounded range aggregates remain supported.

The next-release source also rounds `ROUND`, `ROUNDUP`, `ROUNDDOWN` and `TRUNC`
using a bounded 15-significant-digit decimal coefficient before applying the
requested decimal step. Nearest ties round away from zero; `ROUNDUP` moves away
and `ROUNDDOWN`/`TRUNC` move toward zero. This removes binary scaling noise while
retaining genuine 15-digit offsets. Fractional precision truncates toward zero;
`TRUNC` defaults to zero digits. Precisions whose binary `10^digits` is infinite
or zero (including `+/-400`) return `#NUM!`, retaining the existing engine bound.
Decimal normalization or final-result overflow also returns `#NUM!` (including
rounding `f64::MAX` at zero digits); no nonfinite result is emitted.
This policy applies to rounding functions; general arithmetic and literal
parsing still use binary64, and full Excel arithmetic compensation is outside
the subset.

## Batch evaluation (core 0.1.4)

The source API has this signature (`Result` here is `std::result::Result`):

```text
Workbook::evaluate_cells(
    &self,
    targets: &[(&str, u32, u16)],
) -> Result<Vec<FormulaEvaluation>, FormulaUnsupportedReason>
```

Each target is a sheet name and zero-based row/column. At most **10,000 targets**
are accepted. Results keep input order, including repeated targets; an empty
slice returns `Ok(Vec::new())`. Targets share one semantic-operation budget,
cumulative range and text budgets, and memoized successful results across
referenced cells. They do not each receive a fresh budget. Entering each target
costs one operation, even for a memo hit, so 10,000 targets is a ceiling, not a
promise that 10,000 arbitrary formulas fit.

`Ok(results)` may mix `Computed` and `Fallback` entries. Unsupported semantics,
such as `Volatile` or `UnsupportedFunction`, retain that target's stored cached
value. A missing sheet gives a `SheetNotFound` fallback with empty text; a
missing cell on an existing sheet computes empty text. Ordinary value cells
can also be targets. Neither evaluation method modifies cells or stored caches.

The following limit reasons abort the **entire batch** with `Err(reason)` and
return no partial vector: `OperationLimitExceeded`, `TextLimitExceeded`,
`DependencyDepthExceeded`, `ExpressionTooComplex`, and `RangeTooLarge`.
Even copying a fallback's cached text can exhaust a shared budget. By contrast,
`evaluate_cell` returns `FormulaEvaluation::Fallback` for evaluation limits as
well as unsupported semantics; it has no outer batch `Result`.

This example uses the core 0.1.4 API and deliberately stores stale caches:

```rust
use rxls::{Cell, FormulaEvaluation, FormulaUnsupportedReason, Workbook};

let mut workbook = Workbook::new();
let sheet = workbook.add_sheet("Data");
sheet.write_formula(0, 0, "1+1", 0.0);
sheet.write_formula(1, 0, "A1*3", 0.0);
sheet.write_formula(2, 0, "NOW()", 123.0);

let targets = [("Data", 1, 0), ("Data", 0, 0), ("Data", 2, 0), ("Data", 1, 0)];
let results = workbook.evaluate_cells(&targets).expect("bounded batch");
assert_eq!(results.len(), targets.len());
assert_eq!(results[0], FormulaEvaluation::Computed(Cell::Number(6.0)));
assert_eq!(results[1], FormulaEvaluation::Computed(Cell::Number(2.0)));
assert_eq!(results[3], results[0]);
assert_eq!(
    results[2],
    FormulaEvaluation::Fallback {
        cached: Cell::Number(123.0),
        reason: FormulaUnsupportedReason::Volatile,
    }
);
// Evaluation does not replace the stored cache, even when it computes a value.
match workbook.sheets[0].cell(0, 0) {
    Some(Cell::Formula { cached, .. }) => assert_eq!(cached.as_ref(), &Cell::Number(0.0)),
    other => panic!("unexpected stored cell: {other:?}"),
}
```

To persist selected computed results, use the separately validated
[formula-cache update API](preservation.md#formula-cache-updates-core-014).

## Typed fallback reasons

`FormulaUnsupportedReason::code()` provides a stable machine-readable code.
This table describes core 0.1.4; `TextLimitExceeded` was added after core
0.1.3. Both enums are `#[non_exhaustive]`, so consumers
should retain a catch-all arm when matching them.

| Reason | Code | Meaning |
|---|---|---|
| `UnsupportedFunction` | `unsupported_function` | Function is outside the deterministic subset |
| `Volatile` | `volatile` | Result depends on time, randomness, environment, or recalculation state |
| `ExternalRef` | `external_reference` | Formula references another workbook |
| `CircularReference` | `circular_reference` | A dependency cycle was detected |
| `UnresolvedName` | `unresolved_name` | A name or bare identifier could not be resolved |
| `UnparsableExpression` | `unparsable_expression` | Formula text is outside the supported grammar |
| `ArraySemantics` | `array_semantics` | Array or dynamic-array behavior is required |
| `RangeTooLarge` | `range_too_large` | Traversal would exceed the bounded range limit |
| `SheetNotFound` | `sheet_not_found` | A referenced worksheet is missing |
| `ExpressionTooComplex` | `expression_too_complex` | Parser nesting exceeds its recursion bound |
| `OperationLimitExceeded` | `operation_limit_exceeded` | Evaluation exceeds the semantic work budget |
| `DependencyDepthExceeded` | `dependency_depth_exceeded` | Referenced formulas exceed the dependency-depth bound |
| `TextLimitExceeded` (core 0.1.4) | `text_limit_exceeded` | Text production exceeded 1 MiB per value or the shared 8 MiB UTF-8 budget. |

The result never substitutes a guessed value for unsupported semantics.
Callers can use the cached value, surface the reason, or require a computed
result according to their own policy.

## Resource limits

### Current source budgets

The evaluator uses the following independent ceilings. One `evaluate_cells`
call shares the cumulative budgets across all its targets and dependencies;
separate calls start fresh. `evaluate_cell` uses a fresh evaluator state when
it evaluates a formula. These are fixed implementation limits, not per-call
options.

| Resource | Ceiling | Limit reason |
|---|---:|---|
| Batch targets | 10,000 | `OperationLimitExceeded` |
| Semantic parser/evaluator work, including batch target entry | 10,000 operations | `OperationLimitExceeded` |
| Cumulative range traversal | 10,000 cells | `RangeTooLarge` |
| Cumulative sparse range scanning | 1,000,000 stored-cell entries examined | `RangeTooLarge` |
| Active formula dependency chain | 64 formula bodies | `DependencyDepthExceeded` |
| Recursive expression nesting | 128 levels | `ExpressionTooComplex` |
| One evaluator-produced/copied text value | 1,048,576 UTF-8 bytes (1 MiB) | `TextLimitExceeded` |
| Cumulative text generation/copy allowance | 8,388,608 UTF-8 bytes (8 MiB) | `TextLimitExceeded` |

Text accounting includes intermediate strings, memo storage and memo-hit
copies, text/error values, and evaluator bookkeeping strings; releasing a
string does not refund the call's allowance. A small final result can therefore
hit a text limit. Bytes are not Unicode characters or UTF-16 units. This is a
conservative work/allocation allowance, not a measurement of peak process
memory or a workbook-file limit. It also does not constrain the original cached
value returned by the single-cell fallback path. The non-formula fast path in
`evaluate_cell` clones the existing scalar directly, without these evaluator
text counters; batch targets go through the shared accounting instead.

The two text limits and the shared batch behavior were released in core 0.1.4.
Do not infer these safeguards from a dependency on registry core 0.1.3. That
release documents 10,000 range cells, 10,000 semantic operations, dependency
depth 64, and expression depth 128 for its single-cell evaluation; it has no
batch API or `TextLimitExceeded` variant. Current range counters are cumulative,
not a fresh 10,000-cell allowance for every range in a formula.

The core 0.1.4 example below checks a target-count error and a text limit.
The initial Korean string is small enough for an Excel text cell; successive
formulas double its UTF-8 content without writing the computed strings back:

```rust
use rxls::{FormulaEvaluation, FormulaUnsupportedReason, Workbook};

let mut workbook = Workbook::new();
let sheet = workbook.add_sheet("Text");
sheet.write(0, 0, "가".repeat(4_000));
for row in 1..=7 {
    sheet.write_formula(row, 0, format!("A{row}&A{row}"), "");
}

let too_many = vec![("Text", 0, 0); 10_001];
assert_eq!(
    workbook.evaluate_cells(&too_many),
    Err(FormulaUnsupportedReason::OperationLimitExceeded)
);
assert_eq!(
    workbook.evaluate_cells(&[("Text", 7, 0)]),
    Err(FormulaUnsupportedReason::TextLimitExceeded)
);
// Each value is only 12,000 UTF-8 bytes, but copies share the 8 MiB allowance.
let repeated = vec![("Text", 0, 0); 1_000];
assert_eq!(
    workbook.evaluate_cells(&repeated),
    Err(FormulaUnsupportedReason::TextLimitExceeded)
);
assert!(matches!(
    workbook.evaluate_cell("Text", 7, 0),
    FormulaEvaluation::Fallback { reason: FormulaUnsupportedReason::TextLimitExceeded, .. }
));
assert_eq!(FormulaUnsupportedReason::TextLimitExceeded.code(), "text_limit_exceeded");
```

A computed value is not automatically a valid edit value. Cache updates enforce
[separate cell-text and XML limits](preservation.md#formula-cache-updates-core-014),
including 32,767 UTF-16 units for `Cell::Text`, before changing a package.

## Current boundaries

Volatile and environment-dependent functions, external workbooks, dynamic
arrays, unsupported functions, locale-specific calendars, digit substitution,
and expressions outside the documented grammar return typed fallback reasons.
Formula source recovery from legacy or malformed records remains best effort;
the cached value is retained even when source text cannot be evaluated.

Formula parsing, evaluation, editing, and writer behavior are covered by
focused unit and integration evidence in the release workflow. See
[Validation and reproducibility](validation.md) for the release contract.
