//! `rxls` — a native Rust spreadsheet library: no JVM, no Apache POI, no subprocess.
//!
//! **Reads** legacy `.xls` (BIFF8 & BIFF5/7 — [MS-XLS]), `.xlsx` (SpreadsheetML),
//! `.xlsb` (BIFF12), and `.ods` (OpenDocument); **writes** `.xlsx`. It surfaces
//! typed cells ([`Cell`]) — number, date, boolean, error, text, and formula (the
//! source is decompiled where supported, alongside the cached value).
//!
//! Legacy `.xls` is an OLE2/CFB compound file whose `Workbook` stream is a sequence
//! of BIFF records; the OOXML/ODF formats are ZIP packages of XML (binary records
//! for `.xlsb`). The reader walks the shared-string table and the cell records of
//! each worksheet.
//!
//! Two ways to consume a file: **plain text** for search/indexing, or **typed
//! cells** ([`Cell`]) for structured reading.
//!
//! ```no_run
//! // Text:
//! let bytes = std::fs::read("book.xls").unwrap();
//! println!("{}", rxls::extract_text(&bytes).unwrap());
//!
//! // Typed cells:
//! let wb = rxls::Workbook::open(&bytes).unwrap();
//! for sheet in &wb.sheets {
//!     for (row, col, cell) in sheet.cells() {
//!         match cell {
//!             rxls::Cell::Number(n)      => println!("{row},{col} = {n}"),
//!             rxls::Cell::Date(serial)   => println!("{row},{col} = serial {serial}"),
//!             rxls::Cell::Text(t)        => println!("{row},{col} = {t}"),
//!             _ => {}
//!         }
//!     }
//! }
//! ```
//!
//! For modern files (BIFF8) strings are UTF-16. For older BIFF5/7 files strings
//! are 8-bit in the workbook's ANSI codepage (announced by the `CODEPAGE`
//! record); rxls decodes those correctly — e.g. Korean cp949 — via
//! [`encoding_rs`]. Use [`Workbook::open_with_codepage`] to force a codepage.
//! Date/time serials and percentages are rendered as Excel displays them
//! (`2024-03-15`, `50%`) via the `XF` / `FORMAT` / `DATEMODE` records. The parser
//! is panic-free / bounds-checked: malformed input yields [`Error`], never a crash.
//! Authoring (`.xlsx`, default feature) covers styles, merges, formulas, data
//! validation, conditional formatting, images, charts, tables, rich strings,
//! comments, defined names, and document properties — see `Workbook::to_xlsx`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(missing_debug_implementations, rust_2018_idioms)]

mod error;
mod eval;
mod format;
mod model;
#[cfg(feature = "ods")]
mod ods;
mod ole;
#[cfg(feature = "xlsx")]
mod package;
mod ptg;
mod report;
#[cfg(feature = "xlsx")]
mod spreadsheet;
mod sst;
pub mod wasm;
#[cfg(feature = "xlsx")]
mod write;
mod xls;
#[cfg(feature = "xlsb")]
mod xlsb;
#[cfg(feature = "xlsx")]
mod xlsx;
#[cfg(feature = "xlsx")]
mod xmltree;

pub use error::{Error, Result};
pub use eval::{FormulaEvaluation, FormulaUnsupportedReason};
#[cfg(all(feature = "serde", feature = "chrono"))]
pub use model::{
    deserialize_as_date_1900_or_none, deserialize_as_date_1900_or_string,
    deserialize_as_date_1904_or_none, deserialize_as_date_1904_or_string,
    deserialize_as_datetime_1900_or_none, deserialize_as_datetime_1900_or_string,
    deserialize_as_datetime_1904_or_none, deserialize_as_datetime_1904_or_string,
    deserialize_as_duration_or_none, deserialize_as_duration_or_string,
    deserialize_as_time_1900_or_none, deserialize_as_time_1900_or_string,
    deserialize_as_time_1904_or_none, deserialize_as_time_1904_or_string,
};
#[cfg(feature = "serde")]
pub use model::{
    deserialize_as_f64_or_none, deserialize_as_f64_or_string, deserialize_as_i64_or_none,
    deserialize_as_i64_or_string, DeError, RangeDeserializer, RangeDeserializerBuilder,
};
pub use model::{
    excel_serial_to_datetime, Alignment, Border, BorderStyle, Cell, CellErrorType, CellProtection,
    CellStyle, CfRule, Chart, ChartKind, Color, Comment, CondFormat, Data, DataRef, DataType,
    DataValidation, Dimensions, DocProperties, DvKind, DvOp, ExcelDateTime, Fill, Font, Format,
    FormatAlign, FormatBorder, FormatPattern, FormatScript, FormulaRange, FormulaRangeRow,
    FormulaRangeRowCells, FormulaRangeRows, HAlign, HeaderRow, Image, ImageFmt, PageSetup, Picture,
    ProtectionOptions, Range, RangeRow, RangeRowCells, RangeRows, Reader, Series, Sheet,
    SheetMetadata, SheetType, SheetView, SheetVisible, Sparkline, SparklineKind, Table, TextRun,
    VAlign, Workbook, WorkbookMetadata,
};
#[cfg(feature = "chrono")]
pub use model::{excel_serial_to_duration, excel_serial_to_naive_datetime};
pub use report::WorkbookReport;
#[cfg(feature = "xlsx")]
pub use spreadsheet::{EditCapability, EditReadOnlyReason, Spreadsheet};
/// The error type returned by [`Workbook::to_xlsx_checked`].
#[cfg(feature = "xlsx")]
pub use write::WriteError;

// Crate-internal: the reader cell record — not part of the public API, but
// referenced as `crate::CellEntry` by the feature-gated `.xlsx`/`.xlsb`/`.ods`
// readers. The `.xls` reader (`xls.rs`) imports it from `model` directly, so
// this root re-export only exists for the optional readers.
#[cfg(any(feature = "xlsx", feature = "ods"))]
pub(crate) use model::CellEntry;

/// Upper bound on total accumulated cell-text bytes per workbook. Caps
/// shared-string reference amplification — a small file that references one
/// large pooled string from very many cells, cloning it each time — so untrusted
/// input cannot drive an unbounded allocation (an OOM abort that `catch_unwind`
/// cannot catch). Far above any realistic workbook; extraction is best-effort up
/// to the cap. Shared by the `.xls` and `.xlsx` paths.
pub(crate) const MAX_TEXT_BYTES: usize = 256 << 20; // 256 MiB

/// Upper bound on entity/reference events accepted from one XML part. A tiny,
/// highly compressible part can otherwise expand into millions of parser events
/// and temporary allocations even when adjacent text is coalesced.
#[cfg(any(feature = "xlsx", feature = "ods", test))]
pub(crate) const MAX_XML_GENERAL_REFS: usize = 1 << 20;

#[cfg(any(feature = "xlsx", feature = "ods", test))]
pub(crate) fn xml_reference_work_within_budget(xml: &str) -> bool {
    xml_reference_bytes_within_budget(xml.as_bytes())
}

#[cfg(any(feature = "xlsx", feature = "ods", test))]
pub(crate) fn xml_reference_bytes_within_budget(xml: &[u8]) -> bool {
    xml_reference_work_within_limit(xml, MAX_XML_GENERAL_REFS)
}

#[cfg(any(feature = "xlsx", feature = "ods", test))]
fn xml_reference_work_within_limit(xml: &[u8], limit: usize) -> bool {
    let mut remaining = limit;
    for &byte in xml {
        if byte == b'&' {
            if remaining == 0 {
                return false;
            }
            remaining -= 1;
        }
    }
    true
}

impl Workbook {
    /// Parse an Excel workbook from its raw bytes. Detects the format: legacy
    /// `.xls` (OLE2/BIFF) or, with the `xlsx` feature, modern `.xlsx` (OOXML).
    pub fn open(bytes: &[u8]) -> Result<Self> {
        // `.xlsb` and `.xlsx` are both ZIP packages, so probe `.xlsb` (binary
        // `workbook.bin`) before the generic OOXML magic.
        #[cfg(feature = "xlsb")]
        if xlsb::is_xlsb(bytes) {
            return xlsb::open(bytes);
        }
        #[cfg(feature = "ods")]
        if ods::is_ods(bytes) {
            return ods::open(bytes);
        }
        #[cfg(feature = "xlsx")]
        if xlsx::is_xlsx(bytes) {
            return xlsx::open(bytes);
        }
        Self::open_with_codepage(bytes, None)
    }

    /// Serialize this workbook to a modern **`.xlsx`** (SpreadsheetML) byte
    /// buffer — the inverse of the reader. `read → Workbook → write → read`
    /// preserves the typed cells (text, number, date serial, bool, error), so a
    /// legacy `.xls` can be converted to a clean, Office-openable `.xlsx`.
    /// Available with the default `xlsx` feature.
    #[cfg(feature = "xlsx")]
    pub fn to_xlsx(&self) -> Vec<u8> {
        write::to_xlsx(self)
    }
}

/// Convenience: decode `.xls` bytes into normalized plain text. Errors with
/// [`Error::NoText`] if nothing indexable was found.
pub fn extract_text(bytes: &[u8]) -> Result<String> {
    let text = Workbook::open(bytes)?.text();
    if has_indexable(&text) {
        Ok(text)
    } else {
        Err(Error::NoText)
    }
}

fn has_indexable(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_alphanumeric() || ('가'..='힣').contains(&c))
}

/// Decode an IEEE-754 `RkNumber` (the BIFF compressed-number encoding). Shared by
/// the `.xls` and `.xlsb` readers.
pub(crate) fn rk_to_f64(rk: u32) -> f64 {
    let val = if rk & 0x02 != 0 {
        ((rk as i32) >> 2) as f64
    } else {
        f64::from_bits(u64::from(rk & 0xFFFF_FFFC) << 32)
    };
    if rk & 0x01 != 0 {
        val / 100.0
    } else {
        val
    }
}

/// Render a numeric cell value the way search indexing expects: whole numbers
/// without a trailing `.0`. Shared with the `ptg` formula decompiler.
pub(crate) fn format_number(f: f64) -> String {
    if f == 0.0 {
        "0".to_string()
    } else if f.is_finite() && f.fract() == 0.0 && f.abs() <= i64::MAX as f64 {
        format!("{f:.0}")
    } else {
        normalize_scientific_exponent(&format!("{f:?}"))
    }
}

fn normalize_scientific_exponent(text: &str) -> String {
    let Some((mantissa, exponent)) = text.split_once('e') else {
        return text.to_string();
    };
    let (sign, digits) = match exponent.as_bytes().first().copied() {
        Some(b'+') => ('+', &exponent[1..]),
        Some(b'-') => ('-', &exponent[1..]),
        Some(_) => ('+', exponent),
        None => return text.to_string(),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return text.to_string();
    }
    let mut out = String::with_capacity(mantissa.len() + 2 + digits.len().max(2));
    out.push_str(mantissa);
    out.push('e');
    out.push(sign);
    if digits.len() == 1 {
        out.push('0');
    }
    out.push_str(digits);
    out
}

/// Map a BIFF error value to its display string ([MS-XLS] 2.5.10 BErr). Shared by
/// the `.xls` and `.xlsb` readers.
pub(crate) fn error_code(v: u8) -> &'static str {
    match v {
        0x00 => "#NULL!",
        0x07 => "#DIV/0!",
        0x0F => "#VALUE!",
        0x17 => "#REF!",
        0x1D => "#NAME?",
        0x24 => "#NUM!",
        0x2A => "#N/A",
        0x2B => "#GETTING_DATA",
        _ => "#ERR!",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn xml_reference_work_budget_rejects_one_past_the_limit() {
        assert!(super::xml_reference_work_within_budget("&amp;"));
        assert!(super::xml_reference_bytes_within_budget(b"&amp;"));
        assert!(super::xml_reference_work_within_limit(b"&amp;&lt;", 2));
        assert!(!super::xml_reference_work_within_limit(b"&amp;&lt;&gt;", 2));
    }

    #[test]
    fn format_number_keeps_signed_zero_normalized() {
        assert_eq!(super::format_number(0.0), "0");
        assert_eq!(super::format_number(-0.0), "0");
    }

    #[test]
    fn format_number_uses_scientific_notation_for_tiny_general_values() {
        let tiny_values = [
            ("3.0000000000000002E-104", "3e-104"),
            ("4.9999999999999998E-104", "5e-104"),
            ("4.9999999999999998E-106", "5e-106"),
        ];

        for (source, expected) in tiny_values {
            let value = source.parse::<f64>().unwrap();
            assert_eq!(super::format_number(value), expected);
        }
    }

    #[test]
    fn format_number_uses_python_style_scientific_exponents() {
        let values = [
            ("1.23456789E-05", "1.23456789e-05"),
            ("-1.23456789E-05", "-1.23456789e-05"),
            ("1.2345678E142", "1.2345678e+142"),
            ("1.0E20", "1e+20"),
        ];

        for (source, expected) in values {
            let value = source.parse::<f64>().unwrap();
            assert_eq!(super::format_number(value), expected);
        }
    }
}
