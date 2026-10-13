//! Public formula literal/reference coercion regressions for #133.

use rxls::{Cell, FormulaEvaluation, FormulaUnsupportedReason, Workbook};

fn workbook(formula: &str) -> Workbook {
    let mut workbook = Workbook::new();
    let data = workbook.add_sheet("Inputs");
    data.write(0, 0, "2");
    data.write(0, 1, 2.0);
    data.write(0, 2, true);
    // D1 is a true missing cell, not an empty string.
    data.write(0, 4, Cell::Error("#N/A".into()));
    data.write_formula(0, 5, r#""""#, "sentinel");
    data.write_formula(0, 6, r#""2""#, "sentinel");
    data.write_formula(0, 7, "TRUE", false);
    workbook
        .add_sheet("Results")
        .write_formula(0, 0, formula, 999.0);
    workbook.define_name("NamedCell", "Inputs!$A$1");
    workbook.define_name("NamedRange", "Inputs!$A$1:$A$1");
    workbook.define_name("NamedConstant", r#""2""#);
    workbook.define_name("NamedAlias", "NamedCell");
    workbook.define_name("Choice", "Inputs!$A$1");
    workbook.define_local_name("Results", "Choice", "Inputs!$B$1");
    workbook
}

fn assert_computed(formula: &str, expected: Cell) {
    let workbook = workbook(formula);
    let source = workbook.sheets[1].cell(0, 0).cloned();
    let result = FormulaEvaluation::Computed(expected);
    assert_eq!(workbook.evaluate_cell("Results", 0, 0), result, "{formula}");
    assert_eq!(
        workbook
            .evaluate_cells(&[("Results", 0, 0), ("Results", 0, 0)])
            .unwrap(),
        vec![result.clone(), result],
        "batch/memo {formula}"
    );
    assert_eq!(workbook.sheets[1].cell(0, 0).cloned(), source);
}

fn no_number(function: &str) -> Cell {
    if function == "AVERAGE" {
        Cell::Error("#DIV/0!".into())
    } else {
        Cell::Number(0.0)
    }
}

#[test]
fn single_cell_and_one_cell_ranges_share_numeric_aggregate_reference_rules() {
    for function in ["SUM", "AVERAGE", "MIN", "MAX", "PRODUCT", "COUNT"] {
        for argument in [
            "Inputs!A1",
            "Inputs!A1:A1",
            "Inputs!$A$1",
            "Inputs!C1",
            "Inputs!C1:C1",
            "Inputs!G1",
            "Inputs!H1",
            "Inputs!D1",
            "Inputs!F1",
        ] {
            assert_computed(&format!("{function}({argument})"), no_number(function));
        }
        assert_computed(
            &format!("{function}(Inputs!B1)"),
            Cell::Number(if function == "COUNT" { 1.0 } else { 2.0 }),
        );
        assert_computed(
            &format!(r#"{function}("2")"#),
            Cell::Number(if function == "COUNT" { 1.0 } else { 2.0 }),
        );
        assert_computed(&format!("{function}(TRUE)"), Cell::Number(1.0));
    }
}

#[test]
fn invalid_direct_text_is_a_value_error_for_numeric_reducers() {
    for function in ["SUM", "AVERAGE", "MIN", "MAX", "PRODUCT"] {
        for argument in [
            r#""foo""#,
            r#""""#,
            r#"" ""#,
            r#""NaN""#,
            r#""inf""#,
            r#""1e9999""#,
        ] {
            assert_computed(
                &format!("{function}(1,{argument})"),
                Cell::Error("#VALUE!".into()),
            );
        }
    }
}

#[test]
fn count_ignores_errors_and_counta_distinguishes_blank_from_empty_text() {
    for argument in [
        "1/0",
        "#N/A",
        "Inputs!E1",
        "Inputs!E1:E1",
        r#""foo""#,
        r#""NaN""#,
        r#""1e9999""#,
    ] {
        assert_computed(&format!("COUNT({argument})"), Cell::Number(0.0));
        assert_computed(&format!("COUNTA({argument})"), Cell::Number(1.0));
    }
    assert_computed("COUNTA(Inputs!D1)", Cell::Number(0.0));
    assert_computed("COUNTA(Inputs!F1)", Cell::Number(1.0));
    assert_computed("COUNTA(Inputs!A1:H1)", Cell::Number(7.0));
    assert_computed("COUNT(Inputs!A1:H1)", Cell::Number(1.0));
    assert_computed(r#"COUNT(" 2 ")"#, Cell::Number(1.0));
    for function in ["SUM", "AVERAGE", "MIN", "MAX", "PRODUCT"] {
        assert_computed(
            &format!("{function}(Inputs!E1)"),
            Cell::Error("#N/A".into()),
        );
        assert_computed(
            &format!("{function}(Inputs!A1:H1)"),
            Cell::Error("#N/A".into()),
        );
    }
}

#[test]
fn defined_names_retain_reference_or_literal_origin() {
    for function in ["SUM", "AVERAGE", "MIN", "MAX", "PRODUCT", "COUNT"] {
        for argument in ["NamedCell", "NamedRange", "NamedAlias"] {
            assert_computed(&format!("{function}({argument})"), no_number(function));
        }
        for argument in ["NamedConstant", "Choice"] {
            assert_computed(
                &format!("{function}({argument})"),
                Cell::Number(if function == "COUNT" { 1.0 } else { 2.0 }),
            );
        }
    }
}

#[test]
fn logical_aggregates_ignore_reference_text_and_reject_direct_numeric_text() {
    for function in ["AND", "OR"] {
        for argument in [
            "Inputs!A1",
            "Inputs!A1:A1",
            "Inputs!D1",
            r#""2""#,
            r#""""#,
            r#"" ""#,
        ] {
            assert_computed(
                &format!("{function}({argument})"),
                Cell::Error("#VALUE!".into()),
            );
        }
        assert_computed(&format!("{function}(Inputs!C1)"), Cell::Bool(true));
        assert_computed(&format!(r#"{function}("TRUE")"#), Cell::Bool(true));
        assert_computed(&format!(r#"{function}("FALSE")"#), Cell::Bool(false));
        assert_computed(
            &format!("{function}(Inputs!E1)"),
            Cell::Error("#N/A".into()),
        );
        assert_computed(&format!("{function}(TRUE,Inputs!A1)"), Cell::Bool(true));
    }
}

#[test]
fn scalar_consumers_and_selected_branch_provenance_remain_coherent() {
    for (formula, expected) in [
        ("Inputs!A1", Cell::Text("2".into())),
        ("Inputs!A1+1", Cell::Number(3.0)),
        (r#"Inputs!A1&"x""#, Cell::Text("2x".into())),
        (r#"Inputs!A1="2""#, Cell::Bool(true)),
        ("LEN(Inputs!A1)", Cell::Number(1.0)),
        ("ISTEXT(Inputs!A1)", Cell::Bool(true)),
        ("ISNA(Inputs!E1)", Cell::Bool(true)),
        ("ISERROR(Inputs!E1)", Cell::Bool(true)),
        ("ISBLANK(Inputs!D1)", Cell::Bool(true)),
        ("IFERROR(Inputs!E1,7)", Cell::Number(7.0)),
        ("SUM((Inputs!A1))", Cell::Number(0.0)),
        ("SUM(Inputs!A1+0)", Cell::Number(2.0)),
        ("SUM(+Inputs!A1)", Cell::Number(2.0)),
        ("+Inputs!A1", Cell::Text("2".into())),
        ("COUNT(+Inputs!A1)", Cell::Number(1.0)),
        ("SUM(IF(TRUE,Inputs!A1,7))", Cell::Number(0.0)),
        ("COUNT(IF(TRUE,Inputs!D1,7))", Cell::Number(0.0)),
        ("SUM(IFERROR(Inputs!A1,7))", Cell::Number(2.0)),
        ("SUM(IFNA(Inputs!A1,7))", Cell::Number(2.0)),
        ("COUNT(IFERROR(Inputs!D1,7))", Cell::Number(1.0)),
        ("AND(IFERROR(Inputs!D1,7))", Cell::Bool(false)),
    ] {
        assert_computed(formula, expected);
    }
}

#[test]
fn named_ranges_keep_existing_range_and_batch_budget_failures() {
    let mut workbook = workbook("SUM(HugeRange)");
    workbook.define_name("HugeRange", "Inputs!A1:A10001");
    assert!(matches!(
        workbook.evaluate_cell("Results", 0, 0),
        FormulaEvaluation::Fallback {
            reason: FormulaUnsupportedReason::RangeTooLarge,
            ..
        }
    ));
    assert_eq!(
        workbook.evaluate_cells(&[("Results", 0, 0)]),
        Err(FormulaUnsupportedReason::RangeTooLarge)
    );
}

#[test]
fn conditional_one_cell_ranges_keep_the_selected_argument_contract() {
    for argument in ["Inputs!A1", "Inputs!A1:A1", "NamedRange"] {
        for wrapper in [
            format!("IF(TRUE,{argument},7)"),
            format!("IF(FALSE,7,{argument})"),
        ] {
            assert_computed(&format!("SUM({wrapper})"), Cell::Number(0.0));
            assert_computed(&format!("COUNT({wrapper})"), Cell::Number(0.0));
            assert_computed(&wrapper, Cell::Text("2".into()));
        }
        for wrapper in [
            format!("IFERROR({argument},7)"),
            format!("IFNA({argument},7)"),
            format!("IFERROR(#N/A,{argument})"),
            format!("IFNA(#N/A,{argument})"),
        ] {
            assert_computed(&format!("SUM({wrapper})"), Cell::Number(2.0));
            assert_computed(&format!("COUNT({wrapper})"), Cell::Number(1.0));
            assert_computed(&wrapper, Cell::Text("2".into()));
        }
    }
    for argument in ["Inputs!D1", "Inputs!D1:D1"] {
        assert_computed(&format!("COUNTA(IF(TRUE,{argument},7))"), Cell::Number(0.0));
        assert_computed(&format!("IF(TRUE,{argument},7)"), Cell::Number(0.0));
        assert_computed(&format!("+{argument}"), Cell::Number(0.0));
        for function in ["IFERROR", "IFNA"] {
            assert_computed(
                &format!("COUNTA({function}({argument},7))"),
                Cell::Number(1.0),
            );
        }
    }
    assert_computed("SUM(+Inputs!C1)", Cell::Number(1.0));
    assert_computed("+Inputs!C1", Cell::Bool(true));
    assert_computed("AND(+Inputs!A1)", Cell::Error("#VALUE!".into()));
    assert_computed("IFERROR(Inputs!E1:E1,7)", Cell::Number(7.0));
}

#[test]
fn logical_aggregates_ignore_invalid_text_when_other_logical_values_exist() {
    for function in ["AND", "OR"] {
        for arguments in [r#"1,"2",TRUE,"""#, r#"1,"foo""#] {
            assert_computed(&format!("{function}({arguments})"), Cell::Bool(true));
        }
        assert_computed(
            &format!(r#"{function}("foo","2","")"#),
            Cell::Error("#VALUE!".into()),
        );
    }
}

#[test]
fn a_formula_returning_a_missing_reference_differs_from_a_missing_target() {
    assert_computed("Inputs!D1", Cell::Number(0.0));
    let workbook = workbook("Inputs!D1");
    assert_eq!(
        workbook.evaluate_cell("Inputs", 0, 3),
        FormulaEvaluation::Computed(Cell::Text(String::new()))
    );
}

#[test]
fn whole_axis_scalar_functions_require_array_semantics_even_with_one_stored_cell() {
    for reference in ["Inputs!A:A", "Inputs!1:1", "Inputs!A1:A2", "NamedAxis"] {
        for formula in [
            format!("SUM(IFERROR({reference},7))"),
            format!("COUNTA(IFNA({reference},7))"),
            format!("SUM(+{reference})"),
            format!("SUM(IF(TRUE,{reference},7))"),
            format!("IFERROR(#N/A,{reference})"),
        ] {
            let mut workbook = Workbook::new();
            workbook.add_sheet("Inputs").write(0, 0, "2");
            workbook
                .add_sheet("Results")
                .write_formula(0, 0, &formula, 999.0);
            workbook.define_name("NamedAxis", "Inputs!A:A");
            let expected = FormulaEvaluation::Fallback {
                cached: Cell::Number(999.0),
                reason: FormulaUnsupportedReason::ArraySemantics,
            };
            assert_eq!(
                workbook.evaluate_cell("Results", 0, 0),
                expected,
                "{formula}"
            );
            assert_eq!(
                workbook.evaluate_cells(&[("Results", 0, 0)]).unwrap(),
                vec![expected],
                "{formula}"
            );
        }
    }
    assert_computed("SUM(Inputs!A:A)", Cell::Number(0.0));
    assert_computed("IF(FALSE,Inputs!A:A,7)", Cell::Number(7.0));
}
