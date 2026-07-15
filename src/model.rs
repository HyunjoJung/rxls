//! The spreadsheet data model and authoring API.
//!
//! This module holds the typed value types ([`Cell`], [`Color`], [`Font`], …),
//! the worksheet/workbook containers ([`Sheet`], [`Workbook`]), and the authoring
//! builder methods (`Sheet::write`, `Workbook::add_sheet`, …). The reader
//! populates these types; the writer serializes them. See the crate root for the
//! format dispatch and the `.xls` reader internals.

use std::collections::{btree_map::Range as BTreeMapRange, BTreeMap, BTreeSet};

#[cfg(feature = "serde")]
use serde::de::{IntoDeserializer, VariantAccess};

/// Calendar date/time decoded from an Excel serial number.
///
/// This is a small dependency-free alternative to a `chrono` type. Pass the
/// workbook's [`Workbook::date1904`] flag to [`excel_serial_to_datetime`] or
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
/// `date1904` should be the workbook's [`Workbook::date1904`] value. Returns
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
/// `date1904` should be the workbook's [`Workbook::date1904`] value. Available
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
    /// workbook's date system to convert, or [`Sheet::to_text`] for the
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
    /// workbook's [`Workbook::date1904`] value.
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
    /// workbook's [`Workbook::date1904`] value. Available with the optional
    /// `chrono` feature.
    #[cfg(feature = "chrono")]
    pub fn as_naive_datetime(&self, date1904: bool) -> Option<chrono::NaiveDateTime> {
        self.as_datetime(date1904)?.to_naive_datetime()
    }

    /// Decode this cell as chrono's [`chrono::NaiveDate`], if it is a date or
    /// numeric Excel serial candidate.
    ///
    /// Formula cells delegate to their cached value. `date1904` should be the
    /// workbook's [`Workbook::date1904`] value. Available with the optional
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
    /// [`Workbook::date1904`] value. Available with the optional `chrono`
    /// feature.
    #[cfg(feature = "chrono")]
    pub fn as_date(&self, date1904: bool) -> Option<chrono::NaiveDate> {
        self.as_naive_date(date1904)
    }

    /// Decode this cell as chrono's [`chrono::NaiveTime`], if it is a date or
    /// numeric Excel serial candidate.
    ///
    /// Formula cells delegate to their cached value. `date1904` should be the
    /// workbook's [`Workbook::date1904`] value. Available with the optional
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
    /// [`Workbook::date1904`] value. Available with the optional `chrono`
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

/// An RGB color, emitted as an 8-hex ARGB string (`FF` + `RRGGBB`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Color(pub [u8; 3]);

impl Color {
    /// Build an RGB color from red, green, and blue bytes.
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self([red, green, blue])
    }

    /// Return this color as `[red, green, blue]`.
    pub const fn as_rgb(self) -> [u8; 3] {
        self.0
    }
}

impl From<[u8; 3]> for Color {
    fn from(rgb: [u8; 3]) -> Self {
        Self(rgb)
    }
}

/// Font superscript/subscript setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum FormatScript {
    /// No superscript or subscript.
    #[default]
    None,
    /// Superscript.
    Superscript,
    /// Subscript.
    Subscript,
}

/// Excel cell fill pattern.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum FormatPattern {
    /// Automatic or empty pattern.
    #[default]
    None,
    /// Solid fill.
    Solid,
    /// Medium gray pattern.
    MediumGray,
    /// Dark gray pattern.
    DarkGray,
    /// Light gray pattern.
    LightGray,
    /// Dark horizontal lines.
    DarkHorizontal,
    /// Dark vertical lines.
    DarkVertical,
    /// Dark diagonal stripes.
    DarkDown,
    /// Reverse dark diagonal stripes.
    DarkUp,
    /// Dark grid pattern.
    DarkGrid,
    /// Dark trellis pattern.
    DarkTrellis,
    /// Light horizontal lines.
    LightHorizontal,
    /// Light vertical lines.
    LightVertical,
    /// Light diagonal stripes.
    LightDown,
    /// Reverse light diagonal stripes.
    LightUp,
    /// Light grid pattern.
    LightGrid,
    /// Light trellis pattern.
    LightTrellis,
    /// 12.5% gray pattern.
    Gray125,
    /// 6.25% gray pattern.
    Gray0625,
}

/// Cell fill formatting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Fill {
    /// Pattern type.
    pub pattern: FormatPattern,
    /// Background color.
    pub background: Option<Color>,
    /// Foreground or pattern color.
    pub foreground: Option<Color>,
}

impl Fill {
    /// Construct an empty fill.
    pub fn new() -> Self {
        Self::default()
    }

    /// A solid fill with the given RGB color.
    pub fn solid(color: impl Into<Color>) -> Self {
        Self {
            pattern: FormatPattern::Solid,
            background: Some(color.into()),
            foreground: None,
        }
    }

    /// Set the fill pattern.
    pub fn with_pattern(mut self, pattern: FormatPattern) -> Self {
        self.pattern = pattern;
        self
    }

    /// Set the fill background color.
    pub fn with_background(mut self, color: impl Into<Color>) -> Self {
        self.background = Some(color.into());
        if self.pattern == FormatPattern::None {
            self.pattern = FormatPattern::Solid;
        }
        self
    }

    /// Set the fill foreground or pattern color.
    pub fn with_foreground(mut self, color: impl Into<Color>) -> Self {
        self.foreground = Some(color.into());
        self
    }
}

/// Horizontal cell alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HAlign {
    /// Left-aligned.
    Left,
    /// Centered.
    Center,
    /// Right-aligned.
    Right,
}

/// Vertical cell alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VAlign {
    /// Top.
    Top,
    /// Middle.
    Middle,
    /// Bottom.
    Bottom,
}

/// Cell text alignment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Alignment {
    /// Horizontal alignment.
    pub horizontal: Option<HAlign>,
    /// Vertical alignment.
    pub vertical: Option<VAlign>,
    /// Wrap long text within the cell (essential for long Korean `공고명`).
    pub wrap: bool,
    /// Text rotation in degrees (`-90..=90`).
    pub rotation: i16,
    /// Left indent in character units (`0` = none).
    pub indent: u8,
    /// Shrink text to fit within the cell width.
    pub shrink_to_fit: bool,
}

impl Alignment {
    /// Construct an empty alignment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set horizontal alignment.
    pub fn with_horizontal(mut self, horizontal: HAlign) -> Self {
        self.horizontal = Some(horizontal);
        self
    }

    /// Set vertical alignment.
    pub fn with_vertical(mut self, vertical: VAlign) -> Self {
        self.vertical = Some(vertical);
        self
    }

    /// Wrap long text within the cell.
    pub fn wrapped(mut self) -> Self {
        self.wrap = true;
        self
    }

    /// Set whether long text wraps within the cell.
    pub fn with_wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    /// Set the left indent in character units.
    pub fn with_indent(mut self, level: u8) -> Self {
        self.indent = level;
        self
    }

    /// Set text rotation in degrees (`-90..=90`).
    pub fn with_rotation(mut self, degrees: i16) -> Self {
        self.rotation = degrees.clamp(-90, 90);
        self
    }

    /// Shrink text to fit within the cell width.
    pub fn with_shrink_to_fit(mut self) -> Self {
        self.shrink_to_fit = true;
        self
    }
}

/// A cell font.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Font {
    /// Font family name (e.g. `맑은 고딕`).
    pub name: Option<String>,
    /// Size in points.
    pub size_pt: Option<u16>,
    /// Text color.
    pub color: Option<Color>,
    /// Bold.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
    /// Single underline.
    pub underline: bool,
    /// Strikethrough.
    pub strikethrough: bool,
    /// Superscript/subscript setting.
    pub script: FormatScript,
}

impl Font {
    /// Construct a font with inherited/default run properties.
    pub fn new() -> Self {
        Font::default()
    }

    /// Set the font family name.
    pub fn with_name(mut self, name: impl AsRef<str>) -> Self {
        self.name = Some(name.as_ref().to_string());
        self
    }

    /// Set the font size in points.
    pub fn with_size(mut self, points: u16) -> Self {
        self.size_pt = Some(points);
        self
    }

    /// Set the font color.
    pub fn with_color(mut self, color: impl Into<Color>) -> Self {
        self.color = Some(color.into());
        self
    }

    /// Make the font bold.
    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    /// Make the font italic.
    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    /// Apply single underline.
    pub fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    /// Apply strikethrough.
    pub fn strikethrough(mut self) -> Self {
        self.strikethrough = true;
        self
    }

    /// Set superscript/subscript.
    pub fn with_script(mut self, script: FormatScript) -> Self {
        self.script = script;
        self
    }
}

/// One run of a rich (mixed-format) string: a text fragment plus the font applied
/// to it. Author a multi-format cell with [`Sheet::write_rich`]; readers retain
/// supported run metadata through [`Sheet::rich_text_runs`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct TextRun {
    /// The run's text.
    pub text: String,
    /// The run's font (`Font::default()` inherits the cell font).
    pub font: Font,
}

impl TextRun {
    /// A run with the given text and font.
    pub fn new(text: impl Into<String>, font: Font) -> Self {
        Self {
            text: text.into(),
            font,
        }
    }
}

/// A single border edge style.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum BorderStyle {
    /// No edge.
    #[default]
    None,
    /// Thin edge.
    Thin,
    /// Medium edge.
    Medium,
    /// Thick edge.
    Thick,
    /// Double edge.
    Double,
}

/// Cell borders (per edge).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Border {
    /// Left edge.
    pub left: BorderStyle,
    /// Right edge.
    pub right: BorderStyle,
    /// Top edge.
    pub top: BorderStyle,
    /// Bottom edge.
    pub bottom: BorderStyle,
    /// Border color (all edges).
    pub color: Option<Color>,
    /// Left edge color, overriding [`Self::color`] for the left edge.
    pub left_color: Option<Color>,
    /// Right edge color, overriding [`Self::color`] for the right edge.
    pub right_color: Option<Color>,
    /// Top edge color, overriding [`Self::color`] for the top edge.
    pub top_color: Option<Color>,
    /// Bottom edge color, overriding [`Self::color`] for the bottom edge.
    pub bottom_color: Option<Color>,
}

impl Border {
    /// Construct an empty border.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the same style on every edge.
    pub fn with_all(mut self, style: BorderStyle) -> Self {
        self.left = style;
        self.right = style;
        self.top = style;
        self.bottom = style;
        self
    }

    /// Set the left edge style.
    pub fn with_left(mut self, style: BorderStyle) -> Self {
        self.left = style;
        self
    }

    /// Set the right edge style.
    pub fn with_right(mut self, style: BorderStyle) -> Self {
        self.right = style;
        self
    }

    /// Set the top edge style.
    pub fn with_top(mut self, style: BorderStyle) -> Self {
        self.top = style;
        self
    }

    /// Set the bottom edge style.
    pub fn with_bottom(mut self, style: BorderStyle) -> Self {
        self.bottom = style;
        self
    }

    /// Set the color used by all configured edges.
    pub fn with_color(mut self, color: impl Into<Color>) -> Self {
        self.color = Some(color.into());
        self
    }

    /// Set the left edge color.
    pub fn with_left_color(mut self, color: impl Into<Color>) -> Self {
        self.left_color = Some(color.into());
        self
    }

    /// Set the right edge color.
    pub fn with_right_color(mut self, color: impl Into<Color>) -> Self {
        self.right_color = Some(color.into());
        self
    }

    /// Set the top edge color.
    pub fn with_top_color(mut self, color: impl Into<Color>) -> Self {
        self.top_color = Some(color.into());
        self
    }

    /// Set the bottom edge color.
    pub fn with_bottom_color(mut self, color: impl Into<Color>) -> Self {
        self.bottom_color = Some(color.into());
        self
    }
}

/// Inline cell style for authoring. All `None`/default ⇒ Excel "General"; the
/// writer interns these into the workbook's deduped style tables.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CellStyle {
    /// Font.
    pub font: Option<Font>,
    /// Legacy solid background fill color. Prefer [`CellStyle::pattern_fill`]
    /// for non-solid fills.
    pub fill: Option<Color>,
    /// Pattern fill.
    pub pattern_fill: Option<Fill>,
    /// Cell borders.
    pub border: Option<Border>,
    /// Number format code (e.g. `₩#,##0`, `0.0%`, `yyyy"년" mm"월"`).
    pub num_fmt: Option<String>,
    /// Text alignment.
    pub align: Option<Alignment>,
    /// Cell protection flags used when worksheet protection is enabled.
    pub protection: Option<CellProtection>,
}

/// Cell-level protection flags in an authored cell style.
///
/// Excel treats cells as locked by default, so `locked = None` means "inherit the
/// default locked state"; `Some(false)` explicitly unlocks a cell on a protected
/// worksheet. `hidden` hides formula text while sheet protection is enabled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CellProtection {
    /// Explicit locked state. `None` leaves Excel's default locked state.
    pub locked: Option<bool>,
    /// Hide formula text when the worksheet is protected.
    pub hidden: bool,
}

/// Writer format object, compatible with the existing [`CellStyle`] model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Format {
    style: CellStyle,
}

fn merge_font(base: Option<&Font>, overlay: Option<&Font>) -> Option<Font> {
    match (base, overlay) {
        (None, None) => None,
        (Some(base), None) => Some(base.clone()),
        (None, Some(overlay)) => Some(overlay.clone()),
        (Some(base), Some(overlay)) => {
            let mut merged = base.clone();
            if overlay.name.is_some() {
                merged.name = overlay.name.clone();
            }
            if overlay.size_pt.is_some() {
                merged.size_pt = overlay.size_pt;
            }
            if overlay.color.is_some() {
                merged.color = overlay.color;
            }
            if overlay.bold {
                merged.bold = true;
            }
            if overlay.italic {
                merged.italic = true;
            }
            if overlay.underline {
                merged.underline = true;
            }
            if overlay.strikethrough {
                merged.strikethrough = true;
            }
            if overlay.script != FormatScript::None {
                merged.script = overlay.script;
            }
            Some(merged)
        }
    }
}

fn merge_alignment(base: Option<&Alignment>, overlay: Option<&Alignment>) -> Option<Alignment> {
    match (base, overlay) {
        (None, None) => None,
        (Some(base), None) => Some(base.clone()),
        (None, Some(overlay)) => Some(overlay.clone()),
        (Some(base), Some(overlay)) => {
            let mut merged = base.clone();
            if overlay.horizontal.is_some() {
                merged.horizontal = overlay.horizontal;
            }
            if overlay.vertical.is_some() {
                merged.vertical = overlay.vertical;
            }
            if overlay.wrap {
                merged.wrap = true;
            }
            if overlay.rotation != 0 {
                merged.rotation = overlay.rotation;
            }
            if overlay.indent != 0 {
                merged.indent = overlay.indent;
            }
            if overlay.shrink_to_fit {
                merged.shrink_to_fit = true;
            }
            Some(merged)
        }
    }
}

fn merge_border(base: Option<&Border>, overlay: Option<&Border>) -> Option<Border> {
    match (base, overlay) {
        (None, None) => None,
        (Some(base), None) => Some(base.clone()),
        (None, Some(overlay)) => Some(overlay.clone()),
        (Some(base), Some(overlay)) => {
            let mut merged = base.clone();
            if overlay.left != BorderStyle::None {
                merged.left = overlay.left;
            }
            if overlay.right != BorderStyle::None {
                merged.right = overlay.right;
            }
            if overlay.top != BorderStyle::None {
                merged.top = overlay.top;
            }
            if overlay.bottom != BorderStyle::None {
                merged.bottom = overlay.bottom;
            }
            if overlay.color.is_some() {
                merged.color = overlay.color;
            }
            if overlay.left_color.is_some() {
                merged.left_color = overlay.left_color;
            }
            if overlay.right_color.is_some() {
                merged.right_color = overlay.right_color;
            }
            if overlay.top_color.is_some() {
                merged.top_color = overlay.top_color;
            }
            if overlay.bottom_color.is_some() {
                merged.bottom_color = overlay.bottom_color;
            }
            Some(merged)
        }
    }
}

fn merge_protection(
    base: Option<&CellProtection>,
    overlay: Option<&CellProtection>,
) -> Option<CellProtection> {
    match (base, overlay) {
        (None, None) => None,
        (Some(base), None) => Some(base.clone()),
        (None, Some(overlay)) => Some(overlay.clone()),
        (Some(base), Some(overlay)) => {
            let mut merged = base.clone();
            if overlay.locked.is_some() {
                merged.locked = overlay.locked;
            }
            if overlay.hidden {
                merged.hidden = true;
            }
            Some(merged)
        }
    }
}

impl Format {
    /// A new empty writer format.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap an existing [`CellStyle`] as a writer format.
    pub fn from_cell_style(style: CellStyle) -> Self {
        Self { style }
    }

    /// Borrow the underlying [`CellStyle`].
    pub fn as_cell_style(&self) -> &CellStyle {
        &self.style
    }

    /// Convert this format into its underlying [`CellStyle`].
    pub fn into_cell_style(self) -> CellStyle {
        self.style
    }

    /// Merge this format with `overlay`, where fields explicitly set on
    /// `overlay` override this format and unset overlay fields preserve `self`.
    pub fn merge(&self, overlay: &Format) -> Self {
        Self {
            style: self.style.merge(overlay.as_cell_style()),
        }
    }

    /// Set the font family name.
    pub fn font_name(mut self, name: impl AsRef<str>) -> Self {
        self.style = self.style.font_name(name);
        self
    }

    /// Set the font size in points.
    pub fn size(mut self, points: u16) -> Self {
        self.style = self.style.size(points);
        self
    }

    /// Set the text color.
    pub fn color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.color(color);
        self
    }

    /// Make the font bold.
    pub fn bold(mut self) -> Self {
        self.style = self.style.bold();
        self
    }

    /// Make the font italic.
    pub fn italic(mut self) -> Self {
        self.style = self.style.italic();
        self
    }

    /// Apply single underline to the font.
    pub fn underline(mut self) -> Self {
        self.style = self.style.underline();
        self
    }

    /// Apply strikethrough to the font.
    pub fn strikethrough(mut self) -> Self {
        self.style = self.style.strikethrough();
        self
    }

    /// Set the font superscript/subscript property.
    pub fn font_script(mut self, script: FormatScript) -> Self {
        self.style = self.style.font_script(script);
        self
    }

    /// Set a solid background fill color.
    pub fn fill(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.fill(color);
        self
    }

    /// Set the fill pattern.
    pub fn pattern(mut self, pattern: FormatPattern) -> Self {
        self.style = self.style.pattern(pattern);
        self
    }

    /// Set the fill background color.
    pub fn background_color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.background_color(color);
        self
    }

    /// Set the fill foreground or pattern color.
    pub fn foreground_color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.foreground_color(color);
        self
    }

    /// Set the fill object.
    pub fn pattern_fill(mut self, fill: Fill) -> Self {
        self.style = self.style.pattern_fill(fill);
        self
    }

    /// Set the number format code (e.g. `0.0%`).
    pub fn num_fmt(mut self, code: impl AsRef<str>) -> Self {
        self.style = self.style.num_fmt(code);
        self
    }

    /// Wrap long text within the cell.
    pub fn wrap(mut self) -> Self {
        self.style = self.style.wrap();
        self
    }

    /// Set horizontal alignment.
    pub fn align(mut self, h: HAlign) -> Self {
        self.style = self.style.align(h);
        self
    }

    /// Set vertical alignment.
    pub fn valign(mut self, v: VAlign) -> Self {
        self.style = self.style.valign(v);
        self
    }

    /// Set the alignment object.
    pub fn alignment(mut self, alignment: Alignment) -> Self {
        self.style = self.style.alignment(alignment);
        self
    }

    /// Set the left indent in character units.
    pub fn indent(mut self, level: u8) -> Self {
        self.style = self.style.indent(level);
        self
    }

    /// Shrink text to fit within the cell width.
    pub fn shrink_to_fit(mut self) -> Self {
        self.style = self.style.shrink_to_fit();
        self
    }

    /// Set text rotation in degrees (`-90..=90`).
    pub fn text_rotation(mut self, degrees: i16) -> Self {
        self.style = self.style.text_rotation(degrees);
        self
    }

    /// Explicitly lock the cell when worksheet protection is enabled.
    pub fn locked(mut self) -> Self {
        self.style = self.style.locked();
        self
    }

    /// Unlock the cell when worksheet protection is enabled.
    pub fn unlocked(mut self) -> Self {
        self.style = self.style.unlocked();
        self
    }

    /// Hide formula text when worksheet protection is enabled.
    pub fn hidden(mut self) -> Self {
        self.style = self.style.hidden();
        self
    }

    /// Set the cell borders.
    pub fn border(mut self, border: Border) -> Self {
        self.style = self.style.border(border);
        self
    }

    /// Set the top border edge style.
    pub fn border_top(mut self, style: FormatBorder) -> Self {
        self.style = self.style.border_top(style);
        self
    }

    /// Set the bottom border edge style.
    pub fn border_bottom(mut self, style: FormatBorder) -> Self {
        self.style = self.style.border_bottom(style);
        self
    }

    /// Set the left border edge style.
    pub fn border_left(mut self, style: FormatBorder) -> Self {
        self.style = self.style.border_left(style);
        self
    }

    /// Set the right border edge style.
    pub fn border_right(mut self, style: FormatBorder) -> Self {
        self.style = self.style.border_right(style);
        self
    }

    /// Set the top border edge color.
    pub fn border_top_color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.border_top_color(color);
        self
    }

    /// Set the bottom border edge color.
    pub fn border_bottom_color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.border_bottom_color(color);
        self
    }

    /// Set the left border edge color.
    pub fn border_left_color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.border_left_color(color);
        self
    }

    /// Set the right border edge color.
    pub fn border_right_color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.border_right_color(color);
        self
    }

    /// Set the font family name.
    pub fn set_font_name(self, name: impl AsRef<str>) -> Self {
        self.font_name(name)
    }

    /// Set the font size in points.
    pub fn set_font_size(self, points: u16) -> Self {
        self.size(points)
    }

    /// Set the text color.
    pub fn set_font_color(self, color: impl Into<Color>) -> Self {
        self.color(color)
    }

    /// Make the font bold.
    pub fn set_bold(self) -> Self {
        self.bold()
    }

    /// Make the font italic.
    pub fn set_italic(self) -> Self {
        self.italic()
    }

    /// Apply single underline to the font.
    pub fn set_underline(self) -> Self {
        self.underline()
    }

    /// Apply strikethrough to the font.
    pub fn set_font_strikethrough(self) -> Self {
        self.strikethrough()
    }

    /// Apply strikethrough to the font.
    pub fn set_strikethrough(self) -> Self {
        self.strikethrough()
    }

    /// Set the font superscript/subscript property.
    pub fn set_font_script(self, script: FormatScript) -> Self {
        self.font_script(script)
    }

    /// Set a solid background fill color.
    pub fn set_bg_color(self, color: impl Into<Color>) -> Self {
        self.fill(color)
    }

    /// Set the fill background color.
    pub fn set_background_color(self, color: impl Into<Color>) -> Self {
        self.background_color(color)
    }

    /// Set the fill foreground or pattern color.
    pub fn set_foreground_color(self, color: impl Into<Color>) -> Self {
        self.foreground_color(color)
    }

    /// Set the fill object.
    pub fn set_pattern_fill(self, fill: Fill) -> Self {
        self.pattern_fill(fill)
    }

    /// Set the fill pattern.
    pub fn set_pattern(self, pattern: FormatPattern) -> Self {
        self.pattern(pattern)
    }

    /// Set the number format code.
    pub fn set_num_format(self, code: impl AsRef<str>) -> Self {
        self.num_fmt(code)
    }

    /// Set horizontal alignment.
    pub fn set_align(self, h: FormatAlign) -> Self {
        self.align(h)
    }

    /// Set vertical alignment.
    pub fn set_valign(self, v: VAlign) -> Self {
        self.valign(v)
    }

    /// Set the alignment object.
    pub fn set_alignment(self, alignment: Alignment) -> Self {
        self.alignment(alignment)
    }

    /// Wrap long text within the cell.
    pub fn set_text_wrap(self) -> Self {
        self.wrap()
    }

    /// Set the left indent in character units.
    pub fn set_indent(self, level: u8) -> Self {
        self.indent(level)
    }

    /// Shrink text to fit within the cell width.
    pub fn set_shrink_to_fit(self) -> Self {
        self.shrink_to_fit()
    }

    /// Set text rotation in degrees (`-90..=90`).
    pub fn set_text_rotation(self, degrees: i16) -> Self {
        self.text_rotation(degrees)
    }

    /// Explicitly lock the cell when worksheet protection is enabled.
    pub fn set_locked(self) -> Self {
        self.locked()
    }

    /// Unlock the cell when worksheet protection is enabled.
    pub fn set_unlocked(self) -> Self {
        self.unlocked()
    }

    /// Hide formula text when worksheet protection is enabled.
    pub fn set_hidden(self) -> Self {
        self.hidden()
    }

    /// Set the same border style on every cell edge.
    pub fn set_border(mut self, style: FormatBorder) -> Self {
        self.style = self.style.set_border(style);
        self
    }

    /// Set the top border edge style.
    pub fn set_border_top(self, style: FormatBorder) -> Self {
        self.border_top(style)
    }

    /// Set the bottom border edge style.
    pub fn set_border_bottom(self, style: FormatBorder) -> Self {
        self.border_bottom(style)
    }

    /// Set the left border edge style.
    pub fn set_border_left(self, style: FormatBorder) -> Self {
        self.border_left(style)
    }

    /// Set the right border edge style.
    pub fn set_border_right(self, style: FormatBorder) -> Self {
        self.border_right(style)
    }

    /// Set the top border edge color.
    pub fn set_border_top_color(self, color: impl Into<Color>) -> Self {
        self.border_top_color(color)
    }

    /// Set the bottom border edge color.
    pub fn set_border_bottom_color(self, color: impl Into<Color>) -> Self {
        self.border_bottom_color(color)
    }

    /// Set the left border edge color.
    pub fn set_border_left_color(self, color: impl Into<Color>) -> Self {
        self.border_left_color(color)
    }

    /// Set the right border edge color.
    pub fn set_border_right_color(self, color: impl Into<Color>) -> Self {
        self.border_right_color(color)
    }

    /// Set the color used by all configured border edges.
    pub fn set_border_color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.set_border_color(color);
        self
    }
}

impl From<CellStyle> for Format {
    fn from(style: CellStyle) -> Self {
        Self::from_cell_style(style)
    }
}

impl From<Format> for CellStyle {
    fn from(format: Format) -> Self {
        format.into_cell_style()
    }
}

impl std::ops::Deref for Format {
    type Target = CellStyle;

    fn deref(&self) -> &Self::Target {
        self.as_cell_style()
    }
}

impl std::ops::DerefMut for Format {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.style
    }
}

/// Writer alignment enum alias for format-builder APIs.
pub type FormatAlign = HAlign;

/// Writer border enum alias for format-builder APIs.
pub type FormatBorder = BorderStyle;

/// Inclusive rectangular worksheet dimensions.
///
/// Coordinates are zero-based `(row, col)` pairs. Empty worksheet/range APIs
/// return `None` instead of a default `Dimensions` value.
#[derive(Debug, Default, PartialEq, Eq, Hash, Ord, PartialOrd, Copy, Clone)]
pub struct Dimensions {
    /// Top-left coordinate of the rectangle.
    pub start: (u32, u32),
    /// Bottom-right coordinate of the rectangle.
    pub end: (u32, u32),
}

impl Dimensions {
    /// Construct worksheet dimensions from inclusive top-left and bottom-right
    /// coordinates.
    pub fn new(start: (u32, u32), end: (u32, u32)) -> Self {
        Self { start, end }
    }

    /// Convert rxls' tuple representation
    /// `(first_row, first_col, last_row, last_col)` into [`Dimensions`].
    pub fn from_range_tuple(range: (u32, u16, u32, u16)) -> Self {
        Self::new((range.0, u32::from(range.1)), (range.2, u32::from(range.3)))
    }

    /// `true` when `row, col` is inside these inclusive dimensions.
    pub fn contains(&self, row: u32, col: u32) -> bool {
        !self.is_empty()
            && row >= self.start.0
            && row <= self.end.0
            && col >= self.start.1
            && col <= self.end.1
    }

    /// Number of worksheet positions covered by this rectangle.
    pub fn len(&self) -> u64 {
        if self.is_empty() {
            0
        } else {
            (u64::from(self.end.0) - u64::from(self.start.0) + 1)
                * (u64::from(self.end.1) - u64::from(self.start.1) + 1)
        }
    }

    /// `true` when the end coordinate is above or left of the start coordinate.
    pub fn is_empty(&self) -> bool {
        self.start.0 > self.end.0 || self.start.1 > self.end.1
    }
}

impl From<(u32, u16, u32, u16)> for Dimensions {
    fn from(range: (u32, u16, u32, u16)) -> Self {
        Self::from_range_tuple(range)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CellEntry {
    pub(crate) row: u32,
    pub(crate) col: u16,
    pub(crate) value: Cell,
    /// Display text used by [`Sheet::to_text`] (e.g. `50%`, `TRUE`).
    pub(crate) text: String,
    /// Inline authoring style (`None` for cells produced by the reader). Read
    /// only by the `.xlsx` writer.
    #[cfg_attr(not(feature = "xlsx"), allow(dead_code))]
    pub(crate) style: Option<CellStyle>,
    /// External hyperlink target (authoring). Read only by the `.xlsx` writer.
    #[cfg_attr(not(feature = "xlsx"), allow(dead_code))]
    pub(crate) hyperlink: Option<String>,
}

/// A rectangular, calamine-style view over a worksheet's effective cells.
///
/// `Range` is built from a [`Sheet`] using Excel's last-write-wins semantics for
/// duplicate coordinates. Positions passed to [`Range::get`] are relative to the
/// range start; absolute positions are available from [`Range::used_cells_abs`].
#[derive(Debug, Clone, Default)]
pub struct Range<'a> {
    start: Option<(u32, u16)>,
    end: Option<(u32, u16)>,
    cells: BTreeMap<(u32, u16), RangeCell<'a>>,
}

/// A rectangular view over cells that contain formula source text.
///
/// `FormulaRange` is the formula-text counterpart to [`Range`]: it uses the
/// same worksheet coordinates, but only cells represented as [`Cell::Formula`]
/// are populated. It is returned by [`Workbook::worksheet_formula`].
#[derive(Debug, Clone, Default)]
pub struct FormulaRange<'a> {
    start: Option<(u32, u16)>,
    end: Option<(u32, u16)>,
    formulas: BTreeMap<(u32, u16), FormulaEntry<'a>>,
}

#[derive(Debug, Clone)]
enum RangeCell<'a> {
    Borrowed { value: &'a Cell, text: &'a str },
    Owned { value: Cell, text: String },
}

impl RangeCell<'_> {
    fn value(&self) -> &Cell {
        match self {
            RangeCell::Borrowed { value, .. } => value,
            RangeCell::Owned { value, .. } => value,
        }
    }

    fn text(&self) -> &str {
        match self {
            RangeCell::Borrowed { text, .. } => text,
            RangeCell::Owned { text, .. } => text,
        }
    }
}

impl<'left, 'right> PartialEq<RangeCell<'right>> for RangeCell<'left> {
    fn eq(&self, other: &RangeCell<'right>) -> bool {
        self.value() == other.value() && self.text() == other.text()
    }
}

#[derive(Debug, Clone)]
enum FormulaEntry<'a> {
    Borrowed(&'a str),
    Owned(String),
}

impl FormulaEntry<'_> {
    fn as_str(&self) -> &str {
        match self {
            FormulaEntry::Borrowed(formula) => formula,
            FormulaEntry::Owned(formula) => formula.as_str(),
        }
    }
}

impl<'left, 'right> PartialEq<FormulaEntry<'right>> for FormulaEntry<'left> {
    fn eq(&self, other: &FormulaEntry<'right>) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<'a> Eq for FormulaEntry<'a> {}

impl<'left, 'right> PartialEq<Range<'right>> for Range<'left> {
    fn eq(&self, other: &Range<'right>) -> bool {
        self.start == other.start
            && self.end == other.end
            && self.cells.len() == other.cells.len()
            && self
                .cells
                .iter()
                .zip(other.cells.iter())
                .all(|(left, right)| left.0 == right.0 && left.1 == right.1)
    }
}

impl<'left, 'right> PartialEq<FormulaRange<'right>> for FormulaRange<'left> {
    fn eq(&self, other: &FormulaRange<'right>) -> bool {
        self.start == other.start
            && self.end == other.end
            && self.formulas.len() == other.formulas.len()
            && self
                .formulas
                .iter()
                .zip(other.formulas.iter())
                .all(|(left, right)| left.0 == right.0 && left.1 == right.1)
    }
}

impl<'a> Eq for FormulaRange<'a> {}

fn row_span_len(start: u32, end: u32) -> usize {
    if start > end {
        return 0;
    }
    let span = u64::from(end) - u64::from(start) + 1;
    usize::try_from(span).unwrap_or(usize::MAX)
}

fn col_span_len(start: u16, end: u16) -> usize {
    if start > end {
        return 0;
    }
    usize::from(end) - usize::from(start) + 1
}

impl<'a> Range<'a> {
    /// Construct a rectangular sparse range with no populated cells.
    ///
    /// The positions use absolute worksheet coordinates. rxls stores worksheet
    /// columns as `u16`, so columns outside that grid panic instead of silently
    /// changing the requested rectangle.
    ///
    /// # Panics
    ///
    /// Panics if `start` is after `end`, or if either column is outside rxls'
    /// worksheet grid.
    pub fn new(start: (u32, u32), end: (u32, u32)) -> Self {
        assert!(
            start.0 <= end.0 && start.1 <= end.1,
            "range start must not be after range end"
        );
        let start_col =
            u16::try_from(start.1).expect("range start column exceeds rxls worksheet grid");
        let end_col = u16::try_from(end.1).expect("range end column exceeds rxls worksheet grid");
        Self {
            start: Some((start.0, start_col)),
            end: Some((end.0, end_col)),
            cells: BTreeMap::new(),
        }
    }

    /// Construct an empty range.
    ///
    /// rxls represents missing worksheet positions as `None` in range APIs, so
    /// an empty range has no rectangular bounds and iterates no cells.
    pub fn empty() -> Self {
        Self {
            start: None,
            end: None,
            cells: BTreeMap::new(),
        }
    }

    /// Construct a range from sparse owned cells.
    ///
    /// The input positions use absolute worksheet coordinates. The resulting
    /// range bounds are the minimum rectangular area covering all supplied
    /// cells, while missing positions remain `None` in rxls' sparse facade.
    ///
    /// # Panics
    ///
    /// Panics if any column is outside rxls' worksheet grid.
    pub fn from_sparse<I, V>(cells: I) -> Self
    where
        I: IntoIterator<Item = ((u32, u32), V)>,
        V: Into<Cell>,
    {
        let mut start: Option<(u32, u16)> = None;
        let mut end: Option<(u32, u16)> = None;
        let mut entries = BTreeMap::new();

        for ((row, col), value) in cells {
            let col = u16::try_from(col).expect("range column exceeds rxls worksheet grid");
            start = Some(match start {
                Some((r0, c0)) => (r0.min(row), c0.min(col)),
                None => (row, col),
            });
            end = Some(match end {
                Some((r1, c1)) => (r1.max(row), c1.max(col)),
                None => (row, col),
            });
            let value = value.into();
            let text = display_text(&value);
            entries.insert((row, col), RangeCell::Owned { value, text });
        }

        Self {
            start,
            end,
            cells: entries,
        }
    }

    /// Set a cell value at an absolute worksheet position.
    ///
    /// If the position extends beyond the current bottom-right bound, the range
    /// grows to include it. Positions above or left of an existing range start
    /// panic, matching calamine's `Range::set_value` contract while preserving
    /// rxls' sparse `None` representation for other missing cells.
    ///
    /// # Panics
    ///
    /// Panics if `pos` is above or left of an existing range start, or if the
    /// column is outside rxls' worksheet grid.
    pub fn set_value(&mut self, pos: (u32, u32), value: impl Into<Cell>) {
        let col = u16::try_from(pos.1).expect("range column exceeds rxls worksheet grid");
        let row = pos.0;
        match (self.start, self.end) {
            (Some((r0, c0)), Some((r1, c1))) => {
                assert!(
                    row >= r0 && col >= c0,
                    "range value position must not be above or left of range start"
                );
                self.end = Some((r1.max(row), c1.max(col)));
            }
            _ => {
                self.start = Some((row, col));
                self.end = Some((row, col));
            }
        }
        let value = value.into();
        let text = display_text(&value);
        self.cells
            .insert((row, col), RangeCell::Owned { value, text });
    }

    fn from_sheet(sheet: &'a Sheet) -> Self {
        let mut cells = BTreeMap::new();
        for c in &sheet.cells {
            cells.insert(
                (c.row, c.col),
                RangeCell::Borrowed {
                    value: &c.value,
                    text: c.text.as_str(),
                },
            );
        }
        let start = cells
            .keys()
            .fold(None, |acc: Option<(u32, u16)>, &(row, col)| match acc {
                Some((r0, c0)) => Some((r0.min(row), c0.min(col))),
                None => Some((row, col)),
            });
        let end = cells
            .keys()
            .fold(None, |acc: Option<(u32, u16)>, &(row, col)| match acc {
                Some((r1, c1)) => Some((r1.max(row), c1.max(col))),
                None => Some((row, col)),
            });
        Self { start, end, cells }
    }

    /// `true` when the range contains no cells.
    pub fn is_empty(&self) -> bool {
        self.start.is_none() || self.end.is_none()
    }

    /// Absolute `(row, col)` of the top-left cell in the used rectangle.
    pub fn start(&self) -> Option<(u32, u32)> {
        self.start.map(|(row, col)| (row, u32::from(col)))
    }

    /// Absolute `(row, col)` of the bottom-right cell in the used rectangle.
    pub fn end(&self) -> Option<(u32, u32)> {
        self.end.map(|(row, col)| (row, u32::from(col)))
    }

    /// Inclusive dimensions of the used rectangle.
    pub fn dimensions_info(&self) -> Option<Dimensions> {
        match (self.start, self.end) {
            (Some((r0, c0)), Some((r1, c1))) => {
                Some(Dimensions::new((r0, u32::from(c0)), (r1, u32::from(c1))))
            }
            _ => None,
        }
    }

    /// Number of rows in the used rectangle.
    pub fn height(&self) -> usize {
        match (self.start, self.end) {
            (Some((r0, _)), Some((r1, _))) => row_span_len(r0, r1),
            _ => 0,
        }
    }

    /// Number of columns in the used rectangle.
    pub fn width(&self) -> usize {
        match (self.start, self.end) {
            (Some((_, c0)), Some((_, c1))) => col_span_len(c0, c1),
            _ => 0,
        }
    }

    /// Size of the used rectangle as `(height, width)`.
    pub fn size(&self) -> (usize, usize) {
        (self.height(), self.width())
    }

    /// Alias for [`Range::size`], matching calamine naming.
    pub fn get_size(&self) -> (usize, usize) {
        self.size()
    }

    /// Build a new rectangular subrange from absolute worksheet coordinates.
    ///
    /// This mirrors calamine's `Range::range` shape while preserving rxls'
    /// sparse representation: positions without a cell remain `None` in
    /// [`Range::rows`] and [`Range::cells`].
    pub fn range(&self, start: (u32, u32), end: (u32, u32)) -> Self {
        if start.0 > end.0 || start.1 > end.1 {
            return Self::empty();
        }
        let Some(start_col) = u16::try_from(start.1).ok() else {
            return Self::empty();
        };
        let end_col = u16::try_from(end.1).unwrap_or(u16::MAX);
        if start_col > end_col {
            return Self::empty();
        }

        let start = (start.0, start_col);
        let end = (end.0, end_col);
        let cells = self
            .cells
            .iter()
            .filter(|&(&(row, col), _)| {
                row >= start.0 && row <= end.0 && col >= start.1 && col <= end.1
            })
            .map(|(&(row, col), entry)| ((row, col), entry.clone()))
            .collect();
        Self {
            start: Some(start),
            end: Some(end),
            cells,
        }
    }

    /// Get a cell by relative `(row, col)` within the range.
    pub fn get(&self, pos: (usize, usize)) -> Option<&Cell> {
        let (r0, c0) = self.start?;
        let (r1, c1) = self.end?;
        let row = r0.checked_add(u32::try_from(pos.0).ok()?)?;
        let col = c0.checked_add(u16::try_from(pos.1).ok()?)?;
        if row > r1 || col > c1 {
            return None;
        }
        self.get_abs(row, col)
    }

    /// Get a cell by absolute worksheet `(row, col)`.
    pub fn get_abs(&self, row: u32, col: u16) -> Option<&Cell> {
        self.entry_abs(row, col).map(RangeCell::value)
    }

    fn entry_abs(&self, row: u32, col: u16) -> Option<&RangeCell<'a>> {
        self.cells.get(&(row, col))
    }

    /// Get a cell by absolute worksheet `(row, col)`, matching calamine's
    /// `Range::get_value` naming. Columns outside this crate's `u16` grid return
    /// `None`.
    pub fn get_value(&self, pos: (u32, u32)) -> Option<&Cell> {
        let col = u16::try_from(pos.1).ok()?;
        self.get_abs(pos.0, col)
    }

    /// Get a cell's formatted display text by relative `(row, col)` within the
    /// range.
    pub fn formatted(&self, pos: (usize, usize)) -> Option<&str> {
        let (r0, c0) = self.start?;
        let (r1, c1) = self.end?;
        let row = r0.checked_add(u32::try_from(pos.0).ok()?)?;
        let col = c0.checked_add(u16::try_from(pos.1).ok()?)?;
        if row > r1 || col > c1 {
            return None;
        }
        self.formatted_abs(row, col)
    }

    /// Get a cell's formatted display text by absolute worksheet `(row, col)`.
    pub fn formatted_abs(&self, row: u32, col: u16) -> Option<&str> {
        self.entry_abs(row, col).map(RangeCell::text)
    }

    /// First rectangular row as display strings, suitable for serde headers.
    /// Missing sparse cells are represented as empty strings.
    pub fn headers(&self) -> Option<Vec<String>> {
        let (row, c0, c1) = match (self.start, self.end) {
            (Some((row, c0)), Some((_, c1))) => (row, c0, c1),
            _ => return None,
        };
        Some(
            (c0..=c1)
                .map(|col| {
                    self.formatted_abs(row, col)
                        .map(str::to_string)
                        .unwrap_or_default()
                })
                .collect(),
        )
    }

    /// Iterate rectangular rows from top to bottom.
    ///
    /// Each row contains one entry per column in the used rectangle. Missing
    /// sparse cells are represented as `None`.
    pub fn rows(
        &self,
    ) -> impl DoubleEndedIterator<Item = Vec<Option<&Cell>>>
           + ExactSizeIterator
           + std::iter::FusedIterator
           + '_ {
        let (r0, c0, r1, c1) = match (self.start, self.end) {
            (Some((r0, c0)), Some((r1, c1))) => (r0, c0, r1, c1),
            _ => (1, 1, 0, 0),
        };
        let row_count = row_span_len(r0, r1);
        (0..row_count).map(move |row_idx| {
            let row = r0 + row_idx as u32;
            (c0..=c1).map(move |col| self.get_abs(row, col)).collect()
        })
    }

    /// Iterate borrowed row views from top to bottom without allocating one
    /// `Vec` per row.
    pub fn row_views(&self) -> RangeRows<'_, 'a> {
        match (self.start, self.end) {
            (Some((r0, c0)), Some((r1, c1))) => RangeRows {
                range: self,
                next_row: r0,
                end_row: r1,
                start_col: c0,
                end_col: c1,
                done: false,
            },
            _ => RangeRows {
                range: self,
                next_row: 0,
                end_row: 0,
                start_col: 0,
                end_col: 0,
                done: true,
            },
        }
    }

    /// Iterate the non-empty effective cells as relative `(row, col, cell)`.
    ///
    /// Coordinates are zero-based offsets from [`Range::start`], matching
    /// calamine's `Range::used_cells` semantics. Use [`Range::used_cells_abs`]
    /// when worksheet-absolute coordinates are needed.
    pub fn used_cells(
        &self,
    ) -> impl DoubleEndedIterator<Item = (u32, u16, &Cell)>
           + ExactSizeIterator
           + std::iter::FusedIterator
           + '_ {
        let (r0, c0) = self.start.unwrap_or((0, 0));
        self.cells
            .iter()
            .map(move |(&(row, col), entry)| (row - r0, col - c0, entry.value()))
    }

    /// Iterate the non-empty effective cells as absolute worksheet
    /// `(row, col, cell)`.
    pub fn used_cells_abs(
        &self,
    ) -> impl DoubleEndedIterator<Item = (u32, u16, &Cell)>
           + ExactSizeIterator
           + std::iter::FusedIterator
           + '_ {
        self.cells
            .iter()
            .map(|(&(row, col), entry)| (row, col, entry.value()))
    }

    /// Iterate every rectangular cell position as `(relative_row, relative_col,
    /// cell)`. Missing sparse cells are represented as `None`.
    pub fn cells(
        &self,
    ) -> impl DoubleEndedIterator<Item = (usize, usize, Option<&Cell>)>
           + ExactSizeIterator
           + std::iter::FusedIterator
           + '_ {
        let (r0, c0, r1, c1) = match (self.start, self.end) {
            (Some((r0, c0)), Some((r1, c1))) => (r0, c0, r1, c1),
            _ => (1, 1, 0, 0),
        };
        let row_count = row_span_len(r0, r1);
        let width = col_span_len(c0, c1);
        (0..row_count * width).map(move |idx| {
            let row_idx = idx / width;
            let col_idx = idx % width;
            let row = r0 + row_idx as u32;
            let col = c0 + col_idx as u16;
            (row_idx, col_idx, self.get_abs(row, col))
        })
    }

    /// Build a typed row deserializer with the default header-row behavior.
    #[cfg(feature = "serde")]
    pub fn deserialize<D>(&'a self) -> std::result::Result<RangeDeserializer<'a, D>, DeError>
    where
        D: serde::Deserialize<'a>,
    {
        RangeDeserializerBuilder::new().from_range(self)
    }
}

/// Iterator over borrowed [`RangeRow`] views.
#[derive(Debug, Clone)]
pub struct RangeRows<'range, 'cell> {
    range: &'range Range<'cell>,
    next_row: u32,
    end_row: u32,
    start_col: u16,
    end_col: u16,
    done: bool,
}

impl<'range, 'cell> Iterator for RangeRows<'range, 'cell> {
    type Item = RangeRow<'range, 'cell>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.next_row > self.end_row {
            return None;
        }
        let row = self.next_row;
        if row == self.end_row {
            self.done = true;
        } else {
            self.next_row = row + 1;
        }
        Some(RangeRow {
            range: self.range,
            row,
            start_col: self.start_col,
            end_col: self.end_col,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = if self.done || self.next_row > self.end_row {
            0
        } else {
            (self.end_row - self.next_row + 1) as usize
        };
        (len, Some(len))
    }
}

impl<'range, 'cell> ExactSizeIterator for RangeRows<'range, 'cell> {}

impl<'range, 'cell> DoubleEndedIterator for RangeRows<'range, 'cell> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.done || self.next_row > self.end_row {
            return None;
        }
        let row = self.end_row;
        if row == self.next_row {
            self.done = true;
        } else {
            self.end_row = row - 1;
        }
        Some(RangeRow {
            range: self.range,
            row,
            start_col: self.start_col,
            end_col: self.end_col,
        })
    }
}

impl<'range, 'cell> std::iter::FusedIterator for RangeRows<'range, 'cell> {}

/// A borrowed row view inside a [`Range`].
#[derive(Debug, Clone, Copy)]
pub struct RangeRow<'range, 'cell> {
    range: &'range Range<'cell>,
    row: u32,
    start_col: u16,
    end_col: u16,
}

impl<'range, 'cell> RangeRow<'range, 'cell> {
    /// Absolute worksheet row index.
    pub fn row(&self) -> u32 {
        self.row
    }

    /// Absolute worksheet column index where this row view starts.
    pub fn start_col(&self) -> u16 {
        self.start_col
    }

    /// Absolute worksheet column index where this row view ends.
    pub fn end_col(&self) -> u16 {
        self.end_col
    }

    /// Number of columns in this rectangular row view.
    pub fn len(&self) -> usize {
        col_span_len(self.start_col, self.end_col)
    }

    /// `true` when this row contains no columns.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get a cell by relative column offset within this row.
    pub fn get(&self, col: usize) -> Option<&Cell> {
        let col = self.start_col.checked_add(u16::try_from(col).ok()?)?;
        if col > self.end_col {
            return None;
        }
        self.range.get_abs(self.row, col)
    }

    /// Get a cell by absolute worksheet column within this row view.
    pub fn get_abs(&self, col: u16) -> Option<&Cell> {
        if col < self.start_col || col > self.end_col {
            return None;
        }
        self.range.get_abs(self.row, col)
    }

    /// Iterate cells across the row. Missing sparse cells are `None`.
    pub fn iter(&self) -> RangeRowCells<'range, 'cell> {
        RangeRowCells {
            range: self.range,
            row: self.row,
            next_col: self.start_col,
            end_col: self.end_col,
            done: false,
        }
    }

    /// Iterate every rectangular cell position as `(relative_col, cell)`.
    /// Missing sparse cells are represented as `None`.
    pub fn cells(
        &self,
    ) -> impl DoubleEndedIterator<Item = (usize, Option<&'range Cell>)>
           + ExactSizeIterator
           + std::iter::FusedIterator
           + '_ {
        self.iter().enumerate()
    }

    /// Iterate non-empty cells in this row as absolute `(col, cell)` pairs.
    pub fn used_cells(&self) -> RangeRowUsedCells<'range, 'cell> {
        let bounds = (self.row, self.start_col)..=(self.row, self.end_col);
        let remaining = self.range.cells.range(bounds.clone()).count();
        RangeRowUsedCells {
            entries: self.range.cells.range(bounds),
            remaining,
        }
    }
}

/// Iterator over non-empty cells in one borrowed [`RangeRow`].
#[derive(Debug, Clone)]
pub struct RangeRowUsedCells<'range, 'cell> {
    entries: BTreeMapRange<'range, (u32, u16), RangeCell<'cell>>,
    remaining: usize,
}

impl<'range, 'cell> Iterator for RangeRowUsedCells<'range, 'cell> {
    type Item = (u16, &'range Cell);

    fn next(&mut self) -> Option<Self::Item> {
        let (&(_, col), entry) = self.entries.next()?;
        self.remaining = self.remaining.saturating_sub(1);
        Some((col, entry.value()))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'range, 'cell> ExactSizeIterator for RangeRowUsedCells<'range, 'cell> {
    fn len(&self) -> usize {
        self.remaining
    }
}

impl<'range, 'cell> DoubleEndedIterator for RangeRowUsedCells<'range, 'cell> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let (&(_, col), entry) = self.entries.next_back()?;
        self.remaining = self.remaining.saturating_sub(1);
        Some((col, entry.value()))
    }
}

impl<'range, 'cell> std::iter::FusedIterator for RangeRowUsedCells<'range, 'cell> {}

/// Iterator over one borrowed [`RangeRow`]'s cells.
#[derive(Debug, Clone)]
pub struct RangeRowCells<'range, 'cell> {
    range: &'range Range<'cell>,
    row: u32,
    next_col: u16,
    end_col: u16,
    done: bool,
}

impl<'range, 'cell> Iterator for RangeRowCells<'range, 'cell> {
    type Item = Option<&'range Cell>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.next_col > self.end_col {
            return None;
        }
        let col = self.next_col;
        if col == self.end_col {
            self.done = true;
        } else {
            self.next_col = col + 1;
        }
        Some(self.range.get_abs(self.row, col))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = if self.done || self.next_col > self.end_col {
            0
        } else {
            usize::from(self.end_col - self.next_col) + 1
        };
        (len, Some(len))
    }
}

impl<'range, 'cell> ExactSizeIterator for RangeRowCells<'range, 'cell> {}

impl<'range, 'cell> DoubleEndedIterator for RangeRowCells<'range, 'cell> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.done || self.next_col > self.end_col {
            return None;
        }
        let col = self.end_col;
        if col == self.next_col {
            self.done = true;
        } else {
            self.end_col = col - 1;
        }
        Some(self.range.get_abs(self.row, col))
    }
}

impl<'range, 'cell> std::iter::FusedIterator for RangeRowCells<'range, 'cell> {}

impl<'a> FormulaRange<'a> {
    /// Construct a rectangular sparse formula range with no populated formula
    /// cells.
    ///
    /// The positions use absolute worksheet coordinates. rxls stores worksheet
    /// columns as `u16`, so columns outside that grid panic instead of silently
    /// changing the requested rectangle.
    ///
    /// # Panics
    ///
    /// Panics if `start` is after `end`, or if either column is outside rxls'
    /// worksheet grid.
    pub fn new(start: (u32, u32), end: (u32, u32)) -> Self {
        assert!(
            start.0 <= end.0 && start.1 <= end.1,
            "formula range start must not be after range end"
        );
        let start_col =
            u16::try_from(start.1).expect("formula range start column exceeds rxls worksheet grid");
        let end_col =
            u16::try_from(end.1).expect("formula range end column exceeds rxls worksheet grid");
        Self {
            start: Some((start.0, start_col)),
            end: Some((end.0, end_col)),
            formulas: BTreeMap::new(),
        }
    }

    /// Construct an empty formula range.
    ///
    /// Missing formula positions are represented as `None`, so an empty formula
    /// range has no rectangular bounds and iterates no cells.
    pub fn empty() -> Self {
        Self {
            start: None,
            end: None,
            formulas: BTreeMap::new(),
        }
    }

    /// Construct a formula range from sparse owned formula source text.
    ///
    /// The input positions use absolute worksheet coordinates. The resulting
    /// bounds cover all supplied formulas, while missing formula positions
    /// remain `None`.
    ///
    /// # Panics
    ///
    /// Panics if any column is outside rxls' worksheet grid.
    pub fn from_sparse<I, S>(formulas: I) -> Self
    where
        I: IntoIterator<Item = ((u32, u32), S)>,
        S: Into<String>,
    {
        let mut start: Option<(u32, u16)> = None;
        let mut end: Option<(u32, u16)> = None;
        let mut entries = BTreeMap::new();

        for ((row, col), formula) in formulas {
            let col = u16::try_from(col).expect("formula range column exceeds rxls worksheet grid");
            start = Some(match start {
                Some((r0, c0)) => (r0.min(row), c0.min(col)),
                None => (row, col),
            });
            end = Some(match end {
                Some((r1, c1)) => (r1.max(row), c1.max(col)),
                None => (row, col),
            });
            entries.insert((row, col), FormulaEntry::Owned(formula.into()));
        }

        Self {
            start,
            end,
            formulas: entries,
        }
    }

    /// Set formula source text at an absolute worksheet position.
    ///
    /// If the position extends beyond the current bottom-right bound, the range
    /// grows to include it. Positions above or left of an existing range start
    /// panic, matching the value range mutation contract.
    ///
    /// # Panics
    ///
    /// Panics if `pos` is above or left of an existing range start, or if the
    /// column is outside rxls' worksheet grid.
    pub fn set_value(&mut self, pos: (u32, u32), formula: impl Into<String>) {
        let col = u16::try_from(pos.1).expect("formula range column exceeds rxls worksheet grid");
        let row = pos.0;
        match (self.start, self.end) {
            (Some((r0, c0)), Some((r1, c1))) => {
                assert!(
                    row >= r0 && col >= c0,
                    "formula range value position must not be above or left of range start"
                );
                self.end = Some((r1.max(row), c1.max(col)));
            }
            _ => {
                self.start = Some((row, col));
                self.end = Some((row, col));
            }
        }
        self.formulas
            .insert((row, col), FormulaEntry::Owned(formula.into()));
    }

    fn from_sheet(sheet: &'a Sheet) -> Self {
        let mut formulas = BTreeMap::new();
        for c in &sheet.cells {
            if let Cell::Formula { formula, .. } = &c.value {
                formulas.insert((c.row, c.col), FormulaEntry::Borrowed(formula.as_str()));
            }
        }
        let start = formulas
            .keys()
            .fold(None, |acc: Option<(u32, u16)>, &(row, col)| match acc {
                Some((r0, c0)) => Some((r0.min(row), c0.min(col))),
                None => Some((row, col)),
            });
        let end = formulas
            .keys()
            .fold(None, |acc: Option<(u32, u16)>, &(row, col)| match acc {
                Some((r1, c1)) => Some((r1.max(row), c1.max(col))),
                None => Some((row, col)),
            });
        Self {
            start,
            end,
            formulas,
        }
    }

    /// `true` when no formula cells are present.
    pub fn is_empty(&self) -> bool {
        self.start.is_none() || self.end.is_none()
    }

    /// Absolute `(row, col)` of the top-left formula cell.
    pub fn start(&self) -> Option<(u32, u32)> {
        self.start.map(|(row, col)| (row, u32::from(col)))
    }

    /// Absolute `(row, col)` of the bottom-right formula cell.
    pub fn end(&self) -> Option<(u32, u32)> {
        self.end.map(|(row, col)| (row, u32::from(col)))
    }

    /// Inclusive dimensions of the formula rectangle.
    pub fn dimensions_info(&self) -> Option<Dimensions> {
        match (self.start, self.end) {
            (Some((r0, c0)), Some((r1, c1))) => {
                Some(Dimensions::new((r0, u32::from(c0)), (r1, u32::from(c1))))
            }
            _ => None,
        }
    }

    /// Number of rows in the formula range rectangle.
    pub fn height(&self) -> usize {
        match (self.start, self.end) {
            (Some((r0, _)), Some((r1, _))) => row_span_len(r0, r1),
            _ => 0,
        }
    }

    /// Number of columns in the formula range rectangle.
    pub fn width(&self) -> usize {
        match (self.start, self.end) {
            (Some((_, c0)), Some((_, c1))) => col_span_len(c0, c1),
            _ => 0,
        }
    }

    /// Size of the formula rectangle as `(height, width)`.
    pub fn size(&self) -> (usize, usize) {
        (self.height(), self.width())
    }

    /// Alias for [`FormulaRange::size`], matching calamine naming.
    pub fn get_size(&self) -> (usize, usize) {
        self.size()
    }

    /// Build a new rectangular formula subrange from absolute worksheet
    /// coordinates.
    pub fn range(&self, start: (u32, u32), end: (u32, u32)) -> Self {
        if start.0 > end.0 || start.1 > end.1 {
            return Self::empty();
        }
        let Some(start_col) = u16::try_from(start.1).ok() else {
            return Self::empty();
        };
        let end_col = u16::try_from(end.1).unwrap_or(u16::MAX);
        if start_col > end_col {
            return Self::empty();
        }

        let start = (start.0, start_col);
        let end = (end.0, end_col);
        let formulas = self
            .formulas
            .iter()
            .filter(|&(&(row, col), _)| {
                row >= start.0 && row <= end.0 && col >= start.1 && col <= end.1
            })
            .map(|(&(row, col), formula)| ((row, col), formula.clone()))
            .collect();
        Self {
            start: Some(start),
            end: Some(end),
            formulas,
        }
    }

    /// Get a formula by relative `(row, col)` within the formula range.
    pub fn get(&self, pos: (usize, usize)) -> Option<&str> {
        let (r0, c0) = self.start?;
        let (r1, c1) = self.end?;
        let row = r0.checked_add(u32::try_from(pos.0).ok()?)?;
        let col = c0.checked_add(u16::try_from(pos.1).ok()?)?;
        if row > r1 || col > c1 {
            return None;
        }
        self.get_abs(row, col)
    }

    /// Get a formula by absolute worksheet `(row, col)`.
    pub fn get_abs(&self, row: u32, col: u16) -> Option<&str> {
        self.formulas.get(&(row, col)).map(FormulaEntry::as_str)
    }

    /// Get a formula by absolute worksheet `(row, col)`, matching calamine's
    /// `Range::get_value` naming. Columns outside this crate's `u16` grid return
    /// `None`.
    pub fn get_value(&self, pos: (u32, u32)) -> Option<&str> {
        let col = u16::try_from(pos.1).ok()?;
        self.get_abs(pos.0, col)
    }

    /// First rectangular row as formula source strings. Missing sparse formula
    /// cells are represented as empty strings.
    pub fn headers(&self) -> Option<Vec<String>> {
        let (row, c0, c1) = match (self.start, self.end) {
            (Some((row, c0)), Some((_, c1))) => (row, c0, c1),
            _ => return None,
        };
        Some(
            (c0..=c1)
                .map(|col| {
                    self.get_abs(row, col)
                        .map(str::to_string)
                        .unwrap_or_default()
                })
                .collect(),
        )
    }

    /// Iterate rectangular rows from top to bottom. Missing formula cells are
    /// represented as `None`.
    pub fn rows(
        &self,
    ) -> impl DoubleEndedIterator<Item = Vec<Option<&str>>>
           + ExactSizeIterator
           + std::iter::FusedIterator
           + '_ {
        let (r0, c0, r1, c1) = match (self.start, self.end) {
            (Some((r0, c0)), Some((r1, c1))) => (r0, c0, r1, c1),
            _ => (1, 1, 0, 0),
        };
        let row_count = row_span_len(r0, r1);
        (0..row_count).map(move |row_idx| {
            let row = r0 + row_idx as u32;
            (c0..=c1).map(move |col| self.get_abs(row, col)).collect()
        })
    }

    /// Iterate borrowed row views from top to bottom without allocating one
    /// `Vec` per row.
    pub fn row_views(&self) -> FormulaRangeRows<'_, 'a> {
        match (self.start, self.end) {
            (Some((r0, c0)), Some((r1, c1))) => FormulaRangeRows {
                range: self,
                next_row: r0,
                end_row: r1,
                start_col: c0,
                end_col: c1,
                done: false,
            },
            _ => FormulaRangeRows {
                range: self,
                next_row: 0,
                end_row: 0,
                start_col: 0,
                end_col: 0,
                done: true,
            },
        }
    }

    /// Iterate every rectangular formula position as `(relative_row,
    /// relative_col, formula)`. Missing sparse formula cells are represented as
    /// `None`.
    pub fn cells(
        &self,
    ) -> impl DoubleEndedIterator<Item = (usize, usize, Option<&str>)>
           + ExactSizeIterator
           + std::iter::FusedIterator
           + '_ {
        let (r0, c0, r1, c1) = match (self.start, self.end) {
            (Some((r0, c0)), Some((r1, c1))) => (r0, c0, r1, c1),
            _ => (1, 1, 0, 0),
        };
        let row_count = row_span_len(r0, r1);
        let width = col_span_len(c0, c1);
        (0..row_count * width).map(move |idx| {
            let row_idx = idx / width;
            let col_idx = idx % width;
            let row = r0 + row_idx as u32;
            let col = c0 + col_idx as u16;
            (row_idx, col_idx, self.get_abs(row, col))
        })
    }

    /// Iterate non-empty formula cells as relative `(row, col, formula)`.
    ///
    /// Coordinates are zero-based offsets from [`FormulaRange::start`], matching
    /// the value range facade. Use [`FormulaRange::used_cells_abs`] when
    /// worksheet-absolute coordinates are needed.
    pub fn used_cells(
        &self,
    ) -> impl DoubleEndedIterator<Item = (u32, u16, &str)>
           + ExactSizeIterator
           + std::iter::FusedIterator
           + '_ {
        let (r0, c0) = self.start.unwrap_or((0, 0));
        self.formulas
            .iter()
            .map(move |(&(row, col), formula)| (row - r0, col - c0, formula.as_str()))
    }

    /// Iterate non-empty formula cells as absolute worksheet
    /// `(row, col, formula)`.
    pub fn used_cells_abs(
        &self,
    ) -> impl DoubleEndedIterator<Item = (u32, u16, &str)>
           + ExactSizeIterator
           + std::iter::FusedIterator
           + '_ {
        self.formulas
            .iter()
            .map(|(&(row, col), formula)| (row, col, formula.as_str()))
    }
}

/// Iterator over borrowed [`FormulaRangeRow`] views.
#[derive(Debug, Clone)]
pub struct FormulaRangeRows<'range, 'formula> {
    range: &'range FormulaRange<'formula>,
    next_row: u32,
    end_row: u32,
    start_col: u16,
    end_col: u16,
    done: bool,
}

impl<'range, 'formula> Iterator for FormulaRangeRows<'range, 'formula> {
    type Item = FormulaRangeRow<'range, 'formula>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.next_row > self.end_row {
            return None;
        }
        let row = self.next_row;
        if row == self.end_row {
            self.done = true;
        } else {
            self.next_row = row + 1;
        }
        Some(FormulaRangeRow {
            range: self.range,
            row,
            start_col: self.start_col,
            end_col: self.end_col,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = if self.done || self.next_row > self.end_row {
            0
        } else {
            (self.end_row - self.next_row + 1) as usize
        };
        (len, Some(len))
    }
}

impl<'range, 'formula> ExactSizeIterator for FormulaRangeRows<'range, 'formula> {}

impl<'range, 'formula> DoubleEndedIterator for FormulaRangeRows<'range, 'formula> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.done || self.next_row > self.end_row {
            return None;
        }
        let row = self.end_row;
        if row == self.next_row {
            self.done = true;
        } else {
            self.end_row = row - 1;
        }
        Some(FormulaRangeRow {
            range: self.range,
            row,
            start_col: self.start_col,
            end_col: self.end_col,
        })
    }
}

impl<'range, 'formula> std::iter::FusedIterator for FormulaRangeRows<'range, 'formula> {}

/// A borrowed row view inside a [`FormulaRange`].
#[derive(Debug, Clone, Copy)]
pub struct FormulaRangeRow<'range, 'formula> {
    range: &'range FormulaRange<'formula>,
    row: u32,
    start_col: u16,
    end_col: u16,
}

impl<'range, 'formula> FormulaRangeRow<'range, 'formula> {
    /// Absolute worksheet row index.
    pub fn row(&self) -> u32 {
        self.row
    }

    /// Absolute worksheet column index where this formula row view starts.
    pub fn start_col(&self) -> u16 {
        self.start_col
    }

    /// Absolute worksheet column index where this formula row view ends.
    pub fn end_col(&self) -> u16 {
        self.end_col
    }

    /// Number of columns in this rectangular formula row view.
    pub fn len(&self) -> usize {
        col_span_len(self.start_col, self.end_col)
    }

    /// `true` when this row contains no columns.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get a formula by relative column offset within this row.
    pub fn get(&self, col: usize) -> Option<&str> {
        let col = self.start_col.checked_add(u16::try_from(col).ok()?)?;
        if col > self.end_col {
            return None;
        }
        self.range.get_abs(self.row, col)
    }

    /// Get a formula by absolute worksheet column within this row view.
    pub fn get_abs(&self, col: u16) -> Option<&str> {
        if col < self.start_col || col > self.end_col {
            return None;
        }
        self.range.get_abs(self.row, col)
    }

    /// Iterate formulas across the row. Missing sparse formula cells are `None`.
    pub fn iter(&self) -> FormulaRangeRowCells<'range, 'formula> {
        FormulaRangeRowCells {
            range: self.range,
            row: self.row,
            next_col: self.start_col,
            end_col: self.end_col,
            done: false,
        }
    }

    /// Iterate every rectangular formula position as `(relative_col, formula)`.
    /// Missing sparse formula cells are represented as `None`.
    pub fn cells(
        &self,
    ) -> impl DoubleEndedIterator<Item = (usize, Option<&'range str>)>
           + ExactSizeIterator
           + std::iter::FusedIterator
           + '_ {
        self.iter().enumerate()
    }

    /// Iterate non-empty formula cells in this row as absolute `(col, formula)` pairs.
    pub fn used_cells(&self) -> FormulaRangeRowUsedCells<'range, 'formula> {
        let bounds = (self.row, self.start_col)..=(self.row, self.end_col);
        let remaining = self.range.formulas.range(bounds.clone()).count();
        FormulaRangeRowUsedCells {
            entries: self.range.formulas.range(bounds),
            remaining,
        }
    }
}

/// Iterator over non-empty formulas in one borrowed [`FormulaRangeRow`].
#[derive(Debug, Clone)]
pub struct FormulaRangeRowUsedCells<'range, 'formula> {
    entries: BTreeMapRange<'range, (u32, u16), FormulaEntry<'formula>>,
    remaining: usize,
}

impl<'range, 'formula> Iterator for FormulaRangeRowUsedCells<'range, 'formula> {
    type Item = (u16, &'range str);

    fn next(&mut self) -> Option<Self::Item> {
        let (&(_, col), formula) = self.entries.next()?;
        self.remaining = self.remaining.saturating_sub(1);
        Some((col, formula.as_str()))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'range, 'formula> ExactSizeIterator for FormulaRangeRowUsedCells<'range, 'formula> {
    fn len(&self) -> usize {
        self.remaining
    }
}

impl<'range, 'formula> DoubleEndedIterator for FormulaRangeRowUsedCells<'range, 'formula> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let (&(_, col), formula) = self.entries.next_back()?;
        self.remaining = self.remaining.saturating_sub(1);
        Some((col, formula.as_str()))
    }
}

impl<'range, 'formula> std::iter::FusedIterator for FormulaRangeRowUsedCells<'range, 'formula> {}

/// Iterator over one borrowed [`FormulaRangeRow`]'s formulas.
#[derive(Debug, Clone)]
pub struct FormulaRangeRowCells<'range, 'formula> {
    range: &'range FormulaRange<'formula>,
    row: u32,
    next_col: u16,
    end_col: u16,
    done: bool,
}

impl<'range, 'formula> Iterator for FormulaRangeRowCells<'range, 'formula> {
    type Item = Option<&'range str>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.next_col > self.end_col {
            return None;
        }
        let col = self.next_col;
        if col == self.end_col {
            self.done = true;
        } else {
            self.next_col = col + 1;
        }
        Some(self.range.get_abs(self.row, col))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = if self.done || self.next_col > self.end_col {
            0
        } else {
            usize::from(self.end_col - self.next_col) + 1
        };
        (len, Some(len))
    }
}

impl<'range, 'formula> ExactSizeIterator for FormulaRangeRowCells<'range, 'formula> {}

impl<'range, 'formula> DoubleEndedIterator for FormulaRangeRowCells<'range, 'formula> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.done || self.next_col > self.end_col {
            return None;
        }
        let col = self.end_col;
        if col == self.next_col {
            self.done = true;
        } else {
            self.end_col = col - 1;
        }
        Some(self.range.get_abs(self.row, col))
    }
}

impl<'range, 'formula> std::iter::FusedIterator for FormulaRangeRowCells<'range, 'formula> {}

/// Error type returned by range row deserialization.
#[cfg(feature = "serde")]
pub type DeError = serde::de::value::Error;

/// Deserialize a spreadsheet cell as `f64`, returning `None` for invalid cells.
///
/// Intended for Serde's `deserialize_with` field attribute when a numeric
/// column may contain non-numeric placeholders. Empty cells, errors, and text
/// that cannot be parsed as a number are non-fatal.
#[cfg(feature = "serde")]
pub fn deserialize_as_f64_or_none<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cell = <Option<Cell> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(cell.and_then(|cell| cell.as_f64()))
}

/// Deserialize a spreadsheet cell as `i64`, returning `None` for invalid cells.
///
/// Intended for Serde's `deserialize_with` field attribute when an integer
/// column may contain non-integer placeholders. Empty cells, errors, and text
/// that cannot be parsed as an integer are non-fatal.
#[cfg(feature = "serde")]
pub fn deserialize_as_i64_or_none<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cell = <Option<Cell> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(cell.and_then(|cell| cell.as_i64()))
}

/// Deserialize a spreadsheet cell as `f64`, preserving invalid cells as text.
///
/// Intended for Serde's `deserialize_with` field attribute. Numeric cells and
/// parseable numeric text produce `Ok(value)`; invalid cells produce
/// `Err(display_text)`. Empty cells return `Err(String::new())`.
#[cfg(feature = "serde")]
pub fn deserialize_as_f64_or_string<'de, D>(
    deserializer: D,
) -> std::result::Result<std::result::Result<f64, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cell = <Option<Cell> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(match cell {
        Some(cell) => cell.as_f64().ok_or_else(|| display_text(&cell)),
        None => Err(String::new()),
    })
}

/// Deserialize a spreadsheet cell as `i64`, preserving invalid cells as text.
///
/// Intended for Serde's `deserialize_with` field attribute. Integer cells and
/// parseable integer text produce `Ok(value)`; invalid cells produce
/// `Err(display_text)`. Empty cells return `Err(String::new())`.
#[cfg(feature = "serde")]
pub fn deserialize_as_i64_or_string<'de, D>(
    deserializer: D,
) -> std::result::Result<std::result::Result<i64, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cell = <Option<Cell> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(match cell {
        Some(cell) => cell.as_i64().ok_or_else(|| display_text(&cell)),
        None => Err(String::new()),
    })
}

/// Deserialize a spreadsheet cell as a chrono duration, returning `None` for
/// invalid cells.
///
/// Intended for Serde's `deserialize_with` field attribute when an elapsed
/// duration column may contain non-duration placeholders. Numeric and date cells
/// are interpreted as Excel day-based duration serials, so `1.5` is 36 hours.
/// Empty cells, errors, and text are non-fatal.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_duration_or_none<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<chrono::Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cell = <Option<Cell> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(cell.and_then(|cell| cell.as_duration()))
}

/// Deserialize a spreadsheet cell as a chrono duration, preserving invalid
/// cells as text.
///
/// Intended for Serde's `deserialize_with` field attribute. Numeric and date
/// cells produce `Ok(duration)` by interpreting Excel day-based duration
/// serials; invalid cells produce `Err(display_text)`. Empty cells return
/// `Err(String::new())`.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_duration_or_string<'de, D>(
    deserializer: D,
) -> std::result::Result<std::result::Result<chrono::Duration, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cell = <Option<Cell> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(match cell {
        Some(cell) => cell.as_duration().ok_or_else(|| display_text(&cell)),
        None => Err(String::new()),
    })
}

#[cfg(all(feature = "serde", feature = "chrono"))]
fn deserialize_date_or_none_with_epoch<'de, D>(
    deserializer: D,
    date1904: bool,
) -> std::result::Result<Option<chrono::NaiveDate>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cell = <Option<Cell> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(cell.and_then(|cell| cell.as_date(date1904)))
}

#[cfg(all(feature = "serde", feature = "chrono"))]
fn deserialize_date_or_string_with_epoch<'de, D>(
    deserializer: D,
    date1904: bool,
) -> std::result::Result<std::result::Result<chrono::NaiveDate, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cell = <Option<Cell> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(match cell {
        Some(cell) => cell.as_date(date1904).ok_or_else(|| display_text(&cell)),
        None => Err(String::new()),
    })
}

#[cfg(all(feature = "serde", feature = "chrono"))]
fn deserialize_time_or_none_with_epoch<'de, D>(
    deserializer: D,
    date1904: bool,
) -> std::result::Result<Option<chrono::NaiveTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cell = <Option<Cell> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(cell.and_then(|cell| cell.as_time(date1904)))
}

#[cfg(all(feature = "serde", feature = "chrono"))]
fn deserialize_time_or_string_with_epoch<'de, D>(
    deserializer: D,
    date1904: bool,
) -> std::result::Result<std::result::Result<chrono::NaiveTime, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cell = <Option<Cell> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(match cell {
        Some(cell) => cell.as_time(date1904).ok_or_else(|| display_text(&cell)),
        None => Err(String::new()),
    })
}

#[cfg(all(feature = "serde", feature = "chrono"))]
fn deserialize_datetime_or_none_with_epoch<'de, D>(
    deserializer: D,
    date1904: bool,
) -> std::result::Result<Option<chrono::NaiveDateTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cell = <Option<Cell> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(cell.and_then(|cell| cell.as_naive_datetime(date1904)))
}

#[cfg(all(feature = "serde", feature = "chrono"))]
fn deserialize_datetime_or_string_with_epoch<'de, D>(
    deserializer: D,
    date1904: bool,
) -> std::result::Result<std::result::Result<chrono::NaiveDateTime, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cell = <Option<Cell> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(match cell {
        Some(cell) => cell
            .as_naive_datetime(date1904)
            .ok_or_else(|| display_text(&cell)),
        None => Err(String::new()),
    })
}

/// Deserialize a spreadsheet cell as a 1900-epoch chrono date, returning
/// `None` for invalid cells.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_date_1900_or_none<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<chrono::NaiveDate>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_date_or_none_with_epoch(deserializer, false)
}

/// Deserialize a spreadsheet cell as a 1900-epoch chrono date, preserving
/// invalid cells as text.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_date_1900_or_string<'de, D>(
    deserializer: D,
) -> std::result::Result<std::result::Result<chrono::NaiveDate, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_date_or_string_with_epoch(deserializer, false)
}

/// Deserialize a spreadsheet cell as a 1900-epoch chrono time, returning
/// `None` for invalid cells.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_time_1900_or_none<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<chrono::NaiveTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_time_or_none_with_epoch(deserializer, false)
}

/// Deserialize a spreadsheet cell as a 1900-epoch chrono time, preserving
/// invalid cells as text.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_time_1900_or_string<'de, D>(
    deserializer: D,
) -> std::result::Result<std::result::Result<chrono::NaiveTime, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_time_or_string_with_epoch(deserializer, false)
}

/// Deserialize a spreadsheet cell as a 1900-epoch chrono datetime, returning
/// `None` for invalid cells.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_datetime_1900_or_none<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<chrono::NaiveDateTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_datetime_or_none_with_epoch(deserializer, false)
}

/// Deserialize a spreadsheet cell as a 1900-epoch chrono datetime, preserving
/// invalid cells as text.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_datetime_1900_or_string<'de, D>(
    deserializer: D,
) -> std::result::Result<std::result::Result<chrono::NaiveDateTime, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_datetime_or_string_with_epoch(deserializer, false)
}

/// Deserialize a spreadsheet cell as a 1904-epoch chrono date, returning
/// `None` for invalid cells.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_date_1904_or_none<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<chrono::NaiveDate>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_date_or_none_with_epoch(deserializer, true)
}

/// Deserialize a spreadsheet cell as a 1904-epoch chrono date, preserving
/// invalid cells as text.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_date_1904_or_string<'de, D>(
    deserializer: D,
) -> std::result::Result<std::result::Result<chrono::NaiveDate, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_date_or_string_with_epoch(deserializer, true)
}

/// Deserialize a spreadsheet cell as a 1904-epoch chrono time, returning
/// `None` for invalid cells.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_time_1904_or_none<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<chrono::NaiveTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_time_or_none_with_epoch(deserializer, true)
}

/// Deserialize a spreadsheet cell as a 1904-epoch chrono time, preserving
/// invalid cells as text.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_time_1904_or_string<'de, D>(
    deserializer: D,
) -> std::result::Result<std::result::Result<chrono::NaiveTime, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_time_or_string_with_epoch(deserializer, true)
}

/// Deserialize a spreadsheet cell as a 1904-epoch chrono datetime, returning
/// `None` for invalid cells.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_datetime_1904_or_none<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<chrono::NaiveDateTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_datetime_or_none_with_epoch(deserializer, true)
}

/// Deserialize a spreadsheet cell as a 1904-epoch chrono datetime, preserving
/// invalid cells as text.
#[cfg(all(feature = "serde", feature = "chrono"))]
pub fn deserialize_as_datetime_1904_or_string<'de, D>(
    deserializer: D,
) -> std::result::Result<std::result::Result<chrono::NaiveDateTime, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_datetime_or_string_with_epoch(deserializer, true)
}

/// Builds a typed row deserializer for a [`Range`].
///
/// Text and error cells are offered to serde as borrowed strings, so row types
/// may contain `&str` fields that borrow directly from the backing [`Range`].
#[cfg(feature = "serde")]
#[derive(Debug, Clone)]
pub struct RangeDeserializerBuilder {
    has_headers: bool,
    header_row: HeaderRow,
    headers: Option<Vec<String>>,
    skip_missing_headers: bool,
}

#[cfg(feature = "serde")]
impl Default for RangeDeserializerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "serde")]
impl RangeDeserializerBuilder {
    /// Construct a builder. By default, the first row is treated as headers.
    pub fn new() -> Self {
        Self {
            has_headers: true,
            header_row: HeaderRow::FirstNonEmptyRow,
            headers: None,
            skip_missing_headers: false,
        }
    }

    /// Decide whether the first row should be treated as a header row.
    pub fn has_headers(&mut self, yes: bool) -> &mut Self {
        self.has_headers = yes;
        if !yes {
            self.header_row = HeaderRow::FirstNonEmptyRow;
            self.headers = None;
        }
        self
    }

    /// Select the row that contains header names.
    ///
    /// Rows up to and including the selected header row are skipped. Explicit
    /// [`HeaderRow::Row`] positions are absolute worksheet row indexes; if the
    /// selected row is outside the supplied range, the deserializer yields no
    /// rows.
    pub fn with_header_row(&mut self, row: impl Into<HeaderRow>) -> &mut Self {
        self.has_headers = true;
        self.header_row = row.into();
        self
    }

    /// Construct a builder that deserializes only the named headers, in the
    /// provided order. The first range row is used as the source header row
    /// unless [`RangeDeserializerBuilder::with_header_row`] overrides it.
    pub fn with_headers<H>(headers: &[H]) -> Self
    where
        H: AsRef<str>,
    {
        Self {
            has_headers: true,
            header_row: HeaderRow::FirstNonEmptyRow,
            headers: Some(headers.iter().map(|h| h.as_ref().to_string()).collect()),
            skip_missing_headers: false,
        }
    }

    /// Construct a builder that deserializes only the fields of `D`, using
    /// serde's field names (including `rename` attributes) as headers.
    ///
    /// Serde aliases are accepted only when the worksheet actually contains the
    /// alias header; absent aliases are ignored so they do not synthesize empty
    /// columns. Types that deserialize as maps rather than structs cannot expose
    /// a field list for this helper.
    pub fn with_deserialize_headers<D>() -> Self
    where
        D: for<'de> serde::Deserialize<'de>,
    {
        let mut headers = Vec::new();
        let _ = D::deserialize(HeaderExtractor {
            headers: &mut headers,
        });
        Self {
            has_headers: true,
            header_row: HeaderRow::FirstNonEmptyRow,
            headers: Some(headers),
            skip_missing_headers: true,
        }
    }

    /// Build an iterator that deserializes each row into `D`.
    pub fn from_range<'cell, D>(
        &self,
        range: &'cell Range<'cell>,
    ) -> std::result::Result<RangeDeserializer<'cell, D>, DeError>
    where
        D: serde::Deserialize<'cell>,
    {
        RangeDeserializer::new(
            range,
            self.has_headers,
            self.header_row,
            self.headers.as_deref(),
            self.skip_missing_headers,
        )
    }
}

#[cfg(feature = "serde")]
#[derive(Debug)]
struct HeaderError;

#[cfg(feature = "serde")]
impl std::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("header extraction stopped")
    }
}

#[cfg(feature = "serde")]
impl std::error::Error for HeaderError {}

#[cfg(feature = "serde")]
impl serde::de::Error for HeaderError {
    fn custom<T>(_msg: T) -> Self
    where
        T: std::fmt::Display,
    {
        HeaderError
    }
}

#[cfg(feature = "serde")]
struct HeaderExtractor<'a> {
    headers: &'a mut Vec<String>,
}

#[cfg(feature = "serde")]
impl<'de, 'a> serde::de::Deserializer<'de> for HeaderExtractor<'a> {
    type Error = HeaderError;

    fn deserialize_any<V>(self, _visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(HeaderError)
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        _visitor: V,
    ) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.headers
            .extend(fields.iter().map(|field| (*field).to_string()));
        Err(HeaderError)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf option unit
        unit_struct newtype_struct seq tuple tuple_struct map enum identifier ignored_any
    }
}

#[cfg(feature = "serde")]
#[derive(Debug, Clone)]
struct DeserColumn {
    header: Option<String>,
    offset: Option<usize>,
}

#[cfg(feature = "serde")]
fn first_non_empty_row(
    range: &Range<'_>,
    start_row: u32,
    start_col: u16,
    end_row: u32,
    width: usize,
) -> Option<u32> {
    if start_row > end_row || width == 0 {
        return None;
    }
    (start_row..=end_row).find(|&row| {
        (0..width).any(|idx| {
            let col = start_col + idx as u16;
            range.get_abs(row, col).is_some()
        })
    })
}

/// Iterator returned by [`RangeDeserializerBuilder`].
///
/// The output type may borrow from the source range, for example a struct with
/// `&str` fields.
#[cfg(feature = "serde")]
#[derive(Debug)]
pub struct RangeDeserializer<'cell, D>
where
    D: serde::Deserialize<'cell>,
{
    range: &'cell Range<'cell>,
    row: u32,
    end_row: u32,
    start_col: u16,
    columns: Vec<DeserColumn>,
    has_header_names: bool,
    done: bool,
    _marker: std::marker::PhantomData<D>,
}

#[cfg(feature = "serde")]
impl<'cell, D> RangeDeserializer<'cell, D>
where
    D: serde::Deserialize<'cell>,
{
    fn new(
        range: &'cell Range<'cell>,
        has_headers: bool,
        header_row: HeaderRow,
        selected_headers: Option<&[String]>,
        skip_missing_headers: bool,
    ) -> std::result::Result<Self, DeError> {
        let (start_row, start_col, end_row, end_col) = match (range.start, range.end) {
            (Some((r0, c0)), Some((r1, c1))) => (r0, c0, r1, c1),
            _ => (1, 1, 0, 0),
        };
        let width = if start_row <= end_row {
            col_span_len(start_col, end_col)
        } else {
            0
        };
        let use_header_row = has_headers || selected_headers.is_some();
        let header_row = if use_header_row {
            match header_row {
                HeaderRow::FirstNonEmptyRow => {
                    first_non_empty_row(range, start_row, start_col, end_row, width)
                }
                HeaderRow::Row(row) => Some(row),
            }
        } else {
            None
        };
        let header_row_in_range = header_row.is_some_and(|row| start_row <= row && row <= end_row);
        let source_headers = if use_header_row && header_row_in_range && width > 0 {
            let header_row = header_row.expect("checked header row");
            Some(
                (0..width)
                    .map(|idx| {
                        let col = start_col + idx as u16;
                        range
                            .formatted_abs(header_row, col)
                            .map(str::to_string)
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        let has_source_headers = source_headers.is_some();
        let columns: Vec<DeserColumn> = match selected_headers {
            Some(headers) => {
                let source = source_headers.as_deref().unwrap_or(&[]);
                headers
                    .iter()
                    .filter_map(|header| {
                        let requested = header.trim();
                        let offset = source
                            .iter()
                            .position(|source_header| source_header.trim() == requested);
                        if skip_missing_headers && offset.is_none() {
                            return None;
                        }
                        let row_header = offset
                            .and_then(|idx| source.get(idx).cloned())
                            .unwrap_or_else(|| header.clone());
                        Some(DeserColumn {
                            header: Some(row_header),
                            offset,
                        })
                    })
                    .collect::<Vec<_>>()
            }
            None if has_headers && width > 0 => source_headers
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .map(|(offset, header)| DeserColumn {
                    header: Some(header),
                    offset: Some(offset),
                })
                .collect(),
            None => (0..width)
                .map(|offset| DeserColumn {
                    header: None,
                    offset: Some(offset),
                })
                .collect(),
        };
        if selected_headers.is_some() && !skip_missing_headers && has_source_headers {
            if let Some(missing) = columns
                .iter()
                .find_map(|column| column.offset.is_none().then_some(column.header.as_deref()))
                .flatten()
            {
                return Err(serde::de::Error::custom(format!(
                    "missing range header: {missing}"
                )));
            }
        }
        let has_header_names = columns.iter().any(|column| column.header.is_some());
        let empty = start_row > end_row || width == 0;
        let (row, done) = if empty {
            (start_row, true)
        } else if use_header_row {
            let Some(header_row) = header_row else {
                return Ok(Self {
                    range,
                    row: start_row,
                    end_row,
                    start_col,
                    columns,
                    has_header_names,
                    done: true,
                    _marker: std::marker::PhantomData,
                });
            };
            match header_row.checked_add(1) {
                Some(row) if header_row_in_range && row <= end_row => (row, false),
                _ => (start_row, true),
            }
        } else {
            (start_row, false)
        };
        Ok(Self {
            range,
            row,
            end_row,
            start_col,
            columns,
            has_header_names,
            done,
            _marker: std::marker::PhantomData,
        })
    }

    fn remaining_len(&self) -> usize {
        if self.done || self.row > self.end_row {
            0
        } else {
            (self.end_row - self.row + 1) as usize
        }
    }
}

#[cfg(feature = "serde")]
impl<'cell, D> Iterator for RangeDeserializer<'cell, D>
where
    D: serde::Deserialize<'cell>,
{
    type Item = std::result::Result<D, DeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.row > self.end_row {
            return None;
        }
        let row = self.row;
        if row == self.end_row {
            self.done = true;
        } else {
            self.row = row + 1;
        }
        let de = RowDeserializer {
            range: self.range,
            row,
            start_col: self.start_col,
            columns: &self.columns,
            has_header_names: self.has_header_names,
        };
        Some(D::deserialize(de))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.remaining_len();
        (len, Some(len))
    }
}

#[cfg(feature = "serde")]
impl<'cell, D> ExactSizeIterator for RangeDeserializer<'cell, D> where D: serde::Deserialize<'cell> {}

#[cfg(feature = "serde")]
impl<'cell, D> std::iter::FusedIterator for RangeDeserializer<'cell, D> where
    D: serde::Deserialize<'cell>
{
}

#[cfg(feature = "serde")]
#[derive(Clone, Copy)]
struct CellValue<'a> {
    value: Option<&'a Cell>,
    text: Option<&'a str>,
}

#[cfg(feature = "serde")]
impl<'a> CellValue<'a> {
    fn empty() -> Self {
        Self {
            value: None,
            text: None,
        }
    }

    fn from_entry(entry: &'a RangeCell<'_>) -> Self {
        Self {
            value: Some(entry.value()),
            text: Some(entry.text()),
        }
    }

    fn from_formula_cached(cached: &'a Cell, text: Option<&'a str>) -> Self {
        Self {
            value: Some(cached),
            text,
        }
    }
}

#[cfg(feature = "serde")]
impl<'de, 'a: 'de> serde::de::Deserializer<'de> for CellValue<'a> {
    type Error = DeError;

    fn deserialize_any<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        match self.value {
            None => visitor.visit_unit(),
            Some(value) => match value {
                Cell::Text(s) => visitor.visit_borrowed_str(s),
                Cell::Number(n) | Cell::Date(n) => visitor.visit_f64(*n),
                Cell::Bool(b) => visitor.visit_bool(*b),
                Cell::Error(e) => visitor.visit_borrowed_str(e),
                Cell::Formula { cached, .. } => {
                    CellValue::from_formula_cached(cached.as_ref(), self.text)
                        .deserialize_any(visitor)
                }
            },
        }
    }

    fn deserialize_str<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        match self.value {
            Some(value) => match value {
                Cell::Text(s) | Cell::Error(s) => visitor.visit_borrowed_str(s),
                Cell::Number(_) | Cell::Date(_) | Cell::Bool(_) | Cell::Formula { .. } => {
                    visitor.visit_borrowed_str(self.text.unwrap_or_default())
                }
            },
            None => Err(serde::de::Error::custom(
                "expected text cell, got empty cell",
            )),
        }
    }

    fn deserialize_string<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_bool<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        match self.value {
            Some(value) => match value {
                Cell::Bool(b) => visitor.visit_bool(*b),
                Cell::Formula { cached, .. } => {
                    CellValue::from_formula_cached(cached.as_ref(), self.text)
                        .deserialize_bool(visitor)
                }
                other => Err(serde::de::Error::custom(format!(
                    "expected bool cell, got {other:?}"
                ))),
            },
            None => Err(serde::de::Error::custom(
                "expected bool cell, got empty cell",
            )),
        }
    }

    fn deserialize_f64<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let n = cell_to_f64(self.value)?;
        visitor.visit_f64(n)
    }

    fn deserialize_f32<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let n = cell_to_f64(self.value)?;
        if !n.is_finite() || n < f64::from(f32::MIN) || n > f64::from(f32::MAX) {
            return Err(serde::de::Error::custom(format!(
                "numeric cell out of range for f32: {n}"
            )));
        }
        visitor.visit_f32(n as f32)
    }

    fn deserialize_i8<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let n = cell_to_i64(self.value)?;
        let n = i8::try_from(n).map_err(serde::de::Error::custom)?;
        visitor.visit_i8(n)
    }

    fn deserialize_i16<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let n = cell_to_i64(self.value)?;
        let n = i16::try_from(n).map_err(serde::de::Error::custom)?;
        visitor.visit_i16(n)
    }

    fn deserialize_i32<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let n = cell_to_i64(self.value)?;
        let n = i32::try_from(n).map_err(serde::de::Error::custom)?;
        visitor.visit_i32(n)
    }

    fn deserialize_i64<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let n = cell_to_i64(self.value)?;
        visitor.visit_i64(n)
    }

    fn deserialize_u8<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let n = cell_to_i64(self.value)?;
        let n = u8::try_from(n).map_err(serde::de::Error::custom)?;
        visitor.visit_u8(n)
    }

    fn deserialize_u16<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let n = cell_to_i64(self.value)?;
        let n = u16::try_from(n).map_err(serde::de::Error::custom)?;
        visitor.visit_u16(n)
    }

    fn deserialize_u32<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let n = cell_to_i64(self.value)?;
        let n = u32::try_from(n).map_err(serde::de::Error::custom)?;
        visitor.visit_u32(n)
    }

    fn deserialize_u64<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let n = cell_to_i64(self.value)?;
        let n = u64::try_from(n).map_err(serde::de::Error::custom)?;
        visitor.visit_u64(n)
    }

    fn deserialize_option<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        match self.value {
            None => visitor.visit_none(),
            Some(_) => visitor.visit_some(self),
        }
    }

    fn deserialize_enum<V>(
        self,
        name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        if name == "Cell" {
            let Some(value) = self.value else {
                return Err(serde::de::Error::custom("expected cell, got empty cell"));
            };
            visitor.visit_enum(CellEnumAccess {
                cell: value.clone(),
            })
        } else {
            self.deserialize_any(visitor)
        }
    }

    serde::forward_to_deserialize_any! {
        char bytes byte_buf unit unit_struct
        newtype_struct seq tuple tuple_struct map struct identifier ignored_any
    }
}

#[cfg(feature = "serde")]
struct CellEnumAccess {
    cell: Cell,
}

#[cfg(feature = "serde")]
impl<'de> serde::de::EnumAccess<'de> for CellEnumAccess {
    type Error = DeError;
    type Variant = CellVariantAccess;

    fn variant_seed<V>(self, seed: V) -> std::result::Result<(V::Value, Self::Variant), Self::Error>
    where
        V: serde::de::DeserializeSeed<'de>,
    {
        let variant = match &self.cell {
            Cell::Text(_) => "Text",
            Cell::Number(_) => "Number",
            Cell::Date(_) => "Date",
            Cell::Bool(_) => "Bool",
            Cell::Error(_) => "Error",
            Cell::Formula { .. } => "Formula",
        };
        let value = seed.deserialize(variant.into_deserializer())?;
        Ok((value, CellVariantAccess { cell: self.cell }))
    }
}

#[cfg(feature = "serde")]
struct CellVariantAccess {
    cell: Cell,
}

#[cfg(feature = "serde")]
impl<'de> serde::de::VariantAccess<'de> for CellVariantAccess {
    type Error = DeError;

    fn unit_variant(self) -> std::result::Result<(), Self::Error> {
        Err(serde::de::Error::custom("rxls Cell variants carry values"))
    }

    fn newtype_variant_seed<T>(self, seed: T) -> std::result::Result<T::Value, Self::Error>
    where
        T: serde::de::DeserializeSeed<'de>,
    {
        match self.cell {
            Cell::Text(s) | Cell::Error(s) => seed.deserialize(s.into_deserializer()),
            Cell::Number(n) | Cell::Date(n) => seed.deserialize(n.into_deserializer()),
            Cell::Bool(b) => seed.deserialize(b.into_deserializer()),
            Cell::Formula { formula, cached } => seed.deserialize(FormulaTupleDeserializer {
                formula,
                cached: *cached,
            }),
        }
    }

    fn tuple_variant<V>(self, _len: usize, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        match self.cell {
            Cell::Formula { formula, cached } => visitor.visit_seq(FormulaTupleAccess {
                idx: 0,
                formula: Some(formula),
                cached: Some(*cached),
            }),
            _ => Err(serde::de::Error::custom(
                "only Formula is represented as a tuple variant",
            )),
        }
    }

    fn struct_variant<V>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.tuple_variant(2, visitor)
    }
}

#[cfg(feature = "serde")]
struct FormulaTupleDeserializer {
    formula: String,
    cached: Cell,
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserializer<'de> for FormulaTupleDeserializer {
    type Error = DeError;

    fn deserialize_any<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_seq(FormulaTupleAccess {
            idx: 0,
            formula: Some(self.formula),
            cached: Some(self.cached),
        })
    }

    fn deserialize_tuple<V>(
        self,
        _len: usize,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq map struct enum identifier ignored_any
    }
}

#[cfg(feature = "serde")]
struct FormulaTupleAccess {
    idx: u8,
    formula: Option<String>,
    cached: Option<Cell>,
}

#[cfg(feature = "serde")]
impl<'de> serde::de::SeqAccess<'de> for FormulaTupleAccess {
    type Error = DeError;

    fn next_element_seed<T>(
        &mut self,
        seed: T,
    ) -> std::result::Result<Option<T::Value>, Self::Error>
    where
        T: serde::de::DeserializeSeed<'de>,
    {
        match self.idx {
            0 => {
                self.idx = 1;
                let formula = self.formula.take().unwrap_or_default();
                seed.deserialize(formula.into_deserializer()).map(Some)
            }
            1 => {
                self.idx = 2;
                let Some(cached) = self.cached.take() else {
                    return Ok(None);
                };
                seed.deserialize(CellOwnedDeserializer { cell: cached })
                    .map(Some)
            }
            _ => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(usize::from(2u8.saturating_sub(self.idx)))
    }
}

#[cfg(feature = "serde")]
struct CellOwnedDeserializer {
    cell: Cell,
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserializer<'de> for CellOwnedDeserializer {
    type Error = DeError;

    fn deserialize_any<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        match self.cell {
            Cell::Text(s) | Cell::Error(s) => visitor.visit_string(s),
            Cell::Number(n) | Cell::Date(n) => visitor.visit_f64(n),
            Cell::Bool(b) => visitor.visit_bool(b),
            Cell::Formula { cached, .. } => {
                CellOwnedDeserializer { cell: *cached }.deserialize_any(visitor)
            }
        }
    }

    fn deserialize_enum<V>(
        self,
        name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        if name == "Cell" {
            visitor.visit_enum(CellEnumAccess { cell: self.cell })
        } else {
            self.deserialize_any(visitor)
        }
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map struct identifier ignored_any
    }
}

#[cfg(feature = "serde")]
fn cell_to_f64(cell: Option<&Cell>) -> std::result::Result<f64, DeError> {
    match cell {
        Some(Cell::Number(n)) | Some(Cell::Date(n)) => Ok(*n),
        Some(Cell::Formula { cached, .. }) => cell_to_f64(Some(cached.as_ref())),
        Some(Cell::Text(s)) => s.parse::<f64>().map_err(serde::de::Error::custom),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected numeric cell, got {other:?}"
        ))),
        None => Err(serde::de::Error::custom(
            "expected numeric cell, got empty cell",
        )),
    }
}

#[cfg(feature = "serde")]
fn cell_to_i64(cell: Option<&Cell>) -> std::result::Result<i64, DeError> {
    match cell {
        Some(Cell::Number(n)) | Some(Cell::Date(n)) if n.is_finite() && n.fract() == 0.0 => {
            Ok(*n as i64)
        }
        Some(Cell::Formula { cached, .. }) => cell_to_i64(Some(cached.as_ref())),
        Some(Cell::Text(s)) => s.parse::<i64>().map_err(serde::de::Error::custom),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected integer cell, got {other:?}"
        ))),
        None => Err(serde::de::Error::custom(
            "expected integer cell, got empty cell",
        )),
    }
}

#[cfg(feature = "serde")]
fn cell_at_column<'a>(
    range: &'a Range<'a>,
    row: u32,
    start_col: u16,
    offset: Option<usize>,
) -> CellValue<'a> {
    let Some(offset) = offset.and_then(|offset| u16::try_from(offset).ok()) else {
        return CellValue::empty();
    };
    let Some(col) = start_col.checked_add(offset) else {
        return CellValue::empty();
    };
    range
        .entry_abs(row, col)
        .map(CellValue::from_entry)
        .unwrap_or_else(CellValue::empty)
}

#[cfg(feature = "serde")]
struct RowDeserializer<'cols, 'cell> {
    range: &'cell Range<'cell>,
    row: u32,
    start_col: u16,
    columns: &'cols [DeserColumn],
    has_header_names: bool,
}

#[cfg(feature = "serde")]
impl<'de, 'cols, 'cell: 'de> serde::de::Deserializer<'de> for RowDeserializer<'cols, 'cell> {
    type Error = DeError;

    fn deserialize_any<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        if self.has_header_names {
            visitor.visit_map(RowMapAccess {
                range: self.range,
                row: self.row,
                start_col: self.start_col,
                columns: self.columns,
                idx: 0,
                pending: None,
            })
        } else {
            visitor.visit_seq(RowSeqAccess {
                range: self.range,
                row: self.row,
                start_col: self.start_col,
                columns: self.columns,
                idx: 0,
            })
        }
    }

    fn deserialize_seq<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_seq(RowSeqAccess {
            range: self.range,
            row: self.row,
            start_col: self.start_col,
            columns: self.columns,
            idx: 0,
        })
    }

    fn deserialize_tuple<V>(
        self,
        _len: usize,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct enum identifier ignored_any
    }
}

#[cfg(feature = "serde")]
struct RowSeqAccess<'cols, 'cell> {
    range: &'cell Range<'cell>,
    row: u32,
    start_col: u16,
    columns: &'cols [DeserColumn],
    idx: usize,
}

#[cfg(feature = "serde")]
impl<'de, 'cols, 'cell: 'de> serde::de::SeqAccess<'de> for RowSeqAccess<'cols, 'cell> {
    type Error = DeError;

    fn next_element_seed<T>(
        &mut self,
        seed: T,
    ) -> std::result::Result<Option<T::Value>, Self::Error>
    where
        T: serde::de::DeserializeSeed<'de>,
    {
        if self.idx >= self.columns.len() {
            return Ok(None);
        }
        let column = &self.columns[self.idx];
        self.idx += 1;
        seed.deserialize(cell_at_column(
            self.range,
            self.row,
            self.start_col,
            column.offset,
        ))
        .map(Some)
    }
}

#[cfg(feature = "serde")]
struct RowMapAccess<'cols, 'cell> {
    range: &'cell Range<'cell>,
    row: u32,
    start_col: u16,
    columns: &'cols [DeserColumn],
    idx: usize,
    pending: Option<usize>,
}

#[cfg(feature = "serde")]
impl<'de, 'cols, 'cell: 'de> serde::de::MapAccess<'de> for RowMapAccess<'cols, 'cell> {
    type Error = DeError;

    fn next_key_seed<K>(&mut self, seed: K) -> std::result::Result<Option<K::Value>, Self::Error>
    where
        K: serde::de::DeserializeSeed<'de>,
    {
        while self.idx < self.columns.len() {
            let idx = self.idx;
            self.idx += 1;
            let Some(header) = self.columns[idx].header.as_deref() else {
                continue;
            };
            if header.is_empty() {
                continue;
            }
            self.pending = Some(idx);
            return seed.deserialize(header.into_deserializer()).map(Some);
        }
        Ok(None)
    }

    fn next_value_seed<V>(&mut self, seed: V) -> std::result::Result<V::Value, Self::Error>
    where
        V: serde::de::DeserializeSeed<'de>,
    {
        let idx = self
            .pending
            .take()
            .ok_or_else(|| serde::de::Error::custom("range row value without key"))?;
        seed.deserialize(cell_at_column(
            self.range,
            self.row,
            self.start_col,
            self.columns[idx].offset,
        ))
    }
}

/// Type of sheet in workbook metadata.
///
/// Excel formats distinguish worksheets, chart sheets, macro sheets, dialog
/// sheets, and VBA modules. ODS sheets report [`SheetType::WorkSheet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SheetType {
    /// Regular worksheet grid.
    WorkSheet,
    /// Excel dialog sheet.
    DialogSheet,
    /// Excel macro sheet.
    MacroSheet,
    /// Excel chart sheet.
    ChartSheet,
    /// VBA module sheet.
    Vba,
}

/// Sheet visibility state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SheetVisible {
    /// Visible in the workbook UI.
    Visible,
    /// Hidden but user-unhideable.
    Hidden,
    /// Very hidden; Excel hides it from the unhide UI.
    VeryHidden,
}

/// Public workbook sheet metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetMetadata {
    /// Sheet name.
    pub name: String,
    /// Sheet type.
    pub typ: SheetType,
    /// Sheet visibility.
    pub visible: SheetVisible,
}

impl SheetMetadata {
    /// Sheet name.
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// Sheet type.
    pub fn sheet_type(&self) -> SheetType {
        self.typ
    }

    /// Sheet visibility.
    pub fn visible(&self) -> SheetVisible {
        self.visible
    }

    /// `true` when this metadata describes a regular worksheet grid.
    pub fn is_worksheet(&self) -> bool {
        self.typ == SheetType::WorkSheet
    }

    /// `true` when this sheet is visible in the workbook UI.
    pub fn is_visible(&self) -> bool {
        self.visible == SheetVisible::Visible
    }

    /// `true` when this sheet is hidden but user-unhideable.
    pub fn is_hidden(&self) -> bool {
        self.visible == SheetVisible::Hidden
    }

    /// `true` when this sheet is very hidden.
    pub fn is_very_hidden(&self) -> bool {
        self.visible == SheetVisible::VeryHidden
    }
}

/// Public worksheet view metadata.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SheetView {
    /// Frozen panes split at `(row, col)`, 0-based.
    pub freeze: Option<(u32, u16)>,
    /// Whether worksheet gridlines are hidden in the active sheet view.
    pub hide_gridlines: bool,
    /// Sheet zoom percentage, for example `125`.
    pub zoom: Option<u16>,
    /// Explicit row/column header visibility. `None` means the workbook did not
    /// override Excel's default visible headers.
    pub show_headers: Option<bool>,
    /// Whether the sheet view is laid out right-to-left.
    pub right_to_left: bool,
}

impl SheetView {
    /// Construct default worksheet view metadata.
    pub fn new() -> Self {
        SheetView::default()
    }

    /// Freeze panes below `row` and to the right of `col`.
    pub fn with_freeze(mut self, row: u32, col: u16) -> Self {
        self.freeze = Some((row, col));
        self
    }

    /// Hide worksheet gridlines in the active sheet view.
    pub fn with_hidden_gridlines(mut self) -> Self {
        self.hide_gridlines = true;
        self
    }

    /// Set the sheet zoom percentage.
    pub fn with_zoom(mut self, percent: u16) -> Self {
        self.zoom = Some(percent);
        self
    }

    /// Set explicit row/column header visibility.
    pub fn with_show_headers(mut self, show: bool) -> Self {
        self.show_headers = Some(show);
        self
    }

    /// Lay the sheet out right-to-left.
    pub fn with_right_to_left(mut self, right_to_left: bool) -> Self {
        self.right_to_left = right_to_left;
        self
    }
}

/// Public workbook-level metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbookMetadata<'a> {
    /// `true` when the workbook uses the 1904 date system.
    pub date1904: bool,
    /// `true` when the reader omitted additional text-bearing cells after hitting
    /// the workbook-wide text allocation cap.
    pub text_truncated: bool,
    /// `true` when workbook structure protection is enabled.
    pub structure_protected: bool,
    /// 0-based active/selected sheet index, if it points at an existing sheet.
    pub active_sheet: Option<usize>,
    /// Active/selected sheet name, if the active sheet index is valid.
    pub active_sheet_name: Option<&'a str>,
    /// Document properties parsed from the workbook package.
    pub properties: &'a DocProperties,
    /// Workbook-global defined names as `(name, refers_to)`.
    pub defined_names: &'a [(String, String)],
    /// Worksheet-scoped defined names.
    pub local_defined_names: &'a [LocalDefinedName],
    /// Sheet metadata in workbook order.
    pub sheets: Vec<SheetMetadata>,
}

impl<'a> WorkbookMetadata<'a> {
    /// `true` when the workbook uses the 1904 date system.
    pub fn has_1904_epoch(&self) -> bool {
        self.date1904
    }

    /// `true` when text-bearing cells were omitted after hitting the text cap.
    pub fn is_text_truncated(&self) -> bool {
        self.text_truncated
    }

    /// `true` when workbook structure protection is enabled.
    pub fn is_structure_protected(&self) -> bool {
        self.structure_protected
    }

    /// 0-based active/selected sheet index, if it points at an existing sheet.
    pub fn active_sheet_index(&self) -> Option<usize> {
        self.active_sheet
    }

    /// Active/selected sheet name, if the active sheet index is valid.
    pub fn active_sheet_name(&self) -> Option<&'a str> {
        self.active_sheet_name
    }

    /// Document properties parsed from the workbook package.
    pub fn document_properties(&self) -> &'a DocProperties {
        self.properties
    }

    /// Workbook-global defined names as `(name, refers_to)`.
    pub fn defined_names(&self) -> &'a [(String, String)] {
        self.defined_names
    }

    /// Sheet metadata in workbook order.
    pub fn sheets(&self) -> &[SheetMetadata] {
        self.sheets.as_slice()
    }
}

/// Public grouped worksheet-level metadata facade.
#[derive(Debug, Clone, PartialEq)]
pub struct WorksheetMetadata<'a> {
    /// Sheet name.
    pub name: &'a str,
    /// Sheet type.
    pub sheet_type: SheetType,
    /// Sheet visibility.
    pub visible: SheetVisible,
    /// Used cell dimensions as `(first_row, first_col, last_row, last_col)`.
    pub dimensions: Option<(u32, u16, u32, u16)>,
    /// Merged cell ranges.
    pub merged_ranges: &'a [(u32, u16, u32, u16)],
    /// External hyperlinks.
    pub hyperlinks: &'a [(u32, u16, String)],
    /// Legacy comments / notes.
    pub comments: &'a [Comment],
    /// Worksheet tables.
    pub tables: &'a [Table],
    /// Data-validation rules.
    pub data_validations: &'a [DataValidation],
    /// Conditional-formatting rules.
    pub conditional_formats: &'a [CondFormat],
    /// Whether the worksheet is protected against editing.
    pub protected: bool,
    /// Granular worksheet-protection allowances, when the source exposes them.
    pub protection_options: Option<ProtectionOptions>,
    /// Page setup metadata.
    pub page_setup: Option<&'a PageSetup>,
    /// Worksheet view metadata.
    pub sheet_view: SheetView,
    /// Autofilter range.
    pub autofilter_range: Option<(u32, u16, u32, u16)>,
    /// Worksheet tab color.
    pub tab_color: Option<Color>,
    /// Whether printed pages include worksheet gridlines.
    pub print_gridlines: bool,
    /// Whether printed pages include row and column headings.
    pub print_headings: bool,
    /// Row outline levels keyed by 0-based row index.
    pub row_outline_levels: &'a BTreeMap<u32, u8>,
    /// Column outline levels keyed by 0-based column index.
    pub col_outline_levels: &'a BTreeMap<u16, u8>,
    /// Rows marked as collapsed outline summary rows.
    pub collapsed_rows: &'a BTreeSet<u32>,
    /// Whether outline summary rows appear below grouped detail rows.
    pub outline_summary_below: bool,
    /// Whether outline summary columns appear to the right of grouped detail columns.
    pub outline_summary_right: bool,
    /// Embedded images.
    pub images: &'a [Image],
    /// Charts.
    pub charts: &'a [Chart],
    /// Sparklines.
    pub sparklines: &'a [Sparkline],
}

impl<'a> WorksheetMetadata<'a> {
    /// Sheet name.
    pub fn name(&self) -> &'a str {
        self.name
    }

    /// Sheet type.
    pub fn sheet_type(&self) -> SheetType {
        self.sheet_type
    }

    /// Sheet visibility.
    pub fn visible(&self) -> SheetVisible {
        self.visible
    }

    /// `true` when this metadata describes a regular worksheet grid.
    pub fn is_worksheet(&self) -> bool {
        self.sheet_type == SheetType::WorkSheet
    }

    /// `true` when this worksheet is visible in the workbook UI.
    pub fn is_visible(&self) -> bool {
        self.visible == SheetVisible::Visible
    }

    /// `true` when this worksheet is hidden but user-unhideable.
    pub fn is_hidden(&self) -> bool {
        self.visible == SheetVisible::Hidden
    }

    /// `true` when this worksheet is very hidden.
    pub fn is_very_hidden(&self) -> bool {
        self.visible == SheetVisible::VeryHidden
    }

    /// `true` when worksheet protection is enabled.
    pub fn is_protected(&self) -> bool {
        self.protected
    }

    /// Merged cell ranges.
    pub fn merged_ranges(&self) -> &'a [(u32, u16, u32, u16)] {
        self.merged_ranges
    }

    /// External hyperlinks.
    pub fn hyperlinks(&self) -> &'a [(u32, u16, String)] {
        self.hyperlinks
    }

    /// Legacy comments / notes.
    pub fn comments(&self) -> &'a [Comment] {
        self.comments
    }

    /// Worksheet tables.
    pub fn tables(&self) -> &'a [Table] {
        self.tables
    }

    /// Data-validation rules.
    pub fn data_validations(&self) -> &'a [DataValidation] {
        self.data_validations
    }

    /// Conditional-formatting rules.
    pub fn conditional_formats(&self) -> &'a [CondFormat] {
        self.conditional_formats
    }

    /// Granular worksheet-protection allowances, when supplied.
    pub fn protection_options(&self) -> Option<ProtectionOptions> {
        self.protection_options
    }

    /// Page setup metadata.
    pub fn page_setup(&self) -> Option<&'a PageSetup> {
        self.page_setup
    }

    /// Worksheet view metadata.
    pub fn sheet_view(&self) -> SheetView {
        self.sheet_view
    }

    /// Autofilter range.
    pub fn autofilter_range(&self) -> Option<(u32, u16, u32, u16)> {
        self.autofilter_range
    }

    /// Worksheet tab color.
    pub fn tab_color(&self) -> Option<Color> {
        self.tab_color
    }

    /// Whether printed pages include worksheet gridlines.
    pub fn print_gridlines(&self) -> bool {
        self.print_gridlines
    }

    /// Whether printed pages include row and column headings.
    pub fn print_headings(&self) -> bool {
        self.print_headings
    }

    /// Row outline levels keyed by 0-based row index.
    pub fn row_outline_levels(&self) -> &'a BTreeMap<u32, u8> {
        self.row_outline_levels
    }

    /// Column outline levels keyed by 0-based column index.
    pub fn col_outline_levels(&self) -> &'a BTreeMap<u16, u8> {
        self.col_outline_levels
    }

    /// Rows marked as collapsed outline summary rows.
    pub fn collapsed_rows(&self) -> &'a BTreeSet<u32> {
        self.collapsed_rows
    }

    /// Whether outline summary rows appear below grouped detail rows.
    pub fn outline_summary_below(&self) -> bool {
        self.outline_summary_below
    }

    /// Whether outline summary columns appear to the right of grouped detail columns.
    pub fn outline_summary_right(&self) -> bool {
        self.outline_summary_right
    }

    /// Embedded images.
    pub fn images(&self) -> &'a [Image] {
        self.images
    }

    /// Charts.
    pub fn charts(&self) -> &'a [Chart] {
        self.charts
    }

    /// Sparklines.
    pub fn sparklines(&self) -> &'a [Sparkline] {
        self.sparklines
    }

    /// Used cell dimensions as a typed inclusive rectangle.
    pub fn dimensions_info(&self) -> Option<Dimensions> {
        self.dimensions.map(Dimensions::from_range_tuple)
    }
}

/// One worksheet: a name, its non-empty cells, and layout/structure (authoring).
#[derive(Debug, Clone)]
pub struct Sheet {
    /// Sheet name as stored in the workbook globals (`BOUNDSHEET`).
    pub name: String,
    /// Whether this is an actual worksheet (vs. a chart/macro sheet).
    pub is_worksheet: bool,
    /// Parsed sheet type for metadata when the source format exposes it.
    pub(crate) sheet_type: Option<SheetType>,
    pub(crate) cells: Vec<CellEntry>,
    /// Per-column widths in character units, populated by readers and authoring.
    pub(crate) col_widths: BTreeMap<u16, f32>,
    /// Per-row heights in points, populated by readers and authoring.
    pub(crate) row_heights: BTreeMap<u32, f32>,
    /// Explicitly hidden columns.
    pub(crate) hidden_cols: BTreeSet<u16>,
    /// Explicitly hidden rows.
    pub(crate) hidden_rows: BTreeSet<u32>,
    /// Per-column default formats (authoring).
    pub(crate) col_formats: BTreeMap<u16, CellStyle>,
    /// Per-row default formats (authoring).
    pub(crate) row_formats: BTreeMap<u32, CellStyle>,
    /// Worksheet default format (authoring), applied below column/row/cell formats.
    pub(crate) default_format: Option<CellStyle>,
    /// Format-only blank cells (authoring), separate from typed reader cells.
    pub(crate) blank_styles: BTreeMap<(u32, u16), CellStyle>,
    /// Default row height in points (authoring); `<sheetFormatPr defaultRowHeight>`.
    pub(crate) default_row_height: Option<f32>,
    /// Default column width in character units (authoring).
    pub(crate) default_col_width: Option<f32>,
    /// Merged ranges `(r0, c0, r1, c1)` set when **authoring** (via
    /// [`Sheet::merge`]). The writer emits these as `<mergeCells>` and omits
    /// cells under them for OOXML conformance.
    pub(crate) merges: Vec<(u32, u16, u32, u16)>,
    /// Merged ranges discovered when **reading** a file (`.xls MERGECELLS` /
    /// `.xlsx <mergeCells>`). Kept separate from [`Self::merges`] so surfacing
    /// them via [`Sheet::merged_ranges`] never makes the writer drop the source's
    /// cells on a read→write — extraction stays full-fidelity.
    pub(crate) read_merges: Vec<(u32, u16, u32, u16)>,
    /// External hyperlinks discovered when **reading** a file (for example `.xlsx`
    /// worksheet rels or `.ods` `text:a` links). Each entry is `(row, col, url)`,
    /// 0-based. Kept separate from the per-cell authoring [`CellEntry::hyperlink`]
    /// used by the writer so surfacing them via [`Sheet::hyperlinks`] never
    /// disturbs authoring state.
    pub(crate) read_hyperlinks: Vec<(u32, u16, String)>,
    /// Frozen panes split at `(row, col)` (authoring).
    pub(crate) freeze: Option<(u32, u16)>,
    /// Autofilter range `(r0, c0, r1, c1)` (authoring).
    pub(crate) autofilter: Option<(u32, u16, u32, u16)>,
    /// Print/page setup (authoring).
    pub(crate) page_setup: Option<PageSetup>,
    /// Sheet tab color (authoring).
    pub(crate) tab_color: Option<Color>,
    /// Worksheet protection (authoring): lock cells against editing.
    pub(crate) protect: bool,
    /// Granular protection allowances (authoring); only consulted when
    /// [`Self::protect`] is set. `None` = lock everything (the `protect()`
    /// default).
    pub(crate) protect_options: Option<ProtectionOptions>,
    /// Data validations (authoring): dropdowns / numeric constraints.
    pub(crate) data_validations: Vec<DataValidation>,
    /// Conditional formats (authoring).
    pub(crate) cond_formats: Vec<CondFormat>,
    /// Embedded images (authoring).
    pub(crate) images: Vec<Image>,
    /// Charts (authoring).
    pub(crate) charts: Vec<Chart>,
    /// Sparklines (authoring): compact in-cell charts emitted as x14 worksheet
    /// extensions.
    pub(crate) sparklines: Vec<Sparkline>,
    /// Worksheet tables (authoring).
    pub(crate) tables: Vec<Table>,
    /// Per-table header row formats keyed by the authored table name.
    pub(crate) table_header_formats: BTreeMap<String, CellStyle>,
    /// Legacy cell comments / notes (authoring).
    pub(crate) comments: Vec<Comment>,
    /// Rich (mixed-format) string cells (authoring): coordinate → runs. Emitted as
    /// an inline rich string; the plain concatenation also lives in `cells` so
    /// position/merge logic and readers see a value.
    pub(crate) rich: BTreeMap<(u32, u16), Vec<TextRun>>,
    /// Hide the worksheet gridlines (authoring).
    pub(crate) hide_gridlines: bool,
    /// Sheet zoom as a percentage, e.g. `150` (authoring).
    pub(crate) zoom: Option<u16>,
    /// Show the row and column headers in the sheet view (authoring). `None`
    /// leaves Excel's default (shown); `Some(false)` emits
    /// `<sheetView showRowColHeaders="0">`.
    pub(crate) show_headers: Option<bool>,
    /// Lay the sheet out right-to-left (authoring): `<sheetView rightToLeft="1">`.
    pub(crate) right_to_left: bool,
    /// Hide this worksheet in the workbook: `<sheet state="hidden">`. Set when
    /// authoring (via [`Sheet::hide`]) and on read from every format, so a
    /// read→write round-trip preserves visibility. Surfaced by [`Sheet::is_hidden`].
    pub(crate) hidden: bool,
    /// The worksheet is *very* hidden (`<sheet state="veryHidden">`) — only
    /// unhideable via VBA, not the Excel UI. Set when authoring via
    /// [`Sheet::hide_very`], populated by the reader (`.xlsx` `state`,
    /// `.xls`/`.xlsb` `hsState == 2`), and surfaced by
    /// [`Sheet::is_very_hidden`].
    pub(crate) very_hidden: bool,
    /// Auto-size column widths from cell text on write (authoring).
    pub(crate) autofit: bool,
    /// Row outline (grouping) levels (authoring): row → outline depth.
    pub(crate) row_outline: BTreeMap<u32, u8>,
    /// Column outline (grouping) levels (authoring): col → outline depth.
    pub(crate) col_outline: BTreeMap<u16, u8>,
    /// Print the gridlines on the printed page (authoring).
    pub(crate) print_gridlines: bool,
    /// Print the row and column headings on the printed page (authoring).
    pub(crate) print_headings: bool,
    /// Outline summary rows appear *below* the grouped detail rows (authoring);
    /// the Excel default. When `false`, emits `<outlinePr summaryBelow="0"/>`.
    pub(crate) outline_summary_below: bool,
    /// Outline summary columns appear to the *right* of the grouped detail
    /// columns (authoring); the Excel default. When `false`, emits
    /// `<outlinePr summaryRight="0"/>`.
    pub(crate) outline_summary_right: bool,
    /// Rows whose group is collapsed (authoring): the summary row stays visible
    /// (`<row collapsed="1" hidden="1">`) while its detail rows are hidden.
    pub(crate) collapsed_rows: BTreeSet<u32>,
}

impl Default for Sheet {
    fn default() -> Self {
        Sheet {
            name: String::default(),
            is_worksheet: bool::default(),
            sheet_type: None,
            cells: Vec::default(),
            col_widths: BTreeMap::default(),
            row_heights: BTreeMap::default(),
            hidden_cols: BTreeSet::default(),
            hidden_rows: BTreeSet::default(),
            col_formats: BTreeMap::default(),
            row_formats: BTreeMap::default(),
            default_format: None,
            blank_styles: BTreeMap::default(),
            default_row_height: None,
            default_col_width: None,
            merges: Vec::default(),
            read_merges: Vec::default(),
            read_hyperlinks: Vec::default(),
            freeze: None,
            autofilter: None,
            page_setup: None,
            tab_color: None,
            protect: bool::default(),
            protect_options: None,
            data_validations: Vec::default(),
            cond_formats: Vec::default(),
            images: Vec::default(),
            charts: Vec::default(),
            sparklines: Vec::default(),
            tables: Vec::default(),
            table_header_formats: BTreeMap::default(),
            comments: Vec::default(),
            rich: BTreeMap::default(),
            hide_gridlines: bool::default(),
            zoom: None,
            show_headers: None,
            right_to_left: bool::default(),
            hidden: bool::default(),
            very_hidden: bool::default(),
            autofit: bool::default(),
            row_outline: BTreeMap::default(),
            col_outline: BTreeMap::default(),
            print_gridlines: bool::default(),
            print_headings: bool::default(),
            // Excel's outline defaults: summaries below/right of the detail.
            outline_summary_below: true,
            outline_summary_right: true,
            collapsed_rows: BTreeSet::default(),
        }
    }
}

/// A legacy cell comment / note (authoring) — the yellow pop-up note anchored to
/// a cell, emitted as `xl/comments{N}.xml` plus a VML shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// Anchor cell row (0-based).
    pub row: u32,
    /// Anchor cell column (0-based).
    pub col: u16,
    /// Note body text.
    pub text: String,
    /// Optional author; defaults to a blank author when `None`.
    pub author: Option<String>,
}

/// Author input for [`Sheet::add_comment`].
///
/// Existing `Some("author")` and `None` calls are supported, and passing a
/// direct `String` or `&str` stores that value as the comment author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentAuthor(Option<String>);

impl From<Option<&str>> for CommentAuthor {
    fn from(author: Option<&str>) -> Self {
        Self(author.map(str::to_string))
    }
}

impl From<&str> for CommentAuthor {
    fn from(author: &str) -> Self {
        Self(Some(author.to_string()))
    }
}

impl From<&String> for CommentAuthor {
    fn from(author: &String) -> Self {
        Self(Some(author.to_string()))
    }
}

impl From<String> for CommentAuthor {
    fn from(author: String) -> Self {
        Self(Some(author))
    }
}

/// A worksheet table (authoring) — a styled, autofiltered range with named
/// header columns (the OOXML `<table>` feature).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// Range `(r0, c0, r1, c1)` (0-based, inclusive); the first row is the header.
    pub range: (u32, u16, u32, u16),
    /// Table name (must be unique + a valid Excel name; sanitized on emit).
    pub name: String,
    /// Header column names (left→right); must match the header row width.
    pub columns: Vec<String>,
    /// Table style name (default `TableStyleMedium2`).
    pub style: Option<String>,
}

impl Table {
    /// Construct a worksheet table over `range` with a name and header columns.
    pub fn new<I, S>(range: (u32, u16, u32, u16), name: impl AsRef<str>, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Table {
            range,
            name: name.as_ref().to_string(),
            columns: columns
                .into_iter()
                .map(|column| column.as_ref().to_string())
                .collect(),
            style: None,
        }
    }

    /// Set the table style name.
    pub fn with_style(mut self, style: impl AsRef<str>) -> Self {
        self.style = Some(style.as_ref().to_string());
        self
    }

    /// Table name.
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// Header column names from the table definition.
    pub fn columns(&self) -> &[String] {
        self.columns.as_slice()
    }

    /// Inclusive table range `(first_row, first_col, last_row, last_col)`.
    ///
    /// The first row is the table header row.
    pub fn range(&self) -> (u32, u16, u32, u16) {
        self.range
    }

    /// Build a borrowed range over this table's data body in `sheet`.
    ///
    /// The returned range excludes the table header row, matching calamine's
    /// table-data surface while preserving rxls' borrowed sparse range model.
    pub fn data<'a>(&self, sheet: &'a Sheet) -> Range<'a> {
        table_data_range(sheet, self)
    }
}

/// An embedded image (authoring). The bytes are stored as-is (no decoding); the
/// image is anchored to a cell box and scaled to fit it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Raw image bytes.
    pub data: Vec<u8>,
    /// Image format (selects the media extension + content type).
    pub format: ImageFmt,
    /// Top-left anchor cell `(row, col)`, 0-based.
    pub from: (u32, u16),
    /// Bottom-right anchor cell `(row, col)`; defaults to a small box if `None`.
    pub to: Option<(u32, u16)>,
}

impl Image {
    /// Construct an embedded image anchored at `from`.
    pub fn new(data: impl Into<Vec<u8>>, format: ImageFmt, from: (u32, u16)) -> Self {
        Image {
            data: data.into(),
            format,
            from,
            to: None,
        }
    }

    /// Set the bottom-right anchor cell for this image.
    pub fn with_to(mut self, to: (u32, u16)) -> Self {
        self.to = Some(to);
        self
    }
}

/// Workbook-level embedded picture metadata for calamine-style read facades.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    /// Top-left anchor row, 0-based.
    pub row: u32,
    /// Top-left anchor column, 0-based.
    pub col: u32,
    /// Worksheet name that owns the picture.
    pub sheet_name: String,
    /// Media extension such as `png` or `jpg`.
    pub extension: String,
    /// Raw image bytes.
    pub data: Vec<u8>,
    /// Drawing object name when available; currently empty for rxls-owned images.
    pub name: String,
}

/// Image format for an embedded [`Image`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFmt {
    /// PNG.
    Png,
    /// JPEG.
    Jpeg,
}

/// Sparkline kind for an in-cell mini chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SparklineKind {
    /// Line sparkline.
    Line,
    /// Column sparkline.
    Column,
    /// Win/loss sparkline (OOXML `stacked`).
    WinLoss,
}

/// A sparkline (authoring): an in-cell mini chart that summarizes a source
/// range. The range is an A1 reference such as `Sheet1!$A$1:$A$12`; `location`
/// is the destination cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Sparkline {
    /// Destination cell `(row, col)`, 0-based.
    pub location: (u32, u16),
    /// Source data range, e.g. `Sheet1!$A$1:$A$12`.
    pub range: String,
    /// Sparkline visual type.
    pub kind: SparklineKind,
}

impl Sparkline {
    /// Construct a line sparkline anchored at `location` over `range`.
    pub fn new(location: (u32, u16), range: impl AsRef<str>) -> Self {
        Sparkline {
            location,
            range: range.as_ref().to_string(),
            kind: SparklineKind::Line,
        }
    }

    /// Set the sparkline visual kind.
    pub fn with_kind(mut self, kind: SparklineKind) -> Self {
        self.kind = kind;
        self
    }
}

/// A chart anchored to a cell box, plotting one or more data series from
/// worksheet ranges. Used for authoring and populated by readers that surface
/// chart metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chart {
    /// Chart kind.
    pub kind: ChartKind,
    /// Optional title.
    pub title: Option<String>,
    /// Data series.
    pub series: Vec<Series>,
    /// Show a legend (to the right of the plot).
    pub legend: bool,
    /// Show data-value labels on the series points.
    pub data_labels: bool,
    /// Optional category (X) axis title.
    pub x_axis_title: Option<String>,
    /// Optional value (Y) axis title.
    pub y_axis_title: Option<String>,
    /// Top-left anchor cell `(row, col)`, 0-based.
    pub from: (u32, u16),
    /// Bottom-right anchor cell `(row, col)`.
    pub to: (u32, u16),
}

impl Chart {
    /// Construct an empty chart anchored to a worksheet cell box.
    pub fn new(kind: ChartKind, from: (u32, u16), to: (u32, u16)) -> Self {
        Chart {
            kind,
            title: None,
            series: Vec::new(),
            legend: false,
            data_labels: false,
            x_axis_title: None,
            y_axis_title: None,
            from,
            to,
        }
    }

    /// Set the chart title.
    pub fn with_title(mut self, title: impl AsRef<str>) -> Self {
        self.title = Some(title.as_ref().to_string());
        self
    }

    /// Set the category/X-axis title.
    pub fn with_x_axis_title(mut self, title: impl AsRef<str>) -> Self {
        self.x_axis_title = Some(title.as_ref().to_string());
        self
    }

    /// Set the value/Y-axis title.
    pub fn with_y_axis_title(mut self, title: impl AsRef<str>) -> Self {
        self.y_axis_title = Some(title.as_ref().to_string());
        self
    }

    /// Show or hide the chart legend.
    pub fn with_legend(mut self, show: bool) -> Self {
        self.legend = show;
        self
    }

    /// Show or hide point value labels.
    pub fn with_data_labels(mut self, show: bool) -> Self {
        self.data_labels = show;
        self
    }

    /// Replace the chart series collection.
    pub fn with_series<I>(mut self, series: I) -> Self
    where
        I: IntoIterator<Item = Series>,
    {
        self.series = series.into_iter().collect();
        self
    }

    /// Append one chart series.
    pub fn add_series(mut self, series: Series) -> Self {
        self.series.push(series);
        self
    }
}

/// Chart kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind {
    /// Clustered column/bar chart.
    Bar,
    /// Line chart.
    Line,
    /// Pie chart.
    Pie,
    /// Scatter (XY) chart.
    Scatter,
    /// Area chart.
    Area,
    /// Doughnut chart.
    Doughnut,
    /// Radar chart.
    Radar,
    /// Bubble chart.
    Bubble,
}

/// One chart data series. Ranges are A1 references into a sheet, e.g.
/// `Sheet1!$B$2:$B$9`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series {
    /// Optional series name.
    pub name: Option<String>,
    /// Category (X) axis range (e.g. labels), or `None` for 1..N.
    pub categories: Option<String>,
    /// Value (Y) axis range.
    pub values: String,
    /// Bubble size range for bubble charts.
    pub bubble_sizes: Option<String>,
}

impl Series {
    /// Construct a chart data series with a value range.
    pub fn new(values: impl AsRef<str>) -> Self {
        Series {
            name: None,
            categories: None,
            values: values.as_ref().to_string(),
            bubble_sizes: None,
        }
    }

    /// Set the series display name.
    pub fn with_name(mut self, name: impl AsRef<str>) -> Self {
        self.name = Some(name.as_ref().to_string());
        self
    }

    /// Set the category/X-axis source range.
    pub fn with_categories(mut self, categories: impl AsRef<str>) -> Self {
        self.categories = Some(categories.as_ref().to_string());
        self
    }

    /// Set the bubble-size source range for bubble charts.
    pub fn with_bubble_sizes(mut self, bubble_sizes: impl AsRef<str>) -> Self {
        self.bubble_sizes = Some(bubble_sizes.as_ref().to_string());
        self
    }
}

/// A conditional-formatting rule applied to a cell range (authoring).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CondFormat {
    /// Target range `(r0, c0, r1, c1)` (0-based, inclusive).
    pub sqref: (u32, u16, u32, u16),
    /// The rule.
    pub rule: CfRule,
}

impl CondFormat {
    /// Construct a conditional-formatting rule over a target range.
    pub fn new(sqref: (u32, u16, u32, u16), rule: CfRule) -> Self {
        CondFormat { sqref, rule }
    }
}

/// A conditional-formatting rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CfRule {
    /// Highlight cells whose value satisfies `op formula1 [formula2]`, with `fill`.
    CellIs {
        /// Comparison operator.
        op: DvOp,
        /// First operand.
        formula1: String,
        /// Second operand (for `Between`/`NotBetween`).
        formula2: Option<String>,
        /// Highlight fill color.
        fill: Color,
    },
    /// Two-color scale from `min` (lowest) to `max` (highest).
    ColorScale2 {
        /// Color at the minimum.
        min: Color,
        /// Color at the maximum.
        max: Color,
    },
    /// Three-color scale `min` → `mid` (50th pct) → `max`.
    ColorScale3 {
        /// Color at the minimum.
        min: Color,
        /// Color at the midpoint.
        mid: Color,
        /// Color at the maximum.
        max: Color,
    },
    /// Gradient data bar in `color`.
    DataBar {
        /// Bar color.
        color: Color,
    },
    /// Highlight the top or bottom `rank` cells (or `rank` percent) in the range.
    TopBottom {
        /// How many cells (top/bottom N), or the percentile when `percent`.
        rank: u32,
        /// `true` selects the bottom, `false` the top.
        bottom: bool,
        /// Interpret `rank` as a percentage rather than a count.
        percent: bool,
        /// Highlight fill.
        fill: Color,
    },
    /// Highlight cells above (or below) the range's average.
    AboveAverage {
        /// `true` selects below-average cells, `false` above-average.
        below: bool,
        /// Highlight fill.
        fill: Color,
    },
    /// Highlight duplicate (or unique) values in the range.
    DuplicateValues {
        /// `true` highlights unique values instead of duplicates.
        unique: bool,
        /// Highlight fill.
        fill: Color,
    },
    /// Highlight cells where a custom `formula` evaluates to true.
    Expression {
        /// The condition formula (e.g. `$A1>100`).
        formula: String,
        /// Highlight fill.
        fill: Color,
    },
}

impl CfRule {
    /// Highlight cells whose value satisfies `op formula1 [formula2]`.
    pub fn cell_is(
        op: DvOp,
        formula1: impl AsRef<str>,
        formula2: Option<impl AsRef<str>>,
        fill: impl Into<Color>,
    ) -> Self {
        CfRule::CellIs {
            op,
            formula1: formula1.as_ref().to_string(),
            formula2: formula2.map(|formula| formula.as_ref().to_string()),
            fill: fill.into(),
        }
    }

    /// Build a two-color scale rule.
    pub fn color_scale2(min: impl Into<Color>, max: impl Into<Color>) -> Self {
        CfRule::ColorScale2 {
            min: min.into(),
            max: max.into(),
        }
    }

    /// Build a three-color scale rule.
    pub fn color_scale3(
        min: impl Into<Color>,
        mid: impl Into<Color>,
        max: impl Into<Color>,
    ) -> Self {
        CfRule::ColorScale3 {
            min: min.into(),
            mid: mid.into(),
            max: max.into(),
        }
    }

    /// Build a data-bar rule.
    pub fn data_bar(color: impl Into<Color>) -> Self {
        CfRule::DataBar {
            color: color.into(),
        }
    }

    /// Highlight the top or bottom ranked values in a range.
    pub fn top_bottom(rank: u32, bottom: bool, percent: bool, fill: impl Into<Color>) -> Self {
        CfRule::TopBottom {
            rank,
            bottom,
            percent,
            fill: fill.into(),
        }
    }

    /// Highlight cells above or below the range average.
    pub fn above_average(below: bool, fill: impl Into<Color>) -> Self {
        CfRule::AboveAverage {
            below,
            fill: fill.into(),
        }
    }

    /// Highlight duplicate or unique values.
    pub fn duplicate_values(unique: bool, fill: impl Into<Color>) -> Self {
        CfRule::DuplicateValues {
            unique,
            fill: fill.into(),
        }
    }

    /// Highlight cells where a custom formula evaluates to true.
    pub fn expression(formula: impl AsRef<str>, fill: impl Into<Color>) -> Self {
        CfRule::Expression {
            formula: formula.as_ref().to_string(),
            fill: fill.into(),
        }
    }
}

/// A data validation rule (authoring) — a dropdown list or a numeric/date/text
/// constraint applied to a cell range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataValidation {
    /// Target range `(r0, c0, r1, c1)` (0-based, inclusive).
    pub sqref: (u32, u16, u32, u16),
    /// Validation kind.
    pub kind: DvKind,
    /// Comparison operator (ignored for [`DvKind::List`]).
    pub operator: DvOp,
    /// First formula/operand — for a list, a quoted CSV (`"a,b,c"`) or a range
    /// (`$A$1:$A$9`); for numeric/date kinds, the bound.
    pub formula1: String,
    /// Second operand (for `Between`/`NotBetween`).
    pub formula2: Option<String>,
    /// Allow an empty cell (default `true`).
    pub allow_blank: bool,
    /// Show the optional input prompt when the cell is selected.
    pub show_input_message: bool,
    /// Show the optional error alert when invalid data is entered.
    pub show_error_message: bool,
    /// Optional input-prompt `(title, message)`.
    pub prompt: Option<(String, String)>,
    /// Optional error-alert `(title, message)`.
    pub error: Option<(String, String)>,
}

impl DataValidation {
    /// Construct a data-validation rule over `sqref`.
    pub fn new(
        sqref: (u32, u16, u32, u16),
        kind: DvKind,
        operator: DvOp,
        formula1: impl AsRef<str>,
    ) -> Self {
        DataValidation {
            sqref,
            kind,
            operator,
            formula1: formula1.as_ref().to_string(),
            formula2: None,
            allow_blank: true,
            show_input_message: true,
            show_error_message: true,
            prompt: None,
            error: None,
        }
    }

    /// A dropdown list over `sqref` from a quoted CSV (`"가,나,다"`) or a range.
    pub fn list(sqref: (u32, u16, u32, u16), source: impl AsRef<str>) -> Self {
        DataValidation::new(sqref, DvKind::List, DvOp::Between, source)
    }

    /// Set the second formula/operand.
    pub fn with_formula2(mut self, formula2: impl AsRef<str>) -> Self {
        self.formula2 = Some(formula2.as_ref().to_string());
        self
    }

    /// Set whether blank cells are allowed.
    pub fn with_allow_blank(mut self, allow_blank: bool) -> Self {
        self.allow_blank = allow_blank;
        self
    }

    /// Set the input prompt shown when the cell is selected.
    pub fn with_prompt(mut self, title: impl AsRef<str>, message: impl AsRef<str>) -> Self {
        self.show_input_message = true;
        self.prompt = Some((title.as_ref().to_string(), message.as_ref().to_string()));
        self
    }

    /// Set the error alert shown when invalid data is entered.
    pub fn with_error(mut self, title: impl AsRef<str>, message: impl AsRef<str>) -> Self {
        self.show_error_message = true;
        self.error = Some((title.as_ref().to_string(), message.as_ref().to_string()));
        self
    }
}

/// Data-validation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DvKind {
    /// Dropdown list.
    List,
    /// Whole number.
    Whole,
    /// Decimal number.
    Decimal,
    /// Date.
    Date,
    /// Time.
    Time,
    /// Text length.
    TextLength,
    /// Custom: `formula1` is a boolean expression that must hold (the operator is
    /// ignored).
    Custom,
}

/// Data-validation comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DvOp {
    /// `formula1 ≤ x ≤ formula2`.
    Between,
    /// Outside `[formula1, formula2]`.
    NotBetween,
    /// `x == formula1`.
    Equal,
    /// `x != formula1`.
    NotEqual,
    /// `x > formula1`.
    GreaterThan,
    /// `x < formula1`.
    LessThan,
    /// `x ≥ formula1`.
    GreaterThanOrEqual,
    /// `x ≤ formula1`.
    LessThanOrEqual,
}

/// Granular worksheet-protection allowances (authoring). Each field, when
/// `true`, *permits* the corresponding action even while the sheet is
/// protected; the [`Default`] (all `false`) locks everything, matching
/// [`Sheet::protect`]. Pass to [`Sheet::protect_with`].
///
/// In the OOXML `<sheetProtection>` element these map to attributes whose
/// `"1"`/absent value means *not allowed* — so an allowed action is emitted as
/// `attr="0"` (e.g. `sort="0"` allows sorting).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ProtectionOptions {
    /// Allow sorting (`sort="0"`).
    pub sort: bool,
    /// Allow using AutoFilter dropdowns (`autoFilter="0"`).
    pub auto_filter: bool,
    /// Allow formatting cells (`formatCells="0"`).
    pub format_cells: bool,
    /// Allow formatting columns (`formatColumns="0"`).
    pub format_columns: bool,
    /// Allow formatting rows (`formatRows="0"`).
    pub format_rows: bool,
    /// Allow inserting columns (`insertColumns="0"`).
    pub insert_columns: bool,
    /// Allow inserting rows (`insertRows="0"`).
    pub insert_rows: bool,
    /// Allow inserting hyperlinks (`insertHyperlinks="0"`).
    pub insert_hyperlinks: bool,
    /// Allow deleting columns (`deleteColumns="0"`).
    pub delete_columns: bool,
    /// Allow deleting rows (`deleteRows="0"`).
    pub delete_rows: bool,
    /// Allow editing pivot tables (`pivotTables="0"`).
    pub pivot_tables: bool,
}

impl ProtectionOptions {
    /// Construct protection options that lock every protected action.
    pub fn new() -> Self {
        ProtectionOptions::default()
    }

    /// Allow sorting while the worksheet is protected.
    pub fn allow_sort(mut self) -> Self {
        self.sort = true;
        self
    }

    /// Allow using AutoFilter dropdowns while the worksheet is protected.
    pub fn allow_auto_filter(mut self) -> Self {
        self.auto_filter = true;
        self
    }

    /// Allow formatting cells while the worksheet is protected.
    pub fn allow_format_cells(mut self) -> Self {
        self.format_cells = true;
        self
    }

    /// Allow formatting columns while the worksheet is protected.
    pub fn allow_format_columns(mut self) -> Self {
        self.format_columns = true;
        self
    }

    /// Allow formatting rows while the worksheet is protected.
    pub fn allow_format_rows(mut self) -> Self {
        self.format_rows = true;
        self
    }

    /// Allow inserting columns while the worksheet is protected.
    pub fn allow_insert_columns(mut self) -> Self {
        self.insert_columns = true;
        self
    }

    /// Allow inserting rows while the worksheet is protected.
    pub fn allow_insert_rows(mut self) -> Self {
        self.insert_rows = true;
        self
    }

    /// Allow inserting hyperlinks while the worksheet is protected.
    pub fn allow_insert_hyperlinks(mut self) -> Self {
        self.insert_hyperlinks = true;
        self
    }

    /// Allow deleting columns while the worksheet is protected.
    pub fn allow_delete_columns(mut self) -> Self {
        self.delete_columns = true;
        self
    }

    /// Allow deleting rows while the worksheet is protected.
    pub fn allow_delete_rows(mut self) -> Self {
        self.delete_rows = true;
        self
    }

    /// Allow editing pivot tables while the worksheet is protected.
    pub fn allow_pivot_tables(mut self) -> Self {
        self.pivot_tables = true;
        self
    }
}

/// Print / page setup for a worksheet (authoring). All fields optional; an
/// unset field uses Excel's default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageSetup {
    /// Landscape orientation (default portrait).
    pub landscape: bool,
    /// Page margins in inches: `(left, right, top, bottom, header, footer)`.
    pub margins: Option<(f64, f64, f64, f64, f64, f64)>,
    /// Print area `(r0, c0, r1, c1)` (0-based, inclusive).
    pub print_area: Option<(u32, u16, u32, u16)>,
    /// Rows to repeat at the top of every printed page, `first..=last` (0-based).
    pub repeat_rows: Option<(u32, u32)>,
    /// Columns to repeat at the left of every printed page, `first..=last`
    /// (0-based).
    pub repeat_cols: Option<(u16, u16)>,
    /// Scale to fit this many pages wide (`fit_to_width`) / tall (`fit_to_height`).
    pub fit_to_width: Option<u16>,
    /// See [`Self::fit_to_width`].
    pub fit_to_height: Option<u16>,
    /// Header text (Excel `&`-codes, e.g. `&C&"Bold"Title`).
    pub header: Option<String>,
    /// Footer text (e.g. `&CPage &P of &N`).
    pub footer: Option<String>,
    /// Paper size code (Excel's `paperSize`, e.g. `1` = Letter, `9` = A4).
    pub paper_size: Option<u16>,
    /// Print scale as a percentage (10–400); ignored when fit-to-page is set.
    pub scale: Option<u16>,
    /// Center the print area horizontally on the page (`<printOptions
    /// horizontalCentered="1">`).
    pub center_horizontally: bool,
    /// Center the print area vertically on the page (`<printOptions
    /// verticalCentered="1">`).
    pub center_vertically: bool,
    /// First printed page number; emits `firstPageNumber="N" useFirstPageNumber="1"`
    /// on `<pageSetup>`. `None` uses Excel's default (auto).
    pub first_page_number: Option<u16>,
}

impl PageSetup {
    /// Construct page setup metadata using Excel defaults.
    pub fn new() -> Self {
        PageSetup::default()
    }

    /// Set landscape orientation.
    pub fn with_landscape(mut self) -> Self {
        self.landscape = true;
        self
    }

    /// Set page margins in inches: left, right, top, bottom, header, footer.
    pub fn with_margins(
        mut self,
        left: f64,
        right: f64,
        top: f64,
        bottom: f64,
        header: f64,
        footer: f64,
    ) -> Self {
        self.margins = Some((left, right, top, bottom, header, footer));
        self
    }

    /// Set the print area `(first_row, first_col, last_row, last_col)`.
    pub fn with_print_area(mut self, range: (u32, u16, u32, u16)) -> Self {
        self.print_area = Some(range);
        self
    }

    /// Set rows repeated at the top of every printed page.
    pub fn with_repeat_rows(mut self, first: u32, last: u32) -> Self {
        self.repeat_rows = Some((first, last));
        self
    }

    /// Set columns repeated at the left of every printed page.
    pub fn with_repeat_cols(mut self, first: u16, last: u16) -> Self {
        self.repeat_cols = Some((first, last));
        self
    }

    /// Scale output to fit this many pages wide and tall.
    pub fn with_fit_to_pages(mut self, width: u16, height: u16) -> Self {
        self.fit_to_width = Some(width);
        self.fit_to_height = Some(height);
        self
    }

    /// Set header text.
    pub fn with_header(mut self, header: impl AsRef<str>) -> Self {
        self.header = Some(header.as_ref().to_string());
        self
    }

    /// Set footer text.
    pub fn with_footer(mut self, footer: impl AsRef<str>) -> Self {
        self.footer = Some(footer.as_ref().to_string());
        self
    }

    /// Set the Excel paper size code.
    pub fn with_paper_size(mut self, paper_size: u16) -> Self {
        self.paper_size = Some(paper_size);
        self
    }

    /// Set print scaling percentage.
    pub fn with_scale(mut self, scale: u16) -> Self {
        self.scale = Some(scale);
        self
    }

    /// Center the print area horizontally on the page.
    pub fn with_center_horizontally(mut self, center: bool) -> Self {
        self.center_horizontally = center;
        self
    }

    /// Center the print area vertically on the page.
    pub fn with_center_vertically(mut self, center: bool) -> Self {
        self.center_vertically = center;
        self
    }

    /// Set the first printed page number.
    pub fn with_first_page_number(mut self, first_page_number: u16) -> Self {
        self.first_page_number = Some(first_page_number);
        self
    }
}

impl Sheet {
    /// Flatten this sheet to text: rows sorted top-to-bottom, cells tab-joined
    /// left-to-right.
    pub fn to_text(&self) -> String {
        // Last-write-wins per (row, col) — agree with `cell()`/`rows()` and Excel,
        // rather than emitting a re-written coordinate twice. A BTreeMap keyed by
        // coordinate both dedups (later insert wins) and sorts.
        let mut by_coord: BTreeMap<(u32, u16), &str> = BTreeMap::new();
        for c in &self.cells {
            by_coord.insert((c.row, c.col), c.text.as_str());
        }
        let mut out = String::new();
        let mut cur_row: Option<u32> = None;
        for ((row, _col), text) in &by_coord {
            // Skip value-less cells (e.g. a formula with a blank cached result):
            // they carry identity for `cells()`/`rows()` but contribute no token.
            if text.is_empty() {
                continue;
            }
            match cur_row {
                Some(r) if r == *row => out.push('\t'),
                _ => {
                    if cur_row.is_some() {
                        out.push('\n');
                    }
                    cur_row = Some(*row);
                }
            }
            out.push_str(text);
        }
        out
    }

    /// Export non-empty worksheet rows as CSV using comma separators.
    ///
    /// Values use the same formatted display text as [`Sheet::to_text`]. Empty
    /// rows are skipped; empty cells between non-empty cells in a row are kept.
    pub fn to_csv(&self) -> String {
        self.to_csv_with_delimiter(',')
    }

    /// Export non-empty worksheet rows as delimiter-separated values.
    ///
    /// Fields are quoted when they contain the delimiter, a quote, or a line
    /// break; embedded quotes are doubled. Empty rows are skipped so sparse
    /// max-coordinate sheets do not expand into unbounded blank output.
    ///
    /// `'"'` is not a valid delimiter: quoted-field boundaries and the field
    /// separator would become the same character, making the output
    /// genuinely ambiguous to any reader. Since this method's return type
    /// cannot signal failure, a `delimiter` of `'"'` is silently normalized
    /// to the default `','` rather than emitting that ambiguous output.
    pub fn to_csv_with_delimiter(&self, delimiter: char) -> String {
        let delimiter = if delimiter == '"' { ',' } else { delimiter };
        let mut by_row: BTreeMap<u32, BTreeMap<u16, &str>> = BTreeMap::new();
        for cell in &self.cells {
            by_row
                .entry(cell.row)
                .or_default()
                .insert(cell.col, cell.text.as_str());
        }

        let mut out = String::new();
        for (_row, cols) in by_row {
            if !out.is_empty() {
                out.push('\n');
            }

            // ponytail: CSV has no row identity; keep sparse row export bounded.
            // Add a checked rectangular exporter if coordinate-faithful blanks matter.
            let mut first = true;
            let mut next_col: Option<u32> = None;
            for (col, text) in cols {
                let col = u32::from(col);
                if let Some(mut expected) = next_col {
                    while expected < col {
                        if !first {
                            out.push(delimiter);
                        }
                        first = false;
                        expected += 1;
                    }
                }
                if !first {
                    out.push(delimiter);
                }
                push_csv_field(&mut out, text, delimiter);
                first = false;
                next_col = col.checked_add(1);
            }
        }
        out
    }

    /// Export the worksheet as an HTML table fragment.
    ///
    /// The fragment contains one `<table>` and no document wrapper. Values use
    /// the same formatted display text as [`Sheet::to_text`].
    pub fn to_html(&self) -> String {
        let mut by_row: BTreeMap<u32, BTreeMap<u16, &str>> = BTreeMap::new();
        for cell in &self.cells {
            by_row
                .entry(cell.row)
                .or_default()
                .insert(cell.col, cell.text.as_str());
        }
        let merges = self.merged_ranges();

        let mut out = String::from("<table>");
        for (row, cols) in by_row {
            out.push_str("<tr>");
            // Dense 0..=max_col iteration (matching to_markdown), not just the
            // sparsely-written coordinates: a coordinate with no CellEntry in
            // the middle of the row still needs an empty <td></td> so later
            // columns don't shift left and visually land in the wrong cell.
            // This also means a merge anchor with no CellEntry of its own
            // (but a covered cell elsewhere in the same row that does) is now
            // visited as an empty-text cell, so the merge's <td rowspan=..
            // colspan=..> is still emitted instead of silently vanishing.
            let max_col = cols.keys().next_back().copied().unwrap_or(0);
            for col in 0..=max_col {
                let merge = html_merge_for_cell(merges, row, col);
                if merge.is_some_and(|merge| merge.skip) {
                    continue;
                }
                let text = cols.get(&col).copied().unwrap_or_default();
                out.push_str("<td");
                if let Some(merge) = merge {
                    if merge.rowspan > 1 {
                        out.push_str(&format!(r#" rowspan="{}""#, merge.rowspan));
                    }
                    if merge.colspan > 1 {
                        out.push_str(&format!(r#" colspan="{}""#, merge.colspan));
                    }
                }
                out.push('>');
                push_html_escaped(&mut out, text);
                out.push_str("</td>");
            }
            out.push_str("</tr>");
        }
        out.push_str("</table>");
        out
    }

    /// Export the worksheet as GitHub-flavored Markdown.
    ///
    /// Merged cells cannot be expressed losslessly in GFM, so sheets with merges
    /// fall back to the HTML fragment. Very wide sheets also fall back to HTML to
    /// keep the Markdown table bounded.
    pub fn to_markdown(&self) -> String {
        const MAX_MD_COLS: usize = 256;
        if !self.merged_ranges().is_empty() {
            return self.to_html();
        }

        let mut by_row: BTreeMap<u32, BTreeMap<u16, &str>> = BTreeMap::new();
        for cell in &self.cells {
            by_row
                .entry(cell.row)
                .or_default()
                .insert(cell.col, cell.text.as_str());
        }
        let max_col = by_row
            .values()
            .filter_map(|cols| cols.keys().next_back().copied())
            .max();
        let Some(max_col) = max_col else {
            return String::new();
        };
        let width = usize::from(max_col) + 1;
        if width > MAX_MD_COLS {
            return self.to_html();
        }

        let mut rows = Vec::new();
        for (_row, cols) in by_row {
            let mut row = Vec::with_capacity(width);
            for col in 0..=max_col {
                row.push(markdown_cell(cols.get(&col).copied().unwrap_or_default()));
            }
            rows.push(row);
        }
        if rows.is_empty() {
            return String::new();
        }

        let mut out = String::new();
        push_markdown_row(&mut out, &rows[0]);
        out.push('\n');
        let separators = vec!["---".to_string(); width];
        push_markdown_row(&mut out, &separators);
        for row in rows.iter().skip(1) {
            out.push('\n');
            push_markdown_row(&mut out, row);
        }
        out
    }

    /// Grouped worksheet-level metadata borrowed from this sheet.
    pub fn metadata(&self) -> WorksheetMetadata<'_> {
        WorksheetMetadata {
            name: &self.name,
            sheet_type: self.sheet_type(),
            visible: self.visible(),
            dimensions: self.dimensions(),
            merged_ranges: self.merged_ranges(),
            hyperlinks: self.hyperlinks(),
            comments: self.comments(),
            tables: self.tables(),
            data_validations: self.data_validations(),
            conditional_formats: self.conditional_formats(),
            protected: self.is_protected(),
            protection_options: self.protection_options(),
            page_setup: self.page_setup(),
            sheet_view: self.sheet_view(),
            autofilter_range: self.autofilter_range(),
            tab_color: self.tab_color(),
            print_gridlines: self.print_gridlines(),
            print_headings: self.print_headings(),
            row_outline_levels: self.row_outline_levels(),
            col_outline_levels: self.col_outline_levels(),
            collapsed_rows: self.collapsed_rows(),
            outline_summary_below: self.outline_summary_below(),
            outline_summary_right: self.outline_summary_right(),
            images: self.images(),
            charts: self.charts(),
            sparklines: self.sparklines(),
        }
    }

    /// Iterate the non-empty cells as `(row, col, &Cell)`, in **record order**.
    ///
    /// This is the raw cell stream and may yield the same `(row, col)` more than
    /// once if a file (or authoring code) writes a coordinate repeatedly — the
    /// later record is the effective value (Excel last-write-wins). For a
    /// deduplicated, ordered view use [`Sheet::rows`] or [`Sheet::cell`]; this
    /// method stays allocation-free by not deduplicating.
    pub fn cells(&self) -> impl Iterator<Item = (u32, u16, &Cell)> {
        self.cells.iter().map(|c| (c.row, c.col, &c.value))
    }

    /// The typed value at `(row, col)`, if that cell is non-empty. When a
    /// coordinate has multiple records the last one wins (Excel semantics).
    pub fn cell(&self, row: u32, col: u16) -> Option<&Cell> {
        self.cells
            .iter()
            .rev()
            .find(|c| c.row == row && c.col == col)
            .map(|c| &c.value)
    }

    /// The rendered **display text** at `(row, col)` — the pre-formatted string
    /// [`Sheet::to_text`] emits for that cell (e.g. `50%`, `2024-03-15`,
    /// `₩1,000`, `TRUE`), as a calamine-`formatted_value`-style accessor. This
    /// is the number-format-applied surface, whereas [`Sheet::cell`] returns the
    /// typed value (`Cell::Number(0.5)`, `Cell::Date(45366.0)`, …). Last-write-
    /// wins per coordinate, matching [`Sheet::cell`]. Returns `None` when the
    /// cell is empty.
    pub fn formatted(&self, row: u32, col: u16) -> Option<&str> {
        self.cells
            .iter()
            .rev()
            .find(|c| c.row == row && c.col == col)
            .map(|c| c.text.as_str())
    }

    /// Effective cell style at `(row, col)`, when retained by the reader or set
    /// explicitly for authoring. A format-only blank cell is also surfaced.
    pub fn cell_style(&self, row: u32, col: u16) -> Option<&CellStyle> {
        self.cells
            .iter()
            .rev()
            .find(|cell| cell.row == row && cell.col == col)
            .and_then(|cell| cell.style.as_ref())
            .or_else(|| self.blank_styles.get(&(row, col)))
    }

    /// Rich-text runs retained for a cell. Plain strings return `None`; the
    /// concatenated value remains available through [`Sheet::cell`].
    pub fn rich_text_runs(&self, row: u32, col: u16) -> Option<&[TextRun]> {
        self.rich.get(&(row, col)).map(Vec::as_slice)
    }

    /// Explicit column widths in character units, keyed by 0-based column.
    pub fn column_widths(&self) -> &BTreeMap<u16, f32> {
        &self.col_widths
    }

    /// Explicit row heights in points, keyed by 0-based row.
    pub fn row_heights(&self) -> &BTreeMap<u32, f32> {
        &self.row_heights
    }

    /// Explicitly hidden columns, as 0-based indexes.
    pub fn hidden_columns(&self) -> &BTreeSet<u16> {
        &self.hidden_cols
    }

    /// Explicitly hidden rows, as 0-based indexes.
    pub fn hidden_rows(&self) -> &BTreeSet<u32> {
        &self.hidden_rows
    }

    /// Worksheet tab color, when the source workbook or authoring model set one.
    ///
    /// Currently read from OOXML `<sheetPr><tabColor .../>` RGB, theme/tint, and
    /// indexed tab colors, XLSB/BIFF tab-color records, and ODS
    /// `style:table-properties table:tab-color`; emitted by the `.xlsx` writer
    /// when set through [`Sheet::set_tab_color`].
    pub fn tab_color(&self) -> Option<Color> {
        self.tab_color
    }

    /// Whether printed pages include worksheet gridlines.
    pub fn print_gridlines(&self) -> bool {
        self.print_gridlines
    }

    /// Whether printed pages include row and column headings.
    pub fn print_headings(&self) -> bool {
        self.print_headings
    }

    /// Row outline levels keyed by 0-based row index.
    pub fn row_outline_levels(&self) -> &BTreeMap<u32, u8> {
        &self.row_outline
    }

    /// Column outline levels keyed by 0-based column index.
    pub fn col_outline_levels(&self) -> &BTreeMap<u16, u8> {
        &self.col_outline
    }

    /// Rows marked as collapsed outline summary rows.
    pub fn collapsed_rows(&self) -> &BTreeSet<u32> {
        &self.collapsed_rows
    }

    /// Whether outline summary rows appear below grouped detail rows.
    pub fn outline_summary_below(&self) -> bool {
        self.outline_summary_below
    }

    /// Whether outline summary columns appear to the right of grouped detail columns.
    pub fn outline_summary_right(&self) -> bool {
        self.outline_summary_right
    }

    /// Merged cell ranges as `(first_row, first_col, last_row, last_col)`,
    /// 0-based and inclusive. Populated on read from `.xls` `MERGECELLS` and
    /// `.xlsx` `<mergeCells>`, or the ranges set when authoring via
    /// [`Sheet::merge`]. The merged value lives in the top-left cell.
    pub fn merged_ranges(&self) -> &[(u32, u16, u32, u16)] {
        if self.merges.is_empty() {
            &self.read_merges
        } else {
            &self.merges
        }
    }

    /// External hyperlinks read from supported spreadsheet formats, as `(row, col,
    /// url)`, 0-based. For `.xlsx`, URLs are resolved through worksheet rels
    /// (`TargetMode="External"`); for `.xls`, they come from BIFF HLINK records;
    /// for `.ods`, they come from `text:a` `xlink:href` links. Empty for files
    /// without hyperlinks. Independent of the per-cell authoring hyperlink
    /// consumed by the writer.
    pub fn hyperlinks(&self) -> &[(u32, u16, String)] {
        &self.read_hyperlinks
    }

    /// Legacy cell comments / notes anchored to cells (`.xlsx`
    /// `xl/comments{N}.xml`, `.xls` BIFF notes, `.xlsb` comments parts, or
    /// `.ods` `office:annotation`). Shares the authoring [`Comment`] storage, so
    /// a read workbook round-trips its comments on write.
    pub fn comments(&self) -> &[Comment] {
        &self.comments
    }

    /// Worksheet tables (`.xlsx` `xl/tables/table{N}.xml`, `.xlsb` binary table
    /// parts, or named `.ods` `table:database-range`) with named header columns.
    /// Shares the authoring [`Table`] storage, so a read workbook round-trips its
    /// tables on write.
    /// Each [`Table`] carries its `range` (0-based, inclusive), `name`, and header
    /// `columns`.
    pub fn tables(&self) -> &[Table] {
        &self.tables
    }

    /// Data-validation rules discovered when reading supported spreadsheet
    /// formats (`.xlsx`, `.xls`, `.xlsb`, `.ods`), or added for authoring with
    /// [`Sheet::add_data_validation`].
    pub fn data_validations(&self) -> &[DataValidation] {
        &self.data_validations
    }

    /// Conditional-formatting rules discovered when reading supported
    /// spreadsheet formats, or added for authoring with
    /// [`Sheet::add_conditional_format`].
    pub fn conditional_formats(&self) -> &[CondFormat] {
        &self.cond_formats
    }

    /// Whether worksheet protection is enabled.
    pub fn is_protected(&self) -> bool {
        self.protect
    }

    /// Granular worksheet-protection allowances, if the source or authoring
    /// model supplied any.
    pub fn protection_options(&self) -> Option<ProtectionOptions> {
        self.protect_options
    }

    /// Print/page setup discovered when reading supported spreadsheet formats,
    /// including `.xls`/`.xlsb` page setup records plus `Print_Area` /
    /// `Print_Titles` built-in name ranges, or set for authoring with
    /// [`Sheet::set_page_setup`].
    pub fn page_setup(&self) -> Option<&PageSetup> {
        self.page_setup.as_ref()
    }

    /// Worksheet view metadata discovered when reading supported spreadsheet
    /// formats, or set for authoring through the sheet-view builder methods.
    pub fn sheet_view(&self) -> SheetView {
        SheetView {
            freeze: self.freeze,
            hide_gridlines: self.hide_gridlines,
            zoom: self.zoom,
            show_headers: self.show_headers,
            right_to_left: self.right_to_left,
        }
    }

    /// Autofilter range as `(first_row, first_col, last_row, last_col)`, 0-based
    /// and inclusive, when the worksheet declares one (`.xlsx` `autoFilter` or
    /// sheet-local `_FilterDatabase`, `.xlsb` `BrtBeginAFilter`, `.xls`
    /// `_FilterDatabase`, or `.ods` `table:database-range`).
    pub fn autofilter_range(&self) -> Option<(u32, u16, u32, u16)> {
        self.autofilter
    }

    /// Embedded images (`xl/media/imageN.*` or ODS `draw:image` package parts)
    /// anchored to worksheet cells. Shares the authoring [`Image`] storage, so a
    /// read workbook round-trips its images on write.
    pub fn images(&self) -> &[Image] {
        &self.images
    }

    /// Charts anchored to worksheet cell boxes.
    /// Currently populated by the `.xlsx` reader; shares the authoring
    /// [`Chart`] storage, so a read workbook round-trips its charts on write.
    pub fn charts(&self) -> &[Chart] {
        &self.charts
    }

    /// Sparklines (`x14:sparklineGroup`) anchored to worksheet cells.
    /// Currently populated by the `.xlsx` reader; shares the authoring
    /// [`Sparkline`] storage, so a read workbook round-trips its sparklines on
    /// write.
    pub fn sparklines(&self) -> &[Sparkline] {
        &self.sparklines
    }

    /// Whether this worksheet is hidden (`<sheet state="hidden">` / `.xls`-`.xlsb`
    /// `hsState == 1`). A hidden sheet is unhideable through the Excel UI but stays
    /// in the workbook. Matches calamine's `Sheet::visible`. Read on every format.
    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// Whether this worksheet is *very* hidden (`<sheet state="veryHidden">` /
    /// `hsState == 2`) — hideable/unhideable only via VBA, never the Excel UI.
    /// A very-hidden sheet reports `false` from [`Self::is_hidden`]; the two states
    /// are distinct.
    pub fn is_very_hidden(&self) -> bool {
        self.very_hidden
    }

    /// Sheet type for metadata views.
    pub fn sheet_type(&self) -> SheetType {
        self.sheet_type.unwrap_or(if self.is_worksheet {
            SheetType::WorkSheet
        } else {
            SheetType::ChartSheet
        })
    }

    /// Sheet visibility for metadata views.
    pub fn visible(&self) -> SheetVisible {
        if self.very_hidden {
            SheetVisible::VeryHidden
        } else if self.hidden {
            SheetVisible::Hidden
        } else {
            SheetVisible::Visible
        }
    }

    /// Used range as `(min_row, min_col, max_row, max_col)` over non-empty cells.
    pub fn dimensions(&self) -> Option<(u32, u16, u32, u16)> {
        let mut it = self.cells.iter();
        let f = it.next()?;
        let (mut r0, mut c0, mut r1, mut c1) = (f.row, f.col, f.row, f.col);
        for c in it {
            r0 = r0.min(c.row);
            c0 = c0.min(c.col);
            r1 = r1.max(c.row);
            c1 = c1.max(c.col);
        }
        Some((r0, c0, r1, c1))
    }

    /// Used range dimensions as a typed inclusive rectangle.
    pub fn dimensions_info(&self) -> Option<Dimensions> {
        self.dimensions().map(Dimensions::from_range_tuple)
    }

    /// Iterate the non-empty cells grouped by row, in ascending `(row, col)`
    /// order: each item is `(row, [(col, &Cell), …])`. A calamine-`Range::rows`-
    /// style view over this crate's sparse cell model.
    pub fn rows(&self) -> impl Iterator<Item = (u32, Vec<(u16, &Cell)>)> {
        // Last-write-wins per (row, col) to agree with `cell()` and Excel — a
        // nested map overwrites duplicate coordinates instead of listing both.
        let mut by_row: BTreeMap<u32, BTreeMap<u16, &Cell>> = BTreeMap::new();
        for c in &self.cells {
            by_row.entry(c.row).or_default().insert(c.col, &c.value);
        }
        by_row
            .into_iter()
            .map(|(r, cols)| (r, cols.into_iter().collect()))
    }
}

/// A worksheet-scoped defined name retained independently from global names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDefinedName {
    /// Worksheet name that owns this local name.
    pub sheet: String,
    /// Name visible within `sheet`.
    pub name: String,
    /// Formula or reference text represented by the name.
    pub refers_to: String,
}

/// A workbook — parsed from `.xls`/`.xlsx`, or built for authoring via
/// [`Workbook::new`].
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Workbook {
    /// Sheets in workbook order.
    pub sheets: Vec<Sheet>,
    /// `true` if the workbook uses the 1904 date system (Mac Excel), which shifts
    /// how [`Cell::Date`] serials map to calendar dates.
    pub date1904: bool,
    /// `true` when a reader hit the workbook-wide text allocation cap and omitted
    /// additional cells to keep extraction bounded.
    pub text_truncated: bool,
    /// Document properties (title / author / dates …). Populated from `.xlsx`
    /// and `.xlsb` `docProps/*`, `.xls` OLE SummaryInformation streams, and
    /// `.ods` `meta.xml` on read, and written into `.xlsx` `docProps/*` when
    /// authoring. Empty fields are omitted on write.
    pub properties: DocProperties,
    /// Workbook-global defined names as `(name, refers_to)` (authoring), e.g.
    /// `("TaxRate", "Sheet1!$B$1")`. Set via [`Workbook::define_name`].
    pub defined_names: Vec<(String, String)>,
    /// Sheet-scoped defined names retained from readers and authoring.
    pub local_defined_names: Vec<LocalDefinedName>,
    /// 0-based index of the active/selected sheet (authoring), emitted as
    /// `<workbookView activeTab="N"/>` plus `tabSelected="1"` on that sheet's
    /// view. Defaults to `0`; set via [`Workbook::set_active_sheet`].
    pub(crate) active_sheet: usize,
    /// Lock the workbook structure (authoring): emit `<workbookProtection
    /// lockStructure="1"/>` so sheets cannot be added, deleted, renamed, or
    /// reordered in Excel. Set via [`Workbook::protect_structure`].
    pub(crate) protect_structure: bool,
    /// Calamine-style header row policy for workbook-level worksheet ranges.
    pub(crate) header_row: HeaderRow,
}

/// Calamine-style read facade for generic workbook consumers.
///
/// `Workbook` exposes these methods directly as inherent methods; this trait
/// adds a small compatibility layer for diagnostics and libraries that want to
/// accept any rxls reader-like workbook value without naming the concrete type.
pub trait Reader {
    /// Worksheet names in workbook order.
    fn sheet_names(&self) -> Vec<&str>;
    /// Sheet metadata in workbook order.
    fn sheets_metadata(&self) -> Vec<SheetMetadata>;
    /// Workbook-global defined names as `(name, refers_to)`.
    fn defined_names(&self) -> &[(String, String)];
    /// Worksheet-scoped defined names in workbook order.
    fn local_defined_names(&self) -> &[LocalDefinedName];
    /// Set the row used as the top of workbook-level worksheet ranges.
    fn with_header_row(&mut self, header_row: HeaderRow) -> &mut Self
    where
        Self: Sized;
    /// Current header row policy for workbook-level worksheet ranges.
    fn header_row(&self) -> HeaderRow;
    /// Build a rectangular [`Range`] view for a worksheet by name.
    fn worksheet_range(&self, name: &str) -> Option<Range<'_>>;
    /// Build a borrowed rectangular [`Range`] view for a worksheet by name.
    ///
    /// This calamine-style `ReaderRef` alias defaults to [`Reader::worksheet_range`]
    /// because rxls ranges already borrow worksheet cells.
    fn worksheet_range_ref(&self, name: &str) -> Option<Range<'_>> {
        self.worksheet_range(name)
    }
    /// Build a rectangular [`Range`] view for the worksheet at workbook index.
    fn worksheet_range_at(&self, index: usize) -> Option<Range<'_>>;
    /// Build a borrowed rectangular [`Range`] view for the worksheet at workbook
    /// index.
    ///
    /// This calamine-style `ReaderRef` alias defaults to
    /// [`Reader::worksheet_range_at`] because rxls ranges already borrow
    /// worksheet cells.
    fn worksheet_range_at_ref(&self, index: usize) -> Option<Range<'_>> {
        self.worksheet_range_at(index)
    }
    /// Build a formula-text range for a worksheet by name.
    fn worksheet_formula(&self, name: &str) -> Option<FormulaRange<'_>>;
    /// Build a borrowed formula-text range for a worksheet by name.
    ///
    /// This calamine-style `ReaderRef` alias defaults to
    /// [`Reader::worksheet_formula`] because rxls formula ranges already borrow
    /// worksheet formula cells.
    fn worksheet_formula_ref(&self, name: &str) -> Option<FormulaRange<'_>> {
        self.worksheet_formula(name)
    }
    /// Build a formula-text range for the worksheet at workbook index.
    fn worksheet_formula_at(&self, index: usize) -> Option<FormulaRange<'_>>;
    /// Build a borrowed formula-text range for the worksheet at workbook index.
    ///
    /// This calamine-style `ReaderRef` alias defaults to
    /// [`Reader::worksheet_formula_at`] because rxls formula ranges already
    /// borrow worksheet formula cells.
    fn worksheet_formula_at_ref(&self, index: usize) -> Option<FormulaRange<'_>> {
        self.worksheet_formula_at(index)
    }
    /// Merged cell ranges for a worksheet by name.
    fn worksheet_merge_cells(&self, name: &str) -> Option<&[(u32, u16, u32, u16)]>;
    /// Merged cell ranges for the worksheet at workbook index.
    fn worksheet_merge_cells_at(&self, index: usize) -> Option<&[(u32, u16, u32, u16)]>;
    /// All merged regions as `(sheet_name, dimensions)` in workbook order.
    fn merged_regions(&self) -> Vec<(&str, Dimensions)>;
    /// Merged regions for one worksheet name.
    fn merged_regions_by_sheet(&self, name: &str) -> Vec<Dimensions>;
    /// Grouped worksheet metadata for a worksheet by name.
    fn worksheet_metadata(&self, name: &str) -> Option<WorksheetMetadata<'_>>;
    /// Grouped worksheet metadata for the worksheet at workbook index.
    fn worksheet_metadata_at(&self, index: usize) -> Option<WorksheetMetadata<'_>>;
    /// Grouped worksheet metadata for all worksheets in workbook order.
    fn worksheets_metadata(&self) -> Vec<WorksheetMetadata<'_>>;
    /// Fetch all worksheet data as `(sheet_name, range)` in workbook order.
    fn worksheets(&self) -> Vec<(String, Range<'_>)>;
    /// Workbook-level metadata grouped into one public facade.
    fn metadata(&self) -> WorkbookMetadata<'_>;
    /// `true` if the workbook uses the 1904 date epoch.
    fn has_1904_epoch(&self) -> bool {
        self.metadata().date1904
    }
    /// 0-based active/selected sheet index, if it points at an existing sheet.
    fn active_sheet_index(&self) -> Option<usize> {
        self.metadata().active_sheet
    }
    /// Active/selected sheet name, if the active sheet index is valid.
    fn active_sheet_name(&self) -> Option<&str> {
        self.metadata().active_sheet_name
    }
    /// Workbook-level embedded pictures as `(extension, bytes)`.
    fn pictures(&self) -> Option<Vec<(String, Vec<u8>)>>;
    /// Workbook-level embedded pictures with sheet and anchor metadata.
    fn pictures_with_metadata(&self) -> Vec<Picture>;
    /// Workbook-level worksheet table names in workbook/sheet order.
    fn table_names(&self) -> Vec<&str>;
    /// Worksheet table names for `sheet_name`.
    fn table_names_in_sheet(&self, sheet_name: &str) -> Vec<&str>;
    /// Find a worksheet table by name.
    fn table_by_name(&self, table_name: &str) -> Option<(&str, &Table)>;
    /// Find a worksheet table by name through the borrowed table facade.
    ///
    /// rxls table metadata is already borrowed from the workbook, so this
    /// calamine-style alias is identical to [`Reader::table_by_name`].
    fn table_by_name_ref(&self, table_name: &str) -> Option<(&str, &Table)> {
        self.table_by_name(table_name)
    }
    /// Find a worksheet table by name and return its data body as a [`Range`].
    fn table_data_by_name(&self, table_name: &str) -> Option<(&str, Range<'_>)>;
    /// Find a worksheet table by name and return a borrowed data-body range.
    ///
    /// rxls ranges already borrow worksheet cells, so this calamine-style alias
    /// is identical to [`Reader::table_data_by_name`].
    fn table_data_by_name_ref(&self, table_name: &str) -> Option<(&str, Range<'_>)> {
        self.table_data_by_name(table_name)
    }
}

/// Workbook document properties (Dublin Core + extended), read from OOXML
/// `docProps/*`, ODF `meta.xml`, and legacy OLE property streams, and written to
/// `docProps/core.xml` and `docProps/app.xml` for `.xlsx`. Every field is
/// optional; only the set ones are emitted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DocProperties {
    /// `dc:title`.
    pub title: Option<String>,
    /// `dc:subject`.
    pub subject: Option<String>,
    /// `dc:creator` (author).
    pub creator: Option<String>,
    /// `cp:keywords`.
    pub keywords: Option<String>,
    /// `dc:description` (comments).
    pub description: Option<String>,
    /// `cp:lastModifiedBy`.
    pub last_modified_by: Option<String>,
    /// `<Company>` in the extended properties.
    pub company: Option<String>,
    /// W3CDTF timestamp (e.g. `2024-01-01T00:00:00Z`) used for both
    /// `dcterms:created` and `dcterms:modified`.
    pub created: Option<String>,
}

impl DocProperties {
    /// Construct empty workbook document properties.
    pub fn new() -> Self {
        DocProperties::default()
    }

    /// Set the document title.
    pub fn with_title(mut self, title: impl AsRef<str>) -> Self {
        self.title = Some(title.as_ref().to_string());
        self
    }

    /// Set the document subject.
    pub fn with_subject(mut self, subject: impl AsRef<str>) -> Self {
        self.subject = Some(subject.as_ref().to_string());
        self
    }

    /// Set the document creator/author.
    pub fn with_creator(mut self, creator: impl AsRef<str>) -> Self {
        self.creator = Some(creator.as_ref().to_string());
        self
    }

    /// Set the document keywords.
    pub fn with_keywords(mut self, keywords: impl AsRef<str>) -> Self {
        self.keywords = Some(keywords.as_ref().to_string());
        self
    }

    /// Set the document description/comments.
    pub fn with_description(mut self, description: impl AsRef<str>) -> Self {
        self.description = Some(description.as_ref().to_string());
        self
    }

    /// Set the last-modified-by property.
    pub fn with_last_modified_by(mut self, last_modified_by: impl AsRef<str>) -> Self {
        self.last_modified_by = Some(last_modified_by.as_ref().to_string());
        self
    }

    /// Set the company extended property.
    pub fn with_company(mut self, company: impl AsRef<str>) -> Self {
        self.company = Some(company.as_ref().to_string());
        self
    }

    /// Set the W3CDTF creation/modification timestamp text.
    pub fn with_created(mut self, created: impl AsRef<str>) -> Self {
        self.created = Some(created.as_ref().to_string());
        self
    }
}

fn image_extension(format: ImageFmt) -> &'static str {
    match format {
        ImageFmt::Png => "png",
        ImageFmt::Jpeg => "jpg",
    }
}

fn push_csv_field(out: &mut String, field: &str, delimiter: char) {
    let quote = field.contains(delimiter)
        || field.contains('"')
        || field.contains('\n')
        || field.contains('\r');
    if !quote {
        out.push_str(field);
        return;
    }

    out.push('"');
    for ch in field.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
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

fn push_html_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

fn markdown_cell(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '|' => out.push_str(r"\|"),
            '\n' | '\r' => out.push_str("<br>"),
            _ => out.push(ch),
        }
    }
    out
}

fn push_markdown_row(out: &mut String, cells: &[String]) {
    out.push('|');
    for cell in cells {
        out.push(' ');
        out.push_str(cell);
        out.push_str(" |");
    }
}

impl Workbook {
    /// Flatten every worksheet to text, each prefixed with `# <name>`.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for sheet in self.sheets.iter().filter(|s| s.is_worksheet) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("# ");
            out.push_str(&sheet.name);
            out.push('\n');
            out.push_str(&sheet.to_text());
            out.push('\n');
        }
        out
    }

    /// Export the worksheet at `sheet_index` as CSV using comma separators.
    pub fn to_csv(&self, sheet_index: usize) -> Option<String> {
        self.to_csv_with_delimiter(sheet_index, ',')
    }

    /// Export the worksheet at `sheet_index` as delimiter-separated values.
    ///
    /// Returns `None` for an out-of-range `sheet_index`, a non-worksheet
    /// (e.g. a chart sheet), or an invalid `delimiter` of `'"'` -- that
    /// character can't act as both the field separator and the quoted-field
    /// boundary without making the output ambiguous, so this method rejects
    /// it outright rather than emitting it (contrast with
    /// [`Sheet::to_csv_with_delimiter`], whose `String` return type can't
    /// signal failure and instead normalizes `'"'` to `','`).
    pub fn to_csv_with_delimiter(&self, sheet_index: usize, delimiter: char) -> Option<String> {
        if delimiter == '"' {
            return None;
        }
        self.sheets
            .get(sheet_index)
            .filter(|sheet| sheet.is_worksheet)
            .map(|sheet| sheet.to_csv_with_delimiter(delimiter))
    }

    /// Export the worksheet at `sheet_index` as an HTML table fragment.
    pub fn to_html(&self, sheet_index: usize) -> Option<String> {
        self.sheets
            .get(sheet_index)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::to_html)
    }

    /// Export the worksheet at `sheet_index` as GitHub-flavored Markdown.
    pub fn to_markdown(&self, sheet_index: usize) -> Option<String> {
        self.sheets
            .get(sheet_index)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::to_markdown)
    }

    /// `true` when parsing produced a bounded, partial workbook rather than every
    /// text-bearing cell in the source file.
    pub fn is_partial(&self) -> bool {
        self.text_truncated
    }

    /// `true` if the workbook uses the 1904 date epoch.
    ///
    /// This is a calamine-style alias over [`Workbook::date1904`].
    pub fn has_1904_epoch(&self) -> bool {
        self.date1904
    }

    /// Set the calamine-style row used as the top of workbook-level worksheet
    /// ranges.
    ///
    /// [`HeaderRow::FirstNonEmptyRow`] leaves worksheet ranges unchanged. An
    /// explicit [`HeaderRow::Row`] clips [`Workbook::worksheet_range`],
    /// [`Workbook::worksheet_range_at`], and [`Workbook::worksheets`] so the
    /// returned range starts at that absolute worksheet row.
    pub fn with_header_row(&mut self, header_row: HeaderRow) -> &mut Self {
        self.header_row = header_row;
        self
    }

    /// Current header row policy for workbook-level worksheet ranges.
    pub fn header_row(&self) -> HeaderRow {
        self.header_row
    }

    fn apply_header_row_to_range<'a>(&self, range: Range<'a>) -> Range<'a> {
        match self.header_row {
            HeaderRow::FirstNonEmptyRow => range,
            HeaderRow::Row(header_row) => {
                let (Some((_, start_col)), Some((end_row, end_col))) = (range.start(), range.end())
                else {
                    return range;
                };
                if header_row > end_row {
                    Range::empty()
                } else {
                    range.range((header_row, start_col), (end_row, end_col))
                }
            }
        }
    }

    /// `true` when workbook structure protection is enabled.
    pub fn is_structure_protected(&self) -> bool {
        self.protect_structure
    }

    /// 0-based active/selected sheet index, if it points at an existing sheet.
    pub fn active_sheet_index(&self) -> Option<usize> {
        (self.active_sheet < self.sheets.len()).then_some(self.active_sheet)
    }

    /// Active/selected sheet name, if the active sheet index is valid.
    pub fn active_sheet_name(&self) -> Option<&str> {
        self.active_sheet_index()
            .and_then(|index| self.sheets.get(index))
            .map(|sheet| sheet.name.as_str())
    }

    /// Workbook-level metadata grouped into one public facade.
    pub fn metadata(&self) -> WorkbookMetadata<'_> {
        WorkbookMetadata {
            date1904: self.date1904,
            text_truncated: self.text_truncated,
            structure_protected: self.is_structure_protected(),
            active_sheet: self.active_sheet_index(),
            active_sheet_name: self.active_sheet_name(),
            properties: &self.properties,
            defined_names: &self.defined_names,
            local_defined_names: &self.local_defined_names,
            sheets: self.sheets_metadata(),
        }
    }

    /// Workbook-level embedded pictures as `(extension, bytes)`, in workbook
    /// sheet order.
    ///
    /// This is a calamine-style aggregate over [`Sheet::images`]. It returns
    /// `None` when no supported embedded pictures are present.
    pub fn pictures(&self) -> Option<Vec<(String, Vec<u8>)>> {
        let pictures: Vec<_> = self
            .sheets
            .iter()
            .flat_map(|sheet| {
                sheet.images.iter().map(|image| {
                    (
                        image_extension(image.format).to_string(),
                        image.data.clone(),
                    )
                })
            })
            .collect();
        (!pictures.is_empty()).then_some(pictures)
    }

    /// Workbook-level embedded pictures with sheet and top-left anchor metadata,
    /// in workbook sheet order.
    ///
    /// This is a calamine-style aggregate over [`Sheet::images`]. `name` is
    /// empty until rxls stores stable drawing object names in [`Image`].
    pub fn pictures_with_metadata(&self) -> Vec<Picture> {
        self.sheets
            .iter()
            .flat_map(|sheet| {
                sheet.images.iter().map(|image| Picture {
                    row: image.from.0,
                    col: u32::from(image.from.1),
                    sheet_name: sheet.name.clone(),
                    extension: image_extension(image.format).to_string(),
                    data: image.data.clone(),
                    name: String::new(),
                })
            })
            .collect()
    }

    /// Workbook-level worksheet table names in workbook/sheet order.
    ///
    /// This is a calamine-style discovery facade over the sheet-owned
    /// [`Sheet::tables`] metadata populated by supported readers.
    pub fn table_names(&self) -> Vec<&str> {
        self.sheets
            .iter()
            .flat_map(|sheet| sheet.tables.iter().map(|table| table.name.as_str()))
            .collect()
    }

    /// Worksheet table names for `sheet_name`, or an empty vector when the sheet
    /// is absent or has no table metadata.
    pub fn table_names_in_sheet(&self, sheet_name: &str) -> Vec<&str> {
        self.sheet_by_name(sheet_name)
            .map(|sheet| {
                sheet
                    .tables
                    .iter()
                    .map(|table| table.name.as_str())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Find a worksheet table by name, returning the parent sheet name plus the
    /// borrowed [`Table`] metadata.
    pub fn table_by_name(&self, table_name: &str) -> Option<(&str, &Table)> {
        self.sheets.iter().find_map(|sheet| {
            sheet
                .tables
                .iter()
                .find(|table| table_name_eq(&table.name, table_name))
                .map(|table| (sheet.name.as_str(), table))
        })
    }

    /// Find a worksheet table by name through the borrowed table facade.
    ///
    /// rxls table metadata is already borrowed from the workbook, so this
    /// calamine-style alias is identical to [`Workbook::table_by_name`].
    pub fn table_by_name_ref(&self, table_name: &str) -> Option<(&str, &Table)> {
        self.table_by_name(table_name)
    }

    /// Find a worksheet table by name, returning the parent sheet name plus a
    /// rectangular [`Range`] over the table's data body.
    ///
    /// The returned range excludes the table header row, matching calamine's
    /// `Table::data` surface. Header-only tables return an empty range while
    /// still reporting the table's parent sheet.
    pub fn table_data_by_name(&self, table_name: &str) -> Option<(&str, Range<'_>)> {
        self.sheets.iter().find_map(|sheet| {
            let table = sheet
                .tables
                .iter()
                .find(|table| table_name_eq(&table.name, table_name))?;
            Some((sheet.name.as_str(), table_data_range(sheet, table)))
        })
    }

    /// Find a worksheet table by name and return a borrowed data-body range.
    ///
    /// rxls ranges already borrow worksheet cells, so this calamine-style alias
    /// is identical to [`Workbook::table_data_by_name`].
    pub fn table_data_by_name_ref(&self, table_name: &str) -> Option<(&str, Range<'_>)> {
        self.table_data_by_name(table_name)
    }
}

fn table_data_range<'a>(sheet: &'a Sheet, table: &Table) -> Range<'a> {
    let Some(first_data_row) = table.range.0.checked_add(1) else {
        return Range::empty();
    };
    if first_data_row > table.range.2 {
        return Range::empty();
    }
    sheet.range().range(
        (first_data_row, u32::from(table.range.1)),
        (table.range.2, u32::from(table.range.3)),
    )
}

fn table_name_eq(left: &str, right: &str) -> bool {
    left.chars()
        .flat_map(char::to_lowercase)
        .eq(right.chars().flat_map(char::to_lowercase))
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

/// A best-effort display string for an authored value (for [`Sheet::to_text`];
/// the written `.xlsx` renders via the cell's number format).
fn display_text(v: &Cell) -> String {
    match v {
        Cell::Text(s) => s.clone(),
        Cell::Number(n) | Cell::Date(n) => crate::format_number(*n),
        Cell::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Cell::Error(e) => e.clone(),
        Cell::Formula { cached, .. } => display_text(cached),
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

impl Workbook {
    /// A new empty workbook for authoring.
    pub fn new() -> Self {
        Workbook::default()
    }

    /// Like [`to_xlsx`](Self::to_xlsx), but **validates first**: returns a typed
    /// [`WriteError`](crate::WriteError) for the first structural problem the
    /// infallible writer would otherwise silently sanitize (out-of-grid or reversed
    /// cells/merges/ranges, authored cell/formula XML text, duplicate/invalid
    /// sheet or table names, a table range whose width disagrees with its column
    /// count, table-header format target mismatches, active-sheet index mistakes,
    /// too many sheets). On success the bytes are exactly what
    /// [`to_xlsx`](Self::to_xlsx) produces, unmodified.
    ///
    /// This is a best-effort structural pre-flight, not an exhaustive Excel
    /// validator: it checks ranges and bounds, but not formula syntax, chart
    /// series references, or consumer-specific rendering details.
    ///
    /// Available with the default `xlsx` feature.
    ///
    /// # Errors
    ///
    /// Returns the first typed [`WriteError`](crate::WriteError) found during
    /// structural, text, range, or output-budget validation.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), rxls::WriteError> {
    /// let mut workbook = rxls::Workbook::new();
    /// workbook.add_sheet("Data").write(0, 0, "ready");
    /// let bytes = workbook.to_xlsx_checked()?;
    /// assert!(bytes.starts_with(b"PK"));
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "xlsx")]
    pub fn to_xlsx_checked(&self) -> Result<Vec<u8>, crate::WriteError> {
        crate::write::validate(self)?;
        Ok(self.to_xlsx())
    }
    /// Append a worksheet and return a mutable handle to it.
    pub fn add_sheet(&mut self, name: impl AsRef<str>) -> &mut Sheet {
        self.sheets.push(Sheet::new(name));
        self.sheets
            .last_mut()
            .expect("just pushed a sheet, so last_mut is Some")
    }
    /// Define a workbook-global name pointing at `refers_to` (e.g.
    /// `define_name("TaxRate", "Sheet1!$B$1")`), emitted as a `<definedName>`
    /// when authoring an `.xlsx`.
    pub fn define_name(&mut self, name: impl AsRef<str>, refers_to: impl AsRef<str>) {
        self.defined_names
            .push((name.as_ref().to_string(), refers_to.as_ref().to_string()));
    }
    /// Define a name scoped to one worksheet. The checked writer rejects an
    /// unknown sheet; the infallible writer omits such an invalid entry.
    pub fn define_local_name(
        &mut self,
        sheet: impl AsRef<str>,
        name: impl AsRef<str>,
        refers_to: impl AsRef<str>,
    ) {
        self.local_defined_names.push(LocalDefinedName {
            sheet: sheet.as_ref().to_string(),
            name: name.as_ref().to_string(),
            refers_to: refers_to.as_ref().to_string(),
        });
    }
    /// Set workbook document properties for `.xlsx` authoring.
    pub fn set_properties(&mut self, properties: DocProperties) {
        self.properties = properties;
    }
    /// Set the 0-based index of the active/selected sheet, emitted as
    /// `<workbookView activeTab="N"/>` and `tabSelected="1"` on that sheet's
    /// view when authoring an `.xlsx`. An out-of-range index is tolerated by the
    /// infallible writer (it falls back to no selection) and rejected by
    /// [`Workbook::to_xlsx_checked`].
    pub fn set_active_sheet(&mut self, idx: usize) {
        self.active_sheet = idx;
    }
    /// Lock the workbook structure (authoring): emits `<workbookProtection
    /// lockStructure="1"/>` so Excel forbids adding, deleting, renaming, hiding,
    /// or reordering sheets. No password is set (structure is locked but
    /// unprotectable without one). Distinct from per-sheet [`Sheet::protect`].
    pub fn protect_structure(&mut self) {
        self.protect_structure = true;
    }
    /// Workbook-global defined names as `(name, refers_to)` — the read accessor
    /// over [`Self::defined_names`], populated by the `.xlsx` reader,
    /// workbook-global `.xls` `Lbl` / `.xlsb` `BrtName` records, and `.ods`
    /// named ranges, then round-tripped by the writer. Built-in `_xlnm.*` names
    /// Sheet-local user names are exposed separately through
    /// [`Self::local_defined_names`].
    pub fn defined_names(&self) -> &[(String, String)] {
        &self.defined_names
    }
    /// Sheet-scoped defined names retained by readers or added for authoring.
    pub fn local_defined_names(&self) -> &[LocalDefinedName] {
        &self.local_defined_names
    }
    /// Find a worksheet by name (case-sensitive) — the calamine-style by-name
    /// accessor over [`Self::sheets`].
    pub fn sheet_by_name(&self, name: &str) -> Option<&Sheet> {
        self.sheets.iter().find(|s| s.name == name)
    }
    /// Build a rectangular [`Range`] view for a worksheet by name.
    pub fn worksheet_range(&self, name: &str) -> Option<Range<'_>> {
        self.sheet_by_name(name)
            .filter(|sheet| sheet.is_worksheet)
            .map(|sheet| self.apply_header_row_to_range(sheet.range()))
    }
    /// Build a borrowed rectangular [`Range`] view for a worksheet by name.
    ///
    /// rxls ranges are already borrowed views over sparse worksheet cells, so
    /// this calamine-style `ReaderRef` alias is identical to
    /// [`Workbook::worksheet_range`].
    pub fn worksheet_range_ref(&self, name: &str) -> Option<Range<'_>> {
        self.worksheet_range(name)
    }
    /// Build a rectangular [`Range`] view for the worksheet at workbook index.
    pub fn worksheet_range_at(&self, index: usize) -> Option<Range<'_>> {
        self.sheets
            .get(index)
            .filter(|sheet| sheet.is_worksheet)
            .map(|sheet| self.apply_header_row_to_range(sheet.range()))
    }
    /// Build a borrowed rectangular [`Range`] view for the worksheet at workbook
    /// index.
    ///
    /// rxls ranges are already borrowed views over sparse worksheet cells, so
    /// this calamine-style `ReaderRef` alias is identical to
    /// [`Workbook::worksheet_range_at`].
    pub fn worksheet_range_at_ref(&self, index: usize) -> Option<Range<'_>> {
        self.worksheet_range_at(index)
    }
    /// Fetch all worksheet data as `(sheet_name, range)` in workbook order.
    pub fn worksheets(&self) -> Vec<(String, Range<'_>)> {
        self.sheets
            .iter()
            .filter(|sheet| sheet.is_worksheet)
            .map(|sheet| {
                (
                    sheet.name.clone(),
                    self.apply_header_row_to_range(sheet.range()),
                )
            })
            .collect()
    }
    /// Build a formula-text range for a worksheet by name.
    pub fn worksheet_formula(&self, name: &str) -> Option<FormulaRange<'_>> {
        self.sheet_by_name(name)
            .filter(|sheet| sheet.is_worksheet)
            .map(FormulaRange::from_sheet)
    }
    /// Build a borrowed formula-text range for a worksheet by name.
    ///
    /// rxls formula ranges are already borrowed views over sparse worksheet
    /// formulas, so this calamine-style `ReaderRef` alias is identical to
    /// [`Workbook::worksheet_formula`].
    pub fn worksheet_formula_ref(&self, name: &str) -> Option<FormulaRange<'_>> {
        self.worksheet_formula(name)
    }
    /// Build a formula-text range for the worksheet at workbook index.
    pub fn worksheet_formula_at(&self, index: usize) -> Option<FormulaRange<'_>> {
        self.sheets
            .get(index)
            .filter(|sheet| sheet.is_worksheet)
            .map(FormulaRange::from_sheet)
    }
    /// Build a borrowed formula-text range for the worksheet at workbook index.
    ///
    /// rxls formula ranges are already borrowed views over sparse worksheet
    /// formulas, so this calamine-style `ReaderRef` alias is identical to
    /// [`Workbook::worksheet_formula_at`].
    pub fn worksheet_formula_at_ref(&self, index: usize) -> Option<FormulaRange<'_>> {
        self.worksheet_formula_at(index)
    }
    /// Merged cell ranges for a worksheet by name.
    pub fn worksheet_merge_cells(&self, name: &str) -> Option<&[(u32, u16, u32, u16)]> {
        self.sheet_by_name(name)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::merged_ranges)
    }
    /// Merged cell ranges for the worksheet at workbook index.
    pub fn worksheet_merge_cells_at(&self, index: usize) -> Option<&[(u32, u16, u32, u16)]> {
        self.sheets
            .get(index)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::merged_ranges)
    }

    /// All merged regions as `(sheet_name, dimensions)` in workbook order.
    ///
    /// This is a calamine-style aggregate over the sheet-owned merge metadata.
    /// rxls does not expose package part paths in this facade, so callers get
    /// the owning sheet name plus typed inclusive dimensions.
    pub fn merged_regions(&self) -> Vec<(&str, Dimensions)> {
        self.sheets
            .iter()
            .filter(|sheet| sheet.is_worksheet)
            .flat_map(|sheet| {
                sheet
                    .merged_ranges()
                    .iter()
                    .map(move |&range| (sheet.name.as_str(), Dimensions::from_range_tuple(range)))
            })
            .collect()
    }

    /// Merged regions for one worksheet name.
    pub fn merged_regions_by_sheet(&self, name: &str) -> Vec<Dimensions> {
        self.sheet_by_name(name)
            .filter(|sheet| sheet.is_worksheet)
            .map(|sheet| {
                sheet
                    .merged_ranges()
                    .iter()
                    .map(|&range| Dimensions::from_range_tuple(range))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Grouped worksheet metadata for a worksheet by name.
    pub fn worksheet_metadata(&self, name: &str) -> Option<WorksheetMetadata<'_>> {
        self.sheet_by_name(name)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::metadata)
    }
    /// Grouped worksheet metadata for the worksheet at workbook index.
    pub fn worksheet_metadata_at(&self, index: usize) -> Option<WorksheetMetadata<'_>> {
        self.sheets
            .get(index)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::metadata)
    }
    /// Grouped worksheet metadata for all worksheets in workbook order.
    pub fn worksheets_metadata(&self) -> Vec<WorksheetMetadata<'_>> {
        self.sheets
            .iter()
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::metadata)
            .collect()
    }
    /// Sheet metadata in workbook order.
    pub fn sheets_metadata(&self) -> Vec<SheetMetadata> {
        self.sheets
            .iter()
            .map(|sheet| SheetMetadata {
                name: sheet.name.clone(),
                typ: sheet.sheet_type(),
                visible: sheet.visible(),
            })
            .collect()
    }
    /// Worksheet names, in order.
    pub fn sheet_names(&self) -> Vec<&str> {
        self.sheets.iter().map(|s| s.name.as_str()).collect()
    }
}

impl Reader for Workbook {
    fn sheet_names(&self) -> Vec<&str> {
        Workbook::sheet_names(self)
    }

    fn sheets_metadata(&self) -> Vec<SheetMetadata> {
        Workbook::sheets_metadata(self)
    }

    fn defined_names(&self) -> &[(String, String)] {
        Workbook::defined_names(self)
    }

    fn local_defined_names(&self) -> &[LocalDefinedName] {
        Workbook::local_defined_names(self)
    }

    fn with_header_row(&mut self, header_row: HeaderRow) -> &mut Self {
        Workbook::with_header_row(self, header_row)
    }

    fn header_row(&self) -> HeaderRow {
        Workbook::header_row(self)
    }

    fn worksheet_range(&self, name: &str) -> Option<Range<'_>> {
        Workbook::worksheet_range(self, name)
    }

    fn worksheet_range_at(&self, index: usize) -> Option<Range<'_>> {
        Workbook::worksheet_range_at(self, index)
    }

    fn worksheet_formula(&self, name: &str) -> Option<FormulaRange<'_>> {
        Workbook::worksheet_formula(self, name)
    }

    fn worksheet_formula_at(&self, index: usize) -> Option<FormulaRange<'_>> {
        Workbook::worksheet_formula_at(self, index)
    }

    fn worksheet_merge_cells(&self, name: &str) -> Option<&[(u32, u16, u32, u16)]> {
        Workbook::worksheet_merge_cells(self, name)
    }

    fn worksheet_merge_cells_at(&self, index: usize) -> Option<&[(u32, u16, u32, u16)]> {
        Workbook::worksheet_merge_cells_at(self, index)
    }

    fn merged_regions(&self) -> Vec<(&str, Dimensions)> {
        Workbook::merged_regions(self)
    }

    fn merged_regions_by_sheet(&self, name: &str) -> Vec<Dimensions> {
        Workbook::merged_regions_by_sheet(self, name)
    }

    fn worksheet_metadata(&self, name: &str) -> Option<WorksheetMetadata<'_>> {
        Workbook::worksheet_metadata(self, name)
    }

    fn worksheet_metadata_at(&self, index: usize) -> Option<WorksheetMetadata<'_>> {
        Workbook::worksheet_metadata_at(self, index)
    }

    fn worksheets_metadata(&self) -> Vec<WorksheetMetadata<'_>> {
        Workbook::worksheets_metadata(self)
    }

    fn worksheets(&self) -> Vec<(String, Range<'_>)> {
        Workbook::worksheets(self)
    }

    fn metadata(&self) -> WorkbookMetadata<'_> {
        Workbook::metadata(self)
    }

    fn active_sheet_index(&self) -> Option<usize> {
        Workbook::active_sheet_index(self)
    }

    fn active_sheet_name(&self) -> Option<&str> {
        Workbook::active_sheet_name(self)
    }

    fn pictures(&self) -> Option<Vec<(String, Vec<u8>)>> {
        Workbook::pictures(self)
    }

    fn pictures_with_metadata(&self) -> Vec<Picture> {
        Workbook::pictures_with_metadata(self)
    }

    fn table_names(&self) -> Vec<&str> {
        Workbook::table_names(self)
    }

    fn table_names_in_sheet(&self, sheet_name: &str) -> Vec<&str> {
        Workbook::table_names_in_sheet(self, sheet_name)
    }

    fn table_by_name(&self, table_name: &str) -> Option<(&str, &Table)> {
        Workbook::table_by_name(self, table_name)
    }

    fn table_data_by_name(&self, table_name: &str) -> Option<(&str, Range<'_>)> {
        Workbook::table_data_by_name(self, table_name)
    }
}

impl Sheet {
    /// A new empty worksheet for authoring.
    pub fn new(name: impl AsRef<str>) -> Self {
        Sheet {
            name: name.as_ref().to_string(),
            is_worksheet: true,
            sheet_type: Some(SheetType::WorkSheet),
            ..Default::default()
        }
    }
    /// Build a rectangular [`Range`] view over this sheet's effective cells.
    pub fn range(&self) -> Range<'_> {
        Range::from_sheet(self)
    }
    /// Write a value at `(row, col)`.
    pub fn write(&mut self, row: u32, col: u16, value: impl Into<Cell>) {
        self.push_authored(row, col, value.into(), None, None);
    }
    /// Write a string value at `(row, col)`.
    pub fn write_string(&mut self, row: u32, col: u16, value: impl AsRef<str>) {
        self.write(row, col, value.as_ref());
    }
    /// Write a number at `(row, col)`.
    pub fn write_number(&mut self, row: u32, col: u16, value: impl Into<f64>) {
        self.write(row, col, value.into());
    }
    /// Write a boolean value at `(row, col)`.
    pub fn write_boolean(&mut self, row: u32, col: u16, value: bool) {
        self.write(row, col, value);
    }
    /// Write a boolean value at `(row, col)` with a [`Format`].
    pub fn write_boolean_with_format(&mut self, row: u32, col: u16, value: bool, format: &Format) {
        self.write_styled(row, col, value, format.as_cell_style());
    }
    /// Write a typed spreadsheet error at `(row, col)`.
    pub fn write_error(&mut self, row: u32, col: u16, error: CellErrorType) {
        self.write(row, col, error);
    }
    /// Write a typed spreadsheet error at `(row, col)` with a [`Format`].
    pub fn write_error_with_format(
        &mut self,
        row: u32,
        col: u16,
        error: CellErrorType,
        format: &Format,
    ) {
        self.write_styled(row, col, error, format.as_cell_style());
    }
    /// Write an Excel serial date/time value at `(row, col)`.
    ///
    /// The serial is stored as [`Cell::Date`], so the writer emits a date cell and
    /// the reader reopens it as a typed date value.
    pub fn write_datetime(&mut self, row: u32, col: u16, serial: impl Into<f64>) {
        self.write(row, col, Cell::Date(serial.into()));
    }
    /// Write an Excel serial date/time value at `(row, col)` with a [`Format`].
    pub fn write_datetime_with_format(
        &mut self,
        row: u32,
        col: u16,
        serial: impl Into<f64>,
        format: &Format,
    ) {
        self.write_styled(row, col, Cell::Date(serial.into()), format.as_cell_style());
    }
    /// Write a value at `(row, col)` with an inline style.
    pub fn write_styled(&mut self, row: u32, col: u16, value: impl Into<Cell>, style: &CellStyle) {
        self.push_authored(row, col, value.into(), Some(style.clone()), None);
    }
    /// Write a value at `(row, col)` with a [`Format`].
    pub fn write_with_format(
        &mut self,
        row: u32,
        col: u16,
        value: impl Into<Cell>,
        format: &Format,
    ) {
        self.write_styled(row, col, value, format.as_cell_style());
    }
    /// Write a string at `(row, col)` with a [`Format`].
    pub fn write_string_with_format(
        &mut self,
        row: u32,
        col: u16,
        value: impl AsRef<str>,
        format: &Format,
    ) {
        self.write_styled(row, col, value.as_ref(), format.as_cell_style());
    }
    /// Write a number at `(row, col)` with a [`Format`].
    pub fn write_number_with_format(
        &mut self,
        row: u32,
        col: u16,
        value: impl Into<f64>,
        format: &Format,
    ) {
        self.write_styled(row, col, value.into(), format.as_cell_style());
    }
    /// Write a formula at `(row, col)` with a cached result value.
    ///
    /// `formula` is stored without a leading `=`. `cached` is the last calculated
    /// result that spreadsheet readers can show before recalculation.
    pub fn write_formula(
        &mut self,
        row: u32,
        col: u16,
        formula: impl AsRef<str>,
        cached: impl Into<Cell>,
    ) {
        self.write(
            row,
            col,
            Cell::Formula {
                formula: formula.as_ref().to_string(),
                cached: Box::new(cached.into()),
            },
        );
    }
    /// Write a formula at `(row, col)` with a [`Format`] and cached result value.
    ///
    /// `formula` is stored without a leading `=`. `cached` is the last calculated
    /// result that spreadsheet readers can show before recalculation.
    pub fn write_formula_with_format(
        &mut self,
        row: u32,
        col: u16,
        formula: impl AsRef<str>,
        cached: impl Into<Cell>,
        format: &Format,
    ) {
        self.write_styled(
            row,
            col,
            Cell::Formula {
                formula: formula.as_ref().to_string(),
                cached: Box::new(cached.into()),
            },
            format.as_cell_style(),
        );
    }
    /// Write a format-only blank cell at `(row, col)` with an inline style.
    pub fn write_blank_styled(&mut self, row: u32, col: u16, style: &CellStyle) {
        self.rich.remove(&(row, col));
        self.cells
            .retain(|entry| entry.row != row || entry.col != col);
        self.blank_styles.insert((row, col), style.clone());
    }
    /// Write a format-only blank cell at `(row, col)` with a [`Format`].
    pub fn write_blank_with_format(&mut self, row: u32, col: u16, format: &Format) {
        self.write_blank_styled(row, col, format.as_cell_style());
    }
    /// Hide the worksheet gridlines (authoring).
    pub fn hide_gridlines(&mut self) {
        self.hide_gridlines = true;
    }
    /// Set the sheet zoom as a percentage, e.g. `150` (authoring).
    pub fn set_zoom(&mut self, percent: u16) {
        self.zoom = Some(percent);
    }
    /// Show or hide the row and column headers in the sheet view (authoring).
    /// Pass `false` to emit `<sheetView showRowColHeaders="0">`.
    pub fn set_show_headers(&mut self, show: bool) {
        self.show_headers = Some(show);
    }
    /// Lay the sheet out right-to-left (authoring): `<sheetView rightToLeft="1">`.
    pub fn set_right_to_left(&mut self, rtl: bool) {
        self.right_to_left = rtl;
    }
    /// Set worksheet view metadata in one object-model call.
    pub fn set_sheet_view(&mut self, view: SheetView) {
        self.freeze = view.freeze;
        self.hide_gridlines = view.hide_gridlines;
        self.zoom = view.zoom;
        self.show_headers = view.show_headers;
        self.right_to_left = view.right_to_left;
    }
    /// Hide this worksheet in the workbook (authoring).
    pub fn hide(&mut self) {
        self.hidden = true;
    }
    /// Very-hide this worksheet (authoring): `state="veryHidden"`, which Excel hides
    /// from the unhide menu (only a macro/VBA can reveal it).
    pub fn hide_very(&mut self) {
        self.very_hidden = true;
    }
    /// Auto-size column widths from the cell text when authoring. An explicit
    /// [`Sheet::set_col_width`] still takes precedence for that column.
    pub fn set_autofit(&mut self) {
        self.autofit = true;
    }
    /// Group rows `first..=last` at outline `level` (1-based depth) for collapsible
    /// row outlines (authoring). The span is clamped to the row grid.
    pub fn group_rows(&mut self, first: u32, last: u32, level: u8) {
        let last = last.min(1_048_575);
        for r in first..=last {
            self.row_outline.insert(r, level);
        }
    }
    /// Group columns `first..=last` at outline `level` (authoring).
    pub fn group_cols(&mut self, first: u16, last: u16, level: u8) {
        for c in first..=last {
            self.col_outline.insert(c, level);
        }
    }
    /// Set whether outline summary rows sit *below* their grouped detail rows and
    /// summary columns sit to the *right* of theirs (authoring). Both default to
    /// `true` (Excel's default); passing `false` for either emits
    /// `<sheetPr><outlinePr summaryBelow="0" summaryRight="0"/></sheetPr>`.
    pub fn set_outline_summary(&mut self, below: bool, right: bool) {
        self.outline_summary_below = below;
        self.outline_summary_right = right;
    }
    /// Mark the summary `row` of a collapsed group (authoring): the row is emitted
    /// as `<row collapsed="1" hidden="1">`, keeping the summary visible while Excel
    /// treats the group as collapsed. Pair with [`Sheet::group_rows`] on the detail
    /// rows.
    pub fn collapse_row(&mut self, row: u32) {
        self.collapsed_rows.insert(row);
    }
    /// Print the gridlines on the printed page (authoring).
    pub fn set_print_gridlines(&mut self) {
        self.print_gridlines = true;
    }
    /// Print the row and column headings on the printed page (authoring).
    pub fn set_print_headings(&mut self) {
        self.print_headings = true;
    }
    /// Write a rich (mixed-format) string at `(row, col)`: each [`TextRun`] carries
    /// its own font. Emitted as an inline rich string; the concatenated text is also
    /// stored so the cell has a plain value for readers and other tooling. Empty-text
    /// runs are dropped, and empty/all-empty `runs` is a no-op. Per-run fonts come
    /// from each [`TextRun`]; use [`Self::write_rich_with_format`] to add a
    /// cell-level style.
    pub fn write_rich<I>(&mut self, row: u32, col: u16, runs: I)
    where
        I: IntoIterator<Item = TextRun>,
    {
        self.push_rich(row, col, runs, None);
    }
    /// Write a rich string at `(row, col)` with a cell-level [`CellStyle`].
    ///
    /// The style applies to the cell (`s="..."`) while each [`TextRun`] still
    /// carries its own run font inside the inline string.
    pub fn write_rich_styled<I>(&mut self, row: u32, col: u16, runs: I, style: &CellStyle)
    where
        I: IntoIterator<Item = TextRun>,
    {
        self.push_rich(row, col, runs, Some(style.clone()));
    }
    /// Write a rich string at `(row, col)` with a writer-facing [`Format`].
    pub fn write_rich_with_format<I>(&mut self, row: u32, col: u16, runs: I, format: &Format)
    where
        I: IntoIterator<Item = TextRun>,
    {
        self.write_rich_styled(row, col, runs, format.as_cell_style());
    }
    fn push_rich<I>(&mut self, row: u32, col: u16, runs: I, style: Option<CellStyle>)
    where
        I: IntoIterator<Item = TextRun>,
    {
        let runs: Vec<TextRun> = runs.into_iter().filter(|r| !r.text.is_empty()).collect();
        if runs.is_empty() {
            return;
        }
        let joined: String = runs.iter().map(|r| r.text.as_str()).collect();
        self.push_authored(row, col, Cell::Text(joined), style, None);
        self.rich.insert((row, col), runs);
    }
    /// Write `text` at `(row, col)` as an external hyperlink to `url`.
    pub fn write_url(&mut self, row: u32, col: u16, url: impl AsRef<str>, text: impl AsRef<str>) {
        self.push_authored(
            row,
            col,
            Cell::Text(text.as_ref().to_string()),
            None,
            Some(url.as_ref().to_string()),
        );
    }
    /// Write `text` at `(row, col)` as an external hyperlink to `url`.
    ///
    /// This is a rust_xlsxwriter-style alias for [`Self::write_url`].
    pub fn write_url_with_text(
        &mut self,
        row: u32,
        col: u16,
        url: impl AsRef<str>,
        text: impl AsRef<str>,
    ) {
        self.write_url(row, col, url, text);
    }
    /// Write `url` at `(row, col)` as an external hyperlink with a [`Format`].
    pub fn write_url_with_format(
        &mut self,
        row: u32,
        col: u16,
        url: impl AsRef<str>,
        format: &Format,
    ) {
        let url = url.as_ref();
        self.write_url_with_text_and_format(row, col, url, url, format);
    }
    /// Write `text` at `(row, col)` as an external hyperlink to `url` with a
    /// [`Format`].
    pub fn write_url_with_text_and_format(
        &mut self,
        row: u32,
        col: u16,
        url: impl AsRef<str>,
        text: impl AsRef<str>,
        format: &Format,
    ) {
        self.push_authored(
            row,
            col,
            Cell::Text(text.as_ref().to_string()),
            Some(format.as_cell_style().clone()),
            Some(url.as_ref().to_string()),
        );
    }
    /// Merge the rectangular range `(r0,c0)..=(r1,c1)`.
    pub fn merge(&mut self, r0: u32, c0: u16, r1: u32, c1: u16) {
        self.merges.push((r0, c0, r1, c1));
    }
    /// Merge the rectangular range `(r0,c0)..=(r1,c1)` and write `text` to the
    /// top-left cell with a [`Format`].
    pub fn merge_range(
        &mut self,
        r0: u32,
        c0: u16,
        r1: u32,
        c1: u16,
        text: impl AsRef<str>,
        format: &Format,
    ) {
        self.merge(r0, c0, r1, c1);
        self.write_with_format(r0, c0, text.as_ref(), format);
    }
    /// Set a column width in character units.
    pub fn set_col_width(&mut self, col: u16, chars: f32) {
        self.col_widths.insert(col, chars);
    }
    /// Set a row height in points.
    pub fn set_row_height(&mut self, row: u32, points: f32) {
        self.row_heights.insert(row, points);
    }
    /// Hide a column by 0-based index.
    pub fn hide_column(&mut self, col: u16) {
        self.hidden_cols.insert(col);
    }
    /// Hide a row by 0-based index.
    pub fn hide_row(&mut self, row: u32) {
        self.hidden_rows.insert(row);
    }
    /// Set the default format for cells in a row.
    pub fn set_row_format(&mut self, row: u32, format: &Format) {
        self.row_formats.insert(row, format.as_cell_style().clone());
    }
    /// Set the default format for cells in a column.
    pub fn set_col_format(&mut self, col: u16, format: &Format) {
        self.col_formats.insert(col, format.as_cell_style().clone());
    }
    /// Set the worksheet default format for cells without a more specific format.
    ///
    /// Column, row, and explicit cell formats merge over this base style.
    pub fn set_default_format(&mut self, format: &Format) {
        self.default_format = Some(format.as_cell_style().clone());
    }
    /// Set the format for the header row cells of the named table.
    ///
    /// The `table_name` is the authored [`Table::name`]. The writer composes this
    /// over worksheet, column, and row defaults; explicit cell formats still win.
    /// [`Workbook::to_xlsx_checked`] rejects names that do not match a table on
    /// this sheet.
    pub fn set_table_header_format(&mut self, table_name: impl AsRef<str>, format: &Format) {
        self.table_header_formats.insert(
            table_name.as_ref().to_string(),
            format.as_cell_style().clone(),
        );
    }
    /// Set the default row height (points) for rows without an explicit height.
    pub fn set_default_row_height(&mut self, points: f32) {
        self.default_row_height = Some(points);
    }
    /// Set the default column width (character units) for columns without an
    /// explicit width.
    pub fn set_default_col_width(&mut self, chars: f32) {
        self.default_col_width = Some(chars);
    }
    /// Freeze the panes above `row` and left of `col`.
    pub fn freeze_panes(&mut self, row: u32, col: u16) {
        self.freeze = Some((row, col));
    }
    /// Apply an autofilter over the range `(r0,c0)..=(r1,c1)`.
    pub fn autofilter(&mut self, r0: u32, c0: u16, r1: u32, c1: u16) {
        self.autofilter = Some((r0, c0, r1, c1));
    }
    /// Set the print / page setup.
    pub fn set_page_setup(&mut self, ps: PageSetup) {
        self.page_setup = Some(ps);
    }
    /// Set the sheet tab color.
    pub fn set_tab_color(&mut self, color: impl Into<Color>) {
        self.tab_color = Some(color.into());
    }
    /// Protect the worksheet (locks cells against editing in Excel).
    pub fn protect(&mut self) {
        self.protect = true;
    }
    /// Protect the worksheet while permitting the actions enabled in `opts`
    /// (e.g. sorting, AutoFilter, formatting). Anything left `false` stays
    /// locked, exactly as [`Self::protect`].
    pub fn protect_with(&mut self, opts: ProtectionOptions) {
        self.protect = true;
        self.protect_options = Some(opts);
    }
    /// Add a data-validation rule (e.g. [`DataValidation::list`] for a dropdown).
    pub fn add_data_validation(&mut self, dv: DataValidation) {
        self.data_validations.push(dv);
    }
    /// Add a conditional-formatting rule over a range.
    pub fn add_conditional_format(&mut self, cf: CondFormat) {
        self.cond_formats.push(cf);
    }
    /// Embed an image anchored to a cell box.
    pub fn add_image(&mut self, img: Image) {
        self.images.push(img);
    }
    /// Add a chart anchored to a cell box.
    pub fn add_chart(&mut self, chart: Chart) {
        self.charts.push(chart);
    }
    /// Add a sparkline anchored to a single destination cell.
    pub fn add_sparkline(&mut self, sparkline: Sparkline) {
        self.sparklines.push(sparkline);
    }
    /// Add a worksheet table over a range (first row = header).
    pub fn add_table(&mut self, table: Table) {
        self.tables.push(table);
    }
    /// Attach a legacy cell comment / note to `(row, col)` with `text` and an
    /// optional `author`. Passing a direct author string is treated as `Some`.
    pub fn add_comment(
        &mut self,
        row: u32,
        col: u16,
        text: impl AsRef<str>,
        author: impl Into<CommentAuthor>,
    ) {
        self.comments.push(Comment {
            row,
            col,
            text: text.as_ref().to_string(),
            author: author.into().0,
        });
    }
    fn push_authored(
        &mut self,
        row: u32,
        col: u16,
        value: Cell,
        style: Option<CellStyle>,
        hyperlink: Option<String>,
    ) {
        // A plain write supersedes any rich-string runs previously set here;
        // `write_rich` re-inserts after calling this, so its own runs survive.
        self.rich.remove(&(row, col));
        self.blank_styles.remove(&(row, col));
        let text = display_text(&value);
        self.cells.push(CellEntry {
            row,
            col,
            value,
            text,
            style,
            hyperlink,
        });
    }
}

impl CellStyle {
    /// A new empty style.
    pub fn new() -> Self {
        Self::default()
    }
    /// Merge this style with `overlay`, where explicitly set overlay fields
    /// override this style and unset overlay fields preserve `self`.
    pub fn merge(&self, overlay: &CellStyle) -> Self {
        let mut merged = self.clone();
        merged.font = merge_font(self.font.as_ref(), overlay.font.as_ref());
        if overlay.fill.is_some() {
            merged.fill = overlay.fill;
        }
        if overlay.pattern_fill.is_some() {
            merged.pattern_fill = overlay.pattern_fill;
        }
        merged.border = merge_border(self.border.as_ref(), overlay.border.as_ref());
        if overlay.num_fmt.is_some() {
            merged.num_fmt = overlay.num_fmt.clone();
        }
        merged.align = merge_alignment(self.align.as_ref(), overlay.align.as_ref());
        merged.protection = merge_protection(self.protection.as_ref(), overlay.protection.as_ref());
        merged
    }
    /// Set the font family name.
    pub fn font_name(mut self, name: impl AsRef<str>) -> Self {
        self.font.get_or_insert_with(Font::default).name = Some(name.as_ref().to_string());
        self
    }
    /// Set the font size in points.
    pub fn size(mut self, points: u16) -> Self {
        self.font.get_or_insert_with(Font::default).size_pt = Some(points);
        self
    }
    /// Set the text color.
    pub fn color(mut self, color: impl Into<Color>) -> Self {
        self.font.get_or_insert_with(Font::default).color = Some(color.into());
        self
    }
    /// Make the font bold.
    pub fn bold(mut self) -> Self {
        self.font.get_or_insert_with(Font::default).bold = true;
        self
    }
    /// Make the font italic.
    pub fn italic(mut self) -> Self {
        self.font.get_or_insert_with(Font::default).italic = true;
        self
    }

    /// Apply single underline to the font.
    pub fn underline(mut self) -> Self {
        self.font.get_or_insert_with(Font::default).underline = true;
        self
    }

    /// Apply strikethrough to the font.
    pub fn strikethrough(mut self) -> Self {
        self.font.get_or_insert_with(Font::default).strikethrough = true;
        self
    }

    /// Set the font superscript/subscript property.
    pub fn font_script(mut self, script: FormatScript) -> Self {
        self.font.get_or_insert_with(Font::default).script = script;
        self
    }

    /// Set a solid background fill color.
    pub fn fill(mut self, color: impl Into<Color>) -> Self {
        let color = color.into();
        self.fill = Some(color);
        self.pattern_fill = Some(Fill::solid(color));
        self
    }
    /// Set the fill pattern.
    pub fn pattern(mut self, pattern: FormatPattern) -> Self {
        self.pattern_fill.get_or_insert_with(Fill::default).pattern = pattern;
        self
    }
    /// Set the fill background color.
    pub fn background_color(mut self, color: impl Into<Color>) -> Self {
        let color = color.into();
        self.fill = Some(color);
        let fill = self.pattern_fill.get_or_insert_with(Fill::default);
        fill.background = Some(color);
        if fill.pattern == FormatPattern::None {
            fill.pattern = FormatPattern::Solid;
        }
        self
    }
    /// Set the fill foreground or pattern color.
    pub fn foreground_color(mut self, color: impl Into<Color>) -> Self {
        self.pattern_fill
            .get_or_insert_with(Fill::default)
            .foreground = Some(color.into());
        self
    }
    /// Set the fill object.
    pub fn pattern_fill(mut self, fill: Fill) -> Self {
        self.fill = fill.background;
        self.pattern_fill = Some(fill);
        self
    }
    #[cfg_attr(not(feature = "xlsx"), allow(dead_code))]
    pub(crate) fn effective_fill(&self) -> Option<Fill> {
        self.pattern_fill
            .or_else(|| self.fill.map(|c| Fill::solid(c.0)))
    }
    /// Set the number format code (e.g. `₩#,##0`, `0.0%`).
    pub fn num_fmt(mut self, code: impl AsRef<str>) -> Self {
        self.num_fmt = Some(code.as_ref().to_string());
        self
    }
    /// Wrap long text within the cell.
    pub fn wrap(mut self) -> Self {
        self.align.get_or_insert_with(Alignment::default).wrap = true;
        self
    }
    /// Set horizontal alignment.
    pub fn align(mut self, h: HAlign) -> Self {
        self.align.get_or_insert_with(Alignment::default).horizontal = Some(h);
        self
    }
    /// Set vertical alignment.
    pub fn valign(mut self, v: VAlign) -> Self {
        self.align.get_or_insert_with(Alignment::default).vertical = Some(v);
        self
    }
    /// Set the alignment object.
    pub fn alignment(mut self, alignment: Alignment) -> Self {
        self.align = Some(alignment);
        self
    }
    /// Set the left indent in character units.
    pub fn indent(mut self, level: u8) -> Self {
        self.align.get_or_insert_with(Alignment::default).indent = level;
        self
    }
    /// Shrink text to fit within the cell width.
    pub fn shrink_to_fit(mut self) -> Self {
        self.align
            .get_or_insert_with(Alignment::default)
            .shrink_to_fit = true;
        self
    }
    /// Set text rotation in degrees (`-90..=90`).
    pub fn text_rotation(mut self, degrees: i16) -> Self {
        self.align.get_or_insert_with(Alignment::default).rotation = degrees.clamp(-90, 90);
        self
    }
    /// Explicitly lock the cell when worksheet protection is enabled.
    pub fn locked(mut self) -> Self {
        self.protection
            .get_or_insert_with(CellProtection::default)
            .locked = Some(true);
        self
    }
    /// Unlock the cell when worksheet protection is enabled.
    pub fn unlocked(mut self) -> Self {
        self.protection
            .get_or_insert_with(CellProtection::default)
            .locked = Some(false);
        self
    }
    /// Hide formula text when worksheet protection is enabled.
    pub fn hidden(mut self) -> Self {
        self.protection
            .get_or_insert_with(CellProtection::default)
            .hidden = true;
        self
    }
    /// Set the cell borders.
    pub fn border(mut self, b: Border) -> Self {
        self.border = Some(b);
        self
    }
    /// Set the top border edge style.
    pub fn border_top(mut self, style: FormatBorder) -> Self {
        self.border.get_or_insert_with(Border::default).top = style;
        self
    }
    /// Set the bottom border edge style.
    pub fn border_bottom(mut self, style: FormatBorder) -> Self {
        self.border.get_or_insert_with(Border::default).bottom = style;
        self
    }
    /// Set the left border edge style.
    pub fn border_left(mut self, style: FormatBorder) -> Self {
        self.border.get_or_insert_with(Border::default).left = style;
        self
    }
    /// Set the right border edge style.
    pub fn border_right(mut self, style: FormatBorder) -> Self {
        self.border.get_or_insert_with(Border::default).right = style;
        self
    }
    /// Set the top border edge color.
    pub fn border_top_color(mut self, color: impl Into<Color>) -> Self {
        self.border.get_or_insert_with(Border::default).top_color = Some(color.into());
        self
    }
    /// Set the bottom border edge color.
    pub fn border_bottom_color(mut self, color: impl Into<Color>) -> Self {
        self.border.get_or_insert_with(Border::default).bottom_color = Some(color.into());
        self
    }
    /// Set the left border edge color.
    pub fn border_left_color(mut self, color: impl Into<Color>) -> Self {
        self.border.get_or_insert_with(Border::default).left_color = Some(color.into());
        self
    }
    /// Set the right border edge color.
    pub fn border_right_color(mut self, color: impl Into<Color>) -> Self {
        self.border.get_or_insert_with(Border::default).right_color = Some(color.into());
        self
    }
    /// Set the font family name.
    pub fn set_font_name(self, name: impl AsRef<str>) -> Self {
        self.font_name(name)
    }
    /// Set the font size in points.
    pub fn set_font_size(self, points: u16) -> Self {
        self.size(points)
    }
    /// Set the text color.
    pub fn set_font_color(self, color: impl Into<Color>) -> Self {
        self.color(color)
    }
    /// Make the font bold.
    pub fn set_bold(self) -> Self {
        self.bold()
    }
    /// Make the font italic.
    pub fn set_italic(self) -> Self {
        self.italic()
    }

    /// Apply single underline to the font.
    pub fn set_underline(self) -> Self {
        self.underline()
    }

    /// Apply strikethrough to the font.
    pub fn set_font_strikethrough(self) -> Self {
        self.strikethrough()
    }

    /// Apply strikethrough to the font.
    pub fn set_strikethrough(self) -> Self {
        self.strikethrough()
    }

    /// Set the font superscript/subscript property.
    pub fn set_font_script(self, script: FormatScript) -> Self {
        self.font_script(script)
    }

    /// Set a solid background fill color.
    pub fn set_bg_color(self, color: impl Into<Color>) -> Self {
        self.fill(color)
    }
    /// Set the fill background color.
    pub fn set_background_color(self, color: impl Into<Color>) -> Self {
        self.background_color(color)
    }
    /// Set the fill foreground or pattern color.
    pub fn set_foreground_color(self, color: impl Into<Color>) -> Self {
        self.foreground_color(color)
    }
    /// Set the fill object.
    pub fn set_pattern_fill(self, fill: Fill) -> Self {
        self.pattern_fill(fill)
    }
    /// Set the fill pattern.
    pub fn set_pattern(self, pattern: FormatPattern) -> Self {
        self.pattern(pattern)
    }
    /// Set the number format code.
    pub fn set_num_format(self, code: impl AsRef<str>) -> Self {
        self.num_fmt(code)
    }
    /// Set horizontal alignment.
    pub fn set_align(self, h: FormatAlign) -> Self {
        self.align(h)
    }
    /// Set vertical alignment.
    pub fn set_valign(self, v: VAlign) -> Self {
        self.valign(v)
    }
    /// Set the alignment object.
    pub fn set_alignment(self, alignment: Alignment) -> Self {
        self.alignment(alignment)
    }
    /// Wrap long text within the cell.
    pub fn set_text_wrap(self) -> Self {
        self.wrap()
    }
    /// Set the left indent in character units.
    pub fn set_indent(self, level: u8) -> Self {
        self.indent(level)
    }
    /// Shrink text to fit within the cell width.
    pub fn set_shrink_to_fit(self) -> Self {
        self.shrink_to_fit()
    }
    /// Set text rotation in degrees (`-90..=90`).
    pub fn set_text_rotation(self, degrees: i16) -> Self {
        self.text_rotation(degrees)
    }
    /// Explicitly lock the cell when worksheet protection is enabled.
    pub fn set_locked(self) -> Self {
        self.locked()
    }
    /// Unlock the cell when worksheet protection is enabled.
    pub fn set_unlocked(self) -> Self {
        self.unlocked()
    }
    /// Hide formula text when worksheet protection is enabled.
    pub fn set_hidden(self) -> Self {
        self.hidden()
    }
    /// Set the same border style on every cell edge.
    pub fn set_border(mut self, style: FormatBorder) -> Self {
        let border = self.border.get_or_insert_with(Border::default);
        border.left = style;
        border.right = style;
        border.top = style;
        border.bottom = style;
        self
    }
    /// Set the top border edge style.
    pub fn set_border_top(self, style: FormatBorder) -> Self {
        self.border_top(style)
    }
    /// Set the bottom border edge style.
    pub fn set_border_bottom(self, style: FormatBorder) -> Self {
        self.border_bottom(style)
    }
    /// Set the left border edge style.
    pub fn set_border_left(self, style: FormatBorder) -> Self {
        self.border_left(style)
    }
    /// Set the right border edge style.
    pub fn set_border_right(self, style: FormatBorder) -> Self {
        self.border_right(style)
    }
    /// Set the top border edge color.
    pub fn set_border_top_color(self, color: impl Into<Color>) -> Self {
        self.border_top_color(color)
    }
    /// Set the bottom border edge color.
    pub fn set_border_bottom_color(self, color: impl Into<Color>) -> Self {
        self.border_bottom_color(color)
    }
    /// Set the left border edge color.
    pub fn set_border_left_color(self, color: impl Into<Color>) -> Self {
        self.border_left_color(color)
    }
    /// Set the right border edge color.
    pub fn set_border_right_color(self, color: impl Into<Color>) -> Self {
        self.border_right_color(color)
    }
    /// Set the color used by all configured border edges.
    pub fn set_border_color(mut self, color: impl Into<Color>) -> Self {
        self.border.get_or_insert_with(Border::default).color = Some(color.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatted_returns_display_text_while_cell_returns_typed_value() {
        let mut s = Sheet::new("s");
        s.write(0, 0, "공고명");
        s.write(0, 1, 1000_i64);
        s.write(0, 2, 0.5);
        s.write(0, 3, true);

        // formatted() yields the rendered display string used by to_text()...
        assert_eq!(s.formatted(0, 0), Some("공고명"));
        assert_eq!(s.formatted(0, 1), Some("1000"));
        assert_eq!(s.formatted(0, 2), Some("0.5"));
        assert_eq!(s.formatted(0, 3), Some("TRUE"));

        // ...while cell() yields the typed value, not a string.
        assert_eq!(s.cell(0, 0), Some(&Cell::Text("공고명".to_string())));
        assert_eq!(s.cell(0, 1), Some(&Cell::Number(1000.0)));
        assert_eq!(s.cell(0, 2), Some(&Cell::Number(0.5)));
        assert_eq!(s.cell(0, 3), Some(&Cell::Bool(true)));

        // An empty cell has no display text.
        assert_eq!(s.formatted(9, 9), None);
    }

    #[test]
    fn formatted_is_last_write_wins_like_cell() {
        let mut s = Sheet::new("s");
        s.write(0, 0, 1_i64);
        s.write(0, 0, 2_i64);
        assert_eq!(s.formatted(0, 0), Some("2"));
        assert_eq!(s.cell(0, 0), Some(&Cell::Number(2.0)));
    }

    // Regression tests: HTML gap-fill column alignment and CSV delimiter
    // validation.

    #[test]
    fn to_html_fills_unwritten_gap_in_middle_of_row_so_columns_stay_aligned() {
        let mut s = Sheet::new("s");
        s.write(0, 0, "Name");
        s.write(0, 1, "Age");
        s.write(0, 2, "City");
        // col1 is deliberately never written on the data row.
        s.write(1, 0, "Alice");
        s.write(1, 2, "Seattle");

        let html = s.to_html();
        let data_row = html
            .split("</tr>")
            .find(|row| row.contains("Alice"))
            .expect("data row present");
        let tds: Vec<&str> = data_row.matches("<td").collect();
        assert_eq!(
            tds.len(),
            3,
            "expected exactly 3 <td> in the data row, got: {data_row}"
        );
        assert_eq!(
            data_row, "<tr><td>Alice</td><td></td><td>Seattle</td>",
            "Seattle must land in the 3rd <td>, not shift into the 2nd \
             because the unwritten col1 was skipped entirely"
        );
    }

    #[test]
    fn to_html_merge_anchor_without_cell_entry_still_emits_td() {
        let mut s = Sheet::new("s");
        s.merge(0, 0, 0, 1);
        // Only the covered cell (0,1) is written; the anchor (0,0) never is.
        s.write(0, 1, "stray");

        let html = s.to_html();
        assert_eq!(
            html, "<table><tr><td colspan=\"2\"></td></tr></table>",
            "the merge anchor must render an empty <td colspan=\"2\"> instead \
             of vanishing (and the covered cell's stray text must stay \
             hidden, matching real merge semantics)"
        );
    }

    #[test]
    fn sheet_to_csv_with_delimiter_normalizes_quote_delimiter_to_comma() {
        let mut s = Sheet::new("s");
        s.write(0, 0, "has \"quote\" inside");
        s.write(0, 1, "plain");

        // '"' as a delimiter is inherently ambiguous (field separator and
        // quoted-field boundary collide); Sheet::to_csv_with_delimiter can't
        // signal failure via its String return type, so it must normalize
        // to the default ',' instead of emitting the ambiguous output that
        // treating '"' literally as the delimiter would produce.
        let out = s.to_csv_with_delimiter('"');
        assert_eq!(
            out,
            s.to_csv(),
            "invalid '\"' delimiter should fall back to ','"
        );
        assert!(
            !out.contains("\"\"\"\""),
            "must not emit the ambiguous quadruple-quote output: {out}"
        );
    }

    #[test]
    fn workbook_to_csv_with_delimiter_rejects_quote_delimiter() {
        let mut wb = Workbook::new();
        {
            let s = wb.add_sheet("CSV");
            s.write(0, 0, "has \"quote\" inside");
        }

        assert_eq!(
            wb.to_csv_with_delimiter(0, '"'),
            None,
            "'\"' is not a valid delimiter and should be rejected like an invalid sheet index"
        );
    }
}
