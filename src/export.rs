//! Deterministic, bounded spreadsheet export helpers.

use std::fmt;

use crate::Sheet;

/// Default maximum size of one in-memory export result (256 MiB).
pub const DEFAULT_EXPORT_MAX_BYTES: usize = 256 << 20;

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

    let mut out = String::new();
    if options.bom {
        push_checked(&mut out, "\u{FEFF}", options.max_output_bytes)?;
    }

    let mut first_row = true;
    for (row, cols) in sheet.rows() {
        if !first_row {
            push_checked(&mut out, options.newline.as_str(), options.max_output_bytes)?;
        }
        first_row = false;

        let mut first_col = true;
        let mut next_col = 0u32;
        for (col, _cell) in cols {
            let col = u32::from(col);
            while next_col < col {
                if !first_col {
                    push_char_checked(&mut out, options.delimiter, options.max_output_bytes)?;
                }
                first_col = false;
                next_col += 1;
            }
            if !first_col {
                push_char_checked(&mut out, options.delimiter, options.max_output_bytes)?;
            }
            let text = sheet.formatted(row, col as u16).unwrap_or_default();
            push_csv_field(&mut out, text, options)?;
            first_col = false;
            next_col = col.saturating_add(1);
        }
    }
    Ok(out)
}

fn push_csv_field(
    out: &mut String,
    field: &str,
    options: CsvOptions,
) -> Result<(), CsvExportError> {
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
    let required = field
        .len()
        .saturating_add(escaped_quotes)
        .saturating_add(usize::from(escape_formula))
        .saturating_add(if quote { 2 } else { 0 });
    ensure_capacity(out, required, options.max_output_bytes)?;

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
    Ok(())
}

fn push_checked(out: &mut String, value: &str, limit: usize) -> Result<(), CsvExportError> {
    ensure_capacity(out, value.len(), limit)?;
    out.push_str(value);
    Ok(())
}

fn push_char_checked(out: &mut String, value: char, limit: usize) -> Result<(), CsvExportError> {
    ensure_capacity(out, value.len_utf8(), limit)?;
    out.push(value);
    Ok(())
}

fn ensure_capacity(out: &str, added: usize, limit: usize) -> Result<(), CsvExportError> {
    if out.len().saturating_add(added) > limit {
        Err(CsvExportError::OutputTooLarge { limit })
    } else {
        Ok(())
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
}
