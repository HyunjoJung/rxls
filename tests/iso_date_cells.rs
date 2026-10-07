//! Generated public ISO date/workbook-epoch regressions for #128.
#![cfg(feature = "xlsx")]

use std::io::{Cursor, Read, Write};

use rxls::{Cell, Spreadsheet, Workbook};
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

const STYLES: &str = r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="2"><numFmt numFmtId="164" formatCode="yyyy-mm-dd hh:mm:ss"/><numFmt numFmtId="165" formatCode="hh:mm:ss"/></numFmts><fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="5"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="14" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="22" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="164" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="165" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#;

fn fixture(epoch1904: bool, cells: &str) -> Vec<u8> {
    let mut workbook = Workbook::new();
    workbook.date1904 = epoch1904;
    workbook.add_sheet("Data").write(0, 0, "seed");
    workbook.add_sheet("Keep").write(0, 0, "untouched");
    let bytes = workbook.to_xlsx_checked().unwrap();
    let sheet = format!(
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1">{cells}</row></sheetData></worksheet>"#
    );
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
            .write_all(match part.name() {
                "xl/styles.xml" => STYLES.as_bytes(),
                "xl/worksheets/sheet1.xml" => sheet.as_bytes(),
                _ => &contents,
            })
            .unwrap();
    }
    output.finish().unwrap().into_inner()
}

fn assert_serial(cell: &Cell, expected: f64) {
    assert!((cell.get_datetime().unwrap() - expected).abs() < 1e-10);
}

#[test]
fn iso_calendar_dates_and_formula_caches_use_the_workbook_epoch() {
    for epoch1904 in [false, true] {
        let serial = if epoch1904 { 43_831.5 } else { 45_293.5 };
        let bytes = fixture(
            epoch1904,
            &format!(
                r#"<c r="A1" t="d" s="3"><v>2024-01-02T12:00:00</v></c><c r="B1" t="d" s="3"><f>DATE(2024,1,2)+TIME(12,0,0)</f><v>2024-01-02T12:00:00</v></c><c r="C1" s="3"><v>{serial}</v></c><c r="D1" t="d"><v>2024-01-02T12:00:00</v></c><c r="E1" t="d" s="1"><v>2024-01-02T12:00:00</v></c><c r="F1" t="d" s="2"><v>2024-01-02T12:00:00</v></c>"#
            ),
        );
        let workbook = Workbook::open(&bytes).unwrap();
        assert_eq!(workbook.has_1904_epoch(), epoch1904);
        let sheet = &workbook.sheets[0];
        for col in 0..6 {
            let cell = sheet.cell(0, col).unwrap();
            assert_serial(cell, serial);
            let date = cell.as_datetime(epoch1904).unwrap();
            assert_eq!(date.to_string(), "2024-01-02 12:00:00");
            #[cfg(feature = "chrono")]
            assert_eq!(
                cell.as_naive_datetime(epoch1904).unwrap().to_string(),
                "2024-01-02 12:00:00"
            );
        }
        assert_eq!(sheet.cell(0, 0), sheet.cell(0, 2));
        assert_eq!(sheet.formatted(0, 0), Some("2024-01-02 12:00:00"));
        assert_eq!(sheet.formatted(0, 1), sheet.formatted(0, 0));
        assert_eq!(sheet.formatted(0, 2), sheet.formatted(0, 0));
        assert_eq!(sheet.formatted(0, 3), Some("2024-01-02T12:00:00"));
        assert_eq!(sheet.formatted(0, 4), Some("2024-01-02"));
        assert_eq!(sheet.formatted(0, 5), Some("2024-01-02 12:00:00"));
        let Some(Cell::Formula { formula, cached }) = sheet.cell(0, 1) else {
            panic!("missing date formula cache");
        };
        assert_eq!(formula, "DATE(2024,1,2)+TIME(12,0,0)");
        assert_eq!(cached.as_ref(), sheet.cell(0, 0).unwrap());
    }
}

#[test]
fn iso_calendar_boundaries_match_numeric_dates_without_phantom_day_drift() {
    for (epoch1904, iso, serial, display) in [
        (false, "1900-01-01", 1.0, "1900-01-01 00:00:00"),
        (false, "1900-02-28T12:00:00", 59.5, "1900-02-28 12:00:00"),
        (false, "1900-03-01", 61.0, "1900-03-01 00:00:00"),
        (false, "1904-01-01", 1462.0, "1904-01-01 00:00:00"),
        (true, "1904-01-01", 0.0, "1904-01-01 00:00:00"),
        (true, "1904-01-01T12:00:00", 0.5, "1904-01-01 12:00:00"),
        (false, "2024-02-29", 45351.0, "2024-02-29 00:00:00"),
        (true, "2024-02-29", 43889.0, "2024-02-29 00:00:00"),
    ] {
        let bytes = fixture(
            epoch1904,
            &format!(
                r#"<c r="A1" t="d" s="3"><v>{iso}</v></c><c r="B1" s="3"><v>{serial}</v></c>"#
            ),
        );
        let workbook = Workbook::open(&bytes).unwrap();
        let sheet = &workbook.sheets[0];
        assert_serial(sheet.cell(0, 0).unwrap(), serial);
        assert_eq!(sheet.cell(0, 0), sheet.cell(0, 1));
        assert_eq!(sheet.formatted(0, 0), Some(display));
        assert_eq!(sheet.formatted(0, 0), sheet.formatted(0, 1));
        assert_eq!(
            sheet
                .cell(0, 0)
                .unwrap()
                .as_datetime(epoch1904)
                .unwrap()
                .to_string(),
            display
        );
    }
}

#[test]
fn iso_time_only_values_remain_day_fractions_in_both_epochs() {
    for epoch1904 in [false, true] {
        for (iso, serial) in [("00:00:00", 0.0), ("12:00:00", 0.5), ("18:00:00", 0.75)] {
            let bytes = fixture(
                epoch1904,
                &format!(
                    r#"<c r="A1" t="d" s="4"><v>{iso}</v></c><c r="B1" s="4"><v>{serial}</v></c><c r="C1" t="d" s="4"><f>TIME(12,0,0)</f><v>{iso}</v></c>"#
                ),
            );
            let workbook = Workbook::open(&bytes).unwrap();
            let sheet = &workbook.sheets[0];
            for col in 0..3 {
                assert_serial(sheet.cell(0, col).unwrap(), serial);
                assert_eq!(sheet.formatted(0, col), Some(iso));
                assert_eq!(
                    sheet
                        .cell(0, col)
                        .unwrap()
                        .as_datetime(epoch1904)
                        .unwrap()
                        .time_string(),
                    iso
                );
            }
        }
    }
}

#[test]
fn rewriting_iso_dates_keeps_calendar_values_and_epoch_for_each_writer() {
    for epoch1904 in [false, true] {
        let serial = if epoch1904 { 43831.5 } else { 45293.5 };
        let bytes = fixture(
            epoch1904,
            r#"<c r="A1" t="d" s="3"><v>2024-01-02T12:00:00</v></c><c r="B1" t="d" s="3"><f>DATE(2024,1,2)+TIME(12,0,0)</f><v>2024-01-02T12:00:00</v></c>"#,
        );
        let workbook = Workbook::open(&bytes).unwrap();
        for saved in [workbook.to_xlsx(), workbook.to_xlsx_checked().unwrap()] {
            let reopened = Workbook::open(&saved).unwrap();
            assert_eq!(reopened.has_1904_epoch(), epoch1904);
            for col in 0..2 {
                let cell = reopened.sheets[0].cell(0, col).unwrap();
                assert_serial(cell, serial);
                assert_eq!(
                    cell.as_datetime(epoch1904).unwrap().to_string(),
                    "2024-01-02 12:00:00"
                );
                assert_eq!(
                    reopened.sheets[0].formatted(0, col),
                    Some("2024-01-02 12:00:00")
                );
            }
            assert_eq!(reopened.sheets[0].cell(0, 1), workbook.sheets[0].cell(0, 1));
        }
    }
}

#[test]
fn preserving_edits_leave_iso_date_source_parts_unchanged() {
    for epoch1904 in [false, true] {
        let bytes = fixture(
            epoch1904,
            r#"<c r="A1" t="d" s="3"><v>2024-01-02T12:00:00</v></c>"#,
        );
        let mut spreadsheet = Spreadsheet::open(&bytes).unwrap();
        spreadsheet
            .set_cell_value("Keep", 0, 0, "edited".into())
            .unwrap();
        let saved = spreadsheet.save().unwrap();
        let mut source = ZipArchive::new(Cursor::new(&bytes)).unwrap();
        let mut result = ZipArchive::new(Cursor::new(&saved)).unwrap();
        for part in [
            "xl/worksheets/sheet1.xml",
            "xl/workbook.xml",
            "xl/styles.xml",
        ] {
            let mut before = Vec::new();
            let mut after = Vec::new();
            source
                .by_name(part)
                .unwrap()
                .read_to_end(&mut before)
                .unwrap();
            result
                .by_name(part)
                .unwrap()
                .read_to_end(&mut after)
                .unwrap();
            assert_eq!(before, after, "{part}");
        }
        let reopened = Workbook::open(&saved).unwrap();
        assert_eq!(
            reopened.sheets[0]
                .cell(0, 0)
                .unwrap()
                .as_datetime(epoch1904)
                .unwrap()
                .to_string(),
            "2024-01-02 12:00:00"
        );
    }
}
