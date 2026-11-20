//! Optional exact _xlfn.IFNA function-call spelling regressions.

use rxls::{Cell, FormulaEvaluation, FormulaUnsupportedReason, Workbook};

#[test]
fn exact_stored_ifna_alias_uses_existing_scalar_reference_and_arity_contracts() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Data");
    sheet.write(0, 0, Cell::Error("#N/A".into()));
    sheet.write(1, 0, 3.0);
    let cases = [
        ("#N/A,7", FormulaEvaluation::Computed(Cell::Number(7.0))),
        (
            "#DIV/0!,7",
            FormulaEvaluation::Computed(Cell::Error("#DIV/0!".into())),
        ),
        (
            r#""@[];",7"#,
            FormulaEvaluation::Computed(Cell::Text("@[];".into())),
        ),
        ("A1,7", FormulaEvaluation::Computed(Cell::Number(7.0))),
        ("A2,7", FormulaEvaluation::Computed(Cell::Number(3.0))),
        ("A3,7", FormulaEvaluation::Computed(Cell::Number(0.0))),
        ("A2:A2,7", FormulaEvaluation::Computed(Cell::Number(3.0))),
        ("#N/A,A2", FormulaEvaluation::Computed(Cell::Number(3.0))),
        ("#N/A,A3", FormulaEvaluation::Computed(Cell::Number(0.0))),
        ("#N/A,A2:A2", FormulaEvaluation::Computed(Cell::Number(3.0))),
        (
            "",
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into())),
        ),
        (
            "1",
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into())),
        ),
        (
            "1,2,3",
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into())),
        ),
        (
            "A1:A2,7",
            FormulaEvaluation::Fallback {
                cached: Cell::Text("sentinel-cache".into()),
                reason: FormulaUnsupportedReason::ArraySemantics,
            },
        ),
    ];
    for (row, (args, _)) in cases.iter().enumerate() {
        for (col, function) in ["IFNA", "_xlfn.IFNA", "_XlFn.iFnA"].iter().enumerate() {
            sheet.write_formula(
                row as u32,
                col as u16 + 1,
                format!("{function}({args})"),
                "sentinel-cache",
            );
        }
    }
    for (row, (_, expected)) in cases.iter().enumerate() {
        for col in 1..=3 {
            let original = workbook.sheets[0].cell(row as u32, col).cloned();
            assert_eq!(workbook.evaluate_cell("Data", row as u32, col), *expected);
            assert_eq!(workbook.sheets[0].cell(row as u32, col).cloned(), original);
        }
    }
}

#[test]
fn ifna_spelling_is_not_rewritten_as_text_name_or_sheet() {
    let mut workbook = Workbook::new();
    workbook.define_name("_xlfn.IFNA", r#""name@[]{}""#);
    workbook.add_sheet("_xlfn.IFNA").write(0, 0, 9.0);
    let sheet = workbook.add_sheet("Data");
    sheet.write_formula(0, 0, r#""_xlfn.IFNA""#, "literal-cache");
    sheet.write_formula(0, 1, "_xlfn.IFNA", "name-cache");
    sheet.write_formula(0, 2, "_xlfn.IFNA!A1", "sheet-cache");
    let expected = [
        Cell::Text("_xlfn.IFNA".into()),
        Cell::Text("name@[]{}".into()),
        Cell::Number(9.0),
    ];
    for (col, expected) in expected.into_iter().enumerate() {
        let original = workbook.sheets[1].cell(0, col as u16).cloned();
        assert_eq!(
            workbook.evaluate_cell("Data", 0, col as u16),
            FormulaEvaluation::Computed(expected)
        );
        assert_eq!(workbook.sheets[1].cell(0, col as u16).cloned(), original);
    }
}
