//! Public column interval regressions for #127.
#![cfg(feature = "xlsx")]

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};

use rxls::{Cell, Format, Spreadsheet, Workbook};
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

const SHEET: &str = "xl/worksheets/sheet1.xml";
type Column = BTreeMap<String, String>;

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

fn seed(columns: &str) -> Vec<u8> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Columns");
    sheet.write_with_format(0, 0, "bold", &Format::new().set_bold());
    sheet.write_with_format(0, 1, "italic", &Format::new().set_italic());
    sheet.write_formula(1, 0, "1+2", 3.0);
    let bytes = workbook.to_xlsx_checked().unwrap();
    let mut archive = ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut output = ZipWriter::new(Cursor::new(Vec::new()));
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let mut contents = Vec::new();
        entry.read_to_end(&mut contents).unwrap();
        output
            .start_file(entry.name(), SimpleFileOptions::default())
            .unwrap();
        if entry.name() == SHEET {
            let xml = String::from_utf8(contents).unwrap();
            assert!(!xml.contains("<cols>"));
            let xml = xml.replace("<sheetData>", &format!("<cols>{columns}</cols><sheetData>"));
            output.write_all(xml.as_bytes()).unwrap();
        } else {
            output.write_all(&contents).unwrap();
        }
    }
    output.finish().unwrap().into_inner()
}

fn columns(bytes: &[u8]) -> Vec<Column> {
    let xml = part(bytes, SHEET);
    let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
    let mut result = Vec::new();
    loop {
        match reader.read_event().unwrap() {
            quick_xml::events::Event::Start(element) | quick_xml::events::Event::Empty(element)
                if element.name().as_ref() == b"col" =>
            {
                result.push(
                    element
                        .attributes()
                        .map(|attribute| {
                            let attribute = attribute.unwrap();
                            (
                                String::from_utf8(attribute.key.as_ref().to_vec()).unwrap(),
                                attribute
                                    .decoded_and_normalized_value(
                                        quick_xml::XmlVersion::Implicit1_0,
                                        reader.decoder(),
                                    )
                                    .unwrap()
                                    .into_owned(),
                            )
                        })
                        .collect(),
                );
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
    result
}

fn intervals(columns: &[Column]) -> Vec<(u16, u16)> {
    columns
        .iter()
        .map(|column| {
            (
                column["min"].parse().unwrap(),
                column["max"].parse().unwrap(),
            )
        })
        .collect()
}

fn assert_layout(source: &Workbook, reopened: &Workbook) {
    let before = &source.sheets[0];
    let after = &reopened.sheets[0];
    assert_eq!(before.hidden_columns(), after.hidden_columns());
    assert_eq!(before.col_outline_levels(), after.col_outline_levels());
    for column in 0..=16383 {
        assert_eq!(
            before.column_widths().get(&column),
            after.column_widths().get(&column),
            "width {column}"
        );
        assert_eq!(
            before.resolved_cell_style(20, column),
            after.resolved_cell_style(20, column),
            "style {column}"
        );
    }
    for (row, column) in [(0, 0), (0, 1), (1, 0)] {
        assert_eq!(before.cell(row, column), after.cell(row, column));
    }
}

#[test]
fn whole_width_import_stays_one_compact_interval_through_repeated_writes() {
    let original = seed(r#"<col min="1" max="16384" style="1" width="9.140625"/>"#);
    let source = Workbook::open(&original).unwrap();
    assert_eq!(source.sheets[0].column_widths().len(), 16384);
    let mut current = original;
    for checked in [false, true] {
        let workbook = Workbook::open(&current).unwrap();
        current = if checked {
            workbook.to_xlsx_checked().unwrap()
        } else {
            workbook.to_xlsx()
        };
        let records = columns(&current);
        assert_eq!(records.len(), 1, "one full-width column record");
        assert_eq!(intervals(&records), [(1, 16384)]);
        let xml = String::from_utf8(part(&current, SHEET)).unwrap();
        let start = xml.find("<cols>").unwrap();
        let end = start + xml[start..].find("</cols>").unwrap() + "</cols>".len();
        assert!(end - start < 200, "full-width column XML must stay compact");
        assert_layout(&source, &Workbook::open(&current).unwrap());
    }
}

#[test]
fn overlapping_imports_keep_width_style_hidden_and_outline_boundaries() {
    let original = seed(concat!(
        r#"<col min="1" max="16384" style="1" width="9.140625"/>"#,
        r#"<col min="4" max="4" width="20"/>"#,
        r#"<col min="6" max="6" style="2"/>"#,
        r#"<col min="8" max="8" hidden="1"/>"#,
        r#"<col min="10" max="10" outlineLevel="2"/>"#,
        r#"<col min="12" max="12" width="0"/>"#,
    ));
    let source = Workbook::open(&original).unwrap();
    let saved = source.to_xlsx_checked().unwrap();
    let records = columns(&saved);
    assert_eq!(records.len(), 11, "column attribute boundaries");
    assert_eq!(
        intervals(&records),
        [
            (1, 3),
            (4, 4),
            (5, 5),
            (6, 6),
            (7, 7),
            (8, 8),
            (9, 9),
            (10, 10),
            (11, 11),
            (12, 12),
            (13, 16384)
        ]
    );
    assert_layout(&source, &Workbook::open(&saved).unwrap());
}

#[test]
fn equal_resolved_columns_merge_with_default_gaps_but_custom_width_stays_distinct() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Defaults");
    let bold = Format::new().set_bold();
    sheet.set_default_format(&bold);
    sheet.set_default_col_width(12.0);
    sheet.set_col_width(0, 12.0);
    sheet.set_col_format(1, &bold);
    sheet.group_cols(2, 3, 1);
    let records = columns(&workbook.to_xlsx_checked().unwrap());
    assert_eq!(intervals(&records), [(1, 1), (2, 2), (3, 4), (5, 16384)]);
    assert_eq!(records[0].get("customWidth").map(String::as_str), Some("1"));
    assert!(!records[1].contains_key("customWidth"));
    assert_eq!(
        records[2].get("outlineLevel").map(String::as_str),
        Some("1")
    );
    assert!(records.iter().all(|record| record["width"] == "12"));

    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Equal");
    sheet.set_default_format(&bold);
    sheet.set_col_format(1, &bold);
    assert_eq!(
        intervals(&columns(&workbook.to_xlsx_checked().unwrap())),
        [(1, 16384)]
    );
}

#[test]
fn absent_column_gaps_remain_absent_and_equal_autofit_widths_can_merge() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Gaps");
    sheet.set_col_width(0, 20.0);
    sheet.set_col_width(2, 20.0);
    assert_eq!(
        intervals(&columns(&workbook.to_xlsx_checked().unwrap())),
        [(1, 1), (3, 3)]
    );

    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Autofit");
    sheet.set_autofit();
    sheet.write(0, 0, "abc");
    sheet.write(0, 1, "def");
    sheet.set_col_width(2, 0.0);
    let records = columns(&workbook.to_xlsx_checked().unwrap());
    assert_eq!(intervals(&records), [(1, 2), (3, 3)]);
    assert_eq!(records[0]["width"], "5");
    assert_eq!(records[1]["width"], "0");
}

#[test]
fn alternating_columns_remain_bounded_and_nonoverlapping_at_the_grid_limit() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Alternating");
    for column in 0..=16383 {
        sheet.set_col_width(column, if column % 2 == 0 { 10.0 } else { 20.0 });
    }
    let saved = workbook.to_xlsx_checked().unwrap();
    let records = columns(&saved);
    assert_eq!(records.len(), 16384);
    for (index, column) in records.iter().enumerate() {
        assert_eq!(column["min"].parse::<usize>().unwrap(), index + 1);
        assert_eq!(column["max"].parse::<usize>().unwrap(), index + 1);
    }
    assert_eq!(
        Workbook::open(&saved).unwrap().sheets[0]
            .column_widths()
            .len(),
        16384
    );
}

#[test]
fn out_of_grid_column_metadata_keeps_existing_checked_writer_boundary() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Limits");
    sheet.set_col_width(16384, 20.0);
    sheet.set_col_format(16384, &Format::new().set_bold());
    assert!(workbook.to_xlsx_checked().is_err());
    assert!(columns(&workbook.to_xlsx()).is_empty());
}

#[test]
fn retained_package_edits_do_not_rewrite_original_column_intervals() {
    let original = seed(r#"<col min="1" max="16384" style="1" width="9.140625" bestFit="1"/>"#);
    let mut spreadsheet = Spreadsheet::open(&original).unwrap();
    assert_eq!(
        part(&spreadsheet.save().unwrap(), SHEET),
        part(&original, SHEET)
    );
    spreadsheet
        .set_cell_value("Columns", 2, 2, Cell::Number(9.0))
        .unwrap();
    let saved = spreadsheet.save().unwrap();
    let xml = String::from_utf8(part(&saved, SHEET)).unwrap();
    assert!(xml.contains(r#"<col min="1" max="16384" style="1" width="9.140625" bestFit="1"/>"#));
    let mut archive = ZipArchive::new(Cursor::new(&original)).unwrap();
    for index in 0..archive.len() {
        let name = archive.by_index(index).unwrap().name().to_owned();
        if name != SHEET {
            assert_eq!(part(&saved, &name), part(&original, &name), "{name}");
        }
    }
}
