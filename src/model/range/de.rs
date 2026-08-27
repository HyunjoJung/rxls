//! Typed Serde deserialization for worksheet ranges.

use serde::de::IntoDeserializer;

use super::super::{display_text, Cell, HeaderRow};
use super::{col_span_len, Range, RangeCell};

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
impl<'de> serde::de::Deserializer<'de> for HeaderExtractor<'_> {
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
impl<'de, 'cell: 'de> serde::de::Deserializer<'de> for RowDeserializer<'_, 'cell> {
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
impl<'de, 'cell: 'de> serde::de::SeqAccess<'de> for RowSeqAccess<'_, 'cell> {
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
impl<'de, 'cell: 'de> serde::de::MapAccess<'de> for RowMapAccess<'_, 'cell> {
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
