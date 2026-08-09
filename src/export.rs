//! Deterministic, bounded spreadsheet export helpers.

use std::collections::BTreeMap;
use std::fmt;

use crate::Sheet;

/// Default maximum size of one in-memory export result (256 MiB).
pub const DEFAULT_EXPORT_MAX_BYTES: usize = 256 << 20;

// Merged HTML currently preserves the established coordinate-by-coordinate
// semantics. Cap the worst-case `coordinate × merge-range` lookup count before
// traversal so a hostile collection of overlapping ranges cannot amplify a
// small output into unbounded CPU work.
const MAX_DENSE_MARKUP_WORK: u64 = 4_000_000;

/// Markup format selected by a checked HTML or Markdown export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MarkupFormat {
    /// HTML table-fragment output.
    Html,
    /// GitHub-flavored Markdown output (with HTML fallback for merges or very
    /// wide sheets).
    Markdown,
}

impl MarkupFormat {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Html => "HTML",
            Self::Markdown => "Markdown",
        }
    }
}

/// Failure returned by [`export_html`] or [`export_markdown`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MarkupExportError {
    /// The configured output byte or dense-grid work limit would be exceeded.
    OutputTooLarge {
        /// Requested output format.
        format: MarkupFormat,
        /// Configured UTF-8 output byte limit.
        limit: usize,
    },
    /// Rendering merged ranges would exceed the bounded coordinate-lookup
    /// budget.
    WorkLimitExceeded {
        /// Requested output format.
        format: MarkupFormat,
        /// Maximum coordinate/merge lookup operations.
        limit: u64,
    },
}

impl fmt::Display for MarkupExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputTooLarge { format, limit } => write!(
                f,
                "{} output exceeds the configured {limit}-byte limit",
                format.as_str()
            ),
            Self::WorkLimitExceeded { format, limit } => write!(
                f,
                "{} export exceeds the {limit}-operation merged-range work limit",
                format.as_str()
            ),
        }
    }
}

impl std::error::Error for MarkupExportError {}

/// Record separator used by [`export_csv`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CsvNewline {
    /// Unix-style line feed (`\n`).
    Lf,
    /// Excel/Windows-style carriage return plus line feed (`\r\n`).
    CrLf,
}

impl CsvNewline {
    fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
        }
    }
}

/// Policy for text fields that spreadsheet programs may interpret as formulas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CsvFormulaPolicy {
    /// Preserve display text byte-for-byte.
    Preserve,
    /// Prefix a leading `=`, `+`, `-`, `@`, tab, CR, or LF with an apostrophe.
    ///
    /// This is an opt-in defense for CSV files that will be opened interactively
    /// in spreadsheet software. The prefix changes the exported field text.
    Escape,
}

/// Stable options for deterministic CSV export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsvOptions {
    /// Field delimiter. Quote, CR, and LF are rejected because they make the
    /// output ambiguous or conflict with record framing.
    pub delimiter: char,
    /// Record separator. Embedded field newlines are preserved inside quotes.
    pub newline: CsvNewline,
    /// Emit a UTF-8 byte-order mark at the start of the result.
    pub bom: bool,
    /// Formula-injection handling for display text.
    pub formula_policy: CsvFormulaPolicy,
    /// Maximum UTF-8 byte length of the returned string.
    pub max_output_bytes: usize,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: ',',
            newline: CsvNewline::Lf,
            bom: false,
            formula_policy: CsvFormulaPolicy::Preserve,
            max_output_bytes: DEFAULT_EXPORT_MAX_BYTES,
        }
    }
}

/// Failure returned by [`export_csv`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CsvExportError {
    /// The selected delimiter conflicts with CSV quoting or record separators.
    InvalidDelimiter(char),
    /// The configured output byte limit would be exceeded.
    OutputTooLarge {
        /// Configured byte limit.
        limit: usize,
    },
}

impl fmt::Display for CsvExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDelimiter(delimiter) => {
                write!(f, "invalid CSV delimiter {delimiter:?}")
            }
            Self::OutputTooLarge { limit } => {
                write!(f, "CSV output exceeds the configured {limit}-byte limit")
            }
        }
    }
}

impl std::error::Error for CsvExportError {}

/// Export one worksheet as deterministic, bounded CSV.
///
/// Rows and columns are emitted in ascending coordinate order with
/// last-write-wins cell semantics. Empty rows are omitted, while gaps between
/// populated columns are retained. Display text is used, matching
/// [`Sheet::to_csv`]. The result has no trailing record separator.
///
/// # Errors
///
/// Returns [`CsvExportError::InvalidDelimiter`] for quote/record-separator
/// delimiters and [`CsvExportError::OutputTooLarge`] before returning any
/// partial output when `max_output_bytes` would be exceeded.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), rxls::CsvExportError> {
/// let mut workbook = rxls::Workbook::new();
/// let sheet = workbook.add_sheet("Data");
/// sheet.write(0, 0, "amount");
/// sheet.write(1, 0, 42.0);
///
/// let csv = rxls::export_csv(sheet, rxls::CsvOptions::default())?;
/// assert_eq!(csv, "amount\n42");
/// # Ok(())
/// # }
/// ```
pub fn export_csv(sheet: &Sheet, options: CsvOptions) -> Result<String, CsvExportError> {
    if matches!(options.delimiter, '"' | '\r' | '\n') {
        return Err(CsvExportError::InvalidDelimiter(options.delimiter));
    }
    export_csv_inner(sheet, options, true)
}

pub(crate) fn export_csv_legacy(
    sheet: &Sheet,
    delimiter: char,
    max_output_bytes: usize,
) -> Result<String, CsvExportError> {
    export_csv_inner(
        sheet,
        CsvOptions {
            delimiter,
            max_output_bytes,
            ..CsvOptions::default()
        },
        false,
    )
}

fn export_csv_inner(
    sheet: &Sheet,
    options: CsvOptions,
    retain_leading_empty_fields: bool,
) -> Result<String, CsvExportError> {
    let required = csv_encoded_len(sheet, options, retain_leading_empty_fields)?;
    let mut out = String::with_capacity(required);
    if options.bom {
        out.push('\u{FEFF}');
    }

    let mut first_row = true;
    let mut current_row = None;
    let mut emitted_delimiters = 0u32;
    for cell in sheet.display_cells() {
        if current_row != Some(cell.row) {
            if !first_row {
                out.push_str(options.newline.as_str());
            }
            first_row = false;
            current_row = Some(cell.row);
            emitted_delimiters = if retain_leading_empty_fields {
                0
            } else {
                u32::from(cell.col)
            };
        }
        let col = u32::from(cell.col);
        while emitted_delimiters < col {
            out.push(options.delimiter);
            emitted_delimiters += 1;
        }
        push_csv_field_unchecked(&mut out, cell.formatted, options);
    }
    debug_assert_eq!(out.len(), required);
    Ok(out)
}

fn csv_encoded_len(
    sheet: &Sheet,
    options: CsvOptions,
    retain_leading_empty_fields: bool,
) -> Result<usize, CsvExportError> {
    let limit = options.max_output_bytes;
    let mut required = if options.bom {
        '\u{FEFF}'.len_utf8()
    } else {
        0
    };
    ensure_csv_total(required, limit)?;

    let delimiter_len = options.delimiter.len_utf8();
    let mut current_row = None;
    let mut min_col = 0u16;
    let mut max_col = 0u16;
    for cell in sheet.display_cells() {
        if current_row != Some(cell.row) {
            if current_row.is_some() {
                add_csv_required(
                    &mut required,
                    usize::from(max_col.saturating_sub(if retain_leading_empty_fields {
                        0
                    } else {
                        min_col
                    }))
                    .saturating_mul(delimiter_len),
                    limit,
                )?;
                add_csv_required(&mut required, options.newline.as_str().len(), limit)?;
            }
            current_row = Some(cell.row);
            min_col = cell.col;
            max_col = cell.col;
        } else {
            max_col = cell.col;
        }
        add_csv_required(
            &mut required,
            csv_field_encoded_len(cell.formatted, options),
            limit,
        )?;
    }
    if current_row.is_some() {
        add_csv_required(
            &mut required,
            usize::from(max_col.saturating_sub(if retain_leading_empty_fields {
                0
            } else {
                min_col
            }))
            .saturating_mul(delimiter_len),
            limit,
        )?;
    }
    Ok(required)
}

fn add_csv_required(
    required: &mut usize,
    added: usize,
    limit: usize,
) -> Result<(), CsvExportError> {
    *required = required.saturating_add(added);
    ensure_csv_total(*required, limit)
}

fn ensure_csv_total(required: usize, limit: usize) -> Result<(), CsvExportError> {
    if required > limit {
        Err(CsvExportError::OutputTooLarge { limit })
    } else {
        Ok(())
    }
}

/// Export one worksheet as a bounded HTML table fragment.
///
/// Empty horizontal gaps are represented by equivalent `colspan` cells, so a
/// sparse value at column XFD does not require allocating 16,383 individual
/// empty tags. The result contains one `<table>` and no document wrapper.
///
/// # Errors
///
/// Returns [`MarkupExportError::OutputTooLarge`] without returning partial
/// output if `max_output_bytes` would be exceeded, or
/// [`MarkupExportError::WorkLimitExceeded`] when merged ranges exceed the
/// conservative coordinate-lookup budget.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), rxls::MarkupExportError> {
/// let mut workbook = rxls::Workbook::new();
/// workbook.add_sheet("Data").write(0, 0, "<ready>");
/// let html = rxls::export_html(&workbook.sheets[0], 1_024)?;
/// assert!(html.contains("&lt;ready&gt;"));
/// # Ok(())
/// # }
/// ```
pub fn export_html(sheet: &Sheet, max_output_bytes: usize) -> Result<String, MarkupExportError> {
    export_html_as(sheet, max_output_bytes, MarkupFormat::Html)
}

/// Export one worksheet as bounded GitHub-flavored Markdown.
///
/// Merged or wider-than-256-column sheets use the same bounded HTML fragment
/// fallback as [`Sheet::to_markdown`].
///
/// # Errors
///
/// Returns [`MarkupExportError::OutputTooLarge`] without returning partial
/// output if `max_output_bytes` would be exceeded, or
/// [`MarkupExportError::WorkLimitExceeded`] when an HTML fallback for merged
/// ranges exceeds the conservative coordinate-lookup budget.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), rxls::MarkupExportError> {
/// let mut workbook = rxls::Workbook::new();
/// workbook.add_sheet("Data").write(0, 0, "heading");
/// let markdown = rxls::export_markdown(&workbook.sheets[0], 1_024)?;
/// assert_eq!(markdown, "| heading |\n| --- |");
/// # Ok(())
/// # }
/// ```
pub fn export_markdown(
    sheet: &Sheet,
    max_output_bytes: usize,
) -> Result<String, MarkupExportError> {
    const MAX_MD_COLS: usize = 256;
    let rows = export_rows(sheet);
    if !sheet.merged_ranges().is_empty() {
        return export_html_rows_as(
            &rows,
            sheet.merged_ranges(),
            max_output_bytes,
            MarkupFormat::Markdown,
        );
    }
    let max_col = rows
        .iter()
        .filter_map(|(_, cols)| cols.keys().next_back().copied())
        .max();
    let Some(max_col) = max_col else {
        return Ok(String::new());
    };
    let width = usize::from(max_col) + 1;
    if width > MAX_MD_COLS {
        return export_html_rows_as(&rows, &[], max_output_bytes, MarkupFormat::Markdown);
    }

    let mut out = String::new();
    for (index, (_, cols)) in rows.iter().enumerate() {
        if index > 0 {
            push_markup(&mut out, "\n", max_output_bytes, MarkupFormat::Markdown)?;
        }
        push_markdown_row_checked(
            &mut out,
            cols,
            max_col,
            max_output_bytes,
            MarkupFormat::Markdown,
        )?;
        if index == 0 {
            push_markup(&mut out, "\n", max_output_bytes, MarkupFormat::Markdown)?;
            push_markdown_separator(&mut out, width, max_output_bytes, MarkupFormat::Markdown)?;
        }
    }
    Ok(out)
}

pub(crate) fn legacy_csv(sheet: &Sheet, delimiter: char, limit: usize) -> String {
    export_csv_legacy(sheet, delimiter, limit).unwrap_or_else(|_| csv_limit_fallback(limit))
}

pub(crate) fn legacy_html(sheet: &Sheet, limit: usize) -> String {
    match export_html(sheet, limit) {
        Ok(output) => output,
        Err(MarkupExportError::OutputTooLarge { .. }) => html_limit_fallback(limit),
        Err(MarkupExportError::WorkLimitExceeded { limit, .. }) => html_work_fallback(limit),
    }
}

pub(crate) fn legacy_markdown(sheet: &Sheet, limit: usize) -> String {
    match export_markdown(sheet, limit) {
        Ok(output) => output,
        Err(MarkupExportError::OutputTooLarge { .. }) => markdown_limit_fallback(limit),
        Err(MarkupExportError::WorkLimitExceeded { limit, .. }) => markdown_work_fallback(limit),
    }
}

fn csv_limit_fallback(limit: usize) -> String {
    format!("# rxls-export-error: output-too-large; format=CSV; limit={limit} bytes")
}

fn html_limit_fallback(limit: usize) -> String {
    format!(
        "<table data-rxls-export-error=\"output-too-large\"><tr><td><strong>rxls export error:</strong> HTML output exceeds the {limit}-byte limit; no source data was exported.</td></tr></table>"
    )
}

fn html_work_fallback(limit: u64) -> String {
    format!(
        "<table data-rxls-export-error=\"work-limit-exceeded\"><tr><td><strong>rxls export error:</strong> HTML merged-range work exceeds the {limit}-operation limit; no source data was exported.</td></tr></table>"
    )
}

fn markdown_limit_fallback(limit: usize) -> String {
    format!(
        "<!-- rxls-export-error: output-too-large -->\n> **rxls export error:** Markdown output exceeds the {limit}-byte limit; no source data was exported."
    )
}

fn markdown_work_fallback(limit: u64) -> String {
    format!(
        "<!-- rxls-export-error: work-limit-exceeded -->\n> **rxls export error:** Markdown merged-range work exceeds the {limit}-operation limit; no source data was exported."
    )
}

fn export_rows(sheet: &Sheet) -> Vec<(u32, BTreeMap<u16, &str>)> {
    sheet
        .rows()
        .map(|(row, cells)| {
            let cols = cells
                .into_iter()
                .map(|(col, _)| (col, sheet.formatted(row, col).unwrap_or_default()))
                .collect();
            (row, cols)
        })
        .collect()
}

fn export_html_as(
    sheet: &Sheet,
    max_output_bytes: usize,
    format: MarkupFormat,
) -> Result<String, MarkupExportError> {
    let rows = export_rows(sheet);
    export_html_rows_as(&rows, sheet.merged_ranges(), max_output_bytes, format)
}

fn export_html_rows_as(
    rows: &[(u32, BTreeMap<u16, &str>)],
    merges: &[(u32, u16, u32, u16)],
    max_output_bytes: usize,
    format: MarkupFormat,
) -> Result<String, MarkupExportError> {
    // Merged cells require coordinate-aware traversal. Bound the full
    // coordinate × merge-range lookup count before entering the loop;
    // no-merge sheets use the sparse colspan path below.
    if !merges.is_empty() {
        let dense_cells = rows.iter().fold(0u64, |total, (_, cols)| {
            total.saturating_add(cols.keys().next_back().map_or(0, |col| u64::from(*col) + 1))
        });
        let lookup_work = dense_cells.saturating_mul(merges.len() as u64);
        if lookup_work > MAX_DENSE_MARKUP_WORK {
            return Err(MarkupExportError::WorkLimitExceeded {
                format,
                limit: MAX_DENSE_MARKUP_WORK,
            });
        }
    }

    let mut out = String::new();
    push_markup(&mut out, "<table>", max_output_bytes, format)?;
    for (row, cols) in rows {
        push_markup(&mut out, "<tr>", max_output_bytes, format)?;
        if merges.is_empty() {
            push_sparse_html_row(&mut out, cols, max_output_bytes, format)?;
        } else {
            let max_col = cols.keys().next_back().copied().unwrap_or(0);
            for col in 0..=u32::from(max_col) {
                let col = col as u16;
                let merge = html_merge_for_cell(merges, *row, col);
                if merge.is_some_and(|merge| merge.skip) {
                    continue;
                }
                push_markup(&mut out, "<td", max_output_bytes, format)?;
                if let Some(merge) = merge {
                    if merge.rowspan > 1 {
                        push_markup(
                            &mut out,
                            &format!(r#" rowspan="{}""#, merge.rowspan),
                            max_output_bytes,
                            format,
                        )?;
                    }
                    if merge.colspan > 1 {
                        push_markup(
                            &mut out,
                            &format!(r#" colspan="{}""#, merge.colspan),
                            max_output_bytes,
                            format,
                        )?;
                    }
                }
                push_markup(&mut out, ">", max_output_bytes, format)?;
                push_html_escaped_checked(
                    &mut out,
                    cols.get(&col).copied().unwrap_or_default(),
                    max_output_bytes,
                    format,
                )?;
                push_markup(&mut out, "</td>", max_output_bytes, format)?;
            }
        }
        push_markup(&mut out, "</tr>", max_output_bytes, format)?;
    }
    push_markup(&mut out, "</table>", max_output_bytes, format)?;
    Ok(out)
}

fn push_sparse_html_row(
    out: &mut String,
    cols: &BTreeMap<u16, &str>,
    limit: usize,
    format: MarkupFormat,
) -> Result<(), MarkupExportError> {
    let mut next_col = 0u32;
    for (&col, &text) in cols {
        let col = u32::from(col);
        if col > next_col {
            push_empty_html_gap(out, col - next_col, limit, format)?;
        }
        push_markup(out, "<td>", limit, format)?;
        push_html_escaped_checked(out, text, limit, format)?;
        push_markup(out, "</td>", limit, format)?;
        next_col = col + 1;
    }
    Ok(())
}

fn push_empty_html_gap(
    out: &mut String,
    count: u32,
    limit: usize,
    format: MarkupFormat,
) -> Result<(), MarkupExportError> {
    if count == 1 {
        push_markup(out, "<td></td>", limit, format)
    } else {
        push_markup(
            out,
            &format!(r#"<td colspan="{count}"></td>"#),
            limit,
            format,
        )
    }
}

#[derive(Clone, Copy)]
struct HtmlMerge {
    rowspan: u32,
    colspan: u32,
    skip: bool,
}

fn html_merge_for_cell(ranges: &[(u32, u16, u32, u16)], row: u32, col: u16) -> Option<HtmlMerge> {
    for &(r0, c0, r1, c1) in ranges {
        let (top, bottom) = (r0.min(r1), r0.max(r1));
        let (left, right) = (c0.min(c1), c0.max(c1));
        if top <= row && row <= bottom && left <= col && col <= right {
            return Some(HtmlMerge {
                rowspan: bottom.saturating_sub(top).saturating_add(1),
                colspan: u32::from(right.saturating_sub(left).saturating_add(1)),
                skip: row != top || col != left,
            });
        }
    }
    None
}

fn push_html_escaped_checked(
    out: &mut String,
    text: &str,
    limit: usize,
    format: MarkupFormat,
) -> Result<(), MarkupExportError> {
    for ch in text.chars() {
        let escaped = match ch {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' => "&quot;",
            _ => {
                ensure_markup_capacity(out, ch.len_utf8(), limit, format)?;
                out.push(ch);
                continue;
            }
        };
        push_markup(out, escaped, limit, format)?;
    }
    Ok(())
}

fn push_markdown_row_checked(
    out: &mut String,
    cols: &BTreeMap<u16, &str>,
    max_col: u16,
    limit: usize,
    format: MarkupFormat,
) -> Result<(), MarkupExportError> {
    push_markup(out, "|", limit, format)?;
    for col in 0..=max_col {
        push_markup(out, " ", limit, format)?;
        push_markdown_cell_checked(
            out,
            cols.get(&col).copied().unwrap_or_default(),
            limit,
            format,
        )?;
        push_markup(out, " |", limit, format)?;
    }
    Ok(())
}

fn push_markdown_separator(
    out: &mut String,
    width: usize,
    limit: usize,
    format: MarkupFormat,
) -> Result<(), MarkupExportError> {
    push_markup(out, "|", limit, format)?;
    for _ in 0..width {
        push_markup(out, " --- |", limit, format)?;
    }
    Ok(())
}

fn push_markdown_cell_checked(
    out: &mut String,
    text: &str,
    limit: usize,
    format: MarkupFormat,
) -> Result<(), MarkupExportError> {
    for ch in text.chars() {
        match ch {
            '|' => push_markup(out, r"\|", limit, format)?,
            '\n' | '\r' => push_markup(out, "<br>", limit, format)?,
            _ => {
                ensure_markup_capacity(out, ch.len_utf8(), limit, format)?;
                out.push(ch);
            }
        }
    }
    Ok(())
}

fn push_markup(
    out: &mut String,
    value: &str,
    limit: usize,
    format: MarkupFormat,
) -> Result<(), MarkupExportError> {
    ensure_markup_capacity(out, value.len(), limit, format)?;
    out.push_str(value);
    Ok(())
}

fn ensure_markup_capacity(
    out: &str,
    added: usize,
    limit: usize,
    format: MarkupFormat,
) -> Result<(), MarkupExportError> {
    if out.len().saturating_add(added) > limit {
        Err(MarkupExportError::OutputTooLarge { format, limit })
    } else {
        Ok(())
    }
}

fn csv_field_encoded_len(field: &str, options: CsvOptions) -> usize {
    let escape_formula = options.formula_policy == CsvFormulaPolicy::Escape
        && field
            .chars()
            .next()
            .is_some_and(|ch| matches!(ch, '=' | '+' | '-' | '@' | '\t' | '\r' | '\n'));
    let quote = escape_formula
        || field.contains(options.delimiter)
        || field.contains('"')
        || field.contains('\n')
        || field.contains('\r');

    let escaped_quotes = field.bytes().filter(|byte| *byte == b'"').count();
    field
        .len()
        .saturating_add(escaped_quotes)
        .saturating_add(usize::from(escape_formula))
        .saturating_add(if quote { 2 } else { 0 })
}

fn push_csv_field_unchecked(out: &mut String, field: &str, options: CsvOptions) {
    let escape_formula = options.formula_policy == CsvFormulaPolicy::Escape
        && field
            .chars()
            .next()
            .is_some_and(|ch| matches!(ch, '=' | '+' | '-' | '@' | '\t' | '\r' | '\n'));
    let quote = escape_formula
        || field.contains(options.delimiter)
        || field.contains('"')
        || field.contains('\n')
        || field.contains('\r');

    if quote {
        out.push('"');
    }
    if escape_formula {
        out.push('\'');
    }
    for ch in field.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    if quote {
        out.push('"');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Workbook;

    #[test]
    fn defaults_match_the_existing_csv_contract() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "name");
        sheet.write(0, 2, "note, \"quoted\"");
        sheet.write(2, 0, "last");

        assert_eq!(
            export_csv(sheet, CsvOptions::default()).unwrap(),
            sheet.to_csv()
        );
    }

    #[test]
    fn options_define_bom_newline_delimiter_and_injection_policy() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "name");
        sheet.write(0, 1, "formula");
        sheet.write(1, 0, "Alice; Bob");
        sheet.write(1, 1, "=HYPERLINK(\"https://example.test\")");

        let options = CsvOptions {
            delimiter: ';',
            newline: CsvNewline::CrLf,
            bom: true,
            formula_policy: CsvFormulaPolicy::Escape,
            ..CsvOptions::default()
        };
        assert_eq!(
            export_csv(sheet, options).unwrap(),
            "\u{FEFF}name;formula\r\n\"Alice; Bob\";\"'=HYPERLINK(\"\"https://example.test\"\")\""
        );
    }

    #[test]
    fn output_limit_returns_an_error_without_a_partial_result() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "12345");

        let error = export_csv(
            sheet,
            CsvOptions {
                max_output_bytes: 4,
                ..CsvOptions::default()
            },
        )
        .unwrap_err();
        assert_eq!(error, CsvExportError::OutputTooLarge { limit: 4 });
    }

    #[test]
    fn repeated_exports_are_byte_identical() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(4, 2, "later");
        sheet.write(0, 1, "first");
        sheet.write(4, 0, "row");

        let options = CsvOptions::default();
        assert_eq!(
            export_csv(sheet, options).unwrap(),
            export_csv(sheet, options).unwrap()
        );
    }

    #[test]
    fn csv_preflights_xfd_gap_amplification_before_emission() {
        const XFD: u16 = 16_383;
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        for row in 0..20_000 {
            sheet.write(row, XFD, "x");
        }

        assert_eq!(
            export_csv(
                sheet,
                CsvOptions {
                    max_output_bytes: 1 << 20,
                    ..CsvOptions::default()
                },
            ),
            Err(CsvExportError::OutputTooLarge { limit: 1 << 20 })
        );
    }

    #[test]
    fn csv_limit_boundary_uses_encoded_utf8_size() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "a§b");
        sheet.write(0, 2, "=x");
        let options = CsvOptions {
            delimiter: '§',
            bom: true,
            formula_policy: CsvFormulaPolicy::Escape,
            max_output_bytes: usize::MAX,
            ..CsvOptions::default()
        };
        let expected = export_csv(sheet, options).unwrap();
        let exact = CsvOptions {
            max_output_bytes: expected.len(),
            ..options
        };
        assert_eq!(export_csv(sheet, exact).unwrap(), expected);
        assert_eq!(
            export_csv(
                sheet,
                CsvOptions {
                    max_output_bytes: expected.len() - 1,
                    ..options
                },
            ),
            Err(CsvExportError::OutputTooLarge {
                limit: expected.len() - 1
            })
        );
    }

    #[test]
    fn html_represents_an_xfd_gap_with_one_bounded_colspan() {
        const XFD: u16 = 16_383;
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, XFD, "edge");

        assert_eq!(
            export_html(sheet, 1_024).unwrap(),
            "<table><tr><td colspan=\"16383\"></td><td>edge</td></tr></table>"
        );
    }

    #[test]
    fn html_many_xfd_rows_remain_compact() {
        const XFD: u16 = 16_383;
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        for row in 0..20_000 {
            sheet.write(row, XFD, "x");
        }

        let html = export_html(sheet, 2 << 20).unwrap();
        assert!(html.len() < 2 << 20);
        assert_eq!(html.matches(r#"colspan="16383""#).count(), 20_000);
        assert_eq!(html.matches("<td>").count(), 20_000);
    }

    #[test]
    fn html_escaped_text_accepts_the_exact_limit_and_rejects_one_less() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "&<>\"한".repeat(1_024));

        let expected = export_html(sheet, usize::MAX).unwrap();
        assert_eq!(export_html(sheet, expected.len()).unwrap(), expected);
        assert_eq!(
            export_html(sheet, expected.len() - 1),
            Err(MarkupExportError::OutputTooLarge {
                format: MarkupFormat::Html,
                limit: expected.len() - 1,
            })
        );
    }

    #[test]
    fn markdown_escaped_text_accepts_the_exact_limit_and_rejects_one_less() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "a|b\nc".repeat(1_024));

        let expected = export_markdown(sheet, usize::MAX).unwrap();
        assert_eq!(export_markdown(sheet, expected.len()).unwrap(), expected);
        assert_eq!(
            export_markdown(sheet, expected.len() - 1),
            Err(MarkupExportError::OutputTooLarge {
                format: MarkupFormat::Markdown,
                limit: expected.len() - 1,
            })
        );
    }

    #[test]
    fn wide_markdown_fallback_stays_sparse_and_reports_markdown_errors() {
        const XFD: u16 = 16_383;
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, XFD, "edge");

        let expected = "<table><tr><td colspan=\"16383\"></td><td>edge</td></tr></table>";
        assert_eq!(export_markdown(sheet, expected.len()).unwrap(), expected);
        assert_eq!(
            export_markdown(sheet, expected.len() - 1),
            Err(MarkupExportError::OutputTooLarge {
                format: MarkupFormat::Markdown,
                limit: expected.len() - 1,
            })
        );
    }

    #[test]
    fn merged_html_rejects_dense_lookup_work_with_a_distinct_error() {
        const XFD: u16 = 16_383;
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.merge(0, 0, 0, 1);
        for row in 0..245 {
            sheet.write(row, XFD, "x");
        }

        assert_eq!(
            export_html(sheet, usize::MAX),
            Err(MarkupExportError::WorkLimitExceeded {
                format: MarkupFormat::Html,
                limit: MAX_DENSE_MARKUP_WORK,
            })
        );
    }

    #[test]
    fn legacy_fallbacks_are_explicit_diagnostics_and_never_partial_data() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "secret-source-value");

        let csv = legacy_csv(sheet, ',', 1);
        assert!(csv.starts_with("# rxls-export-error: output-too-large;"));
        assert!(!csv.contains("secret-source-value"));

        let html = legacy_html(sheet, 1);
        assert!(html.contains(r#"data-rxls-export-error="output-too-large""#));
        assert!(html.contains("rxls export error:"));
        assert!(!html.contains("secret-source-value"));

        let markdown = legacy_markdown(sheet, 1);
        assert!(markdown.contains("rxls-export-error: output-too-large"));
        assert!(markdown.contains("**rxls export error:**"));
        assert!(!markdown.contains("secret-source-value"));
    }
}
