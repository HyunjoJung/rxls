//! Extract plain text from a supported spreadsheet.
//!
//! ```text
//! cargo run -p rxls --example extract -- path/to/book.xls
//! cargo run -p rxls --example extract -- path/to/book.xls --typed-values
//! ```
//!
//! The default output uses the workbook's display-formatted text. The
//! `--typed-values` projection is reserved for parser parity: it emits raw
//! numbers, canonical date/time strings, and literal-aware percentages so
//! number-format fidelity is measured independently from typed-value fidelity.

use std::{
    borrow::Cow,
    fmt,
    io::{self, BufWriter, Write},
    process::ExitCode,
};

use rxls::{excel_serial_to_datetime, Cell, SheetType, Workbook, DEFAULT_EXPORT_MAX_BYTES};

const TYPED_VALUES_FLAG: &str = "--typed-values";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValueKind {
    Plain,
    Percent,
    Date,
    Time,
    ElapsedTime,
    DateTime,
}

#[derive(Clone, Copy, Debug)]
enum Comparison {
    Lt,
    Le,
    Eq,
    Ne,
    Ge,
    Gt,
}

#[derive(Clone, Copy, Debug)]
struct FormatCondition {
    comparison: Comparison,
    threshold: f64,
}

impl FormatCondition {
    fn matches(self, value: f64) -> bool {
        match self.comparison {
            Comparison::Lt => value < self.threshold,
            Comparison::Le => value <= self.threshold,
            Comparison::Eq => value == self.threshold,
            Comparison::Ne => value != self.threshold,
            Comparison::Ge => value >= self.threshold,
            Comparison::Gt => value > self.threshold,
        }
    }
}

#[derive(Debug)]
enum TypedValuesError {
    OutputTooLarge { limit: usize },
    Io(io::Error),
}

impl fmt::Display for TypedValuesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputTooLarge { limit } => write!(
                formatter,
                "typed-value output exceeds the configured {limit}-byte limit"
            ),
            Self::Io(error) => write!(formatter, "write typed-value output: {error}"),
        }
    }
}

impl std::error::Error for TypedValuesError {}

impl From<io::Error> for TypedValuesError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

struct LimitedWriter<W> {
    inner: W,
    written: usize,
    limit: usize,
}

impl<W: Write> LimitedWriter<W> {
    fn new(inner: W, limit: usize) -> Self {
        Self {
            inner,
            written: 0,
            limit,
        }
    }

    fn write_str(&mut self, value: &str) -> Result<(), TypedValuesError> {
        let next = self
            .written
            .checked_add(value.len())
            .filter(|size| *size <= self.limit)
            .ok_or(TypedValuesError::OutputTooLarge { limit: self.limit })?;
        self.inner.write_all(value.as_bytes())?;
        self.written = next;
        Ok(())
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: extract <spreadsheet> [{TYPED_VALUES_FLAG}]");
        return ExitCode::from(64);
    };
    let typed_values = match args.next() {
        None => false,
        Some(flag) if flag == TYPED_VALUES_FLAG => true,
        Some(_) => {
            eprintln!("usage: extract <spreadsheet> [{TYPED_VALUES_FLAG}]");
            return ExitCode::from(64);
        }
    };
    if args.next().is_some() {
        eprintln!("usage: extract <spreadsheet> [{TYPED_VALUES_FLAG}]");
        return ExitCode::from(64);
    }

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("read {path}: {error}");
            return ExitCode::from(66);
        }
    };
    if typed_values {
        let workbook = match Workbook::open(&bytes) {
            Ok(workbook) => workbook,
            Err(error) => {
                eprintln!("{path}: {error}");
                return ExitCode::from(1);
            }
        };
        let stdout = io::stdout();
        let mut output = BufWriter::new(stdout.lock());
        return match write_typed_values_atomic(&workbook, &mut output, DEFAULT_EXPORT_MAX_BYTES)
            .and_then(|()| output.flush().map_err(TypedValuesError::from))
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{path}: {error}");
                ExitCode::from(1)
            }
        };
    }

    match rxls::extract_text(&bytes) {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{path}: {error}");
            ExitCode::from(1)
        }
    }
}

fn write_typed_values_atomic<W: Write>(
    workbook: &Workbook,
    output: &mut W,
    limit: usize,
) -> Result<(), TypedValuesError> {
    // Preflight through a sink so an over-budget projection cannot expose a
    // valid-looking prefix on stdout. The second pass writes cell-by-cell and
    // therefore never retains a row or the complete output in memory.
    emit_typed_values(workbook, io::sink(), limit)?;
    emit_typed_values(workbook, output, limit)
}

fn emit_typed_values<W: Write>(
    workbook: &Workbook,
    output: W,
    limit: usize,
) -> Result<(), TypedValuesError> {
    let mut out = LimitedWriter::new(output, limit);
    let mut emitted_sheet = false;
    for sheet in workbook
        .sheets
        .iter()
        .filter(|sheet| sheet.sheet_type() == SheetType::WorkSheet)
    {
        if emitted_sheet {
            out.write_str("\n")?;
        }
        emitted_sheet = true;
        out.write_str("# ")?;
        out.write_str(&sheet.name)?;

        let mut current_row = None;
        let mut values_in_row = 0usize;
        for cell in sheet.display_cells() {
            if current_row.is_some_and(|row| row != cell.row) {
                values_in_row = 0;
            }
            current_row = Some(cell.row);

            // A retained explicit style is the effective cell XF. Consult
            // inherited row/column/default formats only when no explicit style
            // exists at the coordinate.
            let number_format = match cell.explicit_style {
                // An explicit General format is a real override, represented
                // by `num_fmt=None`; it must not fall through to an inherited
                // row/column percent or date format.
                Some(style) => style.num_fmt.as_deref(),
                None => sheet
                    .row_styles()
                    .get(&cell.row)
                    .and_then(|style| style.num_fmt.as_deref())
                    .or_else(|| {
                        sheet
                            .column_styles()
                            .get(&cell.col)
                            .and_then(|style| style.num_fmt.as_deref())
                    })
                    .or_else(|| {
                        sheet
                            .default_cell_style()
                            .and_then(|style| style.num_fmt.as_deref())
                    }),
            };
            let value = typed_cell_text(cell.value, number_format, workbook.date1904);
            if !value.is_empty() {
                out.write_str(if values_in_row == 0 { "\n" } else { "\t" })?;
                out.write_str(&value)?;
                values_in_row += 1;
            }
        }
    }
    Ok(())
}

fn typed_cell_text<'a>(
    cell: &'a Cell,
    number_format: Option<&str>,
    date1904: bool,
) -> Cow<'a, str> {
    match cell {
        Cell::Text(text) | Cell::Error(text) => Cow::Borrowed(text),
        Cell::Number(number) => Cow::Owned(typed_numeric_text(
            *number,
            number_format.map_or(ValueKind::Plain, |code| {
                classify_number_format(code, *number)
            }),
            date1904,
        )),
        Cell::Date(serial) => Cow::Owned(typed_numeric_text(
            *serial,
            number_format.map_or(ValueKind::Date, |code| {
                classify_number_format(code, *serial)
            }),
            date1904,
        )),
        Cell::Bool(value) => Cow::Borrowed(if *value { "TRUE" } else { "FALSE" }),
        Cell::Formula { cached, .. } => typed_cell_text(cached, number_format, date1904),
    }
}

fn typed_numeric_text(value: f64, kind: ValueKind, date1904: bool) -> String {
    match kind {
        ValueKind::Percent => format!("{}%", plain_number(value * 100.0)),
        ValueKind::Date | ValueKind::Time | ValueKind::ElapsedTime | ValueKind::DateTime => {
            canonical_date_text(value, kind, date1904)
        }
        ValueKind::Plain => plain_number(value),
    }
}

fn plain_number(number: f64) -> String {
    Cell::Number(number).to_string()
}

fn canonical_date_text(serial: f64, kind: ValueKind, date1904: bool) -> String {
    if !date1904 && serial == 0.0 && matches!(kind, ValueKind::Date | ValueKind::DateTime) {
        return "00:00:00".to_string();
    }
    if kind == ValueKind::ElapsedTime {
        return elapsed_time_text(serial).unwrap_or_else(|| plain_number(serial));
    }
    let Some(value) = excel_serial_to_datetime(serial, date1904) else {
        return plain_number(serial);
    };
    match kind {
        ValueKind::Time => value.time_string(),
        ValueKind::DateTime => format!("{} {}", value.date_string(), value.time_string()),
        ValueKind::Date | ValueKind::Plain | ValueKind::Percent => value.date_string(),
        ValueKind::ElapsedTime => unreachable!("elapsed time returned above"),
    }
}

fn elapsed_time_text(serial: f64) -> Option<String> {
    if !serial.is_finite() || serial < 0.0 {
        return None;
    }
    let seconds = (serial * 86_400.0).round();
    if seconds > u64::MAX as f64 {
        return None;
    }
    let seconds = seconds as u64;
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    Some(format!("{hours}:{minutes:02}:{seconds:02}"))
}

fn select_number_format_section(code: &str, value: f64) -> Option<&str> {
    let sections = split_number_format_sections(code)?;
    let numeric = &sections[..sections.len().min(3)];
    if numeric.is_empty() {
        return None;
    }

    let conditions = numeric
        .iter()
        .map(|section| section_condition(section))
        .collect::<Option<Vec<_>>>()?;
    if conditions.iter().any(Option::is_some) {
        for (section, condition) in numeric.iter().zip(&conditions) {
            if condition.is_some_and(|condition| condition.matches(value)) {
                return Some(*section);
            }
        }
        return numeric
            .iter()
            .zip(conditions)
            .rev()
            .find_map(|(section, condition)| condition.is_none().then_some(*section));
    }

    match numeric.len() {
        1 => Some(numeric[0]),
        2 if value.is_sign_negative() => Some(numeric[1]),
        2 => Some(numeric[0]),
        _ if value > 0.0 => Some(numeric[0]),
        _ if value < 0.0 => Some(numeric[1]),
        _ => Some(numeric[2]),
    }
}

fn split_number_format_sections(code: &str) -> Option<Vec<&str>> {
    let mut sections = Vec::new();
    let mut start = 0;
    let mut chars = code.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '"' => {
                let mut closed = false;
                for (_, next) in chars.by_ref() {
                    if next == '"' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return None;
                }
            }
            '[' => {
                let mut closed = false;
                for (_, next) in chars.by_ref() {
                    if next == ']' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return None;
                }
            }
            '\\' | '_' | '*' => {
                chars.next()?;
            }
            ';' => {
                if sections.len() == 3 {
                    return None;
                }
                sections.push(&code[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    sections.push(&code[start..]);
    Some(sections)
}

fn section_condition(section: &str) -> Option<Option<FormatCondition>> {
    let mut condition = None;
    let mut chars = section.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == '"' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return None;
                }
            }
            '[' => {
                let mut inner = String::new();
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == ']' {
                        closed = true;
                        break;
                    }
                    inner.push(next);
                }
                if !closed {
                    return None;
                }
                if let Some(parsed) = parse_format_condition(&inner) {
                    if condition.replace(parsed).is_some() {
                        return None;
                    }
                } else if inner
                    .chars()
                    .next()
                    .is_some_and(|next| matches!(next, '<' | '>' | '='))
                {
                    return None;
                }
            }
            '\\' | '_' | '*' => {
                chars.next()?;
            }
            _ => {}
        }
    }
    Some(condition)
}

fn parse_format_condition(inner: &str) -> Option<FormatCondition> {
    let (comparison, threshold) = if let Some(rest) = inner.strip_prefix("<=") {
        (Comparison::Le, rest)
    } else if let Some(rest) = inner.strip_prefix(">=") {
        (Comparison::Ge, rest)
    } else if let Some(rest) = inner.strip_prefix("<>") {
        (Comparison::Ne, rest)
    } else if let Some(rest) = inner.strip_prefix('<') {
        (Comparison::Lt, rest)
    } else if let Some(rest) = inner.strip_prefix('>') {
        (Comparison::Gt, rest)
    } else {
        (Comparison::Eq, inner.strip_prefix('=')?)
    };
    let threshold = threshold.trim().parse::<f64>().ok()?;
    threshold.is_finite().then_some(FormatCondition {
        comparison,
        threshold,
    })
}

fn classify_number_format(code: &str, value: f64) -> ValueKind {
    let Some(code) = select_number_format_section(code, value) else {
        return ValueKind::Plain;
    };
    let mut cleaned = String::new();
    let mut elapsed_time = false;
    let mut chars = code.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                for next in chars.by_ref() {
                    if next == '"' {
                        break;
                    }
                }
            }
            '[' => {
                let mut inner = String::new();
                for next in chars.by_ref() {
                    if next == ']' {
                        break;
                    }
                    inner.push(next.to_ascii_lowercase());
                }
                if !inner.is_empty() && inner.chars().all(|field| matches!(field, 'h' | 'm' | 's'))
                {
                    elapsed_time = true;
                }
            }
            '\\' | '_' | '*' => {
                chars.next();
            }
            _ => cleaned.push(ch.to_ascii_lowercase()),
        }
    }
    if cleaned.contains('%') {
        return ValueKind::Percent;
    }
    if elapsed_time {
        return ValueKind::ElapsedTime;
    }

    let cleaned = cleaned.replace("am/pm", "").replace("a/p", "");
    let has_time = cleaned.contains('h') || cleaned.contains('s');
    let month_tokens = cleaned.matches('m').count();
    let has_month = month_tokens >= 3 || (month_tokens >= 1 && !has_time);
    let has_date = cleaned.contains('y') || cleaned.contains('d') || has_month;
    match (has_date, has_time) {
        (true, true) => ValueKind::DateTime,
        (true, false) => ValueKind::Date,
        (false, true) => ValueKind::Time,
        (false, false) => ValueKind::Plain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rxls::Format;

    fn typed_values_text(workbook: &Workbook) -> String {
        let mut output = Vec::new();
        write_typed_values_atomic(workbook, &mut output, DEFAULT_EXPORT_MAX_BYTES).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn typed_projection_ignores_visual_number_formatting() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Values");
        sheet.write_with_format(0, 0, 0.5, &Format::new().num_fmt("0"));
        sheet.write_with_format(0, 1, 12.345, &Format::new().num_fmt("$0.00"));
        sheet.write_with_format(0, 2, 0.25, &Format::new().num_fmt("0%"));
        sheet.write_with_format(
            1,
            0,
            Cell::Date(42_803.0),
            &Format::new().num_fmt("dd/mm/yyyy"),
        );
        sheet.write_with_format(
            1,
            1,
            Cell::Date(10.632_060_185_185_185),
            &Format::new().num_fmt("[hh]:mm:ss"),
        );
        sheet.set_row_format(2, &Format::new().num_fmt("0%"));
        sheet.write_with_format(2, 0, 0.5, &Format::new());
        sheet.write(2, 1, 0.5);
        workbook.add_sheet("Empty");
        assert_eq!(
            typed_values_text(&workbook),
            "# Values\n0.5\t12.345\t25%\n2017-03-09\t255:10:10\n0.5\t50%\n# Empty"
        );
    }

    #[test]
    fn format_classifier_ignores_literals_and_distinguishes_time_fields() {
        assert_eq!(classify_number_format("0.00%", 1.0), ValueKind::Percent);
        assert_eq!(classify_number_format(r#"0.0\ "%""#, 1.0), ValueKind::Plain);
        assert_eq!(classify_number_format("yyyy-mm-dd", 1.0), ValueKind::Date);
        assert_eq!(classify_number_format("h:mm:ss", 1.0), ValueKind::Time);
        assert_eq!(
            classify_number_format("yyyy-mm-dd h:mm:ss", 1.0),
            ValueKind::DateTime
        );
        assert_eq!(
            classify_number_format("[hh]:mm:ss", 1.0),
            ValueKind::ElapsedTime
        );
    }

    #[test]
    fn format_classifier_selects_sign_zero_and_conditional_sections() {
        assert_eq!(classify_number_format("0%;0", 0.5), ValueKind::Percent);
        assert_eq!(classify_number_format("0%;0", -0.5), ValueKind::Plain);
        assert_eq!(classify_number_format("0;0%", 0.5), ValueKind::Plain);
        assert_eq!(classify_number_format("0;0%", -0.5), ValueKind::Percent);
        assert_eq!(
            classify_number_format("0;0;yyyy-mm-dd", 0.0),
            ValueKind::Date
        );
        assert_eq!(
            classify_number_format("[>=1]0%;[<1]yyyy-mm-dd", 0.5),
            ValueKind::Date
        );
        assert_eq!(
            classify_number_format("[>=1]0%;[<1]yyyy-mm-dd", 1.5),
            ValueKind::Percent
        );

        assert_eq!(
            typed_cell_text(&Cell::Number(-0.5), Some("0;0%"), false),
            "-50%"
        );
        assert_eq!(
            typed_cell_text(&Cell::Date(42_803.0), Some("yyyy-mm-dd;0"), false),
            "2017-03-09"
        );
        assert_eq!(
            typed_cell_text(&Cell::Date(-1.0), Some("yyyy-mm-dd;0"), false),
            "-1"
        );
    }

    #[test]
    fn typed_projection_cap_accepts_boundary_and_rejects_without_partial_output() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Values");
        sheet.write(0, 0, "alpha");
        sheet.write(0, 1, "beta");

        let expected = b"# Values\nalpha\tbeta";
        let mut boundary = Vec::new();
        write_typed_values_atomic(&workbook, &mut boundary, expected.len()).unwrap();
        assert_eq!(boundary, expected);

        let mut overflow = Vec::new();
        assert!(matches!(
            write_typed_values_atomic(&workbook, &mut overflow, expected.len() - 1),
            Err(TypedValuesError::OutputTooLarge { limit }) if limit == expected.len() - 1
        ));
        assert!(overflow.is_empty());
    }
}
