//! XML serialization leaf utilities and the OOXML namespace / content-type /
//! relationship constants shared across the `write` submodules.

use crate::Color;

pub(super) const XML_DECL: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;
pub(super) const NS_MAIN: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
pub(super) const NS_R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
pub(super) const NS_CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
pub(super) const NS_PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

// Crate-visible (not just `pub(super)`): `crate::package` reuses these exact
// SpreadsheetML content-type URIs when validating/authoring `[Content_Types].xml`
// entries for the retained OOXML package, rather than re-typing the strings.
pub(crate) const CT_RELS: &str = "application/vnd.openxmlformats-package.relationships+xml";
pub(crate) const CT_WORKBOOK: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml";
pub(crate) const CT_WORKSHEET: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml";
pub(crate) const CT_STYLES: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml";
pub(crate) const CT_SST: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml";

pub(super) const REL_OFFICE_DOCUMENT: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
pub(super) const REL_CORE_PROPS: &str =
    "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties";
pub(super) const REL_EXT_PROPS: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties";
pub(crate) const CT_CORE_PROPS: &str = "application/vnd.openxmlformats-package.core-properties+xml";
pub(crate) const CT_EXT_PROPS: &str =
    "application/vnd.openxmlformats-officedocument.extended-properties+xml";
pub(super) const REL_WORKSHEET: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet";
pub(super) const REL_STYLES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
pub(super) const REL_SST: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings";
pub(super) const REL_HYPERLINK: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";
pub(super) const REL_DRAWING: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing";
pub(super) const REL_IMAGE: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
pub(super) const REL_CHART: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart";
pub(super) const REL_TABLE: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/table";
pub(super) const REL_COMMENTS: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";
pub(super) const REL_VML_DRAWING: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/vmlDrawing";
pub(super) const CT_DRAWING: &str = "application/vnd.openxmlformats-officedocument.drawing+xml";
pub(super) const CT_CHART: &str =
    "application/vnd.openxmlformats-officedocument.drawingml.chart+xml";
pub(super) const CT_TABLE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.table+xml";
pub(super) const CT_COMMENTS: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.comments+xml";
pub(super) const CT_VML: &str = "application/vnd.openxmlformats-officedocument.vmlDrawing";
pub(super) const NS_XDR: &str =
    "http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing";
pub(super) const NS_A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
pub(super) const NS_C: &str = "http://schemas.openxmlformats.org/drawingml/2006/chart";

/// A `Color` as an 8-hex ARGB string.
pub(super) fn hex(c: Color) -> String {
    format!("FF{:02X}{:02X}{:02X}", c.0[0], c.0[1], c.0[2])
}

/// 0-based `(row, col)` → an A1 reference (col 0 → `A`, 26 → `AA`).
pub(crate) fn a1(row: u32, col: u16) -> String {
    let mut letters = Vec::new();
    let mut c = col as u32 + 1;
    while c > 0 {
        let rem = ((c - 1) % 26) as u8;
        letters.push(b'A' + rem);
        c = (c - 1) / 26;
    }
    letters.reverse();
    let mut s = String::from_utf8(letters).unwrap_or_else(|_| "A".to_string());
    s.push_str(&(row + 1).to_string());
    s
}

/// `(row, col)` → an absolute A1 reference (`$A$1`).
pub(super) fn abs_ref(row: u32, col: u16) -> String {
    let a = a1(row, col);
    let split = a.find(|c: char| c.is_ascii_digit()).unwrap_or(a.len());
    format!("${}${}", &a[..split], &a[split..])
}

/// `col` as an absolute column reference (`$A`, `$AA`).
pub(super) fn abs_col(col: u16) -> String {
    let a = a1(0, col);
    let split = a.find(|c: char| c.is_ascii_digit()).unwrap_or(a.len());
    format!("${}", &a[..split])
}

/// Format an `f64` so `parse::<f64>` reads it back identically (Rust's shortest
/// round-tripping representation). Non-finite values (which Excel cannot hold)
/// become `0`.
pub(crate) fn num_str(n: f64) -> String {
    if n.is_finite() {
        format!("{n}")
    } else {
        "0".to_string()
    }
}

/// Escape text content for an XML element body.
pub(crate) fn esc_text(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            // XML 1.0 forbids these scalars even escaped; drop them so the part
            // stays well-formed regardless of caller-supplied text.
            c if matches!(c as u32, 0xFFFE | 0xFFFF) => {}
            c => o.push(c),
        }
    }
    o
}

/// Escape an XML attribute value.
pub(crate) fn esc_attr(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            // XML 1.0 forbids these scalars even escaped; drop them so the part
            // stays well-formed regardless of caller-supplied text.
            c if matches!(c as u32, 0xFFFE | 0xFFFF) => {}
            c => o.push(c),
        }
    }
    o
}
