//! Worksheet comments, tables, media, charts, validation, and protection.

use super::super::{CellStyle, Color, StyleLoss};

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

/// Author input for [`crate::Sheet::add_comment`].
///
/// Existing `Some("author")` and `None` calls are supported, and passing a
/// direct `String` or `&str` stores that value as the comment author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentAuthor(pub(in crate::model) Option<String>);

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

/// Read-side OOXML details retained beside a public [`CondFormat`].
///
/// This sidecar keeps the long-standing `CondFormat { sqref, rule }` literal
/// source-compatible while preserving rule ordering and differential styles
/// that cannot be represented by [`CfRule`] alone. Entries returned by
/// [`crate::Sheet::conditional_format_metadata`] are aligned with
/// [`crate::Sheet::conditional_formats`] by index when present.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConditionalFormatMetadata {
    /// Source `cfRule` priority. `None` preserves a missing or invalid value;
    /// authored rules use document order instead.
    pub priority: Option<u32>,
    /// Whether a matching rule prevents lower-priority rules from applying.
    pub stop_if_true: bool,
    /// Full differential style retained from the referenced OOXML `dxf`.
    pub differential_style: Option<CellStyle>,
    /// Typed losses encountered while parsing the referenced differential style.
    pub style_losses: Vec<StyleLoss>,
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
/// [`crate::Sheet::protect`]. Pass to [`crate::Sheet::protect_with`].
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
