//! Scalar ISNUMBER/ISBLANK correctness follow-up to the W20 native cohort.

use rxls::{Cell, FormulaEvaluation, Workbook};

fn assert_cases(cases: &[(&str, bool)]) {
    let mut workbook = Workbook::new();
    let inputs = workbook.add_sheet("Inputs");
    inputs.write(0, 0, "3");
    inputs.write_formula(1, 0, r#""3""#, 123.0);
    inputs.write(2, 0, 3.0);
    inputs.write(3, 0, true);
    inputs.write(4, 0, "");
    inputs.write_formula(5, 0, r#""""#, "stale-nonempty-cache");
    // A7 is deliberately absent, rather than a present empty-text cell.
    inputs.write(7, 0, Cell::Error("#N/A".into()));
    inputs.write(8, 0, Cell::Error("#DIV/0!".into()));
    inputs.write(9, 0, Cell::Date(3.0));
    workbook.define_name("NumericText", "Inputs!A1");
    workbook.define_name("EmptyFormula", "Inputs!A6");
    workbook.define_name("Missing", "Inputs!A7");
    workbook.define_name("EmptyLiteral", r#""""#);
    let results = workbook.add_sheet("Results");
    for (row, (formula, _)) in cases.iter().enumerate() {
        results.write_formula(row as u32, 0, formula, "sentinel-cache");
    }
    let original = workbook
        .sheets
        .iter()
        .map(|sheet| {
            sheet
                .cells()
                .map(|(row, col, cell)| (row, col, cell.clone()))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    for (row, (formula, expected)) in cases.iter().enumerate() {
        let expected = FormulaEvaluation::Computed(Cell::Bool(*expected));
        assert_eq!(
            workbook.evaluate_cell("Results", row as u32, 0),
            expected,
            "{formula}"
        );
        assert_eq!(
            workbook.evaluate_cells(&[("Results", row as u32, 0), ("Results", row as u32, 0)]),
            Ok(vec![expected.clone(), expected]),
            "batch/memo {formula}"
        );
    }
    assert_eq!(
        workbook
            .sheets
            .iter()
            .map(|sheet| {
                sheet
                    .cells()
                    .map(|(row, col, cell)| (row, col, cell.clone()))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>(),
        original
    );
}

#[test]
fn isnumber_inspects_type_without_converting_numeric_text_or_logicals() {
    assert_cases(&[
        (r#"ISNUMBER("3")"#, false),
        ("ISNUMBER(Inputs!A1)", false),
        ("ISNUMBER(Inputs!A2)", false),
        ("ISNUMBER(NumericText)", false),
        ("ISNUMBER(Inputs!A3)", true),
        ("ISNUMBER(Inputs!A4)", false),
        ("ISNUMBER(Inputs!A7)", false),
        ("ISNUMBER(Inputs!A10)", true),
        ("ISNUMBER(TRUE)", false),
        (r#"ISNUMBER("")"#, false),
        ("ISNUMBER(1/0)", false),
        (r#"ISNUMBER(VALUE("3"))"#, true),
        (r#"ISNUMBER("3"+0)"#, true),
        (r#"ISNUMBER(+"3")"#, false),
        (r#"ISNUMBER(-"3")"#, true),
    ]);
}

#[test]
fn isblank_distinguishes_missing_cells_from_empty_literals_and_formula_results() {
    assert_cases(&[
        ("ISBLANK(Inputs!A7)", true),
        ("ISBLANK(Missing)", true),
        ("ISBLANK(Inputs!A5)", false),
        ("ISBLANK(Inputs!A6)", false),
        ("ISBLANK(EmptyFormula)", false),
        ("ISBLANK(EmptyLiteral)", false),
        (r#"ISBLANK("")"#, false),
        (r#"ISBLANK(LEFT("x",0))"#, false),
        ("ISBLANK(0)", false),
        ("ISBLANK(FALSE)", false),
        ("ISBLANK(1/0)", false),
    ]);
}

#[test]
fn selected_references_and_value_producing_operations_keep_blank_and_text_contracts() {
    assert_cases(&[
        ("ISBLANK(IF(TRUE,Inputs!A7,1))", true),
        ("ISBLANK(IF(FALSE,1,Inputs!A7))", true),
        ("ISBLANK(IFERROR(Inputs!A7,1))", false),
        ("ISBLANK(IFNA(Inputs!A7,1))", false),
        ("ISBLANK(+Inputs!A7)", false),
        ("ISNUMBER(+Inputs!A7)", true),
        ("ISNUMBER(IFERROR(Inputs!A7,1))", true),
        ("ISNUMBER(IF(TRUE,Inputs!A1,1))", false),
        ("ISNUMBER(IFERROR(Inputs!A1,1))", false),
        ("ISNUMBER(+Inputs!A1)", false),
        ("ISNUMBER(Inputs!A1+0)", true),
        ("ISBLANK(IF(TRUE,Inputs!A6,1))", false),
        (r#"ISBLANK(IF(TRUE,"",1))"#, false),
    ]);
}

#[test]
fn authored_nonfinite_numbers_keep_existing_isnumber_type_boundary() {
    for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, number);
        sheet.write_formula(0, 1, "ISNUMBER(A1)", "sentinel-cache");
        let original = sheet.cell(0, 1).cloned();
        assert_eq!(
            workbook.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(workbook.sheets[0].cell(0, 1).cloned(), original);
        match workbook.sheets[0].cell(0, 0).unwrap() {
            Cell::Number(value) => assert_eq!(value.to_bits(), number.to_bits()),
            other => panic!("authored number type changed: {other:?}"),
        }
    }
}
