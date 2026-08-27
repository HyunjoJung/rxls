//! Sparse worksheet range views and iterators.

use std::collections::{btree_map::Range as BTreeMapRange, BTreeMap};

use super::{display_text, Cell};

#[cfg(feature = "serde")]
mod de;

#[cfg(all(feature = "serde", feature = "chrono"))]
pub use de::{
    deserialize_as_date_1900_or_none, deserialize_as_date_1900_or_string,
    deserialize_as_date_1904_or_none, deserialize_as_date_1904_or_string,
    deserialize_as_datetime_1900_or_none, deserialize_as_datetime_1900_or_string,
    deserialize_as_datetime_1904_or_none, deserialize_as_datetime_1904_or_string,
    deserialize_as_duration_or_none, deserialize_as_duration_or_string,
    deserialize_as_time_1900_or_none, deserialize_as_time_1900_or_string,
    deserialize_as_time_1904_or_none, deserialize_as_time_1904_or_string,
};
#[cfg(feature = "serde")]
pub use de::{
    deserialize_as_f64_or_none, deserialize_as_f64_or_string, deserialize_as_i64_or_none,
    deserialize_as_i64_or_string, DeError, RangeDeserializer, RangeDeserializerBuilder,
};

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

/// A rectangular, calamine-style view over a worksheet's effective cells.
///
/// `Range` is built from a [`crate::Sheet`] using Excel's last-write-wins
/// semantics for duplicate coordinates. Positions passed to [`Range::get`] are
/// relative to the range start; absolute positions are available from
/// [`Range::used_cells_abs`].
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
/// are populated. It is returned by [`crate::Workbook::worksheet_formula`].
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

impl<'right> PartialEq<RangeCell<'right>> for RangeCell<'_> {
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

impl<'right> PartialEq<FormulaEntry<'right>> for FormulaEntry<'_> {
    fn eq(&self, other: &FormulaEntry<'right>) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for FormulaEntry<'_> {}

impl<'right> PartialEq<Range<'right>> for Range<'_> {
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

impl<'right> PartialEq<FormulaRange<'right>> for FormulaRange<'_> {
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

impl Eq for FormulaRange<'_> {}

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

    pub(super) fn from_borrowed_cells<I>(source: I) -> Self
    where
        I: IntoIterator<Item = (u32, u16, &'a Cell, &'a str)>,
    {
        let mut cells = BTreeMap::new();
        for (row, col, value, text) in source {
            cells.insert((row, col), RangeCell::Borrowed { value, text });
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

impl ExactSizeIterator for RangeRows<'_, '_> {}

impl DoubleEndedIterator for RangeRows<'_, '_> {
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

impl std::iter::FusedIterator for RangeRows<'_, '_> {}

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

impl<'range> Iterator for RangeRowUsedCells<'range, '_> {
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

impl ExactSizeIterator for RangeRowUsedCells<'_, '_> {
    fn len(&self) -> usize {
        self.remaining
    }
}

impl DoubleEndedIterator for RangeRowUsedCells<'_, '_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let (&(_, col), entry) = self.entries.next_back()?;
        self.remaining = self.remaining.saturating_sub(1);
        Some((col, entry.value()))
    }
}

impl std::iter::FusedIterator for RangeRowUsedCells<'_, '_> {}

/// Iterator over one borrowed [`RangeRow`]'s cells.
#[derive(Debug, Clone)]
pub struct RangeRowCells<'range, 'cell> {
    range: &'range Range<'cell>,
    row: u32,
    next_col: u16,
    end_col: u16,
    done: bool,
}

impl<'range> Iterator for RangeRowCells<'range, '_> {
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

impl ExactSizeIterator for RangeRowCells<'_, '_> {}

impl DoubleEndedIterator for RangeRowCells<'_, '_> {
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

impl std::iter::FusedIterator for RangeRowCells<'_, '_> {}

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

    pub(super) fn from_borrowed_cells<I>(source: I) -> Self
    where
        I: IntoIterator<Item = (u32, u16, &'a Cell)>,
    {
        let mut formulas = BTreeMap::new();
        for (row, col, value) in source {
            if let Cell::Formula { formula, .. } = value {
                formulas.insert((row, col), FormulaEntry::Borrowed(formula.as_str()));
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

impl ExactSizeIterator for FormulaRangeRows<'_, '_> {}

impl DoubleEndedIterator for FormulaRangeRows<'_, '_> {
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

impl std::iter::FusedIterator for FormulaRangeRows<'_, '_> {}

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

impl<'range> Iterator for FormulaRangeRowUsedCells<'range, '_> {
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

impl ExactSizeIterator for FormulaRangeRowUsedCells<'_, '_> {
    fn len(&self) -> usize {
        self.remaining
    }
}

impl DoubleEndedIterator for FormulaRangeRowUsedCells<'_, '_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let (&(_, col), formula) = self.entries.next_back()?;
        self.remaining = self.remaining.saturating_sub(1);
        Some((col, formula.as_str()))
    }
}

impl std::iter::FusedIterator for FormulaRangeRowUsedCells<'_, '_> {}

/// Iterator over one borrowed [`FormulaRangeRow`]'s formulas.
#[derive(Debug, Clone)]
pub struct FormulaRangeRowCells<'range, 'formula> {
    range: &'range FormulaRange<'formula>,
    row: u32,
    next_col: u16,
    end_col: u16,
    done: bool,
}

impl<'range> Iterator for FormulaRangeRowCells<'range, '_> {
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

impl ExactSizeIterator for FormulaRangeRowCells<'_, '_> {}

impl DoubleEndedIterator for FormulaRangeRowCells<'_, '_> {
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

impl std::iter::FusedIterator for FormulaRangeRowCells<'_, '_> {}
