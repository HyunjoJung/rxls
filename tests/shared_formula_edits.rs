//! Generated OOXML regressions for shared/array formula replacement (#129).
#![cfg(feature = "xlsx")]

use std::io::{Cursor, Read, Write};

use rxls::{Cell, Spreadsheet, Workbook};
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

const SHARED: &str = r#"<row r="1"><c r="A1"><v>10</v></c><c r="B1"><f t="shared" ref="B1:B2" si="0">A1*2</f><v>20</v></c></row><row r="2"><c r="A2"><v>30</v></c><c r="B2"><f t="shared" si="0"/><v>60</v></c></row>"#;
const ARRAY: &str = r#"<row r="1"><c r="A1"><v>10</v></c><c r="B1"><f t="array" ref="B1:B2">A1:A2*2</f><v>20</v></c></row><row r="2"><c r="A2"><v>30</v></c><c r="B2"><v>60</v></c></row>"#;

fn replace_part(bytes: &[u8], name: &str, xml: &str) -> Vec<u8> {
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
            .write_all(if part.name() == name {
                xml.as_bytes()
            } else {
                &contents
            })
            .unwrap();
    }
    output.finish().unwrap().into_inner()
}

fn fixture(rows: &str) -> Vec<u8> {
    fixture_sheet_data(&format!("<sheetData>{rows}</sheetData>"))
}

fn fixture_sheet_data(data: &str) -> Vec<u8> {
    let mut source = Workbook::new();
    source.add_sheet("Data").write(0, 0, 10.0);
    source.add_sheet("Keep").write(0, 0, "untouched");
    replace_part(
        &source.to_xlsx_checked().unwrap(),
        "xl/worksheets/sheet1.xml",
        &format!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:s="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:B2"/>{data}</worksheet>"#
        ),
    )
}

fn edit(spreadsheet: &mut Spreadsheet, row: u32, col: u16, operation: u8) -> rxls::Result<()> {
    match operation {
        0 => spreadsheet.set_cell_value("Data", row, col, Cell::Number(99.0)),
        1 => spreadsheet.set_cell_formula("Data", row, col, "1+1", 2.0),
        2 => spreadsheet.clear_cell_value("Data", row, col),
        3 => spreadsheet.clear_range("Data", row, col, row, col),
        4 => spreadsheet.set_cell_range_values("Data", row, col, &[vec![Some(Cell::Number(99.0))]]),
        _ => unreachable!(),
    }
}

#[test]
fn shared_formula_partial_replacements_and_clears_are_atomic() {
    let bytes = fixture(SHARED);
    let original = Workbook::open(&bytes).unwrap();
    assert_eq!(
        original.sheets[0].cell(1, 1),
        Some(&Cell::Formula {
            formula: "A2*2".into(),
            cached: Box::new(Cell::Number(60.0)),
        })
    );
    for row in [0, 1] {
        for operation in 0..=4 {
            let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
            // Rejecting the second edit must retain already committed work.
            spreadsheet
                .set_cell_value("Data", 0, 0, Cell::Number(11.0))
                .unwrap();
            let before = spreadsheet.save().unwrap();
            let edited_parts = spreadsheet.edited_parts().to_vec();
            assert!(edit(&mut spreadsheet, row, 1, operation).is_err());
            assert_eq!(spreadsheet.save().unwrap(), before);
            assert_eq!(spreadsheet.edited_parts(), edited_parts);
            assert_eq!(
                spreadsheet.workbook().sheets[0].cell(1, 1),
                original.sheets[0].cell(1, 1)
            );
            assert_eq!(
                Workbook::open(&before).unwrap().sheets[0].cell(1, 1),
                original.sheets[0].cell(1, 1)
            );
        }
    }
}

#[test]
fn array_formula_partial_edits_include_non_formula_followers() {
    let bytes = fixture(ARRAY);
    for row in [0, 1] {
        for operation in 0..=4 {
            let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
            assert!(edit(&mut spreadsheet, row, 1, operation).is_err());
            assert_eq!(spreadsheet.save().unwrap(), bytes);
            assert!(spreadsheet.edited_parts().is_empty());
        }
    }
}

#[test]
fn complete_formula_groups_can_be_replaced_and_cleared() {
    for rows in [SHARED, ARRAY] {
        let bytes = fixture(rows);
        for clear in [false, true] {
            let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
            if clear {
                spreadsheet.clear_range("Data", 0, 1, 1, 1).unwrap();
            } else {
                spreadsheet
                    .set_cell_range_values(
                        "Data",
                        0,
                        1,
                        &[vec![Some(Cell::Number(7.0))], vec![Some(Cell::Number(8.0))]],
                    )
                    .unwrap();
            }
            let saved = spreadsheet.save().unwrap();
            let reopened = Workbook::open(&saved).unwrap();
            assert_eq!(
                reopened.sheets[0].cell(0, 1),
                if clear {
                    None
                } else {
                    Some(&Cell::Number(7.0))
                }
            );
            assert_eq!(
                reopened.sheets[0].cell(1, 1),
                if clear {
                    None
                } else {
                    Some(&Cell::Number(8.0))
                }
            );
            let mut source = ZipArchive::new(Cursor::new(&bytes)).unwrap();
            let mut result = ZipArchive::new(Cursor::new(&saved)).unwrap();
            for index in 0..source.len() {
                let mut part = source.by_index(index).unwrap();
                if part.name() == "xl/worksheets/sheet1.xml" {
                    continue;
                }
                let mut expected = Vec::new();
                part.read_to_end(&mut expected).unwrap();
                let mut actual = Vec::new();
                result
                    .by_name(part.name())
                    .unwrap()
                    .read_to_end(&mut actual)
                    .unwrap();
                assert_eq!(actual, expected, "{}", part.name());
            }
        }
    }
}

#[test]
fn one_cell_formula_groups_remain_editable() {
    for kind in ["shared", "array"] {
        let rows = format!(
            r#"<row r="1"><c r="B1"><f t="{kind}" si="0" ref="B1">1+1</f><v>2</v></c></row>"#
        );
        for operation in 0..=4 {
            let mut spreadsheet = Spreadsheet::open(&fixture(&rows)).unwrap();
            edit(&mut spreadsheet, 0, 1, operation).unwrap();
        }
    }
}

#[test]
fn prefixed_formula_groups_reject_edits_without_losing_unknown_content() {
    for rows in [SHARED, ARRAY] {
        let rows = rows.replace("<f", "<s:f").replace("</f", "</s:f");
        let bytes = fixture(&rows);
        for row in [0, 1] {
            for operation in 0..=4 {
                let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
                assert!(edit(&mut spreadsheet, row, 1, operation).is_err());
                assert_eq!(spreadsheet.save().unwrap(), bytes);
                assert!(spreadsheet.edited_parts().is_empty());
            }
        }
        for clear in [false, true] {
            let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
            assert!(spreadsheet
                .set_cell_range_values(
                    "Data",
                    0,
                    1,
                    &[
                        vec![(!clear).then_some(Cell::Number(7.0))],
                        vec![(!clear).then_some(Cell::Number(8.0))],
                    ],
                )
                .is_err());
            assert_eq!(spreadsheet.save().unwrap(), bytes);
            assert!(spreadsheet.edited_parts().is_empty());
        }
    }
    for kind in ["shared", "array"] {
        let rows = format!(
            r#"<row r="1"><c r="B1"><s:f t="{kind}" si="0" ref="B1">1+1</s:f><v>2</v></c></row>"#
        );
        let bytes = fixture(&rows);
        for operation in 0..=4 {
            let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
            assert!(edit(&mut spreadsheet, 0, 1, operation).is_err());
            assert_eq!(spreadsheet.save().unwrap(), bytes);
        }
    }
}

#[test]
fn cell_value_changes_preserve_foreign_namespace_children() {
    let metadata = r#"<custom:f xmlns:custom="urn:keep">sentinel</custom:f><custom:v xmlns:custom="urn:keep">opaque</custom:v><custom:is xmlns:custom="urn:keep"><custom:t>keep</custom:t></custom:is>"#;
    let bytes = fixture(&format!(
        r#"<row r="1"><c r="A1"><v>1</v>{metadata}</c></row>"#
    ));
    for operation in [0, 1, 2, 4] {
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        edit(&mut spreadsheet, 0, 0, operation).unwrap();
        let saved = spreadsheet.save().unwrap();
        let mut zip = ZipArchive::new(Cursor::new(saved)).unwrap();
        let mut xml = String::new();
        zip.by_name("xl/worksheets/sheet1.xml")
            .unwrap()
            .read_to_string(&mut xml)
            .unwrap();
        assert!(xml.contains(metadata), "{xml}");
    }
}

#[test]
fn bare_and_prefixed_formula_duplicates_are_ambiguous() {
    let rows = SHARED.replace("<f t=\"shared\" ref=", "<s:f>1+1</s:f><f t=\"shared\" ref=");
    let bytes = fixture(&rows);
    let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
    assert!(spreadsheet.clear_range("Data", 0, 1, 1, 1).is_err());
    assert_eq!(spreadsheet.save().unwrap(), bytes);
}

#[test]
fn ambiguous_sheet_data_and_source_rows_cannot_orphan_formula_groups() {
    let duplicate_rows = r#"<row r="1"><c r="A1"><f t="shared" si="0" ref="A1:B1">1+1</f><v>2</v></c></row><row r="1"><c r="B1"><f t="shared" si="0"/><v>2</v></c></row>"#;
    for rows in [
        duplicate_rows.to_owned(),
        SHARED.replace(r#"<row r="2">"#, r#"<row r="1">"#),
        ARRAY.replace(r#"<row r="2">"#, r#"<row r="1">"#),
        SHARED.replace(r#"r="B2""#, r#"r="b2""#),
        SHARED.replace(r#"r="B2""#, r#"r="$B$2""#),
        ARRAY.replace(r#"r="B2""#, r#"r="B02""#),
    ] {
        let bytes = fixture(&rows);
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        assert!(spreadsheet.clear_range("Data", 0, 0, 1, 1).is_err());
        assert_eq!(spreadsheet.save().unwrap(), bytes);
        assert!(spreadsheet.edited_parts().is_empty());
    }
    for data in [
        format!("<s:sheetData/><sheetData>{SHARED}</sheetData>"),
        format!("<sheetData>{SHARED}</sheetData><s:sheetData/>"),
        format!("<s:sheetData>{SHARED}</s:sheetData>"),
    ] {
        let bytes = fixture_sheet_data(&data);
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        assert!(spreadsheet.clear_range("Data", 0, 1, 1, 1).is_err());
        assert_eq!(spreadsheet.save().unwrap(), bytes);
        assert!(spreadsheet.edited_parts().is_empty());
    }
}

#[test]
fn empty_array_formula_followers_allow_complete_group_edits() {
    for follower in [r#"<f t="array"/>"#, r#"<f t="array" ref="B1:B2"/>"#] {
        let rows = ARRAY.replace(r#"<c r="B2"><v>"#, &format!(r#"<c r="B2">{follower}<v>"#));
        let bytes = fixture(&rows);
        for clear in [false, true] {
            let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
            if clear {
                spreadsheet.clear_range("Data", 0, 1, 1, 1).unwrap();
            } else {
                spreadsheet
                    .set_cell_range_values(
                        "Data",
                        0,
                        1,
                        &[vec![Some(Cell::Number(7.0))], vec![Some(Cell::Number(8.0))]],
                    )
                    .unwrap();
            }
            let saved = spreadsheet.save().unwrap();
            let reopened = Workbook::open(&saved).unwrap();
            assert_eq!(
                reopened.sheets[0].cell(1, 1),
                if clear {
                    None
                } else {
                    Some(&Cell::Number(8.0))
                }
            );
        }
    }
    let orphan = fixture(r#"<row r="1"><c r="B1"><f t="array"/><v>20</v></c></row>"#);
    let mut spreadsheet = Spreadsheet::open(&orphan).unwrap();
    assert!(spreadsheet.clear_range("Data", 0, 1, 0, 1).is_err());
    assert_eq!(spreadsheet.save().unwrap(), orphan);
}

#[test]
fn unrelated_edits_preserve_formula_cells_with_implicit_coordinates() {
    let bytes = fixture(&SHARED.replace(r#"<c r="B2">"#, "<c>"));
    for operation in 0..=4 {
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        edit(&mut spreadsheet, 0, 0, operation).unwrap();
    }
    let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
    assert_eq!(
        spreadsheet.append_row("Data", [Cell::Number(5.0)]).unwrap(),
        2
    );
}

#[test]
fn implicit_array_followers_cannot_survive_complete_group_clears() {
    for rows in [
        ARRAY.replace(r#"<c r="B2">"#, "<c>"),
        ARRAY
            .replace(r#"<row r="2">"#, "<row>")
            .replace(r#"<c r="B2">"#, "<c>"),
    ] {
        let bytes = fixture(&rows);
        for operation in 0..3 {
            let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
            let result = match operation {
                0 => spreadsheet.clear_range("Data", 0, 1, 1, 1),
                1 => spreadsheet.set_cell_range_values("Data", 0, 1, &[vec![None], vec![None]]),
                _ => spreadsheet.set_cell_range_values(
                    "Data",
                    0,
                    1,
                    &[vec![Some(Cell::Number(7.0))], vec![Some(Cell::Number(8.0))]],
                ),
            };
            assert!(result.is_err());
            assert_eq!(spreadsheet.save().unwrap(), bytes);
            assert!(spreadsheet.edited_parts().is_empty());
        }
    }
}

#[test]
fn shared_group_cache_only_updates_keep_followers_and_formula_nodes() {
    let bytes = fixture(SHARED);
    for row in [0, 1] {
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        spreadsheet
            .set_formula_cached_values(&[("Data", row, 1, Cell::Number(42.0))])
            .unwrap();
        let saved = spreadsheet.save().unwrap();
        let reopened = Workbook::open(&saved).unwrap();
        for target in [0, 1] {
            assert_eq!(
                reopened.sheets[0].cell(target, 1),
                Some(&Cell::Formula {
                    formula: format!("A{}*2", target + 1),
                    cached: Box::new(Cell::Number(if row == target {
                        42.0
                    } else if target == 0 {
                        20.0
                    } else {
                        60.0
                    })),
                })
            );
        }
    }
}

#[test]
fn ambiguous_or_incomplete_shared_groups_are_not_replaced() {
    for rows in [
        SHARED.replace(" ref=\"B1:B2\"", ""),
        SHARED.replace("B1:B2", "B1"),
        SHARED.replace("si=\"0\"", "si=\"invalid\""),
        SHARED.replace(" si=\"0\"", ""),
        SHARED.replace("B1:B2", "B2:B1"),
        SHARED.replace("B1:B2", "B1:XFE2"),
        SHARED.replace(
            "<f t=\"shared\" si=\"0\"/>",
            "<f t=\"shared\" si=\"0\">A2*2</f>",
        ),
        SHARED.replace(
            "<f t=\"shared\" ref=\"B1:B2\" si=\"0\">A1*2</f>",
            "<f>1+1</f><f t=\"shared\" ref=\"B1:B2\" si=\"0\">A1*2</f>",
        ),
    ] {
        let bytes = fixture(&rows);
        for operation in 0..=4 {
            let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
            assert!(edit(&mut spreadsheet, 0, 1, operation).is_err(), "{rows}");
            assert_eq!(spreadsheet.save().unwrap(), bytes);
            assert!(spreadsheet.edited_parts().is_empty());
        }
    }
}

#[test]
fn equivalent_numeric_shared_indexes_still_protect_the_group() {
    let bytes = fixture(&SHARED.replace("si=\"0\"/>", "si=\"00\"/>"));
    let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
    assert!(spreadsheet
        .set_cell_value("Data", 1, 1, Cell::Number(99.0))
        .is_err());
    assert_eq!(spreadsheet.save().unwrap(), bytes);
    spreadsheet
        .set_cell_range_values("Data", 0, 1, &[vec![None], vec![None]])
        .unwrap();
}

#[test]
fn orphaned_and_out_of_range_shared_followers_cannot_be_replaced() {
    let orphan = SHARED.replace(
        r#"<c r="B1"><f t="shared" ref="B1:B2" si="0">A1*2</f><v>20</v></c>"#,
        "",
    );
    for rows in [orphan, SHARED.replace("B1:B2", "B1")] {
        let bytes = fixture(&rows);
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        assert!(spreadsheet
            .set_cell_range_values(
                "Data",
                0,
                1,
                &[vec![Some(Cell::Number(7.0))], vec![Some(Cell::Number(8.0))]],
            )
            .is_err());
        assert_eq!(spreadsheet.save().unwrap(), bytes);
        assert!(spreadsheet.edited_parts().is_empty());
    }
}

#[test]
fn appending_into_a_declared_formula_group_is_atomic() {
    for kind in ["shared", "array"] {
        let rows = format!(
            r#"<row r="1"><c r="B1"><f t="{kind}" ref="B1:B2" si="0">A1*2</f><v>20</v></c></row>"#
        );
        let bytes = fixture(&rows);
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        assert!(spreadsheet
            .append_row("Data", [Cell::Number(30.0), Cell::Number(99.0)])
            .is_err());
        assert_eq!(spreadsheet.save().unwrap(), bytes);
        assert!(spreadsheet.edited_parts().is_empty());
        // An appended cell outside the declared group remains editable.
        assert_eq!(
            spreadsheet
                .append_row("Data", [Cell::Number(30.0)])
                .unwrap(),
            1
        );
    }
}

#[test]
fn rejected_shared_anchor_edit_keeps_calculation_chain_wiring() {
    let bytes = fixture(SHARED);
    let mut source = ZipArchive::new(Cursor::new(&bytes)).unwrap();
    let mut output = ZipWriter::new(Cursor::new(Vec::new()));
    for index in 0..source.len() {
        let mut part = source.by_index(index).unwrap();
        let mut contents = Vec::new();
        part.read_to_end(&mut contents).unwrap();
        let contents = match part.name() {
            "[Content_Types].xml" => String::from_utf8(contents).unwrap().replace(
                "</Types>",
                r#"<Override PartName="/xl/calcChain.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.calcChain+xml"/></Types>"#,
            ).into_bytes(),
            "xl/_rels/workbook.xml.rels" => String::from_utf8(contents).unwrap().replace(
                "</Relationships>",
                r#"<Relationship Id="rIdCalc" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/calcChain" Target="calcChain.xml"/></Relationships>"#,
            ).into_bytes(),
            _ => contents,
        };
        output
            .start_file(part.name(), SimpleFileOptions::default())
            .unwrap();
        output.write_all(&contents).unwrap();
    }
    output
        .start_file("xl/calcChain.xml", SimpleFileOptions::default())
        .unwrap();
    output.write_all(br#"<calcChain xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><c r="B1" i="1"/><c r="B2" i="1"/></calcChain>"#).unwrap();
    let bytes = output.finish().unwrap().into_inner();
    let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
    assert!(spreadsheet
        .set_cell_value("Data", 0, 1, Cell::Number(99.0))
        .is_err());
    assert_eq!(spreadsheet.save().unwrap(), bytes);
    assert!(spreadsheet.edited_parts().is_empty());
    spreadsheet
        .set_cell_range_values("Data", 0, 1, &[vec![None], vec![None]])
        .unwrap();
    let saved = spreadsheet.save().unwrap();
    let mut archive = ZipArchive::new(Cursor::new(&saved)).unwrap();
    assert!(archive.by_name("xl/calcChain.xml").is_err());
}
