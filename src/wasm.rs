//! Portable read-only adapter used by WebAssembly consumers.
//!
//! The native `*_bytes` functions are the testable core. The repository's
//! standalone `bindings/wasm` crate exposes them to JavaScript with
//! `wasm-bindgen` without changing this crate's native artifact types.

use crate::{CsvExportError, Error, MarkupExportError, Result, Workbook, WorkbookReport};

/// Maximum UTF-8 output returned by one browser-oriented CSV, HTML, or
/// Markdown adapter call (16 MiB).
///
/// This is intentionally lower than [`crate::DEFAULT_EXPORT_MAX_BYTES`]
/// because WebAssembly must retain the Rust string while `wasm-bindgen` copies
/// it into a JavaScript string.
pub const MAX_EXPORT_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Extract workbook text from spreadsheet bytes.
///
/// # Errors
///
/// Returns the same typed parse and empty-text errors as [`crate::extract_text`].
pub fn extract_text_bytes(bytes: &[u8]) -> Result<String> {
    crate::extract_text(bytes)
}

/// Export one worksheet as CSV from spreadsheet bytes.
///
/// # Errors
///
/// Returns a typed parse error for invalid input, [`Error::SheetOutOfRange`]
/// when `sheet_index` does not identify a grid worksheet, or
/// [`Error::ExportOutputTooLarge`] when the result would exceed
/// [`MAX_EXPORT_OUTPUT_BYTES`].
pub fn to_csv_bytes(bytes: &[u8], sheet_index: usize) -> Result<String> {
    to_csv_bytes_with_limit(bytes, sheet_index, MAX_EXPORT_OUTPUT_BYTES)
}

/// Export one worksheet as an HTML table fragment from spreadsheet bytes.
///
/// # Errors
///
/// Returns a typed parse error for invalid input, [`Error::SheetOutOfRange`]
/// when `sheet_index` does not identify a grid worksheet, or a typed output or
/// merged-range work-limit error.
pub fn to_html_bytes(bytes: &[u8], sheet_index: usize) -> Result<String> {
    to_html_bytes_with_limit(bytes, sheet_index, MAX_EXPORT_OUTPUT_BYTES)
}

/// Export one worksheet as Markdown from spreadsheet bytes.
///
/// # Errors
///
/// Returns a typed parse error for invalid input, [`Error::SheetOutOfRange`]
/// when `sheet_index` does not identify a grid worksheet, or a typed output or
/// merged-range work-limit error.
pub fn to_markdown_bytes(bytes: &[u8], sheet_index: usize) -> Result<String> {
    to_markdown_bytes_with_limit(bytes, sheet_index, MAX_EXPORT_OUTPUT_BYTES)
}

fn to_csv_bytes_with_limit(bytes: &[u8], sheet_index: usize, limit: usize) -> Result<String> {
    let workbook = Workbook::open(bytes)?;
    let sheet = worksheet(&workbook, sheet_index)?;
    crate::export::export_csv_legacy(sheet, ',', limit).map_err(map_csv_export_error)
}

fn to_html_bytes_with_limit(bytes: &[u8], sheet_index: usize, limit: usize) -> Result<String> {
    let workbook = Workbook::open(bytes)?;
    let sheet = worksheet(&workbook, sheet_index)?;
    crate::export_html(sheet, limit).map_err(map_markup_export_error)
}

fn to_markdown_bytes_with_limit(bytes: &[u8], sheet_index: usize, limit: usize) -> Result<String> {
    let workbook = Workbook::open(bytes)?;
    let sheet = worksheet(&workbook, sheet_index)?;
    crate::export_markdown(sheet, limit).map_err(map_markup_export_error)
}

fn worksheet(workbook: &Workbook, sheet_index: usize) -> Result<&crate::Sheet> {
    workbook
        .sheets
        .get(sheet_index)
        .filter(|sheet| sheet.is_worksheet)
        .ok_or(Error::SheetOutOfRange)
}

fn map_csv_export_error(error: CsvExportError) -> Error {
    match error {
        CsvExportError::InvalidDelimiter(delimiter) => Error::ExportInvalidDelimiter {
            format: "CSV",
            delimiter,
        },
        CsvExportError::OutputTooLarge { limit } => Error::ExportOutputTooLarge {
            format: "CSV",
            limit,
        },
    }
}

fn map_markup_export_error(error: MarkupExportError) -> Error {
    match error {
        MarkupExportError::OutputTooLarge { format, limit } => Error::ExportOutputTooLarge {
            format: format.as_str(),
            limit,
        },
        MarkupExportError::WorkLimitExceeded { format, limit } => Error::ExportWorkLimitExceeded {
            format: format.as_str(),
            limit,
        },
    }
}

/// Build the machine-readable diagnose JSON report from spreadsheet bytes.
///
/// # Errors
///
/// Returns a typed parse error when the bytes are malformed, encrypted,
/// unsupported by the enabled format features, or exceed a resource bound.
pub fn report_json_bytes(bytes: &[u8]) -> Result<String> {
    let workbook = Workbook::open(bytes)?;
    #[cfg(feature = "xlsx")]
    let report =
        WorkbookReport::from_workbook_with_package(format_from_bytes(bytes), &workbook, bytes);
    #[cfg(not(feature = "xlsx"))]
    let report = WorkbookReport::from_workbook(format_from_bytes(bytes), &workbook);
    Ok(report.to_json())
}

fn format_from_bytes(bytes: &[u8]) -> &'static str {
    const OLE2_MAGIC: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    if bytes.starts_with(OLE2_MAGIC) {
        return "xls";
    }
    #[cfg(feature = "xlsb")]
    if crate::xlsb::is_xlsb(bytes) {
        return "xlsb";
    }
    #[cfg(feature = "ods")]
    if crate::ods::is_ods(bytes) {
        return "ods";
    }
    #[cfg(feature = "xlsx")]
    if crate::xlsx::is_xlsx(bytes) {
        return "xlsx";
    }
    if bytes.starts_with(b"PK") {
        "zip-spreadsheet"
    } else {
        "unknown"
    }
}

#[cfg(all(test, feature = "xlsx"))]
mod tests {
    use super::*;
    use crate::{Cell, Workbook};

    fn fixture_bytes() -> Vec<u8> {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "item");
        sheet.write(0, 1, 2.0);
        sheet.write(
            1,
            1,
            Cell::Formula {
                formula: "B1*2".into(),
                cached: Box::new(Cell::Number(4.0)),
            },
        );
        workbook.to_xlsx()
    }

    #[test]
    fn wasm_native_surface_extracts_exports_and_reports() {
        let bytes = fixture_bytes();

        assert!(extract_text_bytes(&bytes).unwrap().contains("item"));
        assert_eq!(to_csv_bytes(&bytes, 0).unwrap(), "item,2\n4");
        assert!(to_html_bytes(&bytes, 0).unwrap().contains("<table>"));
        assert!(to_markdown_bytes(&bytes, 0)
            .unwrap()
            .starts_with("| item |"));
        let report = report_json_bytes(&bytes).unwrap();
        assert!(report.contains(r#""format":"xlsx""#));
        assert!(report.contains(r#""formulas":1"#));
    }

    #[test]
    fn wasm_native_surface_rejects_missing_sheet() {
        let bytes = fixture_bytes();

        assert!(matches!(
            to_html_bytes(&bytes, 9),
            Err(Error::SheetOutOfRange)
        ));
    }

    /// A minimal, single-cell fixture for exact-value assertions (the
    /// multi-cell `fixture_bytes()` above is for the multi-field smoke test).
    fn tiny_fixture_bytes() -> Vec<u8> {
        let mut workbook = Workbook::new();
        workbook.add_sheet("S").write(0, 0, "hi");
        workbook.to_xlsx()
    }

    // -- WS2: exact-value happy path per function ----------------------------

    #[test]
    fn extract_text_bytes_happy_path_exact_value() {
        assert_eq!(
            extract_text_bytes(&tiny_fixture_bytes()).unwrap(),
            "# S\nhi\n"
        );
    }

    #[test]
    fn to_csv_bytes_happy_path_exact_value() {
        assert_eq!(to_csv_bytes(&tiny_fixture_bytes(), 0).unwrap(), "hi");
    }

    #[test]
    fn to_html_bytes_happy_path_exact_value() {
        assert_eq!(
            to_html_bytes(&tiny_fixture_bytes(), 0).unwrap(),
            "<table><tr><td>hi</td></tr></table>"
        );
    }

    #[test]
    fn to_markdown_bytes_happy_path_exact_value() {
        assert_eq!(
            to_markdown_bytes(&tiny_fixture_bytes(), 0).unwrap(),
            "| hi |\n| --- |"
        );
    }

    #[test]
    fn report_json_bytes_happy_path_exact_value() {
        assert_eq!(
            report_json_bytes(&tiny_fixture_bytes()).unwrap(),
            "{\"schema_version\":2,\"format\":\"xlsx\",\"stats\":{\"sheets\":1,\"cells\":1,\"formulas\":0,\"text_truncated\":false},\
             \"properties\":{\"title\":null,\"subject\":null,\"creator\":null,\"keywords\":null,\"description\":null,\
             \"last_modified_by\":null,\"company\":null,\"created\":null},\"defined_names_count\":0,\
             \"local_defined_names_count\":0,\
             \"features\":{\"comments\":0,\"data_validations\":0,\"tables\":0,\"merged_ranges\":0,\"hyperlinks\":0,\
             \"images\":0,\"charts\":0,\"sparklines\":0,\"conditional_formatting\":0,\"hidden_sheets\":0,\
             \"frozen_panes\":0,\"page_setup\":0,\"protection\":0,\"pivot_tables\":0,\"vba_project\":false,\
             \"threaded_comments\":0,\"external_links\":0,\"custom_xml\":0},\"evaluation\":{\"computed\":0,\"errors\":0,\
             \"cached\":0,\"unsupported\":0,\"truncated\":false,\"by_reason\":{}},\"provenance\":{\"container\":\"primary\",\
             \"recoveries\":[],\"recoveries_truncated\":false,\"partial\":false},\"warnings\":[]}"
        );
    }

    // -- WS2: garbage-bytes error path, all four functions --------------------

    #[test]
    fn extract_text_bytes_rejects_garbage_bytes() {
        assert!(matches!(
            extract_text_bytes(b"not a spreadsheet"),
            Err(Error::NotOle2)
        ));
    }

    #[test]
    fn to_csv_bytes_rejects_garbage_bytes() {
        assert!(matches!(
            to_csv_bytes(b"not a spreadsheet", 0),
            Err(Error::NotOle2)
        ));
    }

    #[test]
    fn to_html_bytes_rejects_garbage_bytes() {
        assert!(matches!(
            to_html_bytes(b"not a spreadsheet", 0),
            Err(Error::NotOle2)
        ));
    }

    #[test]
    fn to_markdown_bytes_rejects_garbage_bytes() {
        assert!(matches!(
            to_markdown_bytes(b"not a spreadsheet", 0),
            Err(Error::NotOle2)
        ));
    }

    #[test]
    fn report_json_bytes_rejects_garbage_bytes() {
        assert!(matches!(
            report_json_bytes(b"not a spreadsheet"),
            Err(Error::NotOle2)
        ));
    }

    #[test]
    fn extract_text_bytes_rejects_empty_input() {
        assert!(matches!(extract_text_bytes(b""), Err(Error::NotOle2)));
    }

    // -- WS2: out-of-range sheet index, both sheet-indexed functions ---------

    #[test]
    fn to_csv_bytes_rejects_out_of_range_sheet_index() {
        let bytes = fixture_bytes();
        assert!(matches!(
            to_csv_bytes(&bytes, 9),
            Err(Error::SheetOutOfRange)
        ));
    }

    #[test]
    fn to_csv_and_to_html_bytes_accept_the_exact_last_valid_sheet_index() {
        // `fixture_bytes()` has exactly one sheet, so index 0 is the last
        // valid index; this pins the boundary right next to the
        // out-of-range assertions above.
        let bytes = fixture_bytes();
        assert!(to_csv_bytes(&bytes, 0).is_ok());
        assert!(to_html_bytes(&bytes, 0).is_ok());
        assert!(to_markdown_bytes(&bytes, 0).is_ok());
    }

    #[test]
    fn browser_exports_map_checked_output_limits_to_typed_errors() {
        let bytes = tiny_fixture_bytes();
        assert!(matches!(
            to_csv_bytes_with_limit(&bytes, 0, 1),
            Err(Error::ExportOutputTooLarge {
                format: "CSV",
                limit: 1
            })
        ));
        assert!(matches!(
            to_html_bytes_with_limit(&bytes, 0, 1),
            Err(Error::ExportOutputTooLarge {
                format: "HTML",
                limit: 1
            })
        ));
        assert!(matches!(
            to_markdown_bytes_with_limit(&bytes, 0, 1),
            Err(Error::ExportOutputTooLarge {
                format: "Markdown",
                limit: 1
            })
        ));
        assert_eq!(MAX_EXPORT_OUTPUT_BYTES, 16 * 1024 * 1024);
    }
}
