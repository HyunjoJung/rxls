//! Borrowed one-cell range inspection and bounded-array policy for IS predicates.

use rxls::{Cell, FormulaEvaluation, FormulaUnsupportedReason, Workbook};

fn workbook(formula: &str) -> Workbook {
    let mut workbook = Workbook::new();
    let inputs = workbook.add_sheet("Inputs");
    inputs.write(0, 0, "3");
    inputs.write(1, 0, 3.0);
    inputs.write(2, 0, "");
    inputs.write_formula(3, 0, r#""""#, "nonempty-stale-cache");
    // A5 remains absent.
    inputs.write(5, 0, Cell::Error("#N/A".into()));
    inputs.write(6, 0, Cell::Error("#DIV/0!".into()));
    workbook.define_name("OneNumber", "Inputs!A2:A2");
    workbook.define_name("OneMissing", "Inputs!A5:A5");
    workbook
        .add_sheet("Results")
        .write_formula(0, 0, formula, 424242.0);
    workbook
}

#[test]
fn one_cell_ranges_inspect_value_type_and_preserve_error_and_blank_semantics() {
    for (formula, expected) in [
        ("ISNUMBER(Inputs!A1:A1)", false),
        ("ISNUMBER(Inputs!A2:A2)", true),
        ("ISNUMBER(OneNumber)", true),
        ("ISTEXT(Inputs!A1:A1)", true),
        ("ISTEXT(Inputs!A3:A3)", true),
        ("ISTEXT(Inputs!A4:A4)", true),
        ("ISBLANK(Inputs!A3:A3)", false),
        ("ISBLANK(Inputs!A4:A4)", false),
        ("ISBLANK(Inputs!A5:A5)", true),
        ("ISBLANK(OneMissing)", true),
        ("ISNA(Inputs!A6:A6)", true),
        ("ISNA(Inputs!A7:A7)", false),
        ("ISERROR(Inputs!A6:A6)", true),
        ("ISERROR(Inputs!A7:A7)", true),
        ("ISNUMBER(Inputs!A6:A6)", false),
        ("ISTEXT(Inputs!A6:A6)", false),
        ("ISBLANK(Inputs!A6:A6)", false),
        ("ISBLANK(IF(TRUE,Inputs!A5:A5,1))", true),
        ("ISNUMBER(IFERROR(Inputs!A5:A5,1))", true),
        ("ISBLANK(IFERROR(Inputs!A5:A5,1))", false),
        ("ISBLANK(+Inputs!A5:A5)", false),
        ("ISNUMBER(+Inputs!A5:A5)", true),
        ("ISBLANK(IFNA(Inputs!A4:A4,1))", false),
    ] {
        let workbook = workbook(formula);
        let original = workbook.sheets[1].cell(0, 0).cloned();
        let expected = FormulaEvaluation::Computed(Cell::Bool(expected));
        assert_eq!(
            workbook.evaluate_cell("Results", 0, 0),
            expected,
            "{formula}"
        );
        assert_eq!(
            workbook.evaluate_cells(&[("Results", 0, 0), ("Results", 0, 0)]),
            Ok(vec![expected.clone(), expected])
        );
        assert_eq!(workbook.sheets[1].cell(0, 0).cloned(), original);
    }
}

#[test]
fn actual_array_shapes_are_not_inferred_from_one_stored_value() {
    for function in ["ISNA", "ISERROR", "ISNUMBER", "ISTEXT", "ISBLANK"] {
        for reference in [
            "Inputs!A1:A2",
            "Inputs!A1:B1",
            "Inputs!A:A",
            "Inputs!1:1",
            "NamedAxis",
            "Inputs:Other!A1",
        ] {
            let formula = format!("{function}({reference})");
            let mut workbook = Workbook::new();
            workbook.add_sheet("Inputs").write(0, 0, 3.0);
            workbook.add_sheet("Other");
            workbook.define_name("NamedAxis", "Inputs!A:A");
            workbook
                .add_sheet("Results")
                .write_formula(0, 0, &formula, 424242.0);
            let original = workbook.sheets[2].cell(0, 0).cloned();
            let expected = FormulaEvaluation::Fallback {
                cached: Cell::Number(424242.0),
                reason: FormulaUnsupportedReason::ArraySemantics,
            };
            assert_eq!(
                workbook.evaluate_cell("Results", 0, 0),
                expected,
                "{formula}"
            );
            assert_eq!(
                workbook.evaluate_cells(&[("Results", 0, 0), ("Results", 0, 0)]),
                Ok(vec![expected.clone(), expected])
            );
            assert_eq!(workbook.sheets[2].cell(0, 0).cloned(), original);
        }
    }
}

#[test]
fn predicate_arity_is_checked_before_array_classification() {
    for function in ["ISNA", "ISERROR", "ISNUMBER", "ISTEXT", "ISBLANK"] {
        for args in ["", "Inputs!A:A,1"] {
            let formula = format!("{function}({args})");
            let workbook = workbook(&formula);
            let original = workbook.sheets[1].cell(0, 0).cloned();
            assert_eq!(
                workbook.evaluate_cell("Results", 0, 0),
                FormulaEvaluation::Computed(Cell::Error("#VALUE!".into())),
                "{formula}"
            );
            assert_eq!(workbook.sheets[1].cell(0, 0).cloned(), original);
        }
    }
}
