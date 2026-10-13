//! Generated cell-string storage regressions for ST_Xstring (#131).
#![cfg(feature = "xlsx")]

use std::io::{Cursor, Read, Write};

use rxls::{Cell, Font, Spreadsheet, TextRun, Workbook};
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

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

fn seed() -> Vec<u8> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Data");
    sheet.write(0, 0, "seed");
    sheet.write(
        0,
        1,
        Cell::Formula {
            formula: "\"seed\"".into(),
            cached: Box::new(Cell::Text("seed".into())),
        },
    );
    workbook.add_sheet("Keep").write(0, 0, "untouched");
    workbook.to_xlsx_checked().unwrap()
}

fn worksheet(bytes: &[u8], cell: &str) -> Vec<u8> {
    replace_part(
        bytes,
        "xl/worksheets/sheet1.xml",
        &format!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" {cell}</c></row></sheetData></worksheet>"#
        ),
    )
}

fn read_text(storage: u8, xml_text: &str) -> Cell {
    let bytes = seed();
    let bytes = match storage {
        0 => {
            let bytes = replace_part(
                &bytes,
                "xl/sharedStrings.xml",
                &format!(
                    r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1"><si><t>{xml_text}</t></si></sst>"#
                ),
            );
            worksheet(&bytes, r#"t="s"><v>0</v>"#)
        }
        1 => worksheet(
            &bytes,
            &format!(r#"t="inlineStr"><is><t>{xml_text}</t></is>"#),
        ),
        2 => worksheet(
            &bytes,
            &format!(r#"t="str"><f>&quot;_x0041_&quot;</f><v>{xml_text}</v>"#),
        ),
        _ => unreachable!(),
    };
    let cell = Workbook::open(&bytes).unwrap().sheets[0]
        .cell(0, 0)
        .unwrap()
        .clone();
    match cell {
        Cell::Formula { formula, cached } => {
            assert_eq!(formula, "\"_x0041_\"");
            *cached
        }
        value => value,
    }
}

fn assert_read_storage(storage: u8) {
    // Token scanning agrees with installed Excel 16.0 build 20326. Literal
    // overlap protection differs from XlsxWriter 3.2.9's single-token regex.
    for (xml, expected) in [
        ("literal _x005F_x0041_", "literal _x0041_"),
        ("_x005F_x005F_x0041_", "_x005FA"),
        ("_x005F_x005F_x005F_x0041_", "_x005F_x0041_"),
        ("_x005F_x0041_x0042_", "_x0041B"),
        ("line_x000D_carriage", "line\rcarriage"),
        ("line_x000D_\r\nnext", "line\r\nnext"),
        ("raw\r\nline\rcarriage", "raw\nline\ncarriage"),
        ("reference&#13;\nnext", "reference\r\nnext"),
        ("line_x000D_<![CDATA[\r\n]]>next", "line\r\nnext"),
        ("low_x0001_control", "low\u{1}control"),
        ("zero_x0000_", "zero\0"),
        ("_xD83D__xDE00_", "😀"),
        ("_xD800_", "_xD800_"),
        ("_x0041_x0042_", "Ax0042_"),
        ("_x0041__x0042_", "AB"),
        ("_x00&#52;1_ &amp; 한글", "A & 한글"),
        ("_x00<![CDATA[41]]>_", "A"),
        ("_xGGGG_ _x123_ _X0041_", "_xGGGG_ _x123_ _X0041_"),
    ] {
        assert_eq!(
            read_text(storage, xml),
            Cell::Text(expected.into()),
            "storage {storage}: {xml}"
        );
    }
}

#[test]
fn shared_strings_decode_xstring_once_after_xml_fragments() {
    assert_read_storage(0);
}

#[test]
fn inline_strings_decode_xstring_once_after_xml_fragments() {
    assert_read_storage(1);
}

#[test]
fn string_formula_caches_decode_without_changing_formula_source() {
    assert_read_storage(2);
}

#[test]
fn rich_text_decodes_each_text_element_before_joining_runs() {
    for (runs, expected) in [
        ("<r><t>_x00</t></r><r><t>41_</t></r>", "_x0041_"),
        (
            "<r><t>_x005F_x0041_</t></r><r><t>_x000D_한글</t></r>",
            "_x0041_\r한글",
        ),
    ] {
        for storage in [0, 1] {
            let bytes = if storage == 0 {
                let bytes = replace_part(
                    &seed(),
                    "xl/sharedStrings.xml",
                    &format!(
                        r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si>{runs}</si></sst>"#
                    ),
                );
                worksheet(&bytes, r#"t="s"><v>0</v>"#)
            } else {
                worksheet(&seed(), &format!(r#"t="inlineStr"><is>{runs}</is>"#))
            };
            assert_eq!(
                Workbook::open(&bytes).unwrap().sheets[0]
                    .cell(0, 0)
                    .cloned(),
                Some(Cell::Text(expected.into()))
            );
        }
    }
}

#[test]
fn encodable_controls_keep_the_semantic_utf16_length_limit_and_atomicity() {
    let accepted = "\u{1}".repeat(32_767);
    let mut workbook = Workbook::new();
    workbook.add_sheet("Data").write(0, 0, accepted.clone());
    let bytes = workbook.to_xlsx_checked().unwrap();
    assert_eq!(
        Workbook::open(&bytes).unwrap().sheets[0].cell(0, 0),
        Some(&Cell::Text(accepted.clone()))
    );
    let mut spreadsheet = Spreadsheet::open(&seed()).unwrap();
    spreadsheet
        .set_cell_value("Data", 0, 0, Cell::Text(accepted))
        .unwrap();
    let before = spreadsheet.save().unwrap();
    let edited_parts = spreadsheet.edited_parts().to_vec();
    assert!(spreadsheet
        .set_cell_value("Data", 0, 0, Cell::Text("\u{1}".repeat(32_768)))
        .is_err());
    assert_eq!(spreadsheet.save().unwrap(), before);
    assert_eq!(spreadsheet.edited_parts(), edited_parts);
}

fn xml_part(bytes: &[u8], name: &str) -> String {
    let mut zip = ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut xml = String::new();
    zip.by_name(name).unwrap().read_to_string(&mut xml).unwrap();
    xml
}

#[test]
fn writer_protects_literal_escape_tokens_and_carriage_returns() {
    for checked in [false, true] {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "literal _x0041_");
        sheet.write(1, 0, "_x005F_x0041_");
        sheet.write(2, 0, "line\rcarriage");
        let bytes = if checked {
            workbook.to_xlsx_checked().unwrap()
        } else {
            workbook.to_xlsx()
        };
        let xml = xml_part(&bytes, "xl/sharedStrings.xml");
        for encoded in [
            "literal _x005F_x0041_",
            "_x005F_x005F_x005F_x0041_",
            "line_x000D_carriage",
        ] {
            assert!(xml.contains(encoded), "{xml}");
        }
        let reopened = Workbook::open(&bytes).unwrap();
        for (row, text) in ["literal _x0041_", "_x005F_x0041_", "line\rcarriage"]
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                reopened.sheets[0].cell(row as u32, 0).cloned(),
                Some(Cell::Text(text.into()))
            );
        }
    }
}

#[test]
fn checked_writer_accepts_encodable_cell_controls_in_plain_rich_and_cache_text() {
    let text = "zero\0 low\u{1} CR\r LF\n tab\t 한글😀 &<> \u{ffff}";
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("Data");
    sheet.write(0, 0, text);
    sheet.write_rich(1, 0, [TextRun::new(text, Font::default())]);
    sheet.write(
        2,
        0,
        Cell::Formula {
            formula: "\"_x0041_\"".into(),
            cached: Box::new(Cell::Text(text.into())),
        },
    );
    let bytes = workbook.to_xlsx_checked().unwrap();
    let reopened = Workbook::open(&bytes).unwrap();
    assert_eq!(
        reopened.sheets[0].cell(0, 0).cloned(),
        Some(Cell::Text(text.into()))
    );
    assert_eq!(
        reopened.sheets[0].cell(1, 0).cloned(),
        Some(Cell::Text(text.into()))
    );
    assert_eq!(
        reopened.sheets[0].cell(2, 0).cloned(),
        Some(Cell::Formula {
            formula: "\"_x0041_\"".into(),
            cached: Box::new(Cell::Text(text.into()))
        })
    );
}

#[test]
fn cell_edits_and_cache_batches_preserve_xstring_text_and_unrelated_parts() {
    let bytes = seed();
    let text = "literal _x005F_x0041_\r\0\u{1}\n\t한글😀 &<>";
    for operation in 0..5 {
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        let (row, col) = match operation {
            0 => {
                spreadsheet
                    .set_cell_value("Data", 0, 0, Cell::Text(text.into()))
                    .unwrap();
                (0, 0)
            }
            1 => {
                spreadsheet
                    .set_cell_range_values("Data", 0, 0, &[vec![Some(Cell::Text(text.into()))]])
                    .unwrap();
                (0, 0)
            }
            2 => (
                spreadsheet
                    .append_row("Data", [Cell::Text(text.into())])
                    .unwrap(),
                0,
            ),
            3 => {
                spreadsheet
                    .set_cell_formula("Data", 0, 0, "\"_x0041_\"", Cell::Text(text.into()))
                    .unwrap();
                (0, 0)
            }
            _ => {
                spreadsheet
                    .set_formula_cached_values(&[("Data", 0, 1, Cell::Text(text.into()))])
                    .unwrap();
                (0, 1)
            }
        };
        let saved = spreadsheet.save().unwrap();
        let reopened = Workbook::open(&saved).unwrap();
        let expected = if operation < 3 {
            Cell::Text(text.into())
        } else {
            Cell::Formula {
                formula: if operation == 3 {
                    "\"_x0041_\""
                } else {
                    "\"seed\""
                }
                .into(),
                cached: Box::new(Cell::Text(text.into())),
            }
        };
        assert_eq!(reopened.sheets[0].cell(row, col), Some(&expected));
        let mut source = ZipArchive::new(Cursor::new(&bytes)).unwrap();
        let mut result = ZipArchive::new(Cursor::new(&saved)).unwrap();
        for index in 0..source.len() {
            let mut part = source.by_index(index).unwrap();
            if part.name() == "xl/worksheets/sheet1.xml" {
                continue;
            }
            let mut original = Vec::new();
            let mut actual = Vec::new();
            part.read_to_end(&mut original).unwrap();
            result
                .by_name(part.name())
                .unwrap()
                .read_to_end(&mut actual)
                .unwrap();
            assert_eq!(actual, original, "{}", part.name());
        }
    }
}

#[test]
fn text_formula_caches_preserve_edge_whitespace_in_authored_and_edited_xml() {
    let text = " \t_x0041_\r\n padded \t";
    for operation in 0..4 {
        let (bytes, col) = if operation < 2 {
            let mut workbook = Workbook::new();
            workbook.add_sheet("Data").write(
                0,
                0,
                Cell::Formula {
                    formula: "\"padded\"".into(),
                    cached: Box::new(Cell::Text(text.into())),
                },
            );
            (
                if operation == 0 {
                    workbook.to_xlsx_checked().unwrap()
                } else {
                    workbook.to_xlsx()
                },
                0,
            )
        } else {
            let mut spreadsheet = Spreadsheet::open(&seed()).unwrap();
            if operation == 2 {
                spreadsheet
                    .set_cell_formula("Data", 0, 1, "\"padded\"", Cell::Text(text.into()))
                    .unwrap();
            } else {
                spreadsheet
                    .set_formula_cached_values(&[("Data", 0, 1, Cell::Text(text.into()))])
                    .unwrap();
            }
            (spreadsheet.save().unwrap(), 1)
        };
        let xml = xml_part(&bytes, "xl/worksheets/sheet1.xml");
        assert!(xml.contains("<v xml:space=\"preserve\"> \t_x005F_x0041__x000D_\n padded \t</v>"));
        let reopened = Workbook::open(&bytes).unwrap();
        let Some(Cell::Formula { cached, .. }) = reopened.sheets[0].cell(0, col) else {
            panic!("expected a text formula cache");
        };
        assert_eq!(cached.as_ref(), &Cell::Text(text.into()));
    }
}
