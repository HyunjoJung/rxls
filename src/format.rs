//! Number-format awareness: render date/time serials and percentages the way
//! Excel displays them, instead of emitting raw IEEE-754 serials.
//!
//! Built from the `XF` (cell format), `FORMAT` (format-code string), and
//! `DATEMODE` (1900 vs 1904 epoch) records. Mirrors xlrd `formatting.py` /
//! `xldate.py` and calamine `format::format_excel_f64`.
//!
//! Reference: [MS-XLS] 2.4.122 (Format), 2.4.353 (XF), 2.4.99 (DateMode);
//! [MS-OSHARED] number-format built-in codes.

use std::collections::HashMap;

#[path = "number_format.rs"]
mod number_format;

/// What a numeric cell's format means for text rendering. Shared by the BIFF
/// (`.xls`) and SpreadsheetML (`.xlsx`) paths — both use the same number-format
/// classification and serial-date arithmetic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Plain,
    Percent,
    Date,
    Time,
    ElapsedTime,
    DateTime,
}

impl Kind {
    pub(crate) fn is_datetime(self) -> bool {
        matches!(
            self,
            Kind::Date | Kind::Time | Kind::ElapsedTime | Kind::DateTime
        )
    }
}

/// Classify a number format from its built-in id and/or custom format-code
/// string (the `.xlsx` entry point; built-in ids are shared with `.xls`).
#[cfg_attr(not(feature = "xlsx"), allow(dead_code))]
pub(crate) fn classify(numfmt_id: u16, format_code: Option<&str>) -> Kind {
    match format_code {
        Some(code) => classify_string(code),
        None => classify_builtin(numfmt_id),
    }
}

/// Render a numeric cell value for the given format kind and date system.
pub(crate) fn render_value(value: f64, kind: Kind, is_1904: bool) -> String {
    match kind {
        Kind::Percent => format!("{}%", crate::format_number(value * 100.0)),
        Kind::Date | Kind::DateTime if !is_1904 && value == 0.0 => "00:00:00".to_string(),
        Kind::Date => match serial_to_datetime(value, is_1904) {
            Some(dt) => dt.date(),
            None => crate::format_number(value),
        },
        Kind::Time => match serial_to_datetime(value, is_1904) {
            Some(dt) => dt.time(),
            None => crate::format_number(value),
        },
        Kind::ElapsedTime => match render_elapsed_time(value) {
            Some(s) => s,
            None => crate::format_number(value),
        },
        Kind::DateTime => match serial_to_datetime(value, is_1904) {
            Some(dt) => format!("{} {}", dt.date(), dt.time()),
            None => crate::format_number(value),
        },
        Kind::Plain => crate::format_number(value),
    }
}

/// Render a numeric value with an explicit spreadsheet number-format code.
/// Malformed or hostile codes fall back to the crate's stable plain-number
/// representation instead of panicking or emitting partial output.
pub(crate) fn render_format(value: f64, format_code: &str, is_1904: bool) -> String {
    number_format::render_number(value, format_code, is_1904)
        .unwrap_or_else(|| crate::format_number(value))
}

/// Apply an explicit fourth text section to an authored text cell. Number
/// formats without a text section leave the source text unchanged.
pub(crate) fn render_text_format(value: &str, format_code: &str) -> String {
    number_format::render_text(value, format_code).unwrap_or_else(|| value.to_string())
}

/// Render a built-in format while retaining the crate's established canonical
/// date/time strings. Percentage precision, grouping, fraction, and scientific
/// built-ins use the complete format engine.
pub(crate) fn render_indexed(value: f64, numfmt_id: u16, is_1904: bool) -> String {
    let kind = classify_builtin(numfmt_id);
    if matches!(
        kind,
        Kind::Date | Kind::Time | Kind::DateTime | Kind::ElapsedTime
    ) || matches!(numfmt_id, 0 | 49)
    {
        return render_value(value, kind, is_1904);
    }
    built_in_format_code(numfmt_id).map_or_else(
        || render_value(value, kind, is_1904),
        |code| render_format(value, code, is_1904),
    )
}

/// Accumulated formatting tables for a workbook.
#[derive(Debug, Default)]
pub(crate) struct Formats {
    /// 1904 date system (vs. the default 1900).
    datemode_1904: bool,
    /// Format-code index per `XF` record, in record order (cell `ixfe` indexes this).
    xf_ifmt: Vec<u16>,
    /// Custom format strings keyed by their format index (`FORMAT` records).
    custom: HashMap<u16, String>,
}

impl Formats {
    /// Record a `DATEMODE` (0x0022) body: u16, 1 → 1904 epoch.
    pub(crate) fn set_datemode(&mut self, data: &[u8]) {
        if let Some(v) = read_u16(data, 0) {
            self.datemode_1904 = v == 1;
        }
    }

    /// Whether the workbook uses the 1904 date system.
    pub(crate) fn date1904(&self) -> bool {
        self.datemode_1904
    }

    /// Record an `XF` (0x00E0) body: the format index is the u16 at offset 2.
    pub(crate) fn push_xf(&mut self, data: &[u8]) {
        self.xf_ifmt.push(read_u16(data, 2).unwrap_or(0));
    }

    /// Record a `FORMAT` (0x041E) body: u16 ifmt, then an `XLUnicodeString`
    /// format code. `ctx` decodes the string per BIFF generation/codepage.
    pub(crate) fn push_format(&mut self, data: &[u8], decode: impl FnOnce() -> Option<String>) {
        if let Some(ifmt) = read_u16(data, 0) {
            if let Some(s) = decode() {
                self.custom.insert(ifmt, s);
            }
        }
    }

    /// Resolve the effective format index for a cell's `ixfe`.
    fn ifmt_for(&self, ixfe: u16) -> u16 {
        self.xf_ifmt.get(ixfe as usize).copied().unwrap_or(0)
    }

    /// Resolve a number-format code for style retention. Locale-dependent
    /// built-ins are deliberately left unset rather than guessed.
    pub(crate) fn code_for_ifmt(&self, ifmt: u16) -> Option<String> {
        self.custom
            .get(&ifmt)
            .cloned()
            .or_else(|| built_in_format_code(ifmt).map(str::to_string))
    }

    fn kind(&self, ixfe: u16) -> Kind {
        let ifmt = self.ifmt_for(ixfe);
        if let Some(s) = self.custom.get(&ifmt) {
            classify_string(s)
        } else {
            classify_builtin(ifmt)
        }
    }

    /// Whether the cell's format is a date/time (so the typed value should be a
    /// date rather than a bare number).
    pub(crate) fn is_datetime(&self, ixfe: u16) -> bool {
        self.kind(ixfe).is_datetime()
    }

    /// Render a numeric cell value according to its format.
    pub(crate) fn render(&self, value: f64, ixfe: u16) -> String {
        let ifmt = self.ifmt_for(ixfe);
        match self.custom.get(&ifmt) {
            Some(code) => render_format(value, code, self.datemode_1904),
            None => render_indexed(value, ifmt, self.datemode_1904),
        }
    }

    /// Apply an explicit fourth text section for a cell XF.
    pub(crate) fn render_text(&self, value: &str, ixfe: u16) -> String {
        let ifmt = self.ifmt_for(ixfe);
        self.custom
            .get(&ifmt)
            .map_or_else(|| value.to_string(), |code| render_text_format(value, code))
    }
}

pub(crate) fn built_in_format_code(ifmt: u16) -> Option<&'static str> {
    match ifmt {
        1 => Some("0"),
        2 => Some("0.00"),
        3 => Some("#,##0"),
        4 => Some("#,##0.00"),
        9 => Some("0%"),
        10 => Some("0.00%"),
        11 => Some("0.00E+00"),
        12 => Some("# ?/?"),
        13 => Some("# ??/??"),
        14 => Some("m/d/yy"),
        15 => Some("d-mmm-yy"),
        16 => Some("d-mmm"),
        17 => Some("mmm-yy"),
        18 => Some("h:mm AM/PM"),
        19 => Some("h:mm:ss AM/PM"),
        20 => Some("h:mm"),
        21 => Some("h:mm:ss"),
        22 => Some("m/d/yy h:mm"),
        45 => Some("mm:ss"),
        46 => Some("[h]:mm:ss"),
        47 => Some("mm:ss.0"),
        48 => Some("##0.0E+0"),
        49 => Some("@"),
        _ => None,
    }
}

fn read_u16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}

/// Classify a built-in number-format index (no `FORMAT` record needed). Codes
/// per the [MS-XLS]/[MS-OSHARED] built-in table, including the East-Asian
/// (27-36, 50-58) and Thai (67-81) locale sets — these are NOT contiguous by
/// kind, so each sub-range is mapped to its true meaning.
fn classify_builtin(ifmt: u16) -> Kind {
    match ifmt {
        // 0%, 0.00%, Thai t0% / t0.00%.
        9 | 10 | 71 | 72 => Kind::Percent,
        // h:mm[:ss], CJK h時mm分[ss秒] (32/33), Thai th:mm (79/80).
        18..=21 | 32 | 33 | 45..=47 | 79 | 80 => Kind::Time,
        // m/d/yy h:mm and the Thai datetime (81).
        22 | 81 => Kind::DateTime,
        // Dates: Western (14-17), CJK eras (27-31, 34-36, 50-58), Thai (75-78).
        14..=17 | 27..=31 | 34..=36 | 50..=58 | 75..=78 => Kind::Date,
        // Everything else, incl. Thai number/fraction (67-70, 73, 74).
        _ => Kind::Plain,
    }
}

/// Classify a custom format-code string (xlrd-style): strip literals, then look
/// for percent and date/time field characters.
fn classify_string(s: &str) -> Kind {
    let s = first_format_section(s);
    let mut cleaned = String::new();
    let mut elapsed_time = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                for c2 in chars.by_ref() {
                    if c2 == '"' {
                        break;
                    }
                }
            }
            '[' => {
                // `[Red]` / `[$-409]` are color/locale (drop), but `[h]`/`[mm]`/
                // `[ss]` are elapsed-time tokens that carry a time component.
                let mut inner = String::new();
                for c2 in chars.by_ref() {
                    if c2 == ']' {
                        break;
                    }
                    inner.push(c2.to_ascii_lowercase());
                }
                if !inner.is_empty() && inner.chars().all(|c| matches!(c, 'h' | 'm' | 's')) {
                    elapsed_time = true;
                }
            }
            '\\' | '_' | '*' => {
                chars.next();
            }
            _ => cleaned.push(c.to_ascii_lowercase()),
        }
    }
    if cleaned.contains('%') {
        return Kind::Percent;
    }
    if elapsed_time {
        return Kind::ElapsedTime;
    }
    // AM/PM markers contain `m` letters that are not month tokens.
    let cleaned = cleaned.replace("am/pm", "").replace("a/p", "");
    let has_h_s = elapsed_time || cleaned.contains('h') || cleaned.contains('s');
    let m_count = cleaned.matches('m').count();
    // `m` is a minute next to h/s, otherwise a month; `mmm`/`mmmm` is always a
    // month name (date). So a month component is present when there are 3+ m's,
    // or any m with no surrounding time context.
    let has_month = m_count >= 3 || (m_count >= 1 && !has_h_s);
    let has_date = cleaned.contains('y') || cleaned.contains('d') || has_month;
    match (has_date, has_h_s) {
        (true, true) => Kind::DateTime,
        (true, false) => Kind::Date,
        (false, true) => Kind::Time,
        (false, false) => Kind::Plain,
    }
}

fn first_format_section(s: &str) -> &str {
    let mut chars = s.char_indices();
    while let Some((idx, c)) = chars.next() {
        match c {
            '"' => {
                for (_, c2) in chars.by_ref() {
                    if c2 == '"' {
                        break;
                    }
                }
            }
            '[' => {
                for (_, c2) in chars.by_ref() {
                    if c2 == ']' {
                        break;
                    }
                }
            }
            '\\' | '_' | '*' => {
                chars.next();
            }
            ';' => return &s[..idx],
            _ => {}
        }
    }
    s
}

fn render_elapsed_time(value: f64) -> Option<String> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let total_seconds = (value * 86_400.0).round() as u64;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    Some(format!("{hours}:{minutes:02}:{seconds:02}"))
}

/// A decoded date-time, rendered ISO-style for indexing.
struct DateTimeParts {
    y: i64,
    mo: u32,
    d: u32,
    h: u32,
    mi: u32,
    s: u32,
}

impl DateTimeParts {
    fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.y, self.mo, self.d)
    }
    fn time(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.h, self.mi, self.s)
    }
}

/// Convert an Excel date serial to calendar parts, honoring the 1900 vs 1904
/// epoch and the Excel 1900 phantom-leap-day bug. (Serial 60 — Excel's
/// fictitious 1900-02-29 — is folded onto 1900-02-28, the common practical
/// choice, since it never appears in real data.)
fn serial_to_datetime(serial: f64, is_1904: bool) -> Option<DateTimeParts> {
    // Upper bound = 9999-12-31. The 1904 epoch is 1462 days later, so its limit
    // is correspondingly lower.
    let max = if is_1904 { 2_957_004.0 } else { 2_958_466.0 };
    let min_epoch = if is_1904 {
        days_from_civil(1904, 1, 1)
    } else {
        days_from_civil(1899, 12, 30)
    };
    let min = (days_from_civil(1, 1, 1) - min_epoch) as f64;
    if !serial.is_finite() || serial < min || serial >= max {
        return None; // out of Excel's representable range
    }
    let whole = serial.floor() as i64;
    let frac = serial - serial.floor();

    // Days from the Unix epoch (1970-01-01).
    let epoch = if is_1904 {
        days_from_civil(1904, 1, 1)
    } else if serial < 0.0 {
        days_from_civil(1899, 12, 30)
    } else {
        days_from_civil(1899, 12, 31)
    };
    let mut days = epoch + whole;
    if !is_1904 && whole >= 60 {
        days -= 1; // fold out the fictitious 1900-02-29
    }

    // Time of day from the fractional part. Round to millisecond precision first
    // to match common OOXML readers, then expose whole-second display parts.
    // A fraction that rounds up to a full day carries into the date.
    let mut millis = (frac * 86_400_000.0).round() as i64;
    if millis >= 86_400_000 {
        millis -= 86_400_000;
        days += 1;
    }
    let secs = millis / 1_000;
    let (y, mo, d) = civil_from_days(days);
    let h = (secs / 3600) as u32;
    let mi = ((secs % 3600) / 60) as u32;
    let s = (secs % 60) as u32;

    Some(DateTimeParts { y, mo, d, h, mi, s })
}

pub(crate) fn serial_to_datetime_parts(
    serial: f64,
    is_1904: bool,
) -> Option<(i64, u32, u32, u32, u32, u32)> {
    let p = serial_to_datetime(serial, is_1904)?;
    Some((p.y, p.mo, p.d, p.h, p.mi, p.s))
}

/// Days from 1970-01-01 for a proleptic-Gregorian date (Howard Hinnant).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Parse an ISO date (`YYYY-MM-DD`, optionally `…THH:MM:SS`) to the Excel 1900
/// serial (days since 1899-12-30 + the time-of-day fraction). Shared by the
/// `.xlsx` (`t="d"`) and `.ods` date readers.
#[cfg_attr(not(any(feature = "xlsx", feature = "ods")), allow(dead_code))]
pub(crate) fn iso_date_to_serial(s: &str) -> Option<f64> {
    if s.len() >= 5 && s.as_bytes().get(2) == Some(&b':') {
        return iso_time_fraction(s);
    }
    let date = s.get(..10)?;
    let mut p = date.split('-');
    let y: i64 = p.next()?.parse().ok()?;
    let mo: u32 = p.next()?.parse().ok()?;
    let d: u32 = p.next()?.parse().ok()?;
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let max_d = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if d < 1 || d > max_d {
        return None;
    }
    // 1970-01-01 is Excel serial 25569.
    let mut serial = (days_from_civil(y, mo, d) + 25_569) as f64;
    if let Some(t) = s.get(11..) {
        serial += iso_time_fraction(t)?;
    }
    Some(serial)
}

fn iso_time_fraction(s: &str) -> Option<f64> {
    let mut tp = s.split(':');
    let h: f64 = tp.next()?.parse().ok()?;
    let mi: f64 = tp.next()?.parse().ok()?;
    let se: f64 = tp.next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
    if !(0.0..24.0).contains(&h) || !(0.0..60.0).contains(&mi) || !(0.0..61.0).contains(&se) {
        return None;
    }
    Some((h * 3600.0 + mi * 60.0 + se) / 86400.0)
}

/// Inverse of [`days_from_civil`] (Howard Hinnant) → `(year, month, day)`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_date_validates_day_of_month() {
        assert_eq!(iso_date_to_serial("2024-03-15"), Some(45366.0));
        assert_eq!(iso_date_to_serial("2024-02-29"), Some(45351.0)); // 2024 is leap
        assert_eq!(iso_date_to_serial("2023-02-29"), None); // 2023 is not
        assert_eq!(iso_date_to_serial("2024-02-31"), None); // never valid
        assert_eq!(iso_date_to_serial("2024-04-31"), None); // April has 30
        assert_eq!(iso_date_to_serial("2024-13-01"), None); // bad month
    }

    fn dt(serial: f64, is_1904: bool) -> String {
        let p = serial_to_datetime(serial, is_1904).unwrap();
        format!("{} {}", p.date(), p.time())
    }

    #[test]
    fn known_excel_serials() {
        // Anchor values verified against Excel/xlrd.
        assert_eq!(serial_to_datetime(1.0, false).unwrap().date(), "1900-01-01");
        assert_eq!(
            serial_to_datetime(59.0, false).unwrap().date(),
            "1900-02-28"
        );
        assert_eq!(
            serial_to_datetime(61.0, false).unwrap().date(),
            "1900-03-01"
        );
        assert_eq!(
            serial_to_datetime(45366.0, false).unwrap().date(),
            "2024-03-15"
        );
        // 1904 system: serial 0 = 1904-01-01.
        assert_eq!(serial_to_datetime(0.0, true).unwrap().date(), "1904-01-01");
    }

    #[test]
    fn datetime_fraction() {
        // 45366.5 = noon on 2024-03-15.
        assert_eq!(dt(45366.5, false), "2024-03-15 12:00:00");
    }

    #[test]
    fn datetime_fraction_matches_openpyxl_millisecond_precision() {
        let rounds_to_next_second = "41224.999988425923".parse::<f64>().unwrap();
        let keeps_fractional_second = "41224.999988368058".parse::<f64>().unwrap();

        assert_eq!(dt(rounds_to_next_second, false), "2012-11-11 23:59:59");
        assert_eq!(dt(keeps_fractional_second, false), "2012-11-11 23:59:58");
    }

    #[test]
    fn edge_serials() {
        // Serial 60 (Excel's fictitious 1900-02-29) is folded onto 1900-02-28.
        assert_eq!(
            serial_to_datetime(60.0, false).unwrap().date(),
            "1900-02-28"
        );
        // 1904 offset: serial 1462 = 1908-01-02 (proves the 1462-day arithmetic).
        assert_eq!(
            serial_to_datetime(1462.0, true).unwrap().date(),
            "1908-01-02"
        );
        // Millisecond-precision fractional time rounding up to a full day
        // carries into the date.
        let almost_next_day = "45366.999999999".parse::<f64>().unwrap();
        assert_eq!(dt(almost_next_day, false), "2024-03-16 00:00:00");
        // Range boundaries (9999-12-31 is the last valid date).
        assert_eq!(
            serial_to_datetime(2_958_465.0, false).unwrap().date(),
            "9999-12-31"
        );
        assert!(serial_to_datetime(2_958_466.0, false).is_none());
        // 1904's limit is 1462 lower.
        assert_eq!(
            serial_to_datetime(2_957_003.0, true).unwrap().date(),
            "9999-12-31"
        );
        assert!(serial_to_datetime(2_957_004.0, true).is_none());
        // Negative 1900-system serials follow openpyxl's practical epoch.
        assert_eq!(
            serial_to_datetime(-1.0, false).unwrap().date(),
            "1899-12-29"
        );
        assert!(serial_to_datetime(f64::NAN, false).is_none());
        assert!(serial_to_datetime(f64::INFINITY, false).is_none());
    }

    #[test]
    fn classification() {
        // Western builtins.
        assert_eq!(classify_builtin(14), Kind::Date);
        assert_eq!(classify_builtin(18), Kind::Time);
        assert_eq!(classify_builtin(9), Kind::Percent);
        assert_eq!(classify_builtin(0), Kind::Plain);
        // Thai builtins are NOT all dates: 67 number, 71 percent, 78 date,
        // 79 time, 81 datetime; and CJK 32/33 are times.
        assert_eq!(classify_builtin(67), Kind::Plain);
        assert_eq!(classify_builtin(71), Kind::Percent);
        assert_eq!(classify_builtin(78), Kind::Date);
        assert_eq!(classify_builtin(79), Kind::Time);
        assert_eq!(classify_builtin(81), Kind::DateTime);
        assert_eq!(classify_builtin(33), Kind::Time);
        // Custom strings.
        assert_eq!(classify_string("yyyy-mm-dd"), Kind::Date);
        assert_eq!(classify_string("yyyy\"년\" mm\"월\""), Kind::Date);
        assert_eq!(classify_string("h:mm:ss"), Kind::Time);
        assert_eq!(classify_string("h:mm:ss\\ AM/PM"), Kind::Time);
        assert_eq!(classify_string("0.00%"), Kind::Percent);
        assert_eq!(classify_string("#,##0"), Kind::Plain);
        // Literal `%` inside quotes is NOT a percent multiplier.
        assert_eq!(classify_string("\"×\"\\ 0.0\\ \"%\""), Kind::Plain);
        // Month names without day/year are dates; bare `m` is a month.
        assert_eq!(classify_string("mmmm"), Kind::Date);
        assert_eq!(classify_string("mmm"), Kind::Date);
        assert_eq!(classify_string("m"), Kind::Date);
        // Elapsed-time brackets carry a time component.
        assert_eq!(classify_string("[hh]"), Kind::ElapsedTime);
        assert_eq!(classify_string("[h]:mm"), Kind::ElapsedTime);
        // Color/locale brackets are dropped, not treated as time.
        assert_eq!(classify_string("[Red]#,##0"), Kind::Plain);
        assert_eq!(
            classify_string(r#"[$-F400]h:mm:ss\ AM/PM;[$-F400]h:mm:ss\ AM/PM;_-* ""??_-;_-@_-"#),
            Kind::Time
        );
    }

    #[test]
    fn render_date_and_percent() {
        let mut f = Formats::default();
        f.push_xf(&[0, 0, 14, 0]); // XF[0] -> ifmt 14 (date)
        f.push_xf(&[0, 0, 9, 0]); // XF[1] -> ifmt 9 (percent)
        f.push_xf(&[0, 0, 10, 0]); // XF[2] -> ifmt 10 (two-decimal percent)
        f.push_xf(&[0, 0, 0, 0]); // XF[3] -> ifmt 0 (plain)
        f.push_format(&[165, 0], || Some("[h]:mm".to_string()));
        f.push_xf(&[0, 0, 165, 0]); // XF[4] -> custom elapsed-time format
        assert_eq!(f.render(45366.0, 0), "2024-03-15");
        assert_eq!(f.render(0.5, 1), "50%");
        assert_eq!(f.render(0.5, 2), "50.00%");
        assert_eq!(f.render(1234.0, 3), "1234");
        assert_eq!(f.render(1.5, 4), "36:00");
    }

    #[test]
    fn date_formatted_serial_zero_renders_as_midnight_time_in_1900_system() {
        assert_eq!(render_value(0.0, Kind::Date, false), "00:00:00");
        assert_eq!(render_value(0.0, Kind::DateTime, false), "00:00:00");
        assert_eq!(render_value(0.0, Kind::Date, true), "1904-01-01");
        assert_eq!(
            render_value(0.0, Kind::DateTime, true),
            "1904-01-01 00:00:00"
        );
    }

    #[test]
    fn negative_1900_date_serials_match_openpyxl_epoch_display() {
        assert_eq!(render_value(-1.0, Kind::Date, false), "1899-12-29");
        assert_eq!(
            render_value(-0.5, Kind::DateTime, false),
            "1899-12-29 12:00:00"
        );
    }

    #[test]
    fn elapsed_time_renders_total_hours() {
        assert_eq!(classify_string("[h]:mm"), Kind::ElapsedTime);
        assert_eq!(render_value(1.5, Kind::ElapsedTime, false), "36:00:00");
    }
}
