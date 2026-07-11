//! Deterministic machine-readable diagnostics for workbook feature inventory.

use crate::model::Workbook;
use crate::{Cell, DocProperties, SheetVisible};

/// A compact, deterministic diagnose report for a workbook.
///
/// The report is intentionally small and stable; consumers can rely on the
/// JSON field order and keys for smoke checks.
#[derive(Debug, Clone)]
pub struct WorkbookReport {
    /// Source format detected from the CLI argument path or caller-provided hint.
    pub format: String,
    /// Workbook-level numeric stats.
    pub stats: ReportStats,
    /// Document properties parsed from the workbook package.
    pub properties: ReportProperties,
    /// Count of workbook-defined names.
    pub defined_names_count: usize,
    /// Aggregate feature counters from workbook and worksheet metadata.
    pub features: ReportFeatures,
    /// Human-readable warnings derived from the parse results.
    pub warnings: Vec<String>,
}

/// Stable workbook stats used by the diagnose report.
#[derive(Debug, Clone, Copy)]
pub struct ReportStats {
    /// Number of worksheets (including non-grid sheets).
    pub sheets: usize,
    /// Count of stored non-empty cells across all sheets.
    pub cells: usize,
    /// Number of formula cells across all sheets.
    pub formulas: usize,
    /// Whether text extraction was truncated by bounded allocation.
    pub text_truncated: bool,
}

/// Document properties surfaced in the diagnose report.
#[derive(Debug, Clone, Default)]
pub struct ReportProperties {
    /// `dc:title` value.
    pub title: Option<String>,
    /// `dc:subject` value.
    pub subject: Option<String>,
    /// `dc:creator` value.
    pub creator: Option<String>,
    /// `cp:keywords` value.
    pub keywords: Option<String>,
    /// `dc:description` value.
    pub description: Option<String>,
    /// `cp:lastModifiedBy` value.
    pub last_modified_by: Option<String>,
    /// Extended `<Company>` value when present.
    pub company: Option<String>,
    /// W3CDTF created timestamp when present.
    pub created: Option<String>,
}

/// Worksheet feature totals from metadata and surface APIs.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReportFeatures {
    /// Legacy comment/notes count.
    pub comments: usize,
    /// Data validations count.
    pub data_validations: usize,
    /// Worksheet tables count.
    pub tables: usize,
    /// Merged cell ranges.
    pub merged_ranges: usize,
    /// Hyperlinks count.
    pub hyperlinks: usize,
    /// Embedded images count.
    pub images: usize,
    /// Embedded charts count.
    pub charts: usize,
    /// Sparklines count.
    pub sparklines: usize,
    /// Conditional-format rules count.
    pub conditional_formatting: usize,
    /// Hidden and very hidden sheets count.
    pub hidden_sheets: usize,
    /// Frozen panes present count.
    pub frozen_panes: usize,
    /// Sheets with page setup metadata.
    pub page_setup: usize,
    /// Protected sheets count.
    pub protection: usize,
    /// Preserved pivot-table definition parts in the retained package.
    pub pivot_tables: usize,
    /// Whether a VBA project payload is present in the retained package.
    pub vba_project: bool,
    /// Preserved threaded-comment parts in the retained package.
    pub threaded_comments: usize,
    /// Preserved external-link parts in the retained package.
    pub external_links: usize,
    /// Preserved custom XML data item parts in the retained package.
    pub custom_xml: usize,
}

impl WorkbookReport {
    /// Build a deterministic report for a parsed workbook.
    pub fn from_workbook(format: impl Into<String>, workbook: &Workbook) -> Self {
        let format = format.into();
        let metadata = workbook.metadata();
        let stats = ReportStats::from_workbook(workbook);
        let properties = ReportProperties::from_doc_properties(metadata.properties);
        let features = ReportFeatures::from_workbook(&metadata, workbook);
        let warnings = derived_warnings(&stats, &features);

        Self {
            format,
            stats,
            properties,
            defined_names_count: metadata.defined_names.len(),
            features,
            warnings,
        }
    }

    /// Build a report and include retained OOXML package inventory counters.
    #[cfg(feature = "xlsx")]
    pub fn from_workbook_with_package(
        format: impl Into<String>,
        workbook: &Workbook,
        bytes: &[u8],
    ) -> Self {
        let mut report = Self::from_workbook(format, workbook);
        if let Ok(package) = crate::package::Package::from_bytes(bytes) {
            report.features.add_package_part_names(package.part_names());
            report.warnings = derived_warnings(&report.stats, &report.features);
        }
        report
    }

    /// Serialize the report as compact JSON without external serializers.
    ///
    /// The output is deterministic and intentionally bounded by the same counters
    /// used to build this structure.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push('{');
        push_json_string_field(&mut out, "format", &self.format);
        out.push(',');

        out.push_str(r#""stats":"#);
        self.stats.write_json(&mut out);
        out.push(',');

        out.push_str(r#""properties":"#);
        self.properties.write_json(&mut out);
        out.push(',');

        out.push_str(&format!(
            "\"defined_names_count\":{}",
            self.defined_names_count
        ));
        out.push(',');

        out.push_str(r#""features":"#);
        self.features.write_json(&mut out);
        out.push(',');

        out.push_str(r#""warnings":"#);
        push_json_string_array(&mut out, self.warnings.as_slice());

        out.push('}');
        out
    }
}

impl ReportStats {
    fn from_workbook(workbook: &Workbook) -> Self {
        let metadata = workbook.metadata();
        let sheets = metadata.sheets.len();
        let cells = workbook
            .sheets
            .iter()
            .map(|sheet| sheet.cells().count())
            .sum();
        let formulas = workbook
            .sheets
            .iter()
            .map(|sheet| {
                sheet
                    .cells()
                    .filter(|(_, _, cell)| matches!(cell, Cell::Formula { .. }))
                    .count()
            })
            .sum();

        Self {
            sheets,
            cells,
            formulas,
            text_truncated: metadata.text_truncated,
        }
    }

    fn write_json(&self, out: &mut String) {
        out.push('{');
        out.push_str(&format!(
            "\"sheets\":{},\"cells\":{},\"formulas\":{},\"text_truncated\":{}",
            self.sheets, self.cells, self.formulas, self.text_truncated
        ));
        out.push('}');
    }
}

impl ReportProperties {
    fn from_doc_properties(properties: &DocProperties) -> Self {
        Self {
            title: properties.title.clone(),
            subject: properties.subject.clone(),
            creator: properties.creator.clone(),
            keywords: properties.keywords.clone(),
            description: properties.description.clone(),
            last_modified_by: properties.last_modified_by.clone(),
            company: properties.company.clone(),
            created: properties.created.clone(),
        }
    }

    fn write_json(&self, out: &mut String) {
        out.push('{');
        out.push_str(r#""title":"#);
        push_json_nullable_string(out, self.title.as_deref());
        out.push(',');
        out.push_str(r#""subject":"#);
        push_json_nullable_string(out, self.subject.as_deref());
        out.push(',');
        out.push_str(r#""creator":"#);
        push_json_nullable_string(out, self.creator.as_deref());
        out.push(',');
        out.push_str(r#""keywords":"#);
        push_json_nullable_string(out, self.keywords.as_deref());
        out.push(',');
        out.push_str(r#""description":"#);
        push_json_nullable_string(out, self.description.as_deref());
        out.push(',');
        out.push_str(r#""last_modified_by":"#);
        push_json_nullable_string(out, self.last_modified_by.as_deref());
        out.push(',');
        out.push_str(r#""company":"#);
        push_json_nullable_string(out, self.company.as_deref());
        out.push(',');
        out.push_str(r#""created":"#);
        push_json_nullable_string(out, self.created.as_deref());
        out.push('}');
    }
}

impl ReportFeatures {
    fn from_workbook(metadata: &crate::model::WorkbookMetadata<'_>, workbook: &Workbook) -> Self {
        Self {
            comments: workbook
                .sheets
                .iter()
                .map(|sheet| sheet.comments().len())
                .sum(),
            data_validations: workbook
                .sheets
                .iter()
                .map(|sheet| sheet.data_validations().len())
                .sum(),
            tables: workbook
                .sheets
                .iter()
                .map(|sheet| sheet.tables().len())
                .sum(),
            merged_ranges: workbook
                .sheets
                .iter()
                .map(|sheet| sheet.merged_ranges().len())
                .sum(),
            hyperlinks: workbook
                .sheets
                .iter()
                .map(|sheet| sheet.hyperlinks().len())
                .sum(),
            images: workbook
                .sheets
                .iter()
                .map(|sheet| sheet.images().len())
                .sum(),
            charts: workbook
                .sheets
                .iter()
                .map(|sheet| sheet.charts().len())
                .sum(),
            sparklines: workbook
                .sheets
                .iter()
                .map(|sheet| sheet.sparklines().len())
                .sum(),
            conditional_formatting: workbook
                .sheets
                .iter()
                .map(|sheet| sheet.conditional_formats().len())
                .sum(),
            hidden_sheets: metadata
                .sheets
                .iter()
                .filter(|sheet| sheet.visible != SheetVisible::Visible)
                .count(),
            frozen_panes: workbook
                .sheets
                .iter()
                .filter(|sheet| sheet.sheet_view().freeze.is_some())
                .count(),
            page_setup: workbook
                .sheets
                .iter()
                .filter(|sheet| sheet.page_setup().is_some())
                .count(),
            protection: workbook
                .sheets
                .iter()
                .filter(|sheet| sheet.is_protected())
                .count(),
            pivot_tables: 0,
            vba_project: false,
            threaded_comments: 0,
            external_links: 0,
            custom_xml: 0,
        }
    }

    #[cfg(any(feature = "xlsx", test))]
    fn add_package_part_names<'a>(&mut self, part_names: impl Iterator<Item = &'a str>) {
        for name in part_names {
            let name = normalize_package_part_name(name);
            if name == "xl/vbaproject.bin" {
                self.vba_project = true;
            } else if name.starts_with("xl/pivottables/pivottable") && name.ends_with(".xml") {
                self.pivot_tables += 1;
            } else if name.starts_with("xl/threadedcomments/threadedcomment")
                && name.ends_with(".xml")
            {
                self.threaded_comments += 1;
            } else if name.starts_with("xl/externallinks/externallink") && name.ends_with(".xml") {
                self.external_links += 1;
            } else if name.starts_with("customxml/item")
                && name.ends_with(".xml")
                && !name.starts_with("customxml/itemprops")
            {
                self.custom_xml += 1;
            }
        }
    }

    fn write_json(&self, out: &mut String) {
        out.push('{');
        out.push_str(&format!(
            "\"comments\":{},\"data_validations\":{},\"tables\":{},\"merged_ranges\":{},\"hyperlinks\":{},\"images\":{},\"charts\":{},\"sparklines\":{},\"conditional_formatting\":{},\"hidden_sheets\":{},\"frozen_panes\":{},\"page_setup\":{},\"protection\":{},\"pivot_tables\":{},\"vba_project\":{},\"threaded_comments\":{},\"external_links\":{},\"custom_xml\":{}",
            self.comments,
            self.data_validations,
            self.tables,
            self.merged_ranges,
            self.hyperlinks,
            self.images,
            self.charts,
            self.sparklines,
            self.conditional_formatting,
            self.hidden_sheets,
            self.frozen_panes,
            self.page_setup,
            self.protection,
            self.pivot_tables,
            self.vba_project,
            self.threaded_comments,
            self.external_links,
            self.custom_xml
        ));
        out.push('}');
    }
}

fn derived_warnings(stats: &ReportStats, features: &ReportFeatures) -> Vec<String> {
    let mut warnings = Vec::new();
    if stats.formulas > 0 {
        warnings.push("FormulaCacheOnly".to_string());
    }
    if stats.text_truncated {
        warnings.push("TextTruncated".to_string());
    }
    if features.vba_project {
        warnings.push("MacrosPresentNotExecuted".to_string());
    }
    if features.pivot_tables > 0 {
        warnings.push("PivotTablesPreservedNotModeled".to_string());
    }
    warnings
}

#[cfg(any(feature = "xlsx", test))]
fn normalize_package_part_name(name: &str) -> String {
    name.replace('\\', "/")
        .trim_start_matches('/')
        .to_ascii_lowercase()
}

fn push_json_string_array(out: &mut String, values: &[String]) {
    out.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_json_string(out, value);
    }
    out.push(']');
}

fn push_json_string_field(out: &mut String, key: &str, value: &str) {
    push_json_string(out, key);
    out.push(':');
    push_json_string(out, value);
}

fn push_json_nullable_string(out: &mut String, value: Option<&str>) {
    match value {
        Some(value) => push_json_string(out, value),
        None => out.push_str("null"),
    }
}

fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let code = c as u32;
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{code:04X}");
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Workbook;

    #[test]
    fn report_json_escapes_control_characters_and_quotes() {
        let mut encoded = String::new();
        push_json_string(&mut encoded, "A\"B\\C\nD\tE");

        assert_eq!(encoded, "\"A\\\"B\\\\C\\nD\\tE\"");
    }

    #[test]
    fn report_includes_required_fields_for_minimal_workbook() {
        let mut workbook = Workbook::new();
        {
            let sheet = workbook.add_sheet("Data");
            sheet.write(0, 0, "value");
            sheet.write(
                1,
                0,
                Cell::Formula {
                    formula: "A1".into(),
                    cached: Box::new(Cell::Text("value".into())),
                },
            );
            sheet.freeze_panes(2, 0);
        }
        workbook.define_name("Value", "Data!$A$1");

        let report = WorkbookReport::from_workbook("xlsx", &workbook);
        let json = report.to_json();

        assert_eq!(report.stats.sheets, 1);
        assert_eq!(report.stats.formulas, 1);
        assert_eq!(report.defined_names_count, 1);
        assert!(json.contains(r#""format":"xlsx""#));
        assert!(json.contains(r#""stats":{""#));
        assert!(json.contains(r#""FormulaCacheOnly""#));
        assert!(json.contains(r#""features":{"comments":0"#));
    }

    #[test]
    fn report_counts_preserved_package_inventory() {
        let mut features = ReportFeatures::default();
        features.add_package_part_names(
            [
                "xl/vbaProject.bin",
                "xl/pivotTables/pivotTable1.xml",
                "xl/threadedComments/threadedComment1.xml",
                "xl/externalLinks/externalLink1.xml",
                "customXml/item1.xml",
                "customXml/itemProps1.xml",
            ]
            .into_iter(),
        );

        assert!(features.vba_project);
        assert_eq!(features.pivot_tables, 1);
        assert_eq!(features.threaded_comments, 1);
        assert_eq!(features.external_links, 1);
        assert_eq!(features.custom_xml, 1);
        assert_eq!(
            derived_warnings(
                &ReportStats {
                    sheets: 0,
                    cells: 0,
                    formulas: 0,
                    text_truncated: false,
                },
                &features,
            ),
            [
                "MacrosPresentNotExecuted".to_string(),
                "PivotTablesPreservedNotModeled".to_string()
            ]
        );
    }

    // -- WS2: a tiny hand-rolled JSON validator (no serde_json dev-dep) -----
    //
    // `to_json()` is hand-rolled too, so this crate has no serde_json dev
    // dependency to lean on for "is this actually valid JSON" checks. This
    // is a minimal recursive-descent validator: it accepts exactly one
    // well-formed JSON value (object/array/string/number/true/false/null)
    // with no trailing garbage, and rejects everything else, including raw
    // unescaped control characters inside strings (which real JSON forbids).

    fn is_valid_json(input: &str) -> bool {
        let mut chars = input.chars().peekable();
        if !json_value(&mut chars) {
            return false;
        }
        json_ws(&mut chars);
        chars.next().is_none()
    }

    fn json_ws(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        while matches!(
            chars.peek(),
            Some(' ') | Some('\t') | Some('\n') | Some('\r')
        ) {
            chars.next();
        }
    }

    fn json_value(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
        json_ws(chars);
        match chars.peek() {
            Some('{') => json_object(chars),
            Some('[') => json_array(chars),
            Some('"') => json_string(chars),
            Some('t') => json_literal(chars, "true"),
            Some('f') => json_literal(chars, "false"),
            Some('n') => json_literal(chars, "null"),
            Some(c) if *c == '-' || c.is_ascii_digit() => json_number(chars),
            _ => false,
        }
    }

    fn json_literal(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, literal: &str) -> bool {
        for expected in literal.chars() {
            if chars.next() != Some(expected) {
                return false;
            }
        }
        true
    }

    fn json_object(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
        chars.next(); // '{'
        json_ws(chars);
        if chars.peek() == Some(&'}') {
            chars.next();
            return true;
        }
        loop {
            json_ws(chars);
            if chars.peek() != Some(&'"') || !json_string(chars) {
                return false;
            }
            json_ws(chars);
            if chars.next() != Some(':') {
                return false;
            }
            if !json_value(chars) {
                return false;
            }
            json_ws(chars);
            match chars.next() {
                Some(',') => continue,
                Some('}') => return true,
                _ => return false,
            }
        }
    }

    fn json_array(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
        chars.next(); // '['
        json_ws(chars);
        if chars.peek() == Some(&']') {
            chars.next();
            return true;
        }
        loop {
            if !json_value(chars) {
                return false;
            }
            json_ws(chars);
            match chars.next() {
                Some(',') => continue,
                Some(']') => return true,
                _ => return false,
            }
        }
    }

    fn json_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
        if chars.next() != Some('"') {
            return false;
        }
        loop {
            match chars.next() {
                None => return false,
                Some('"') => return true,
                Some('\\') => match chars.next() {
                    Some('"') | Some('\\') | Some('/') | Some('b') | Some('f') | Some('n')
                    | Some('r') | Some('t') => {}
                    Some('u') => {
                        for _ in 0..4 {
                            match chars.next() {
                                Some(c) if c.is_ascii_hexdigit() => {}
                                _ => return false,
                            }
                        }
                    }
                    _ => return false,
                },
                Some(c) if (c as u32) < 0x20 => return false,
                Some(_) => {}
            }
        }
    }

    fn json_number(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
        let mut saw_digit = false;
        if chars.peek() == Some(&'-') {
            chars.next();
        }
        while matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
            chars.next();
            saw_digit = true;
        }
        if !saw_digit {
            return false;
        }
        if chars.peek() == Some(&'.') {
            chars.next();
            let mut saw_frac = false;
            while matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
                chars.next();
                saw_frac = true;
            }
            if !saw_frac {
                return false;
            }
        }
        if matches!(chars.peek(), Some('e') | Some('E')) {
            chars.next();
            if matches!(chars.peek(), Some('+') | Some('-')) {
                chars.next();
            }
            let mut saw_exp = false;
            while matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
                chars.next();
                saw_exp = true;
            }
            if !saw_exp {
                return false;
            }
        }
        true
    }

    #[test]
    fn json_validator_accepts_well_formed_json() {
        for sample in [
            r#"{}"#,
            r#"[]"#,
            r#"{"a":1,"b":[1,2,3],"c":{"d":null,"e":true,"f":false},"g":"x\ny"}"#,
            r#""é""#,
            r#"-12.5e+10"#,
        ] {
            assert!(is_valid_json(sample), "expected valid: {sample}");
        }
    }

    #[test]
    fn json_validator_rejects_malformed_json() {
        for sample in [
            r#"{"a":1,}"#,       // trailing comma
            r#"{a:1}"#,          // unquoted key
            r#"{"a":1}garbage"#, // trailing garbage
            "\"unterminated",    // unterminated string
            "\"raw\ncontrol\"",  // raw unescaped control char in a string
            r#"[1,2,]"#,         // trailing comma in array
            "",                  // empty input
        ] {
            assert!(!is_valid_json(sample), "expected invalid: {sample:?}");
        }
    }

    // -- WS2: JSON escaping, per injection point -----------------------------
    //
    // `to_json()` has exactly two kinds of caller-controlled string
    // injection points: the `format` field, and the eight `ReportProperties`
    // fields (sheet names and defined-name text never reach the JSON at
    // all -- only their counts do).

    #[test]
    fn push_json_string_escapes_backspace_and_formfeed() {
        let mut out = String::new();
        push_json_string(&mut out, "\x08\x0c");
        assert_eq!(out, "\"\\b\\f\"");
    }

    #[test]
    fn push_json_string_escapes_generic_control_chars_as_unicode_escape() {
        let mut out = String::new();
        push_json_string(&mut out, "\x01\x1f");
        assert_eq!(out, "\"\\u0001\\u001F\"");
    }

    #[test]
    fn push_json_string_escapes_del_control_char() {
        let mut out = String::new();
        push_json_string(&mut out, "\x7f");
        assert_eq!(out, "\"\\u007F\"");
    }

    #[test]
    fn push_json_string_passes_through_forward_slash_unescaped() {
        // JSON does not require '/' to be escaped, and this encoder doesn't.
        let mut out = String::new();
        push_json_string(&mut out, "a/b");
        assert_eq!(out, "\"a/b\"");
    }

    #[test]
    fn push_json_string_preserves_non_ascii_unicode_literally() {
        let mut out = String::new();
        push_json_string(&mut out, "한글 emoji \u{1F600}");
        assert_eq!(out, "\"한글 emoji \u{1F600}\"");
        assert!(is_valid_json(&out));
    }

    #[test]
    fn push_json_nullable_string_emits_null_for_none_and_escaped_value_for_some() {
        let mut none_out = String::new();
        push_json_nullable_string(&mut none_out, None);
        assert_eq!(none_out, "null");

        let mut some_out = String::new();
        push_json_nullable_string(&mut some_out, Some("a\"b"));
        assert_eq!(some_out, "\"a\\\"b\"");
    }

    #[test]
    fn report_format_field_with_quotes_backslashes_and_control_chars_is_valid_json() {
        let workbook = Workbook::new();
        let report = WorkbookReport::from_workbook("weird\"\\\n\tformat", &workbook);
        let json = report.to_json();

        assert!(is_valid_json(&json), "{json}");
        assert!(json.contains(r#""format":"weird\"\\\n\tformat""#));
    }

    #[test]
    fn report_doc_properties_with_quotes_backslashes_and_control_chars_are_valid_json() {
        let mut workbook = Workbook::new();
        workbook.properties = crate::DocProperties::new()
            .with_title("Title \"quoted\"")
            .with_subject("Sub\\ject")
            .with_creator("Line1\nLine2")
            .with_keywords("Tab\tSep")
            .with_description("Ctrl\u{1}Char")
            .with_last_modified_by("A\"B\\C")
            .with_company("Quote\"Co")
            .with_created("2024-01-01T00:00:00Z");

        let report = WorkbookReport::from_workbook("xlsx", &workbook);
        let json = report.to_json();

        assert!(is_valid_json(&json), "{json}");
        assert!(json.contains(r#""title":"Title \"quoted\"""#));
        assert!(json.contains(r#""subject":"Sub\\ject""#));
        assert!(json.contains(r#""creator":"Line1\nLine2""#));
        assert!(json.contains(r#""keywords":"Tab\tSep""#));
        assert!(json.contains(r#""description":"Ctrl\u0001Char""#));
        assert!(json.contains(r#""last_modified_by":"A\"B\\C""#));
        assert!(json.contains(r#""company":"Quote\"Co""#));
    }

    #[test]
    fn report_doc_properties_with_unicode_and_emoji_are_valid_json() {
        let mut workbook = Workbook::new();
        workbook.properties = crate::DocProperties::new()
            .with_title("한글 제목")
            .with_creator("작성자 \u{1F600}");

        let json = WorkbookReport::from_workbook("xlsx", &workbook).to_json();

        assert!(is_valid_json(&json), "{json}");
        assert!(json.contains(r#""title":"한글 제목""#));
        assert!(json.contains("작성자 \u{1F600}"));
    }

    #[test]
    fn report_all_null_properties_produce_valid_json_with_null_literals() {
        let workbook = Workbook::new();
        let json = WorkbookReport::from_workbook("xlsx", &workbook).to_json();

        assert!(is_valid_json(&json), "{json}");
        assert!(json.contains(
            r#""properties":{"title":null,"subject":null,"creator":null,"keywords":null,"description":null,"last_modified_by":null,"company":null,"created":null}"#
        ));
    }

    // -- WS2: determinism -----------------------------------------------------

    #[test]
    fn to_json_is_byte_identical_across_repeated_calls() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "value");
        sheet.write_formula(0, 1, "A1", "value");
        workbook.define_name("N", "Data!$A$1");

        let report = WorkbookReport::from_workbook("xlsx", &workbook);
        let first = report.to_json();
        let second = report.to_json();
        let third = WorkbookReport::from_workbook("xlsx", &workbook).to_json();

        assert_eq!(first, second);
        assert_eq!(first, third);
    }

    #[test]
    fn structurally_identical_workbooks_built_in_the_same_order_match() {
        fn build() -> Workbook {
            let mut workbook = Workbook::new();
            let sheet = workbook.add_sheet("Data");
            sheet.write(0, 0, 1.0);
            sheet.write(0, 1, "two");
            sheet.write_formula(1, 0, "A1+1", 2.0);
            workbook.define_name("X", "Data!$A$1");
            workbook
        }

        let a = WorkbookReport::from_workbook("xlsx", &build()).to_json();
        let b = WorkbookReport::from_workbook("xlsx", &build()).to_json();

        assert_eq!(a, b);
    }

    // -- WS2: counter correctness ---------------------------------------------

    #[test]
    fn report_counters_match_a_fixture_with_known_counts() {
        let mut workbook = Workbook::new();
        {
            let sheet = workbook.add_sheet("Data");
            sheet.write(0, 0, "item"); // cell 1
            sheet.write_formula(0, 1, "1+1", 2.0); // cell 2, +1 formula
                                                   // `Sheet::hyperlinks()` is documented as read-only inventory,
                                                   // independent of the per-cell authoring hyperlink consumed by
                                                   // the writer, so an authored-but-unwritten `write_url` call is
                                                   // deliberately NOT reflected in `features.hyperlinks` here (see
                                                   // the dedicated round-tripped test below for that counter).
            sheet.write_url(2, 0, "https://example.com", "Link"); // cell 3
            sheet.merge(4, 0, 4, 1);
            sheet.merge(5, 0, 5, 1); // 2 merged ranges
            sheet.add_comment(0, 0, "note", "Author"); // 1 comment
            sheet.add_data_validation(crate::DataValidation::new(
                (6, 0, 6, 0),
                crate::DvKind::Whole,
                crate::DvOp::GreaterThan,
                "0",
            )); // 1 data validation
            sheet.add_table(crate::Table::new((8, 0, 9, 1), "T1", ["Col1", "Col2"])); // 1 table
            sheet.add_image(crate::Image::new(
                vec![0u8, 1, 2, 3],
                crate::ImageFmt::Png,
                (10, 0),
            )); // 1 image
            sheet.add_chart(crate::Chart::new(crate::ChartKind::Bar, (11, 0), (14, 4))); // 1 chart
            sheet.add_sparkline(crate::Sparkline::new((15, 0), "Data!$A$1:$A$3")); // 1 sparkline
            sheet.add_conditional_format(crate::CondFormat::new(
                (0, 0, 20, 20),
                crate::CfRule::CellIs {
                    op: crate::DvOp::GreaterThan,
                    formula1: "0".to_string(),
                    formula2: None,
                    fill: crate::Color([255, 0, 0]),
                },
            )); // 1 conditional format
            sheet.freeze_panes(1, 0); // frozen panes
            sheet.set_page_setup(crate::PageSetup::default()); // page setup
            sheet.protect(); // protection
        }
        workbook.add_sheet("Hidden").hide(); // 1 hidden sheet, 0 cells
        workbook.define_name("N1", "Data!$A$1");
        workbook.define_name("N2", "Data!$B$1");

        let report = WorkbookReport::from_workbook("xlsx", &workbook);

        assert_eq!(report.stats.sheets, 2);
        assert_eq!(report.stats.cells, 3);
        assert_eq!(report.stats.formulas, 1);
        assert_eq!(report.defined_names_count, 2);
        assert_eq!(report.features.comments, 1);
        assert_eq!(report.features.data_validations, 1);
        assert_eq!(report.features.tables, 1);
        assert_eq!(report.features.merged_ranges, 2);
        assert_eq!(
            report.features.hyperlinks, 0,
            "authored-but-not-round-tripped hyperlinks are intentionally invisible to this counter"
        );
        assert_eq!(report.features.images, 1);
        assert_eq!(report.features.charts, 1);
        assert_eq!(report.features.sparklines, 1);
        assert_eq!(report.features.conditional_formatting, 1);
        assert_eq!(report.features.hidden_sheets, 1);
        assert_eq!(report.features.frozen_panes, 1);
        assert_eq!(report.features.page_setup, 1);
        assert_eq!(report.features.protection, 1);
    }

    #[cfg(feature = "xlsx")]
    #[test]
    fn report_hyperlink_counter_reflects_a_round_tripped_workbook() {
        // The realistic diagnose-CLI path always reports on a workbook that
        // was just *read* (round-tripped through the writer + parser here,
        // since the crate is xlsx-write + xlsx-read capable), where
        // `Sheet::hyperlinks()` is populated correctly.
        let mut workbook = Workbook::new();
        workbook
            .add_sheet("Data")
            .write_url(0, 0, "https://example.com", "Link");
        let bytes = workbook.to_xlsx();
        let reopened = Workbook::open(&bytes).unwrap();

        let report = WorkbookReport::from_workbook("xlsx", &reopened);

        assert_eq!(report.features.hyperlinks, 1);
    }

    #[test]
    fn report_cells_and_formulas_counts_sum_across_multiple_sheets() {
        let mut workbook = Workbook::new();
        {
            let sheet = workbook.add_sheet("One");
            sheet.write(0, 0, 1.0);
            sheet.write_formula(0, 1, "A1", 1.0);
        }
        {
            let sheet = workbook.add_sheet("Two");
            sheet.write(0, 0, 2.0);
            sheet.write(0, 1, 3.0);
            sheet.write_formula(0, 2, "A1+B1", 5.0);
        }

        let report = WorkbookReport::from_workbook("xlsx", &workbook);

        assert_eq!(report.stats.cells, 5);
        assert_eq!(report.stats.formulas, 2);
    }

    #[test]
    fn report_defined_names_count_matches_number_defined() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data");
        workbook.define_name("A", "Data!$A$1");
        workbook.define_name("B", "Data!$B$1");
        workbook.define_name("C", "Data!$C$1");

        let report = WorkbookReport::from_workbook("xlsx", &workbook);

        assert_eq!(report.defined_names_count, 3);
    }

    #[test]
    fn hidden_and_very_hidden_sheets_both_count_toward_hidden_sheets_counter() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Visible");
        workbook.add_sheet("Hidden").hide();
        workbook.add_sheet("VeryHidden").hide_very();

        let report = WorkbookReport::from_workbook("xlsx", &workbook);

        assert_eq!(report.features.hidden_sheets, 2);
    }

    // -- WS2: package-scan counters (real synthetic zip through Package) -----

    #[cfg(feature = "xlsx")]
    fn zip_bytes(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = zip::write::SimpleFileOptions::default();
        for (name, bytes) in parts {
            zip.start_file(*name, opt).unwrap();
            std::io::Write::write_all(&mut zip, bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[cfg(feature = "xlsx")]
    const MINIMAL_CONTENT_TYPES: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#;

    #[cfg(feature = "xlsx")]
    const MINIMAL_RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;

    #[cfg(feature = "xlsx")]
    const MINIMAL_WORKBOOK_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"/>"#;

    #[cfg(feature = "xlsx")]
    #[test]
    fn report_with_package_detects_vba_project() {
        let bytes = zip_bytes(&[
            ("[Content_Types].xml", MINIMAL_CONTENT_TYPES),
            ("_rels/.rels", MINIMAL_RELS),
            ("xl/workbook.xml", MINIMAL_WORKBOOK_XML),
            ("xl/vbaProject.bin", b"\x00\x01"),
        ]);
        let workbook = Workbook::new();
        let report = WorkbookReport::from_workbook_with_package("xlsx", &workbook, &bytes);

        assert!(report.features.vba_project);
        assert!(report
            .warnings
            .contains(&"MacrosPresentNotExecuted".to_string()));
        assert!(is_valid_json(&report.to_json()));
    }

    #[cfg(feature = "xlsx")]
    #[test]
    fn report_with_package_counts_pivot_threaded_external_and_custom_xml_parts() {
        let bytes = zip_bytes(&[
            ("[Content_Types].xml", MINIMAL_CONTENT_TYPES),
            ("_rels/.rels", MINIMAL_RELS),
            ("xl/workbook.xml", MINIMAL_WORKBOOK_XML),
            ("xl/pivotTables/pivotTable1.xml", b"<pivotTableDefinition/>"),
            ("xl/pivotTables/pivotTable2.xml", b"<pivotTableDefinition/>"),
            (
                "xl/threadedComments/threadedComment1.xml",
                b"<ThreadedComments/>",
            ),
            ("xl/externalLinks/externalLink1.xml", b"<externalLink/>"),
            ("customXml/item1.xml", b"<root/>"),
            // itemProps must NOT be counted as a custom XML item part.
            ("customXml/itemProps1.xml", b"<ds:datastoreItem/>"),
        ]);
        let workbook = Workbook::new();
        let report = WorkbookReport::from_workbook_with_package("xlsx", &workbook, &bytes);

        assert_eq!(report.features.pivot_tables, 2);
        assert_eq!(report.features.threaded_comments, 1);
        assert_eq!(report.features.external_links, 1);
        assert_eq!(report.features.custom_xml, 1);
        assert!(report
            .warnings
            .contains(&"PivotTablesPreservedNotModeled".to_string()));
        assert!(is_valid_json(&report.to_json()));
    }

    #[test]
    fn report_without_package_scan_leaves_package_counters_at_zero() {
        // `from_workbook` (no package bytes) never populates the
        // package-inventory counters, regardless of what the workbook
        // itself contains.
        let workbook = Workbook::new();
        let report = WorkbookReport::from_workbook("xlsx", &workbook);

        assert!(!report.features.vba_project);
        assert_eq!(report.features.pivot_tables, 0);
        assert_eq!(report.features.threaded_comments, 0);
        assert_eq!(report.features.external_links, 0);
        assert_eq!(report.features.custom_xml, 0);
        assert!(!report
            .warnings
            .contains(&"MacrosPresentNotExecuted".to_string()));
        assert!(!report
            .warnings
            .contains(&"PivotTablesPreservedNotModeled".to_string()));
    }

    // -- WS2: warnings derivation ----------------------------------------------

    #[test]
    fn warning_formula_cache_only_present_iff_formulas_present() {
        let mut with_formula = Workbook::new();
        with_formula
            .add_sheet("Data")
            .write_formula(0, 0, "1+1", 2.0);
        let with_json = WorkbookReport::from_workbook("xlsx", &with_formula).to_json();
        assert!(with_json.contains(r#""FormulaCacheOnly""#));

        let mut without_formula = Workbook::new();
        without_formula.add_sheet("Data").write(0, 0, 1.0);
        let without_json = WorkbookReport::from_workbook("xlsx", &without_formula).to_json();
        assert!(!without_json.contains("FormulaCacheOnly"));
    }

    #[test]
    fn warning_text_truncated_present_iff_flag_set() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Data");
        workbook.text_truncated = true;

        let report = WorkbookReport::from_workbook("xlsx", &workbook);
        assert!(report.stats.text_truncated);
        assert!(report.warnings.contains(&"TextTruncated".to_string()));

        let mut clean = Workbook::new();
        clean.add_sheet("Data");
        let clean_report = WorkbookReport::from_workbook("xlsx", &clean);
        assert!(!clean_report.stats.text_truncated);
        assert!(!clean_report.warnings.contains(&"TextTruncated".to_string()));
    }

    #[test]
    fn warning_macros_and_pivot_tables_absent_when_features_are_zero() {
        let features = ReportFeatures::default();
        let stats = ReportStats {
            sheets: 1,
            cells: 0,
            formulas: 0,
            text_truncated: false,
        };
        assert_eq!(derived_warnings(&stats, &features), Vec::<String>::new());
    }

    #[test]
    fn empty_workbook_with_no_sheets_produces_valid_json_and_no_warnings() {
        let workbook = Workbook::new();
        let report = WorkbookReport::from_workbook("xlsx", &workbook);
        let json = report.to_json();

        assert_eq!(report.stats.sheets, 0);
        assert_eq!(report.stats.cells, 0);
        assert!(report.warnings.is_empty());
        assert!(is_valid_json(&json), "{json}");
        assert!(json.contains(r#""sheets":0"#));
    }

    #[test]
    fn workbook_with_one_empty_sheet_produces_valid_json_without_panicking() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Empty");
        let json = WorkbookReport::from_workbook("xlsx", &workbook).to_json();

        assert!(is_valid_json(&json), "{json}");
        assert!(json.contains(r#""sheets":1"#));
        assert!(json.contains(r#""cells":0"#));
    }
}
