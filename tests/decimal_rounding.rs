//! Public decimal-step rounding regressions for #134.

use rxls::{Cell, FormulaEvaluation, Workbook};

fn assert_number(formula: &str, expected: f64) {
    let mut workbook = Workbook::new();
    workbook
        .add_sheet("Data")
        .write_formula(0, 0, formula, 424242.0);
    let source = workbook.sheets[0].cell(0, 0).cloned();
    let result = FormulaEvaluation::Computed(Cell::Number(expected));
    assert_eq!(workbook.evaluate_cell("Data", 0, 0), result, "{formula}");
    assert_eq!(
        workbook.evaluate_cells(&[("Data", 0, 0), ("Data", 0, 0)]),
        Ok(vec![result.clone(), result]),
        "batch/memo {formula}"
    );
    assert_eq!(workbook.sheets[0].cell(0, 0).cloned(), source);
}

#[test]
fn reported_decimal_boundaries_and_sign_mirrors() {
    for sign in ["", "-"] {
        let multiplier = if sign.is_empty() { 1.0 } else { -1.0 };
        for (function, number, expected) in [
            ("ROUNDUP", "1.1", 1.1),
            ("ROUNDDOWN", "1.15", 1.15),
            ("TRUNC", "1.15", 1.15),
            ("ROUND", "1.005", 1.01),
        ] {
            assert_number(
                &format!("{function}({sign}{number},2)"),
                multiplier * expected,
            );
        }
    }
}

#[test]
fn decimal_ties_and_negative_precision_round_away_from_zero() {
    for sign in ["", "-"] {
        let multiplier = if sign.is_empty() { 1.0 } else { -1.0 };
        for (number, digits, expected) in [
            ("2.675", 2, 2.68),
            ("0.045", 2, 0.05),
            ("0.0005", 3, 0.001),
            ("5", -1, 10.0),
            ("145", -1, 150.0),
            ("150", -2, 200.0),
        ] {
            assert_number(
                &format!("ROUND({sign}{number},{digits})"),
                multiplier * expected,
            );
        }
    }
}

#[test]
fn genuine_fifteen_significant_digit_offsets_keep_their_direction() {
    for sign in ["", "-"] {
        let multiplier = if sign.is_empty() { 1.0 } else { -1.0 };
        for (function, number, digits, expected) in [
            ("ROUNDUP", "1.10000000000001", 1, 1.2),
            ("ROUNDUP", "1.09999999999999", 1, 1.1),
            ("ROUNDDOWN", "1.14999999999999", 2, 1.14),
            ("TRUNC", "1.14999999999999", 2, 1.14),
            ("ROUND", "1.00499999999999", 2, 1.0),
            ("ROUND", "1.00500000000001", 2, 1.01),
            ("ROUNDUP", "100.000000000001", -1, 110.0),
            ("ROUNDDOWN", "99.9999999999999", -1, 90.0),
        ] {
            assert_number(
                &format!("{function}({sign}{number},{digits})"),
                multiplier * expected,
            );
        }
    }
}

#[test]
fn arithmetic_residue_does_not_create_an_extra_decimal_step() {
    for function in ["ROUND", "ROUNDUP", "ROUNDDOWN", "TRUNC"] {
        assert_number(&format!("{function}(0.1+0.2,2)"), 0.3);
        assert_number(&format!("{function}(-(0.1+0.2),2)"), -0.3);
    }
    // This is a real 15-significant-digit offset, not binary multiplication noise.
    assert_number("ROUNDDOWN((43.1-43.2)+1,2)", 0.89);
    assert_number("TRUNC((43.1-43.2)+1,2)", 0.89);
}

#[test]
fn literal_reference_name_and_formula_inputs_share_rounding_and_keep_caches() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Inputs");
    sheet.write(0, 0, 1.1);
    sheet.write(0, 1, 1.15);
    sheet.write_formula(0, 2, "0.1+0.2", 123.0);
    workbook.define_name("Amount", "Inputs!$A$1");
    let sheet = workbook.add_sheet("Results");
    let formulas = [
        ("ROUNDUP(Inputs!A1,2)", 1.1),
        ("ROUNDUP(Amount,2)", 1.1),
        ("ROUNDDOWN(Inputs!B1,2)", 1.15),
        ("TRUNC(Inputs!B1,2)", 1.15),
        ("ROUNDUP(Inputs!C1,2)", 0.3),
    ];
    for (row, (formula, _)) in formulas.iter().enumerate() {
        sheet.write_formula(row as u32, 0, *formula, 424242.0);
    }
    let source: Vec<_> = workbook
        .sheets
        .iter()
        .map(|s| {
            s.cells()
                .map(|(r, c, value)| (r, c, value.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    let targets: Vec<_> = (0..formulas.len())
        .map(|r| ("Results", r as u32, 0))
        .collect();
    let expected: Vec<_> = formulas
        .iter()
        .map(|(_, n)| FormulaEvaluation::Computed(Cell::Number(*n)))
        .collect();
    assert_eq!(workbook.evaluate_cells(&targets), Ok(expected.clone()));
    for (row, result) in expected.into_iter().enumerate() {
        assert_eq!(workbook.evaluate_cell("Results", row as u32, 0), result);
    }
    assert_eq!(
        workbook
            .sheets
            .iter()
            .map(|s| s
                .cells()
                .map(|(r, c, value)| (r, c, value.clone()))
                .collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        source
    );
}

#[test]
fn adjacent_binary_neighbors_follow_decimal_precision_without_erasing_offsets() {
    for (number, function, expected) in [
        (1.1_f64, "ROUNDUP", 1.1),
        (1.15_f64, "ROUNDDOWN", 1.15),
        (1.005_f64, "ROUND", 1.01),
    ] {
        for bits in [number.to_bits() - 1, number.to_bits(), number.to_bits() + 1] {
            for sign in [1.0, -1.0] {
                let mut workbook = Workbook::new();
                let sheet = workbook.add_sheet("Data");
                sheet.write(0, 0, sign * f64::from_bits(bits));
                sheet.write_formula(0, 1, format!("{function}(A1,2)"), 424242.0);
                assert_eq!(
                    workbook.evaluate_cell("Data", 0, 1),
                    FormulaEvaluation::Computed(Cell::Number(sign * expected)),
                    "{function}, bits {bits:016x}, sign {sign}"
                );
            }
        }
    }
}

#[test]
fn optional_fractional_digits_coercion_and_underlying_errors_are_preserved() {
    assert_number("TRUNC(-3.987)", -3.0);
    assert_number("TRUNC(3.987,2.9)", 3.98);
    assert_number("ROUND(145,-1.9)", 150.0);
    assert_number(r#"ROUNDUP("1.1","2")"#, 1.1);
    for function in ["ROUND", "ROUNDUP", "ROUNDDOWN", "TRUNC"] {
        for formula in [format!("{function}(1/0,2)"), format!("{function}(5,1/0)")] {
            let mut workbook = Workbook::new();
            workbook
                .add_sheet("Data")
                .write_formula(0, 0, &formula, 424242.0);
            assert_eq!(
                workbook.evaluate_cell("Data", 0, 0),
                FormulaEvaluation::Computed(Cell::Error("#DIV/0!".into())),
                "{formula}"
            );
        }
    }
}

#[test]
fn finite_large_values_do_not_overflow_an_intermediate_binary_scale() {
    for function in ["ROUND", "ROUNDUP", "ROUNDDOWN", "TRUNC"] {
        assert_number(&format!("{function}(1e300,2)"), 1e300);
        assert_number(&format!("{function}(-1e300,100)"), -1e300);
        assert_number(
            &format!("{function}(1.23456789012345,100)"),
            1.23456789012345,
        );
    }
}

#[test]
fn extreme_precision_and_nonfinite_results_keep_a_bounded_num_error_policy() {
    for function in ["ROUND", "ROUNDUP", "ROUNDDOWN", "TRUNC"] {
        for number in ["0", "5", "-5"] {
            for digits in ["400", "-400", "1e300", "-1e300"] {
                let formula = format!("{function}({number},{digits})");
                let mut workbook = Workbook::new();
                workbook
                    .add_sheet("Data")
                    .write_formula(0, 0, &formula, 424242.0);
                assert_eq!(
                    workbook.evaluate_cell("Data", 0, 0),
                    FormulaEvaluation::Computed(Cell::Error("#NUM!".into())),
                    "{formula}"
                );
            }
        }
    }
    assert_number("ROUND(1e-300,2)", 0.0);
    assert_number("TRUNC(-1e-300,2)", -0.0);
    assert_number("ROUNDUP(1e-300,2)", 0.01);
    assert_number("ROUNDUP(-1e-300,2)", -0.01);
}

#[test]
fn nonfinite_inputs_are_errors_and_finite_signed_zero_remains_zero() {
    for function in ["ROUND", "ROUNDUP", "ROUNDDOWN", "TRUNC"] {
        for number in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let mut workbook = Workbook::new();
            let sheet = workbook.add_sheet("Data");
            sheet.write(0, 0, number);
            sheet.write_formula(0, 1, format!("{function}(A1,2)"), 424242.0);
            sheet.write_formula(0, 2, format!("{function}(5,A1)"), 424242.0);
            for col in [1, 2] {
                assert_eq!(
                    workbook.evaluate_cell("Data", 0, col),
                    FormulaEvaluation::Computed(Cell::Error("#NUM!".into()))
                );
            }
        }
        for number in [0.0_f64, -0.0_f64] {
            let mut workbook = Workbook::new();
            let sheet = workbook.add_sheet("Data");
            sheet.write(0, 0, number);
            sheet.write_formula(0, 1, format!("{function}(A1,2)"), 424242.0);
            let FormulaEvaluation::Computed(Cell::Number(result)) =
                workbook.evaluate_cell("Data", 0, 1)
            else {
                panic!("expected computed zero");
            };
            assert_eq!(result.to_bits(), number.to_bits());
        }
    }
}

#[test]
fn high_precision_keeps_decimal_normalization_and_bounds_overflow_and_subnormals() {
    for function in ["ROUND", "ROUNDUP", "ROUNDDOWN", "TRUNC"] {
        for digits in [15, 16, 17] {
            for sign in [1.0, -1.0] {
                let mut workbook = Workbook::new();
                let sheet = workbook.add_sheet("Data");
                sheet.write(0, 0, sign * (0.1_f64 + 0.2));
                sheet.write_formula(0, 1, format!("{function}(A1,{digits})"), 424242.0);
                assert_eq!(
                    workbook.evaluate_cell("Data", 0, 1),
                    FormulaEvaluation::Computed(Cell::Number(sign * 0.3)),
                    "{function}, precision {digits}, sign {sign}"
                );
            }
        }
        // Fifteen-significant-digit normalization of MAX exceeds binary64.
        // The engine returns an error rather than emitting a nonfinite cell.
        for number in [f64::MAX, -f64::MAX] {
            let mut workbook = Workbook::new();
            let sheet = workbook.add_sheet("Data");
            sheet.write(0, 0, number);
            sheet.write_formula(0, 1, format!("{function}(A1,0)"), 424242.0);
            assert_eq!(
                workbook.evaluate_cell("Data", 0, 1),
                FormulaEvaluation::Computed(Cell::Error("#NUM!".into()))
            );
        }
        // This is a bounded engine-domain contract. Excel does not retain
        // imported subnormal inputs, so it is not a native parity assertion.
        for sign in [1.0, -1.0] {
            let mut workbook = Workbook::new();
            let sheet = workbook.add_sheet("Data");
            sheet.write(0, 0, sign * f64::from_bits(1));
            sheet.write_formula(0, 1, format!("{function}(A1,308)"), 424242.0);
            let expected = if function == "ROUNDUP" { 1e-308 } else { 0.0 };
            assert_eq!(
                workbook.evaluate_cell("Data", 0, 1),
                FormulaEvaluation::Computed(Cell::Number(sign * expected))
            );
        }
    }
    assert_number("ROUNDDOWN(1.7976931348623157e308,-307)", 1.7e308);
    assert_number("TRUNC(-1.7976931348623157e308,-307)", -1.7e308);
}
