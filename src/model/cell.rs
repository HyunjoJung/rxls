#[cfg(feature = "serde")]
use serde::de::VariantAccess;
/// Calendar date/time decoded from an Excel serial number.
///
/// This is a small dependency-free alternative to a `chrono` type. Pass the
/// workbook's [`crate::Workbook::date1904`] flag to [`excel_serial_to_datetime`] or
/// [`Cell::as_datetime`] so the 1900 vs 1904 date system is interpreted
/// correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExcelDateTime {
    /// Calendar year.
    pub year: i64,
    /// Month, 1 through 12.
    pub month: u32,
    /// Day of month, 1 through 31.
    pub day: u32,
    /// Hour, 0 through 23.
    pub hour: u32,
    /// Minute, 0 through 59.
    pub minute: u32,
    /// Second, 0 through 59.
    pub second: u32,
}

impl ExcelDateTime {
    /// Format the calendar date as `YYYY-MM-DD`.
    pub fn date_string(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// Format the time of day as `HH:MM:SS`.
    pub fn time_string(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }

    /// Convert this value to chrono's [`chrono::NaiveDateTime`].
    #[cfg(feature = "chrono")]
    pub fn to_naive_datetime(self) -> Option<chrono::NaiveDateTime> {
        let year = i32::try_from(self.year).ok()?;
        chrono::NaiveDate::from_ymd_opt(year, self.month, self.day)?.and_hms_opt(
            self.hour,
            self.minute,
            self.second,
        )
    }
}

impl std::fmt::Display for ExcelDateTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.date_string(), self.time_string())
    }
}

/// Convert an Excel date/time serial to calendar parts.
///
/// `date1904` should be the workbook's [`crate::Workbook::date1904`] value. Returns
/// `None` for non-finite, negative, or out-of-Excel-range serials.
pub fn excel_serial_to_datetime(serial: f64, date1904: bool) -> Option<ExcelDateTime> {
    let (year, month, day, hour, minute, second) =
        crate::format::serial_to_datetime_parts(serial, date1904)?;
    Some(ExcelDateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
    })
}

/// Convert an Excel date/time serial to chrono's [`chrono::NaiveDateTime`].
///
/// `date1904` should be the workbook's [`crate::Workbook::date1904`] value. Available
/// with the optional `chrono` feature.
#[cfg(feature = "chrono")]
pub fn excel_serial_to_naive_datetime(
    serial: f64,
    date1904: bool,
) -> Option<chrono::NaiveDateTime> {
    excel_serial_to_datetime(serial, date1904)?.to_naive_datetime()
}

/// Convert an Excel duration serial to chrono's [`chrono::Duration`].
///
/// Excel duration serials use the same day-based scale as date/time serials,
/// where `1.5` means 36 hours. Available with the optional `chrono` feature.
#[cfg(feature = "chrono")]
pub fn excel_serial_to_duration(serial: f64) -> Option<chrono::Duration> {
    if !serial.is_finite() {
        return None;
    }
    let milliseconds = (serial * 86_400_000.0).round();
    if !milliseconds.is_finite() || milliseconds < i64::MIN as f64 || milliseconds > i64::MAX as f64
    {
        return None;
    }
    Some(chrono::Duration::milliseconds(milliseconds as i64))
}

/// A typed cell value — the reader API. Mirrors the common spreadsheet cell
/// kinds; dates are pre-rendered to an ISO string (no `chrono` dependency).
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// Text (shared-string or inline-string) cell.
    Text(String),
    /// Numeric cell — the raw value (a percentage keeps its fraction, e.g. 0.5).
    Number(f64),
    /// Date / time / datetime cell — the raw Excel serial (e.g. `45366.0`),
    /// preserving the full value incl. time-of-day (like `calamine`). Use the
    /// workbook's date system to convert, or [`crate::Sheet::to_text`] for the
    /// Excel-formatted string.
    Date(f64),
    /// Boolean cell.
    Bool(bool),
    /// Error cell (`#DIV/0!`, `#N/A`, …).
    Error(String),
    /// A formula cell — the formula text (without the leading `=`) plus its last
    /// cached value. Supported readers use this variant when formula source can
    /// be recovered, and authoring APIs use it for newly written formulas.
    Formula {
        /// Formula text, e.g. `SUM(A1:A9)`.
        formula: String,
        /// The last cached result value.
        cached: Box<Cell>,
    },
}

/// Calamine-style typed spreadsheet error value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellErrorType {
    /// Division by zero (`#DIV/0!`).
    Div0,
    /// Unavailable value (`#N/A`).
    NA,
    /// Invalid name (`#NAME?`).
    Name,
    /// Null intersection (`#NULL!`).
    Null,
    /// Numeric error (`#NUM!`).
    Num,
    /// Invalid reference (`#REF!`).
    Ref,
    /// Invalid value (`#VALUE!`).
    Value,
    /// Data is still being fetched (`#DATA!`; legacy `#GETTING_DATA` is also
    /// accepted by [`CellErrorType::from_excel_error`]).
    GettingData,
}

impl CellErrorType {
    /// Classify an Excel error display string.
    pub fn from_excel_error(error: &str) -> Option<Self> {
        match error {
            "#DIV/0!" => Some(Self::Div0),
            "#N/A" => Some(Self::NA),
            "#NAME?" => Some(Self::Name),
            "#NULL!" => Some(Self::Null),
            "#NUM!" => Some(Self::Num),
            "#REF!" => Some(Self::Ref),
            "#VALUE!" => Some(Self::Value),
            "#GETTING_DATA" | "#DATA!" => Some(Self::GettingData),
            _ => None,
        }
    }

    /// Stable Excel display string used by rxls for this error.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Div0 => "#DIV/0!",
            Self::NA => "#N/A",
            Self::Name => "#NAME?",
            Self::Null => "#NULL!",
            Self::Num => "#NUM!",
            Self::Ref => "#REF!",
            Self::Value => "#VALUE!",
            Self::GettingData => "#DATA!",
        }
    }
}

impl std::fmt::Display for CellErrorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Calamine-style data value name for generic read-side code.
///
/// rxls keeps [`Cell`] as the concrete value model so formula cells can preserve
/// both source text and cached values. `Data` is a compatibility alias rather
/// than a second enum, so `Range` and `Sheet` accessors continue to return the
/// same borrowed values.
pub type Data = Cell;

/// Calamine-style borrowed data value name for generic read-side code.
///
/// rxls ranges already borrow [`Cell`] values from worksheets, so `DataRef` is
/// a compatibility alias rather than a second borrowed enum.
pub type DataRef<'a> = &'a Cell;

/// Header-row selection policy for serde row deserialization.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum HeaderRow {
    /// Use the first row in the range that contains at least one populated cell.
    #[default]
    FirstNonEmptyRow,
    /// Use the absolute worksheet row index as the header row.
    Row(u32),
}

impl From<u32> for HeaderRow {
    fn from(row: u32) -> Self {
        HeaderRow::Row(row)
    }
}

/// Calamine-style value inspection trait implemented by [`Cell`]/[`Data`].
///
/// Missing worksheet positions are represented as `None` in range APIs rather
/// than as an empty cell value, so [`DataType::is_empty`] is always `false` for
/// concrete cells. [`DataRef`] and other references to `DataType` values
/// delegate to the referenced value. Formula cells delegate value predicates and
/// conversions to their cached result while [`DataType::get_formula`] exposes
/// the source text.
pub trait DataType {
    /// `true` when this value represents an empty cell.
    fn is_empty(&self) -> bool;
    /// `true` when this value is numeric and can be represented as an integer.
    fn is_int(&self) -> bool;
    /// `true` when this value is a non-date number.
    fn is_float(&self) -> bool;
    /// `true` when this value is a boolean.
    fn is_bool(&self) -> bool;
    /// `true` when this value is text.
    fn is_string(&self) -> bool;
    /// `true` when this value is an error.
    fn is_error(&self) -> bool;
    /// `true` when this value is a date/time serial.
    fn is_datetime(&self) -> bool;
    /// `true` when this value stores an ISO8601 datetime string distinctly.
    fn is_datetime_iso(&self) -> bool;
    /// `true` when this value stores an ISO8601 duration string distinctly.
    fn is_duration_iso(&self) -> bool;
    /// `true` when this value stores formula source text.
    fn is_formula(&self) -> bool;
    /// Get the integer value when present.
    fn get_int(&self) -> Option<i64>;
    /// Get the non-date floating-point value when present.
    fn get_float(&self) -> Option<f64>;
    /// Get the boolean value when present.
    fn get_bool(&self) -> Option<bool>;
    /// Get the borrowed text value when present.
    fn get_string(&self) -> Option<&str>;
    /// Get the borrowed error text when present.
    fn get_error(&self) -> Option<&str>;
    /// Get the typed spreadsheet error when present and recognized.
    fn get_error_type(&self) -> Option<CellErrorType>;
    /// Get the raw Excel date/time serial when present.
    fn get_datetime(&self) -> Option<f64>;
    /// Get formula source text without a leading `=` when present.
    fn get_formula(&self) -> Option<&str>;
    /// Get the ISO8601 datetime string when represented distinctly.
    fn get_datetime_iso(&self) -> Option<&str>;
    /// Get the ISO8601 duration string when represented distinctly.
    fn get_duration_iso(&self) -> Option<&str>;
    /// Get the cached value for a formula cell.
    fn cached_value(&self) -> Option<&Cell>;
    /// Convert to a string when natural for the underlying value.
    fn as_string(&self) -> Option<String>;
    /// Convert to an integer when possible.
    fn as_i64(&self) -> Option<i64>;
    /// Convert to a floating-point value when possible.
    fn as_f64(&self) -> Option<f64>;
    /// Decode this value as an Excel date/time using the workbook date system.
    fn as_datetime(&self, date1904: bool) -> Option<ExcelDateTime>;
    /// Decode this value as chrono's [`chrono::NaiveDateTime`].
    #[cfg(feature = "chrono")]
    fn as_naive_datetime(&self, date1904: bool) -> Option<chrono::NaiveDateTime>;
    /// Decode this value as chrono's [`chrono::NaiveDate`].
    #[cfg(feature = "chrono")]
    fn as_naive_date(&self, date1904: bool) -> Option<chrono::NaiveDate>;
    /// Decode this value as chrono's [`chrono::NaiveDate`].
    ///
    /// This is a calamine-style alias for [`DataType::as_naive_date`]. rxls keeps
    /// the `date1904` argument explicit because [`Cell::Date`] stores the raw
    /// Excel serial.
    #[cfg(feature = "chrono")]
    fn as_date(&self, date1904: bool) -> Option<chrono::NaiveDate>;
    /// Decode this value as chrono's [`chrono::NaiveTime`].
    #[cfg(feature = "chrono")]
    fn as_naive_time(&self, date1904: bool) -> Option<chrono::NaiveTime>;
    /// Decode this value as chrono's [`chrono::NaiveTime`].
    ///
    /// This is a calamine-style alias for [`DataType::as_naive_time`]. rxls keeps
    /// the `date1904` argument explicit because [`Cell::Date`] stores the raw
    /// Excel serial.
    #[cfg(feature = "chrono")]
    fn as_time(&self, date1904: bool) -> Option<chrono::NaiveTime>;
    /// Decode this value as a chrono duration serial.
    #[cfg(feature = "chrono")]
    fn as_duration(&self) -> Option<chrono::Duration>;
}

impl DataType for Cell {
    fn is_empty(&self) -> bool {
        Cell::is_empty(self)
    }

    fn is_int(&self) -> bool {
        Cell::is_int(self)
    }

    fn is_float(&self) -> bool {
        Cell::is_float(self)
    }

    fn is_bool(&self) -> bool {
        Cell::is_bool(self)
    }

    fn is_string(&self) -> bool {
        Cell::is_string(self)
    }

    fn is_error(&self) -> bool {
        Cell::is_error(self)
    }

    fn is_datetime(&self) -> bool {
        Cell::is_datetime(self)
    }

    fn is_datetime_iso(&self) -> bool {
        Cell::is_datetime_iso(self)
    }

    fn is_duration_iso(&self) -> bool {
        Cell::is_duration_iso(self)
    }

    fn is_formula(&self) -> bool {
        Cell::is_formula(self)
    }

    fn get_int(&self) -> Option<i64> {
        Cell::get_int(self)
    }

    fn get_float(&self) -> Option<f64> {
        Cell::get_float(self)
    }

    fn get_bool(&self) -> Option<bool> {
        Cell::get_bool(self)
    }

    fn get_string(&self) -> Option<&str> {
        Cell::get_string(self)
    }

    fn get_error(&self) -> Option<&str> {
        Cell::get_error(self)
    }

    fn get_error_type(&self) -> Option<CellErrorType> {
        Cell::get_error_type(self)
    }

    fn get_datetime(&self) -> Option<f64> {
        Cell::get_datetime(self)
    }

    fn get_formula(&self) -> Option<&str> {
        Cell::get_formula(self)
    }

    fn get_datetime_iso(&self) -> Option<&str> {
        Cell::get_datetime_iso(self)
    }

    fn get_duration_iso(&self) -> Option<&str> {
        Cell::get_duration_iso(self)
    }

    fn cached_value(&self) -> Option<&Cell> {
        Cell::cached_value(self)
    }

    fn as_string(&self) -> Option<String> {
        Cell::as_string(self)
    }

    fn as_i64(&self) -> Option<i64> {
        Cell::as_i64(self)
    }

    fn as_f64(&self) -> Option<f64> {
        Cell::as_f64(self)
    }

    fn as_datetime(&self, date1904: bool) -> Option<ExcelDateTime> {
        Cell::as_datetime(self, date1904)
    }

    #[cfg(feature = "chrono")]
    fn as_naive_datetime(&self, date1904: bool) -> Option<chrono::NaiveDateTime> {
        Cell::as_naive_datetime(self, date1904)
    }

    #[cfg(feature = "chrono")]
    fn as_naive_date(&self, date1904: bool) -> Option<chrono::NaiveDate> {
        Cell::as_naive_date(self, date1904)
    }

    #[cfg(feature = "chrono")]
    fn as_date(&self, date1904: bool) -> Option<chrono::NaiveDate> {
        Cell::as_date(self, date1904)
    }

    #[cfg(feature = "chrono")]
    fn as_naive_time(&self, date1904: bool) -> Option<chrono::NaiveTime> {
        Cell::as_naive_time(self, date1904)
    }

    #[cfg(feature = "chrono")]
    fn as_time(&self, date1904: bool) -> Option<chrono::NaiveTime> {
        Cell::as_time(self, date1904)
    }

    #[cfg(feature = "chrono")]
    fn as_duration(&self) -> Option<chrono::Duration> {
        Cell::as_duration(self)
    }
}

impl<T> DataType for &T
where
    T: DataType + ?Sized,
{
    fn is_empty(&self) -> bool {
        (**self).is_empty()
    }

    fn is_int(&self) -> bool {
        (**self).is_int()
    }

    fn is_float(&self) -> bool {
        (**self).is_float()
    }

    fn is_bool(&self) -> bool {
        (**self).is_bool()
    }

    fn is_string(&self) -> bool {
        (**self).is_string()
    }

    fn is_error(&self) -> bool {
        (**self).is_error()
    }

    fn is_datetime(&self) -> bool {
        (**self).is_datetime()
    }

    fn is_datetime_iso(&self) -> bool {
        (**self).is_datetime_iso()
    }

    fn is_duration_iso(&self) -> bool {
        (**self).is_duration_iso()
    }

    fn is_formula(&self) -> bool {
        (**self).is_formula()
    }

    fn get_int(&self) -> Option<i64> {
        (**self).get_int()
    }

    fn get_float(&self) -> Option<f64> {
        (**self).get_float()
    }

    fn get_bool(&self) -> Option<bool> {
        (**self).get_bool()
    }

    fn get_string(&self) -> Option<&str> {
        (**self).get_string()
    }

    fn get_error(&self) -> Option<&str> {
        (**self).get_error()
    }

    fn get_error_type(&self) -> Option<CellErrorType> {
        (**self).get_error_type()
    }

    fn get_datetime(&self) -> Option<f64> {
        (**self).get_datetime()
    }

    fn get_formula(&self) -> Option<&str> {
        (**self).get_formula()
    }

    fn get_datetime_iso(&self) -> Option<&str> {
        (**self).get_datetime_iso()
    }

    fn get_duration_iso(&self) -> Option<&str> {
        (**self).get_duration_iso()
    }

    fn cached_value(&self) -> Option<&Cell> {
        (**self).cached_value()
    }

    fn as_string(&self) -> Option<String> {
        (**self).as_string()
    }

    fn as_i64(&self) -> Option<i64> {
        (**self).as_i64()
    }

    fn as_f64(&self) -> Option<f64> {
        (**self).as_f64()
    }

    fn as_datetime(&self, date1904: bool) -> Option<ExcelDateTime> {
        (**self).as_datetime(date1904)
    }

    #[cfg(feature = "chrono")]
    fn as_naive_datetime(&self, date1904: bool) -> Option<chrono::NaiveDateTime> {
        (**self).as_naive_datetime(date1904)
    }

    #[cfg(feature = "chrono")]
    fn as_naive_date(&self, date1904: bool) -> Option<chrono::NaiveDate> {
        (**self).as_naive_date(date1904)
    }

    #[cfg(feature = "chrono")]
    fn as_date(&self, date1904: bool) -> Option<chrono::NaiveDate> {
        (**self).as_date(date1904)
    }

    #[cfg(feature = "chrono")]
    fn as_naive_time(&self, date1904: bool) -> Option<chrono::NaiveTime> {
        (**self).as_naive_time(date1904)
    }

    #[cfg(feature = "chrono")]
    fn as_time(&self, date1904: bool) -> Option<chrono::NaiveTime> {
        (**self).as_time(date1904)
    }

    #[cfg(feature = "chrono")]
    fn as_duration(&self) -> Option<chrono::Duration> {
        (**self).as_duration()
    }
}

impl Cell {
    /// `true` when this cell represents an empty value.
    ///
    /// rxls represents empty worksheet positions as `None` in range APIs rather
    /// than as a `Cell` variant, so every concrete `Cell` returns `false`.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// `true` when this cell is numeric and can be represented as an integer.
    ///
    /// Formula cells delegate to their cached value.
    pub fn is_int(&self) -> bool {
        match self {
            Cell::Number(n) => n.is_finite() && n.fract() == 0.0,
            Cell::Formula { cached, .. } => cached.is_int(),
            _ => false,
        }
    }

    /// `true` when this cell is a numeric value.
    ///
    /// Formula cells delegate to their cached value.
    pub fn is_float(&self) -> bool {
        match self {
            Cell::Number(_) => true,
            Cell::Formula { cached, .. } => cached.is_float(),
            _ => false,
        }
    }

    /// `true` when this cell is a boolean.
    ///
    /// Formula cells delegate to their cached value.
    pub fn is_bool(&self) -> bool {
        match self {
            Cell::Bool(_) => true,
            Cell::Formula { cached, .. } => cached.is_bool(),
            _ => false,
        }
    }

    /// `true` when this cell is a text string.
    ///
    /// Formula cells delegate to their cached value.
    pub fn is_string(&self) -> bool {
        match self {
            Cell::Text(_) => true,
            Cell::Formula { cached, .. } => cached.is_string(),
            _ => false,
        }
    }

    /// `true` when this cell is an error value.
    ///
    /// Formula cells delegate to their cached value.
    pub fn is_error(&self) -> bool {
        match self {
            Cell::Error(_) => true,
            Cell::Formula { cached, .. } => cached.is_error(),
            _ => false,
        }
    }

    /// `true` when this cell is a date/time serial.
    ///
    /// Formula cells delegate to their cached value.
    pub fn is_datetime(&self) -> bool {
        match self {
            Cell::Date(_) => true,
            Cell::Formula { cached, .. } => cached.is_datetime(),
            _ => false,
        }
    }

    /// `true` when this cell stores an ISO8601 datetime string variant.
    ///
    /// rxls currently normalizes parsed datetime cells to serial-backed
    /// [`Cell::Date`], so this compatibility alias returns `false` unless a
    /// future cell variant can carry ISO datetime text distinctly. Formula cells
    /// delegate to their cached value.
    pub fn is_datetime_iso(&self) -> bool {
        match self {
            Cell::Formula { cached, .. } => cached.is_datetime_iso(),
            _ => false,
        }
    }

    /// `true` when this cell stores an ISO8601 duration string variant.
    ///
    /// rxls currently has no distinct duration cell variant. Formula cells
    /// delegate to their cached value.
    pub fn is_duration_iso(&self) -> bool {
        match self {
            Cell::Formula { cached, .. } => cached.is_duration_iso(),
            _ => false,
        }
    }

    /// `true` when this cell stores formula source text.
    pub fn is_formula(&self) -> bool {
        matches!(self, Cell::Formula { .. })
    }

    /// Get this cell's integer value when it is a finite whole number.
    ///
    /// Formula cells delegate to their cached value.
    pub fn get_int(&self) -> Option<i64> {
        match self {
            Cell::Number(n) if n.is_finite() && n.fract() == 0.0 => Some(*n as i64),
            Cell::Formula { cached, .. } => cached.get_int(),
            _ => None,
        }
    }

    /// Get this cell's numeric value when it is a non-date number.
    ///
    /// Formula cells delegate to their cached value.
    pub fn get_float(&self) -> Option<f64> {
        match self {
            Cell::Number(n) => Some(*n),
            Cell::Formula { cached, .. } => cached.get_float(),
            _ => None,
        }
    }

    /// Get this cell's boolean value.
    ///
    /// Formula cells delegate to their cached value.
    pub fn get_bool(&self) -> Option<bool> {
        match self {
            Cell::Bool(b) => Some(*b),
            Cell::Formula { cached, .. } => cached.get_bool(),
            _ => None,
        }
    }

    /// Get this cell's borrowed text value.
    ///
    /// Formula cells delegate to their cached value.
    pub fn get_string(&self) -> Option<&str> {
        match self {
            Cell::Text(s) => Some(s.as_str()),
            Cell::Formula { cached, .. } => cached.get_string(),
            _ => None,
        }
    }

    /// Get this cell's borrowed error text.
    ///
    /// Formula cells delegate to their cached value.
    pub fn get_error(&self) -> Option<&str> {
        match self {
            Cell::Error(e) => Some(e.as_str()),
            Cell::Formula { cached, .. } => cached.get_error(),
            _ => None,
        }
    }

    /// Get this cell's typed spreadsheet error value when the stored error text
    /// is recognized.
    ///
    /// Formula cells delegate to their cached value. The raw display string
    /// remains available through [`Cell::get_error`].
    pub fn get_error_type(&self) -> Option<CellErrorType> {
        match self {
            Cell::Error(error) => CellErrorType::from_excel_error(error),
            Cell::Formula { cached, .. } => cached.get_error_type(),
            _ => None,
        }
    }

    /// Get this cell's raw Excel date/time serial, if it is a date.
    ///
    /// Formula cells delegate to their cached value. Use
    /// [`Cell::as_datetime`] with the workbook date system to decode the serial
    /// into calendar parts.
    pub fn get_datetime(&self) -> Option<f64> {
        match self {
            Cell::Date(serial) => Some(*serial),
            Cell::Formula { cached, .. } => cached.get_datetime(),
            _ => None,
        }
    }

    /// Get formula source text without the leading `=`, if this is a formula
    /// cell.
    pub fn get_formula(&self) -> Option<&str> {
        match self {
            Cell::Formula { formula, .. } => Some(formula.as_str()),
            _ => None,
        }
    }

    /// Get an ISO8601 datetime string if this cell stores one distinctly.
    ///
    /// rxls currently represents parsed datetimes as [`Cell::Date`] serials, so
    /// this returns `None`. Formula cells delegate to their cached value.
    pub fn get_datetime_iso(&self) -> Option<&str> {
        match self {
            Cell::Formula { cached, .. } => cached.get_datetime_iso(),
            _ => None,
        }
    }

    /// Get an ISO8601 duration string if this cell stores one distinctly.
    ///
    /// rxls currently has no distinct duration cell variant. Formula cells
    /// delegate to their cached value.
    pub fn get_duration_iso(&self) -> Option<&str> {
        match self {
            Cell::Formula { cached, .. } => cached.get_duration_iso(),
            _ => None,
        }
    }

    /// Get the cached value for a formula cell.
    pub fn cached_value(&self) -> Option<&Cell> {
        match self {
            Cell::Formula { cached, .. } => Some(cached.as_ref()),
            _ => None,
        }
    }

    /// Convert this cell to a string when the conversion is natural for the
    /// underlying value.
    ///
    /// Text is cloned, numbers use rxls' display-stable numeric formatter, and
    /// formula cells delegate to their cached value.
    pub fn as_string(&self) -> Option<String> {
        match self {
            Cell::Text(s) => Some(s.clone()),
            Cell::Number(n) => Some(crate::format_number(*n)),
            Cell::Formula { cached, .. } => cached.as_string(),
            _ => None,
        }
    }

    /// Convert this cell to an integer when possible.
    ///
    /// Numbers and date serials are truncated toward zero, booleans become
    /// `0`/`1`, strings are parsed as `i64`, and formula cells delegate to their
    /// cached value.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Cell::Number(n) | Cell::Date(n) if n.is_finite() => Some(*n as i64),
            Cell::Bool(b) => Some(i64::from(*b)),
            Cell::Text(s) => s.parse::<i64>().ok(),
            Cell::Formula { cached, .. } => cached.as_i64(),
            _ => None,
        }
    }

    /// Convert this cell to a floating-point number when possible.
    ///
    /// Numbers and date serials return their raw serial value, booleans become
    /// `0.0`/`1.0`, strings are parsed as `f64`, and formula cells delegate to
    /// their cached value.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Cell::Number(n) | Cell::Date(n) => Some(*n),
            Cell::Bool(b) => Some(f64::from(*b as u8)),
            Cell::Text(s) => s.parse::<f64>().ok(),
            Cell::Formula { cached, .. } => cached.as_f64(),
            _ => None,
        }
    }

    /// Decode this cell as an Excel date/time, if it is a [`Cell::Date`] or a
    /// numeric Excel serial candidate.
    ///
    /// Formula cells delegate to their cached value. `date1904` should be the
    /// workbook's [`crate::Workbook::date1904`] value.
    pub fn as_datetime(&self, date1904: bool) -> Option<ExcelDateTime> {
        match self {
            Cell::Number(serial) | Cell::Date(serial) => {
                excel_serial_to_datetime(*serial, date1904)
            }
            Cell::Formula { cached, .. } => cached.as_datetime(date1904),
            _ => None,
        }
    }

    /// Decode this cell as chrono's [`chrono::NaiveDateTime`], if it is a date
    /// or numeric Excel serial candidate.
    ///
    /// Formula cells delegate to their cached value. `date1904` should be the
    /// workbook's [`crate::Workbook::date1904`] value. Available with the optional
    /// `chrono` feature.
    #[cfg(feature = "chrono")]
    pub fn as_naive_datetime(&self, date1904: bool) -> Option<chrono::NaiveDateTime> {
        self.as_datetime(date1904)?.to_naive_datetime()
    }

    /// Decode this cell as chrono's [`chrono::NaiveDate`], if it is a date or
    /// numeric Excel serial candidate.
    ///
    /// Formula cells delegate to their cached value. `date1904` should be the
    /// workbook's [`crate::Workbook::date1904`] value. Available with the optional
    /// `chrono` feature.
    #[cfg(feature = "chrono")]
    pub fn as_naive_date(&self, date1904: bool) -> Option<chrono::NaiveDate> {
        self.as_naive_datetime(date1904).map(|dt| dt.date())
    }

    /// Decode this cell as chrono's [`chrono::NaiveDate`], if it is a date or
    /// numeric Excel serial candidate.
    ///
    /// This is a calamine-style alias for [`Cell::as_naive_date`]. Formula cells
    /// delegate to their cached value. `date1904` should be the workbook's
    /// [`crate::Workbook::date1904`] value. Available with the optional `chrono`
    /// feature.
    #[cfg(feature = "chrono")]
    pub fn as_date(&self, date1904: bool) -> Option<chrono::NaiveDate> {
        self.as_naive_date(date1904)
    }

    /// Decode this cell as chrono's [`chrono::NaiveTime`], if it is a date or
    /// numeric Excel serial candidate.
    ///
    /// Formula cells delegate to their cached value. `date1904` should be the
    /// workbook's [`crate::Workbook::date1904`] value. Available with the optional
    /// `chrono` feature.
    #[cfg(feature = "chrono")]
    pub fn as_naive_time(&self, date1904: bool) -> Option<chrono::NaiveTime> {
        self.as_naive_datetime(date1904).map(|dt| dt.time())
    }

    /// Decode this cell as chrono's [`chrono::NaiveTime`], if it is a date or
    /// numeric Excel serial candidate.
    ///
    /// This is a calamine-style alias for [`Cell::as_naive_time`]. Formula cells
    /// delegate to their cached value. `date1904` should be the workbook's
    /// [`crate::Workbook::date1904`] value. Available with the optional `chrono`
    /// feature.
    #[cfg(feature = "chrono")]
    pub fn as_time(&self, date1904: bool) -> Option<chrono::NaiveTime> {
        self.as_naive_time(date1904)
    }

    /// Decode this cell as chrono's [`chrono::Duration`], if it is a date/time
    /// or numeric duration serial.
    ///
    /// Formula cells delegate to their cached value. Excel durations use the
    /// same day-based serial scale as date/time cells, so `1.5` becomes 36
    /// hours. Available with the optional `chrono` feature.
    #[cfg(feature = "chrono")]
    pub fn as_duration(&self) -> Option<chrono::Duration> {
        match self {
            Cell::Number(serial) | Cell::Date(serial) => excel_serial_to_duration(*serial),
            Cell::Formula { cached, .. } => cached.as_duration(),
            _ => None,
        }
    }
}

#[cfg(feature = "serde")]
const CELL_VARIANTS: &[&str] = &["Text", "Number", "Date", "Bool", "Error", "Formula"];

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Cell {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_enum("Cell", CELL_VARIANTS, CellVisitor)
    }
}

#[cfg(feature = "serde")]
struct CellVisitor;

#[cfg(feature = "serde")]
impl<'de> serde::de::Visitor<'de> for CellVisitor {
    type Value = Cell;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an rxls Cell")
    }

    fn visit_enum<A>(self, data: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: serde::de::EnumAccess<'de>,
    {
        let (variant, access) = data.variant::<String>()?;
        match variant.as_str() {
            "Text" => access.newtype_variant().map(Cell::Text),
            "Number" => access.newtype_variant().map(Cell::Number),
            "Date" => access.newtype_variant().map(Cell::Date),
            "Bool" => access.newtype_variant().map(Cell::Bool),
            "Error" => access.newtype_variant().map(Cell::Error),
            "Formula" => {
                let (formula, cached): (String, Cell) = access.newtype_variant()?;
                Ok(Cell::Formula {
                    formula,
                    cached: Box::new(cached),
                })
            }
            other => Err(serde::de::Error::unknown_variant(other, CELL_VARIANTS)),
        }
    }
}

// --- Authoring API (build a workbook from data; the writer serializes it) ---

impl Cell {
    /// A text cell from owned or borrowed text.
    pub fn text(value: impl Into<String>) -> Self {
        Cell::Text(value.into())
    }

    /// Calamine-style alias for [`Cell::text`].
    pub fn string(value: impl Into<String>) -> Self {
        Self::text(value)
    }

    /// A numeric cell from an integer value.
    pub fn int(value: impl Into<i64>) -> Self {
        Cell::Number(value.into() as f64)
    }

    /// A numeric cell from a floating-point value.
    pub fn float(value: impl Into<f64>) -> Self {
        Cell::Number(value.into())
    }

    /// A boolean cell.
    pub fn boolean(value: bool) -> Self {
        Cell::Bool(value)
    }

    /// A typed spreadsheet error cell.
    pub fn error(error: CellErrorType) -> Self {
        Cell::Error(error.as_str().to_string())
    }

    /// A date/time cell from an Excel serial (days since the workbook epoch).
    pub fn date(serial: f64) -> Self {
        Self::date_time(serial)
    }

    /// Calamine-style date/time constructor over rxls' explicit serial model.
    pub fn date_time(serial: f64) -> Self {
        Cell::Date(serial)
    }

    /// A formula cell with source text and a cached value.
    pub fn formula(formula: impl Into<String>, cached: impl Into<Cell>) -> Self {
        Cell::Formula {
            formula: formula.into(),
            cached: Box::new(cached.into()),
        }
    }
}
impl From<&str> for Cell {
    fn from(s: &str) -> Self {
        Cell::Text(s.to_string())
    }
}
impl From<String> for Cell {
    fn from(s: String) -> Self {
        Cell::Text(s)
    }
}
impl From<&Cell> for Cell {
    fn from(cell: &Cell) -> Self {
        cell.clone()
    }
}
impl From<f64> for Cell {
    fn from(n: f64) -> Self {
        Cell::Number(n)
    }
}
impl From<f32> for Cell {
    fn from(n: f32) -> Self {
        Cell::Number(f64::from(n))
    }
}
impl From<i64> for Cell {
    fn from(n: i64) -> Self {
        Cell::Number(n as f64)
    }
}
impl From<i32> for Cell {
    fn from(n: i32) -> Self {
        Cell::Number(n as f64)
    }
}
macro_rules! impl_cell_from_signed_int {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl From<$ty> for Cell {
                fn from(n: $ty) -> Self {
                    Cell::Number(n as f64)
                }
            }
        )+
    };
}

macro_rules! impl_cell_from_unsigned_int {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl From<$ty> for Cell {
                fn from(n: $ty) -> Self {
                    Cell::Number(n as f64)
                }
            }
        )+
    };
}

impl_cell_from_signed_int!(i8, i16, i128, isize);
impl_cell_from_unsigned_int!(u8, u16, u32, u64, u128, usize);

impl From<bool> for Cell {
    fn from(b: bool) -> Self {
        Cell::Bool(b)
    }
}
impl From<CellErrorType> for Cell {
    fn from(error: CellErrorType) -> Self {
        Cell::Error(error.as_str().to_string())
    }
}

/// A best-effort display string for an authored value (for [`crate::Sheet::to_text`];
/// the written `.xlsx` renders via the cell's number format).
pub(super) fn display_text(v: &Cell) -> String {
    match v {
        Cell::Text(s) => s.clone(),
        Cell::Number(n) | Cell::Date(n) => crate::format_number(*n),
        Cell::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Cell::Error(e) => e.clone(),
        Cell::Formula { cached, .. } => display_text(cached),
    }
}

pub(super) fn display_text_with_num_fmt(v: &Cell, num_fmt: Option<&str>, date1904: bool) -> String {
    let Some(num_fmt) = num_fmt else {
        return display_text(v);
    };
    match v {
        Cell::Text(text) => crate::format::render_text_format(text, num_fmt),
        Cell::Number(number) | Cell::Date(number) => {
            crate::format::render_format(*number, num_fmt, date1904)
        }
        Cell::Formula { cached, .. } => display_text_with_num_fmt(cached, Some(num_fmt), date1904),
        Cell::Bool(_) | Cell::Error(_) => display_text(v),
    }
}

impl std::fmt::Display for Cell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&display_text(self))
    }
}

impl PartialEq<&str> for Cell {
    fn eq(&self, other: &&str) -> bool {
        cell_eq_str(self, other)
    }
}

impl PartialEq<str> for Cell {
    fn eq(&self, other: &str) -> bool {
        cell_eq_str(self, other)
    }
}

impl PartialEq<String> for Cell {
    fn eq(&self, other: &String) -> bool {
        cell_eq_str(self, other)
    }
}

impl PartialEq<&String> for Cell {
    fn eq(&self, other: &&String) -> bool {
        cell_eq_str(self, other)
    }
}

impl PartialEq<Cell> for &str {
    fn eq(&self, other: &Cell) -> bool {
        cell_eq_str(other, self)
    }
}

impl PartialEq<Cell> for String {
    fn eq(&self, other: &Cell) -> bool {
        cell_eq_str(other, self)
    }
}

impl PartialEq<Cell> for &String {
    fn eq(&self, other: &Cell) -> bool {
        cell_eq_str(other, self)
    }
}

impl PartialEq<f64> for Cell {
    fn eq(&self, other: &f64) -> bool {
        cell_eq_f64(self, *other)
    }
}

impl PartialEq<f32> for Cell {
    fn eq(&self, other: &f32) -> bool {
        cell_eq_f64(self, f64::from(*other))
    }
}

impl PartialEq<Cell> for f64 {
    fn eq(&self, other: &Cell) -> bool {
        cell_eq_f64(other, *self)
    }
}

impl PartialEq<Cell> for f32 {
    fn eq(&self, other: &Cell) -> bool {
        cell_eq_f64(other, f64::from(*self))
    }
}

macro_rules! impl_cell_partial_eq_signed_int {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl PartialEq<$ty> for Cell {
                fn eq(&self, other: &$ty) -> bool {
                    cell_eq_signed_int(self, *other as i128)
                }
            }

            impl PartialEq<Cell> for $ty {
                fn eq(&self, other: &Cell) -> bool {
                    cell_eq_signed_int(other, *self as i128)
                }
            }
        )+
    };
}

macro_rules! impl_cell_partial_eq_unsigned_int {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl PartialEq<$ty> for Cell {
                fn eq(&self, other: &$ty) -> bool {
                    cell_eq_unsigned_int(self, *other as u128)
                }
            }

            impl PartialEq<Cell> for $ty {
                fn eq(&self, other: &Cell) -> bool {
                    cell_eq_unsigned_int(other, *self as u128)
                }
            }
        )+
    };
}

impl_cell_partial_eq_signed_int!(i8, i16, i32, i64, i128, isize);
impl_cell_partial_eq_unsigned_int!(u8, u16, u32, u64, u128, usize);

impl PartialEq<bool> for Cell {
    fn eq(&self, other: &bool) -> bool {
        match self {
            Cell::Bool(b) => b == other,
            Cell::Formula { cached, .. } => cached.as_ref() == other,
            _ => false,
        }
    }
}

impl PartialEq<Cell> for bool {
    fn eq(&self, other: &Cell) -> bool {
        match other {
            Cell::Bool(b) => self == b,
            Cell::Formula { cached, .. } => self == cached.as_ref(),
            _ => false,
        }
    }
}

fn cell_eq_str(cell: &Cell, other: &str) -> bool {
    match cell {
        Cell::Text(s) => s == other,
        Cell::Formula { cached, .. } => cell_eq_str(cached, other),
        _ => false,
    }
}

fn cell_eq_f64(cell: &Cell, other: f64) -> bool {
    match cell {
        Cell::Number(n) => *n == other,
        Cell::Formula { cached, .. } => cell_eq_f64(cached, other),
        _ => false,
    }
}

fn cell_eq_signed_int(cell: &Cell, other: i128) -> bool {
    match cell {
        Cell::Number(n) => n.is_finite() && n.fract() == 0.0 && *n == other as f64,
        Cell::Formula { cached, .. } => cell_eq_signed_int(cached, other),
        _ => false,
    }
}

fn cell_eq_unsigned_int(cell: &Cell, other: u128) -> bool {
    match cell {
        Cell::Number(n) => n.is_finite() && *n >= 0.0 && n.fract() == 0.0 && *n == other as f64,
        Cell::Formula { cached, .. } => cell_eq_unsigned_int(cached, other),
        _ => false,
    }
}
