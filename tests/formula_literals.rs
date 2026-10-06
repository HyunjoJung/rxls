//! Public quote-aware formula literal regressions for #135.

use rxls::{Cell, FormulaEvaluation, FormulaUnsupportedReason, Workbook};

fn assert_computed(formula: &str, expected: Cell) {
    let mut workbook = Workbook::new();
    workbook
        .add_sheet("Data")
        .write_formula(0, 0, formula, "sentinel-cache");
    let original = workbook.sheets[0].cell(0, 0).cloned();
    let expected = FormulaEvaluation::Computed(expected);
    assert_eq!(workbook.evaluate_cell("Data", 0, 0), expected, "{formula}");
    assert_eq!(
        workbook.evaluate_cells(&[("Data", 0, 0), ("Data", 0, 0)]),
        Ok(vec![expected.clone(), expected]),
        "batch/memo {formula}"
    );
    assert_eq!(workbook.sheets[0].cell(0, 0).cloned(), original);
}

#[test]
fn issue_text_markers_are_literal_data() {
    for (formula, expected) in [
        (r#"LEN("a@b.com")"#, 7.0),
        (r#"LEN("[x]")"#, 3.0),
        (r#"LEN("{x}")"#, 3.0),
    ] {
        assert_computed(formula, Cell::Number(expected));
    }
}

#[test]
fn quoted_text_is_exact_with_unicode_punctuation_and_doubled_quotes() {
    for text in [
        "[.A1];@{}",
        "[x",
        "한😀;[]@{}",
        r#"{"a":"@;[]"}"#,
        "\"@[]\"",
        "\"\"\"@\"\"",
        "Bob's \"quoted\" @{}",
        "_xlfn.IFNA",
        "",
    ] {
        let formula = format!("\"{}\"", text.replace('"', "\"\""));
        assert_computed(&formula, Cell::Text(text.into()));
    }
}

#[test]
fn separators_and_actual_openformula_references_normalize_outside_literals() {
    for formula in [r#"CONCATENATE("a;b";"[x]")"#, r#"CONCATENATE("a;b","[x]")"#] {
        assert_computed(formula, Cell::Text("a;b[x]".into()));
    }
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Data");
    sheet.write(0, 0, "actual@{}");
    sheet.write_formula(0, 1, r#"CONCATENATE("[.A1]";[.A1])"#, "sentinel-cache");
    let original = sheet.cell(0, 1).cloned();
    assert_eq!(
        workbook.evaluate_cell("Data", 0, 1),
        FormulaEvaluation::Computed(Cell::Text("[.A1]actual@{}".into()))
    );
    assert_eq!(workbook.sheets[0].cell(0, 1).cloned(), original);
}

#[test]
fn quoted_sheet_names_preserve_punctuation_and_quote_escaping() {
    let name = "Semi;@{Bob's}\"한";
    let quoted = format!("'{}'", name.replace('\'', "''"));
    let mut workbook = Workbook::new();
    workbook.add_sheet(name).write(0, 0, 7.0);
    let calc = workbook.add_sheet("Data");
    calc.write(0, 0, 5.0);
    calc.write_formula(0, 1, format!("{quoted}!A1"), "direct-cache");
    calc.write_formula(0, 2, format!("SUM([{quoted}.A1];[.A1])"), "odf-cache");
    let original = [calc.cell(0, 1).cloned(), calc.cell(0, 2).cloned()];
    assert_eq!(
        workbook.evaluate_cells(&[("Data", 0, 1), ("Data", 0, 2), ("Data", 0, 1)]),
        Ok(vec![
            FormulaEvaluation::Computed(Cell::Number(7.0)),
            FormulaEvaluation::Computed(Cell::Number(12.0)),
            FormulaEvaluation::Computed(Cell::Number(7.0)),
        ])
    );
    assert_eq!(
        [
            workbook.sheets[1].cell(0, 1).cloned(),
            workbook.sheets[1].cell(0, 2).cloned()
        ],
        original
    );
}

#[test]
fn literals_survive_defined_name_and_formula_dependencies() {
    let mut workbook = Workbook::new();
    workbook.define_name("Literal", r#""@[];{}""#);
    workbook.define_name("LiteralAlias", "Literal");
    let sheet = workbook.add_sheet("Data");
    sheet.write_formula(0, 0, r#""@[];{}""#, "literal-cache");
    sheet.write_formula(0, 1, "A1&LiteralAlias", "dependency-cache");
    let original = [sheet.cell(0, 0).cloned(), sheet.cell(0, 1).cloned()];
    assert_eq!(
        workbook.evaluate_cells(&[("Data", 0, 1), ("Data", 0, 0), ("Data", 0, 1)]),
        Ok(vec![
            FormulaEvaluation::Computed(Cell::Text("@[];{}@[];{}".into())),
            FormulaEvaluation::Computed(Cell::Text("@[];{}".into())),
            FormulaEvaluation::Computed(Cell::Text("@[];{}@[];{}".into())),
        ])
    );
    assert_eq!(
        [
            workbook.sheets[0].cell(0, 0).cloned(),
            workbook.sheets[0].cell(0, 1).cloned()
        ],
        original
    );
}

#[test]
fn genuine_unsupported_syntax_keeps_reason_and_cache() {
    use FormulaUnsupportedReason::{ArraySemantics, ExternalRef, UnsupportedFunction};
    for (formula, reason) in [
        ("[Book.xlsx]Sheet1!A1", ExternalRef),
        ("'[Book.xlsx]Sheet 1'!A1", ExternalRef),
        ("SUM({1,2})", ArraySemantics),
        ("@A1", ArraySemantics),
        ("B1#", ArraySemantics),
        (r#"LEN("[x]")+SUM({1,2})"#, ArraySemantics),
        (r#"LEN("@")+'[Book.xlsx]Sheet 1'!A1"#, ExternalRef),
        ("'[Book.xlsx]Sheet 1'!A1+SUM({1,2})", ExternalRef),
        ("_xlfn.RXLS_PARTIAL()", UnsupportedFunction),
        ("_xlfn._xlws.FILTER(A1:A2,A1:A2>0)", UnsupportedFunction),
        ("NA()", UnsupportedFunction),
        ("TRUE()", UnsupportedFunction),
    ] {
        let mut workbook = Workbook::new();
        workbook
            .add_sheet("Data")
            .write_formula(0, 0, formula, "sentinel-cache");
        let original = workbook.sheets[0].cell(0, 0).cloned();
        let fallback = FormulaEvaluation::Fallback {
            cached: Cell::Text("sentinel-cache".into()),
            reason,
        };
        assert_eq!(workbook.evaluate_cell("Data", 0, 0), fallback, "{formula}");
        assert_eq!(
            workbook.evaluate_cells(&[("Data", 0, 0), ("Data", 0, 0)]),
            Ok(vec![fallback.clone(), fallback])
        );
        assert_eq!(workbook.sheets[0].cell(0, 0).cloned(), original);
    }
}

#[test]
fn malformed_quotes_and_real_brackets_keep_typed_parser_fallbacks() {
    for formula in [
        r#"LEN("unterminated@[{)"#,
        r#""text "" @[]"#,
        "'Semi;@{Bob''s}!A1",
        "'[Book.xlsx]Sheet!A1",
        "SUM([.A1)",
    ] {
        let mut workbook = Workbook::new();
        workbook
            .add_sheet("Data")
            .write_formula(0, 0, formula, 424242.0);
        let original = workbook.sheets[0].cell(0, 0).cloned();
        assert_eq!(
            workbook.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(424242.0),
                reason: FormulaUnsupportedReason::UnparsableExpression,
            },
            "{formula}"
        );
        assert_eq!(workbook.sheets[0].cell(0, 0).cloned(), original);
    }
}

#[cfg(feature = "xlsx")]
mod transport {
    use super::*;
    use std::io::{Cursor, Read};

    fn part(bytes: &[u8], name: &str) -> Vec<u8> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut contents = Vec::new();
        zip.by_name(name)
            .unwrap()
            .read_to_end(&mut contents)
            .unwrap();
        contents
    }

    #[test]
    fn xlsx_writer_and_retained_formula_edit_preserve_source_and_cache() {
        let formula = r#"CONCATENATE("a&<","@[]{}""quote")"#;
        let expected = Cell::Text("a&<@[]{}\"quote".into());
        let mut workbook = Workbook::new();
        workbook
            .add_sheet("Data")
            .write_formula(0, 0, formula, "original-cache");
        workbook.add_sheet("Keep").write(0, 0, "untouched");
        let original_cell = workbook.sheets[0].cell(0, 0).cloned();
        let original = workbook.to_xlsx_checked().unwrap();
        let reopened = Workbook::open(&original).unwrap();
        assert_eq!(reopened.sheets[0].cell(0, 0).cloned(), original_cell);
        assert_eq!(
            reopened.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(expected.clone())
        );
        assert_eq!(reopened.sheets[0].cell(0, 0).cloned(), original_cell);
        let mut spreadsheet = rxls::Spreadsheet::open(&original).unwrap();
        assert_eq!(spreadsheet.save().unwrap(), original);
        spreadsheet
            .set_cell_formula("Data", 0, 0, formula, "edited-cache")
            .unwrap();
        let saved = spreadsheet.save().unwrap();
        let reopened = Workbook::open(&saved).unwrap();
        assert_eq!(
            reopened.sheets[0].cell(0, 0),
            Some(&Cell::Formula {
                formula: formula.into(),
                cached: Box::new(Cell::Text("edited-cache".into()))
            })
        );
        assert_eq!(
            reopened.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(expected)
        );
        for path in [
            "xl/worksheets/sheet2.xml",
            "xl/styles.xml",
            "docProps/app.xml",
        ] {
            assert_eq!(part(&saved, path), part(&original, path), "{path}");
        }
    }
}
