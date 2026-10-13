//! Public package-preserving worksheet dimension regressions for #132.
#![cfg(feature = "xlsx")]

use std::io::{Cursor, Read, Write};

use rxls::{Cell, Spreadsheet, Workbook};
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

const MAIN: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const STRICT: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";
const PART: &str = "xl/worksheets/sheet1.xml";
const ROWS: &str = r#"<row r="1"><c r="A1"><v>1</v></c><c r="B1"><f>1+1</f><v>2</v></c></row>"#;

fn fixture(namespaces: &str, metadata: &str, rows: &str) -> Vec<u8> {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, 1.0);
    workbook.add_sheet("Keep").write(0, 0, "untouched");
    let bytes = workbook.to_xlsx_checked().unwrap();
    let xml =
        format!("<worksheet {namespaces}>{metadata}<sheetData>{rows}</sheetData></worksheet>");
    let mut source = ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut output = ZipWriter::new(Cursor::new(Vec::new()));
    for index in 0..source.len() {
        let mut part = source.by_index(index).unwrap();
        let mut contents = Vec::new();
        part.read_to_end(&mut contents).unwrap();
        output
            .start_file(part.name(), SimpleFileOptions::default())
            .unwrap();
        output
            .write_all(if part.name() == PART {
                xml.as_bytes()
            } else {
                &contents
            })
            .unwrap();
    }
    output.finish().unwrap().into_inner()
}

fn standard(metadata: &str) -> Vec<u8> {
    fixture(&format!(r#"xmlns="{MAIN}""#), metadata, ROWS)
}

fn part(bytes: &[u8], name: &str) -> Vec<u8> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut output = Vec::new();
    archive
        .by_name(name)
        .unwrap()
        .read_to_end(&mut output)
        .unwrap();
    output
}

fn sheet_xml(bytes: &[u8]) -> String {
    String::from_utf8(part(bytes, PART)).unwrap()
}

fn assert_untouched_parts(source: &[u8], saved: &[u8]) {
    let mut original = ZipArchive::new(Cursor::new(source)).unwrap();
    let result = ZipArchive::new(Cursor::new(saved)).unwrap();
    assert_eq!(original.len(), result.len());
    for index in 0..original.len() {
        let name = original.by_index(index).unwrap().name().to_string();
        if name != PART {
            assert_eq!(part(saved, &name), part(source, &name), "{name}");
        }
    }
}

fn grow(spreadsheet: &mut Spreadsheet, operation: u8) -> rxls::Result<()> {
    match operation {
        0 => spreadsheet.set_cell_value("Data", 98, 25, Cell::Number(99.0)),
        1 => spreadsheet.set_cell_formula("Data", 98, 25, "1+1", 2.0),
        2 => spreadsheet.set_cell_range_values(
            "Data",
            97,
            24,
            &[
                vec![Some(7.0.into()), None],
                vec![Some(8.0.into()), Some(99.0.into())],
            ],
        ),
        3 => spreadsheet
            .append_row("Data", [3.0.into(), 4.0.into()])
            .map(|row| assert_eq!(row, 1)),
        _ => unreachable!(),
    }
}

fn assert_growth(saved: &[u8], operation: u8) {
    let reopened = Workbook::open(saved).unwrap();
    let sheet = &reopened.sheets[0];
    match operation {
        0 | 2 => assert_eq!(sheet.cell(98, 25), Some(&Cell::Number(99.0))),
        1 => assert_eq!(
            sheet.cell(98, 25),
            Some(&Cell::Formula {
                formula: "1+1".into(),
                cached: Box::new(Cell::Number(2.0)),
            })
        ),
        3 => {
            assert_eq!(sheet.cell(1, 0), Some(&Cell::Number(3.0)));
            assert_eq!(sheet.cell(1, 1), Some(&Cell::Number(4.0)));
        }
        _ => unreachable!(),
    }
}

#[test]
fn existing_dimension_is_omitted_after_expanding_value_edits() {
    let bytes = fixture(
        &format!(r#"xmlns="{MAIN}""#),
        r#"<dimension ref="A1"/>"#,
        r#"<row r="1"><c r="A1"><v>1</v></c></row>"#,
    );
    for operation in 0..4 {
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        grow(&mut spreadsheet, operation).unwrap();
        let saved = spreadsheet.save().unwrap();
        assert!(
            !sheet_xml(&saved).contains("<dimension"),
            "operation {operation}"
        );
        assert_growth(&saved, operation);
        assert_eq!(spreadsheet.edited_parts(), &[PART.to_string()]);
        assert_untouched_parts(&bytes, &saved);
    }
}

#[test]
fn dimension_absent_inputs_remain_absent_after_each_growth_api() {
    let bytes = standard("");
    for operation in 0..4 {
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        grow(&mut spreadsheet, operation).unwrap();
        let saved = spreadsheet.save().unwrap();
        assert!(!sheet_xml(&saved).contains("dimension"));
        assert_growth(&saved, operation);
        assert_untouched_parts(&bytes, &saved);
    }
}

#[test]
fn dimension_namespace_aliases_and_bare_compatibility_are_recognized() {
    for (namespaces, dimension) in [
        (
            format!(r#"xmlns="{MAIN}" xmlns:s="{MAIN}""#),
            r#"<s:dimension ref="A1"/>"#.to_string(),
        ),
        (
            format!(r#"xmlns="{MAIN}" xmlns:s="urn:foreign""#),
            format!(r#"<s:dimension xmlns:s="{MAIN}" ref="A1"/>"#),
        ),
        (
            format!(r#"xmlns="{STRICT}" xmlns:s="{STRICT}""#),
            r#"<s:dimension ref="A1"/>"#.to_string(),
        ),
        (
            format!(r#"xmlns="{MAIN}""#),
            format!(r#"<s:dimension xmlns:s="{STRICT}" ref="A1"/>"#),
        ),
        (
            format!(r#"xmlns="{STRICT}""#),
            r#"<dimension ref="A1"/>"#.to_string(),
        ),
        (String::new(), r#"<dimension ref="A1"/>"#.to_string()),
    ] {
        let bytes = fixture(&namespaces, &dimension, ROWS);
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        grow(&mut spreadsheet, 0).unwrap();
        let saved = spreadsheet.save().unwrap();
        assert!(!sheet_xml(&saved).contains("dimension"), "{namespaces}");
        assert_growth(&saved, 0);
        assert_untouched_parts(&bytes, &saved);
    }
}

#[test]
fn foreign_reset_and_nested_dimensions_are_preserved() {
    for foreign in [
        r#"<ext:dimension xmlns:ext="urn:foreign" ref="foreign"/>"#,
        r#"<dimension xmlns="urn:foreign" ref="foreign"/>"#,
        r#"<dimension xmlns="" ref="foreign"/>"#,
        r#"<s:dimension xmlns:s="urn:foreign" ref="foreign"/>"#,
        r#"<extLst><dimension ref="foreign"/></extLst>"#,
    ] {
        let bytes = fixture(
            &format!(r#"xmlns="{MAIN}" xmlns:s="{MAIN}""#),
            &format!(r#"<dimension ref="A1"/>{foreign}"#),
            ROWS,
        );
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        grow(&mut spreadsheet, 0).unwrap();
        let saved = spreadsheet.save().unwrap();
        let xml = sheet_xml(&saved);
        assert!(!xml.contains(r#"<dimension ref="A1""#));
        assert!(xml.contains(foreign), "{xml}");
        assert_growth(&saved, 0);
        assert_untouched_parts(&bytes, &saved);
    }
}

#[test]
fn malformed_dimension_references_are_omitted_without_range_inference() {
    for dimension in [
        "<dimension/>",
        r#"<dimension ref="invalid"/>"#,
        r#"<dimension ref="XFD1048576"/>"#,
        r#"<dimension ref="A1:XFD1048576"/>"#,
    ] {
        let bytes = standard(dimension);
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        grow(&mut spreadsheet, 0).unwrap();
        let saved = spreadsheet.save().unwrap();
        assert!(!sheet_xml(&saved).contains("dimension"));
        assert_growth(&saved, 0);
    }
}

#[test]
fn duplicate_recognized_dimensions_reject_all_growth_apis_atomically() {
    for second in [
        r#"<dimension ref="A1:B1"/>"#.to_string(),
        format!(r#"<s:dimension xmlns:s="{MAIN}" ref="A1:B1"/>"#),
        format!(r#"<s:dimension xmlns:s="{STRICT}" ref="A1:B1"/>"#),
    ] {
        let bytes = standard(&format!(r#"<dimension ref="A1"/>{second}"#));
        for operation in 0..4 {
            let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
            spreadsheet
                .set_cell_value("Keep", 0, 0, "prior edit".into())
                .unwrap();
            let before = spreadsheet.save().unwrap();
            let edited_parts = spreadsheet.edited_parts().to_vec();
            assert!(
                grow(&mut spreadsheet, operation).is_err(),
                "operation {operation}"
            );
            assert_eq!(spreadsheet.save().unwrap(), before);
            assert_eq!(spreadsheet.edited_parts(), edited_parts);
        }
    }
}

#[test]
fn cache_clear_empty_and_noop_operations_retain_dimension_metadata() {
    let dimension = r#"<dimension ref="A1:B1" custom="retained"/>"#;
    let bytes = standard(dimension);
    for operation in 0..9 {
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        match operation {
            0 => {}
            1 => spreadsheet
                .set_formula_cached_values(&[("Data", 0, 1, 3.0.into())])
                .unwrap(),
            2 => spreadsheet.set_formula_cached_values(&[]).unwrap(),
            3 => spreadsheet.clear_cell_value("Data", 0, 0).unwrap(),
            4 => spreadsheet.clear_range("Data", 0, 0, 0, 0).unwrap(),
            5 => spreadsheet
                .set_cell_range_values("Data", 0, 0, &[vec![None, None]])
                .unwrap(),
            6 => {
                spreadsheet.append_row("Data", []).unwrap();
            }
            7 => spreadsheet.clear_cell_value("Data", 98, 25).unwrap(),
            8 => spreadsheet
                .set_cell_range_values("Data", 98, 25, &[vec![None]])
                .unwrap(),
            _ => unreachable!(),
        }
        let saved = spreadsheet.save().unwrap();
        assert!(
            sheet_xml(&saved).contains(dimension),
            "operation {operation}"
        );
        assert_untouched_parts(&bytes, &saved);
    }
}

#[test]
fn a_range_with_only_its_last_cell_written_omits_dimension() {
    let bytes = standard(r#"<dimension ref="A1:B1"/>"#);
    let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
    spreadsheet
        .set_cell_range_values(
            "Data",
            97,
            24,
            &[vec![None, None], vec![None, Some(99.0.into())]],
        )
        .unwrap();
    let saved = spreadsheet.save().unwrap();
    assert!(!sheet_xml(&saved).contains("dimension"));
    assert_growth(&saved, 2);
    assert_untouched_parts(&bytes, &saved);
}

#[test]
fn transaction_failure_restores_dimension_and_prior_edits() {
    let bytes = standard(r#"<dimension ref="A1"/>"#);
    let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
    spreadsheet
        .set_cell_value("Keep", 0, 0, "prior edit".into())
        .unwrap();
    let before = spreadsheet.save().unwrap();
    let edited_parts = spreadsheet.edited_parts().to_vec();
    let result = spreadsheet.transaction(|candidate| {
        grow(candidate, 0)?;
        candidate.set_cell_value("Data", 0, 0, Cell::Number(f64::NAN))
    });
    assert!(result.is_err());
    assert_eq!(spreadsheet.save().unwrap(), before);
    assert_eq!(spreadsheet.edited_parts(), edited_parts);
    assert!(sheet_xml(&before).contains(r#"<dimension ref="A1"/>"#));
}

#[test]
fn unrelated_implicit_cells_survive_dimension_omission() {
    let bytes = fixture(
        &format!(r#"xmlns="{MAIN}""#),
        r#"<dimension ref="A1"/>"#,
        r#"<row><c><v>1</v></c></row><row r="5"><c r="C5"><v>5</v></c><c><v>6</v></c></row><row><c><v>7</v></c></row>"#,
    );
    let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
    grow(&mut spreadsheet, 0).unwrap();
    let saved = spreadsheet.save().unwrap();
    assert!(!sheet_xml(&saved).contains("dimension"));
    let reopened = Workbook::open(&saved).unwrap();
    for (row, col, value) in [
        (0, 0, 1.0),
        (4, 2, 5.0),
        (4, 3, 6.0),
        (5, 0, 7.0),
        (98, 25, 99.0),
    ] {
        assert_eq!(
            reopened.sheets[0].cell(row, col),
            Some(&Cell::Number(value))
        );
    }
    assert_untouched_parts(&bytes, &saved);
}
