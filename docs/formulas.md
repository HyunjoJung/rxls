# Formula support

[Back to README](../README.md)

rxls preserves formula source text and cached values where a reader can recover
them. It also provides a deterministic, bounded evaluation subset through
`Workbook::evaluate_cell`. This subset is useful for controlled pipelines; it
is not a complete Excel calculation engine.

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

## Typed fallback reasons

`FormulaUnsupportedReason::code()` provides a stable machine-readable code:

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

The result never substitutes a guessed value for unsupported semantics.
Callers can use the cached value, surface the reason, or require a computed
result according to their own policy.

## Resource limits

One top-level evaluation is bounded to:

- 10,000 cells traversed through a range;
- 10,000 semantic parser/evaluator work units;
- 64 formula bodies along a dependency chain;
- 128 recursive expression levels.

These limits are independent so a compact expression, a large range, and a long
dependency chain cannot consume each other's budgets. Circular references and
missing sheets are detected explicitly.

## Current boundaries

Volatile and environment-dependent functions, external workbooks, dynamic
arrays, unsupported functions, locale-specific calendars, digit substitution,
and expressions outside the documented grammar return typed fallback reasons.
Formula source recovery from legacy or malformed records remains best effort;
the cached value is retained even when source text cannot be evaluated.

Formula parsing, evaluation, editing, and writer behavior are covered by
focused unit and integration evidence in the release workflow. See
[Validation and reproducibility](validation.md) for the release contract.
