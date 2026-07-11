//! Pre-emission validation for the **checked** authoring path
//! ([`Workbook::to_xlsx_checked`]).
//!
//! The infallible [`to_xlsx`](super::to_xlsx) deliberately *sanitizes* invalid
//! input — it clamps out-of-grid coordinates, rewrites illegal sheet/table names,
//! and de-duplicates collisions — so it can never fail. That is the right default
//! for a best-effort `.xls → .xlsx` conversion, but it hides authoring mistakes:
//! a caller that builds a `Workbook` programmatically gets a *different* file than
//! it described, silently.
//!
//! [`validate`] is the opt-in counterpart. It inspects the same `Workbook` and
//! reports the first problem [`to_xlsx`](super::to_xlsx) *would* have papered over,
//! as a typed [`WriteError`]. It does not mutate anything and emits no bytes.

use std::collections::HashSet;
use std::fmt;

use super::workbook::is_w3cdtf;
use crate::write::{MAX_COL, MAX_ROW, MAX_SHEETS};
use crate::{Cell, CellStyle, CfRule, DataValidation, DvKind, DvOp, Font, Sheet, Workbook};

/// A problem found by the checked validator *before* any `.xlsx` bytes are emitted.
///
/// Returned by [`Workbook::to_xlsx_checked`]. The infallible
/// [`Workbook::to_xlsx`](crate::Workbook::to_xlsx) never produces these — it
/// silently sanitizes the same inputs instead.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteError {
    /// Two tables share a display name (compared case-insensitively across the
    /// whole workbook). Excel rejects duplicate table names. Carries the
    /// offending name.
    DuplicateTableName(String),
    /// Two workbook-global defined names share a name, compared
    /// case-insensitively. Carries the offending name.
    DuplicateDefinedName(String),
    /// A workbook-global defined name is not a valid Excel name: it is empty,
    /// starts with an invalid character, contains an invalid character, or looks
    /// like an A1/R1C1 cell reference. Carries the offending name.
    InvalidDefinedName(String),
    /// A table name is not a valid Excel defined name (starts with a digit,
    /// contains whitespace, or looks like a cell reference such as `A1` or
    /// `R1C1`). Carries the offending name.
    InvalidTableName(String),
    /// A table's range width does not match the number of declared header
    /// columns. Carries the offending table name.
    TableRangeColumnsMismatch {
        /// The table whose range width ≠ `columns.len()`.
        table: String,
    },
    /// A table header format was assigned to a table name that does not exist on
    /// the same sheet. Carries the missing authored table name.
    UnknownTableHeaderFormat {
        /// The table name passed to `Sheet::set_table_header_format`.
        table: String,
    },
    /// A cell sits outside the Excel grid (`row > 1_048_575` or `col > 16_383`).
    CellOutOfGrid {
        /// 0-based row index that exceeds the grid.
        row: u32,
        /// 0-based column index that exceeds the grid.
        col: u16,
    },
    /// A value or format-only blank cell sits inside a merged range but is not
    /// that merge's top-left cell. The unchecked writer omits such cells so
    /// Excel will not repair the file; checked writes report them instead.
    CellUnderMergedRange {
        /// 0-based row index of the hidden authored cell.
        row: u32,
        /// 0-based column index of the hidden authored cell.
        col: u16,
    },
    /// A numeric or date cell carries NaN/infinity, which Excel cannot store.
    NonFiniteNumber {
        /// 0-based row index of the authored cell.
        row: u32,
        /// 0-based column index of the authored cell.
        col: u16,
    },
    /// An authored hyperlink target is empty or contains XML-forbidden control
    /// characters that the unchecked writer would drop before emission. Carries
    /// the 0-based cell coordinate and original target.
    InvalidHyperlinkTarget {
        /// 0-based row index of the authored hyperlink cell.
        row: u32,
        /// 0-based column index of the authored hyperlink cell.
        col: u16,
        /// Authored target.
        target: String,
    },
    /// An authored XML text field contains characters that XML 1.0 forbids and
    /// the unchecked writer would drop. Carries the field label and original
    /// authored value.
    InvalidXmlText {
        /// Human-readable authored field label.
        field: String,
        /// Original authored value.
        value: String,
    },
    /// A checked range or drawing anchor extends past the Excel grid, or is
    /// reversed (last row/col before first — e.g. `F1:D3`).
    MergeOutOfGrid,
    /// Row/column layout metadata cannot be represented exactly in Excel: a
    /// row/column/default size is non-finite or negative, an outline level is
    /// above Excel's limit, or a layout row/column points outside the grid.
    /// Carries a human-readable detail.
    InvalidSheetLayout(String),
    /// A sheet-view zoom percentage is outside Excel's valid 10..=400 range.
    InvalidSheetViewZoom {
        /// Authored zoom value.
        value: u16,
    },
    /// A chart series/category/value/bubble-size reference is not a simple
    /// in-grid A1 range such as `Sheet1!$A$1:$A$5`. Carries the offending
    /// reference.
    InvalidChartReference(String),
    /// A sparkline source reference is not a simple in-grid A1 range such as
    /// `Sheet1!$A$1:$A$5`. Carries the offending reference.
    InvalidSparklineReference(String),
    /// A conditional-formatting rule has invalid authoring metadata.
    InvalidConditionalFormatRule(String),
    /// A data-validation rule has invalid authoring metadata.
    InvalidDataValidationRule(String),
    /// A sheet name is not acceptable to Excel: empty, longer than 31 chars, or
    /// containing one of `: \ / ? * [ ]`. Carries the offending name.
    InvalidSheetName(String),
    /// Two sheets share a name (compared case-insensitively). Carries the
    /// offending name.
    DuplicateSheetName(String),
    /// The workbook has more worksheets than Excel allows.
    TooManySheets,
    /// The active sheet index points past the authored sheet list. Carries the
    /// requested index and current sheet count.
    ActiveSheetOutOfRange {
        /// Requested active sheet index.
        index: usize,
        /// Number of sheets in the workbook.
        sheets: usize,
    },
    /// A page setup scale percentage is outside Excel's valid 10..=400 range.
    InvalidPageSetupScale {
        /// Authored scale value.
        value: u16,
    },
    /// Page setup margins must be finite, non-negative inch values.
    InvalidPageSetupMargins,
    /// The document-property `created` timestamp is not a W3CDTF-shaped value.
    /// Carries the offending authored value.
    InvalidDocPropertyTimestamp(String),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::DuplicateTableName(name) => {
                write!(f, "duplicate table name (case-insensitive): {name:?}")
            }
            WriteError::DuplicateDefinedName(name) => {
                write!(f, "duplicate defined name (case-insensitive): {name:?}")
            }
            WriteError::InvalidDefinedName(name) => {
                write!(f, "invalid Excel defined name: {name:?}")
            }
            WriteError::InvalidTableName(name) => {
                write!(f, "invalid Excel table name: {name:?}")
            }
            WriteError::TableRangeColumnsMismatch { table } => write!(
                f,
                "table {table:?}: range width does not match the number of columns"
            ),
            WriteError::UnknownTableHeaderFormat { table } => {
                write!(
                    f,
                    "table header format references an unknown table: {table:?}"
                )
            }
            WriteError::CellOutOfGrid { row, col } => write!(
                f,
                "cell out of grid at row {row}, col {col} (max row {MAX_ROW}, max col {MAX_COL})"
            ),
            WriteError::CellUnderMergedRange { row, col } => write!(
                f,
                "cell at row {row}, col {col} is hidden by a merged range"
            ),
            WriteError::NonFiniteNumber { row, col } => write!(
                f,
                "non-finite numeric cell at row {row}, col {col} cannot be written to Excel"
            ),
            WriteError::InvalidHyperlinkTarget { row, col, target } => write!(
                f,
                "invalid hyperlink target at row {row}, col {col}: {target:?}"
            ),
            WriteError::InvalidXmlText { field, value } => {
                write!(f, "{field} contains XML-forbidden characters: {value:?}")
            }
            WriteError::MergeOutOfGrid => {
                write!(
                    f,
                    "a checked range or drawing anchor extends past the Excel grid"
                )
            }
            WriteError::InvalidSheetLayout(detail) => {
                write!(f, "invalid sheet layout metadata: {detail}")
            }
            WriteError::InvalidSheetViewZoom { value } => write!(
                f,
                "sheet view zoom {value} is outside Excel's valid 10..=400 range"
            ),
            WriteError::InvalidChartReference(reference) => {
                write!(f, "invalid chart range reference: {reference:?}")
            }
            WriteError::InvalidSparklineReference(reference) => {
                write!(f, "invalid sparkline range reference: {reference:?}")
            }
            WriteError::InvalidConditionalFormatRule(detail) => {
                write!(f, "invalid conditional-formatting rule: {detail}")
            }
            WriteError::InvalidDataValidationRule(detail) => {
                write!(f, "invalid data-validation rule: {detail}")
            }
            WriteError::InvalidSheetName(name) => {
                write!(f, "invalid Excel sheet name: {name:?}")
            }
            WriteError::DuplicateSheetName(name) => {
                write!(f, "duplicate sheet name (case-insensitive): {name:?}")
            }
            WriteError::TooManySheets => {
                write!(f, "too many sheets (Excel allows at most {MAX_SHEETS})")
            }
            WriteError::ActiveSheetOutOfRange { index, sheets } => write!(
                f,
                "active sheet index {index} is out of range for {sheets} sheet(s)"
            ),
            WriteError::InvalidPageSetupScale { value } => write!(
                f,
                "page setup scale {value} is outside Excel's valid 10..=400 range"
            ),
            WriteError::InvalidPageSetupMargins => {
                write!(f, "page setup margins must be finite and non-negative")
            }
            WriteError::InvalidDocPropertyTimestamp(value) => {
                write!(f, "invalid document property created timestamp: {value:?}")
            }
        }
    }
}

impl fmt::Debug for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Forward to Display so the variant payload is human-readable in test
        // failures and `?`-propagated errors alike.
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for WriteError {}

/// Validate `wb` against Excel's structural rules **before** emission, returning
/// the first problem the infallible [`to_xlsx`](super::to_xlsx) would otherwise
/// have silently sanitized away — sheet/table names, grid bounds (cells, comments,
/// finite numeric cell values, authored cell/formula XML text, row/column layout
/// metadata, merges, autofilter, hyperlink targets, table ranges/header-format
/// targets, drawing anchors, conditional formats, data validations, chart series
/// references, sparkline source ranges, print setup ranges, sheet-view zoom,
/// page setup scale/margins, document-property text/timestamps, and
/// active-sheet index), range ordering, and table
/// width-vs-columns.
/// The name/grid checks are kept in lock-step with the writer's sanitizers, so for
/// those surfaces `Ok(())` means [`to_xlsx`](super::to_xlsx) emits exactly what the
/// `Workbook` describes. It is a best-effort pre-flight, not exhaustive: it still
/// does not validate formula syntax or consumer-specific rendering details.
pub(crate) fn validate(wb: &Workbook) -> Result<(), WriteError> {
    // Sheet count first: a workbook far over the cap is the most fundamental
    // problem and the cheapest to detect.
    if wb.sheets.len() > MAX_SHEETS {
        return Err(WriteError::TooManySheets);
    }
    if !wb.sheets.is_empty() && wb.active_sheet >= wb.sheets.len() {
        return Err(WriteError::ActiveSheetOutOfRange {
            index: wb.active_sheet,
            sheets: wb.sheets.len(),
        });
    }
    validate_doc_properties(&wb.properties)?;
    validate_defined_names(wb)?;

    // Sheet names: validity, then duplicates (case-insensitive).
    let mut seen_sheets: HashSet<String> = HashSet::with_capacity(wb.sheets.len());
    for sheet in &wb.sheets {
        if !is_valid_sheet_name(&sheet.name) {
            return Err(WriteError::InvalidSheetName(sheet.name.clone()));
        }
        if !seen_sheets.insert(sheet.name.to_lowercase()) {
            return Err(WriteError::DuplicateSheetName(sheet.name.clone()));
        }
    }

    // Per-sheet structure: layout metadata, cells, comments, ranges, tables.
    for sheet in &wb.sheets {
        validate_sheet_view(sheet)?;
        validate_sheet_layout(sheet)?;
        for cell in &sheet.cells {
            if cell.row > MAX_ROW || cell.col > MAX_COL {
                return Err(WriteError::CellOutOfGrid {
                    row: cell.row,
                    col: cell.col,
                });
            }
            if !cell_number_is_finite(&cell.value) {
                return Err(WriteError::NonFiniteNumber {
                    row: cell.row,
                    col: cell.col,
                });
            }
            if let Some(runs) = sheet.rich.get(&(cell.row, cell.col)) {
                validate_rich_text_runs(runs)?;
            } else {
                validate_cell_xml_text(&cell.value)?;
            }
            if let Some(style) = &cell.style {
                validate_style_xml_text(style)?;
            }
            if let Some(target) = &cell.hyperlink {
                validate_hyperlink_target(cell.row, cell.col, target)?;
            }
        }
        for c in &sheet.comments {
            if c.row > MAX_ROW || c.col > MAX_COL {
                return Err(WriteError::CellOutOfGrid {
                    row: c.row,
                    col: c.col,
                });
            }
            validate_xml_text("comment text", &c.text)?;
            if let Some(author) = &c.author {
                validate_xml_text("comment author", author)?;
            }
        }
        for &(row, col) in sheet.blank_styles.keys() {
            if row > MAX_ROW || col > MAX_COL {
                return Err(WriteError::CellOutOfGrid { row, col });
            }
        }
        for style in sheet.blank_styles.values() {
            validate_style_xml_text(style)?;
        }
        for style in sheet.row_formats.values() {
            validate_style_xml_text(style)?;
        }
        for style in sheet.col_formats.values() {
            validate_style_xml_text(style)?;
        }
        if let Some(style) = &sheet.default_format {
            validate_style_xml_text(style)?;
        }
        for runs in sheet.rich.values() {
            validate_rich_text_runs(runs)?;
        }
        for &(r0, c0, r1, c1) in &sheet.merges {
            if !range_in_grid_ordered(r0, c0, r1, c1) {
                return Err(WriteError::MergeOutOfGrid);
            }
        }
        for cell in &sheet.cells {
            if under_merged_range(&sheet.merges, cell.row, cell.col) {
                return Err(WriteError::CellUnderMergedRange {
                    row: cell.row,
                    col: cell.col,
                });
            }
        }
        for &(row, col) in sheet.blank_styles.keys() {
            if under_merged_range(&sheet.merges, row, col) {
                return Err(WriteError::CellUnderMergedRange { row, col });
            }
        }
        if let Some((r0, c0, r1, c1)) = sheet.autofilter {
            if !range_in_grid_ordered(r0, c0, r1, c1) {
                return Err(WriteError::MergeOutOfGrid);
            }
        }
        for cf in &sheet.cond_formats {
            let (r0, c0, r1, c1) = cf.sqref;
            if !range_in_grid_ordered(r0, c0, r1, c1) {
                return Err(WriteError::MergeOutOfGrid);
            }
            validate_conditional_format_rule(&cf.rule)?;
        }
        for dv in &sheet.data_validations {
            let (r0, c0, r1, c1) = dv.sqref;
            if !range_in_grid_ordered(r0, c0, r1, c1) {
                return Err(WriteError::MergeOutOfGrid);
            }
            validate_data_validation_rule(dv)?;
        }
        if let Some(ps) = &sheet.page_setup {
            if let Some((r0, c0, r1, c1)) = ps.print_area {
                if !range_in_grid_ordered(r0, c0, r1, c1) {
                    return Err(WriteError::MergeOutOfGrid);
                }
            }
            if let Some((first, last)) = ps.repeat_rows {
                if first > MAX_ROW || last > MAX_ROW || first > last {
                    return Err(WriteError::MergeOutOfGrid);
                }
            }
            if let Some((first, last)) = ps.repeat_cols {
                if first > MAX_COL || last > MAX_COL || first > last {
                    return Err(WriteError::MergeOutOfGrid);
                }
            }
            if let Some(value) = ps.scale {
                if !(10..=400).contains(&value) {
                    return Err(WriteError::InvalidPageSetupScale { value });
                }
            }
            if let Some((left, right, top, bottom, header, footer)) = ps.margins {
                if [left, right, top, bottom, header, footer]
                    .iter()
                    .any(|margin| !margin.is_finite() || *margin < 0.0)
                {
                    return Err(WriteError::InvalidPageSetupMargins);
                }
            }
            if let Some(header) = &ps.header {
                validate_xml_text("page setup header", header)?;
            }
            if let Some(footer) = &ps.footer {
                validate_xml_text("page setup footer", footer)?;
            }
        }
        for img in &sheet.images {
            let (r0, c0) = img.from;
            let (r1, c1) = img
                .to
                .unwrap_or((r0.saturating_add(10), c0.saturating_add(4)));
            if !range_in_grid_ordered(r0, c0, r1, c1) {
                return Err(WriteError::MergeOutOfGrid);
            }
        }
        for chart in &sheet.charts {
            let (r0, c0) = chart.from;
            let (r1, c1) = chart.to;
            if !range_in_grid_ordered(r0, c0, r1, c1) {
                return Err(WriteError::MergeOutOfGrid);
            }
            if let Some(title) = &chart.title {
                validate_xml_text("chart title", title)?;
            }
            if let Some(title) = &chart.x_axis_title {
                validate_xml_text("chart x-axis title", title)?;
            }
            if let Some(title) = &chart.y_axis_title {
                validate_xml_text("chart y-axis title", title)?;
            }
            for series in &chart.series {
                if let Some(name) = &series.name {
                    validate_xml_text("chart series name", name)?;
                }
                if let Some(categories) = &series.categories {
                    validate_xml_text("chart categories reference", categories)?;
                    validate_chart_range_ref(categories)?;
                }
                validate_xml_text("chart values reference", &series.values)?;
                validate_chart_range_ref(&series.values)?;
                if let Some(bubble_sizes) = &series.bubble_sizes {
                    validate_xml_text("chart bubble-size reference", bubble_sizes)?;
                    validate_chart_range_ref(bubble_sizes)?;
                }
            }
        }
        for sparkline in &sheet.sparklines {
            let (row, col) = sparkline.location;
            if row > MAX_ROW || col > MAX_COL {
                return Err(WriteError::MergeOutOfGrid);
            }
            validate_xml_text("sparkline reference", &sparkline.range)?;
            validate_sparkline_range_ref(&sparkline.range)?;
        }
        for t in &sheet.tables {
            let (r0, c0, r1, c1) = t.range;
            if !range_in_grid_ordered(r0, c0, r1, c1) {
                return Err(WriteError::MergeOutOfGrid);
            }
            if !is_valid_table_name(&t.name) {
                return Err(WriteError::InvalidTableName(t.name.clone()));
            }
            // Range width (inclusive) must equal the declared header columns.
            let width = u32::from(c1 - c0) + 1;
            if width != t.columns.len() as u32 {
                return Err(WriteError::TableRangeColumnsMismatch {
                    table: t.name.clone(),
                });
            }
            for column in &t.columns {
                validate_xml_text("table column", column)?;
            }
            if let Some(style) = &t.style {
                validate_xml_text("table style", style)?;
            }
        }
        let table_names: HashSet<&str> = sheet
            .tables
            .iter()
            .map(|table| table.name.as_str())
            .collect();
        for table_name in sheet.table_header_formats.keys() {
            if !table_names.contains(table_name.as_str()) {
                return Err(WriteError::UnknownTableHeaderFormat {
                    table: table_name.clone(),
                });
            }
        }
        for style in sheet.table_header_formats.values() {
            validate_style_xml_text(style)?;
        }
    }

    // Duplicate table display names are scoped to the whole workbook.
    let mut seen_tables: HashSet<String> = HashSet::new();
    for sheet in &wb.sheets {
        for t in &sheet.tables {
            if !seen_tables.insert(t.name.to_lowercase()) {
                return Err(WriteError::DuplicateTableName(t.name.clone()));
            }
        }
    }

    Ok(())
}

fn validate_defined_names(wb: &Workbook) -> Result<(), WriteError> {
    let mut seen_names: HashSet<String> = HashSet::with_capacity(wb.defined_names.len());
    for (name, refers_to) in &wb.defined_names {
        if !is_valid_defined_name(name) {
            return Err(WriteError::InvalidDefinedName(name.clone()));
        }
        validate_xml_text("defined name formula", refers_to)?;
        if !seen_names.insert(name.to_lowercase()) {
            return Err(WriteError::DuplicateDefinedName(name.clone()));
        }
    }
    Ok(())
}

/// A range is acceptable iff every endpoint is in the grid **and** it is ordered
/// (`r0 ≤ r1`, `c0 ≤ c1`) — a reversed range emits an invalid `F1:D3`-style ref.
fn range_in_grid_ordered(r0: u32, c0: u16, r1: u32, c1: u16) -> bool {
    r0 <= MAX_ROW && r1 <= MAX_ROW && c0 <= MAX_COL && c1 <= MAX_COL && r0 <= r1 && c0 <= c1
}

fn under_merged_range(merges: &[(u32, u16, u32, u16)], row: u32, col: u16) -> bool {
    merges.iter().any(|&(r0, c0, r1, c1)| {
        row >= r0 && row <= r1 && col >= c0 && col <= c1 && (row, col) != (r0, c0)
    })
}

fn validate_sheet_view(sheet: &Sheet) -> Result<(), WriteError> {
    if let Some(value) = sheet.zoom {
        if !(10..=400).contains(&value) {
            return Err(WriteError::InvalidSheetViewZoom { value });
        }
    }
    Ok(())
}

fn validate_sheet_layout(sheet: &Sheet) -> Result<(), WriteError> {
    if let Some(points) = sheet.default_row_height {
        validate_layout_measure("default row height", points)?;
    }
    if let Some(chars) = sheet.default_col_width {
        validate_layout_measure("default column width", chars)?;
    }

    for (&row, &points) in &sheet.row_heights {
        validate_layout_row("row height", row)?;
        validate_layout_measure("row height", points)?;
    }
    for (&col, &chars) in &sheet.col_widths {
        validate_layout_col("column width", col)?;
        validate_layout_measure("column width", chars)?;
    }
    for &row in sheet.row_formats.keys() {
        validate_layout_row("row format", row)?;
    }
    for &col in sheet.col_formats.keys() {
        validate_layout_col("column format", col)?;
    }
    for (&row, &level) in &sheet.row_outline {
        validate_layout_row("row outline", row)?;
        validate_outline_level("row outline", level)?;
    }
    for (&col, &level) in &sheet.col_outline {
        validate_layout_col("column outline", col)?;
        validate_outline_level("column outline", level)?;
    }
    for &row in &sheet.collapsed_rows {
        validate_layout_row("collapsed row", row)?;
    }

    Ok(())
}

fn validate_layout_measure(name: &str, value: f32) -> Result<(), WriteError> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(WriteError::InvalidSheetLayout(format!(
            "{name} must be finite and non-negative, got {value}"
        )))
    }
}

fn validate_layout_row(name: &str, row: u32) -> Result<(), WriteError> {
    if row <= MAX_ROW {
        Ok(())
    } else {
        Err(WriteError::InvalidSheetLayout(format!(
            "{name} row {row} exceeds max row {MAX_ROW}"
        )))
    }
}

fn validate_layout_col(name: &str, col: u16) -> Result<(), WriteError> {
    if col <= MAX_COL {
        Ok(())
    } else {
        Err(WriteError::InvalidSheetLayout(format!(
            "{name} column {col} exceeds max column {MAX_COL}"
        )))
    }
}

fn validate_outline_level(name: &str, level: u8) -> Result<(), WriteError> {
    if level <= 7 {
        Ok(())
    } else {
        Err(WriteError::InvalidSheetLayout(format!(
            "{name} level {level} exceeds Excel's max outline level 7"
        )))
    }
}

fn cell_number_is_finite(cell: &Cell) -> bool {
    match cell {
        Cell::Number(n) | Cell::Date(n) => n.is_finite(),
        Cell::Formula { cached, .. } => cell_number_is_finite(cached),
        _ => true,
    }
}

fn validate_cell_xml_text(cell: &Cell) -> Result<(), WriteError> {
    match cell {
        Cell::Text(value) => validate_xml_text("cell text", value),
        Cell::Error(value) => validate_xml_text("cell error", value),
        Cell::Formula { formula, cached } => {
            validate_xml_text("formula", formula)?;
            validate_formula_cached_xml_text(cached)
        }
        Cell::Number(_) | Cell::Date(_) | Cell::Bool(_) => Ok(()),
    }
}

fn validate_formula_cached_xml_text(cell: &Cell) -> Result<(), WriteError> {
    match cell {
        Cell::Text(value) => validate_xml_text("formula cached text", value),
        Cell::Error(value) => validate_xml_text("formula cached error", value),
        Cell::Number(_) | Cell::Date(_) | Cell::Bool(_) | Cell::Formula { .. } => Ok(()),
    }
}

fn validate_style_xml_text(style: &CellStyle) -> Result<(), WriteError> {
    if let Some(font) = &style.font {
        validate_font_xml_text(font)?;
    }
    if let Some(value) = &style.num_fmt {
        validate_xml_text("number format", value)?;
    }
    Ok(())
}

fn validate_font_xml_text(font: &Font) -> Result<(), WriteError> {
    if let Some(value) = &font.name {
        validate_xml_text("font name", value)?;
    }
    Ok(())
}

fn validate_rich_text_runs(runs: &[crate::TextRun]) -> Result<(), WriteError> {
    for run in runs {
        validate_xml_text("rich string text", &run.text)?;
        validate_font_xml_text(&run.font)?;
    }
    Ok(())
}

fn validate_doc_properties(properties: &crate::DocProperties) -> Result<(), WriteError> {
    if let Some(value) = &properties.title {
        validate_xml_text("document property title", value)?;
    }
    if let Some(value) = &properties.subject {
        validate_xml_text("document property subject", value)?;
    }
    if let Some(value) = &properties.creator {
        validate_xml_text("document property creator", value)?;
    }
    if let Some(value) = &properties.keywords {
        validate_xml_text("document property keywords", value)?;
    }
    if let Some(value) = &properties.description {
        validate_xml_text("document property description", value)?;
    }
    if let Some(value) = &properties.last_modified_by {
        validate_xml_text("document property lastModifiedBy", value)?;
    }
    if let Some(value) = &properties.company {
        validate_xml_text("document property company", value)?;
    }
    if let Some(value) = &properties.created {
        validate_xml_text("document property created", value)?;
        if !is_w3cdtf(value) {
            return Err(WriteError::InvalidDocPropertyTimestamp(value.clone()));
        }
    }
    Ok(())
}

fn validate_hyperlink_target(row: u32, col: u16, target: &str) -> Result<(), WriteError> {
    if is_valid_hyperlink_target(target) {
        Ok(())
    } else {
        Err(WriteError::InvalidHyperlinkTarget {
            row,
            col,
            target: target.to_string(),
        })
    }
}

fn is_valid_hyperlink_target(target: &str) -> bool {
    !target.is_empty() && is_xml_text_preserved(target)
}

fn is_xml_text_preserved(value: &str) -> bool {
    value.chars().all(|c| {
        let scalar = c as u32;
        (scalar >= 0x20 || matches!(c, '\t' | '\n' | '\r')) && !matches!(scalar, 0xFFFE | 0xFFFF)
    })
}

fn validate_xml_text(field: &str, value: &str) -> Result<(), WriteError> {
    if is_xml_text_preserved(value) {
        Ok(())
    } else {
        Err(WriteError::InvalidXmlText {
            field: field.to_string(),
            value: value.to_string(),
        })
    }
}

fn validate_chart_range_ref(reference: &str) -> Result<(), WriteError> {
    if parse_chart_range_ref(reference).is_some() {
        Ok(())
    } else {
        Err(WriteError::InvalidChartReference(reference.to_string()))
    }
}

fn validate_sparkline_range_ref(reference: &str) -> Result<(), WriteError> {
    if parse_chart_range_ref(reference).is_some() {
        Ok(())
    } else {
        Err(WriteError::InvalidSparklineReference(reference.to_string()))
    }
}

fn validate_conditional_format_rule(rule: &CfRule) -> Result<(), WriteError> {
    match rule {
        CfRule::CellIs {
            op,
            formula1,
            formula2,
            ..
        } => {
            if formula1.trim().is_empty() {
                return Err(WriteError::InvalidConditionalFormatRule(
                    "cellIs formula1 must not be empty".to_string(),
                ));
            }
            validate_xml_text("conditional format formula1", formula1)?;
            if matches!(op, DvOp::Between | DvOp::NotBetween)
                && formula2
                    .as_deref()
                    .is_none_or(|formula| formula.trim().is_empty())
            {
                return Err(WriteError::InvalidConditionalFormatRule(
                    "cellIs between/notBetween rules require formula2".to_string(),
                ));
            }
            if let Some(formula2) = formula2 {
                validate_xml_text("conditional format formula2", formula2)?;
            }
        }
        CfRule::Expression { formula, .. } => {
            if formula.trim().is_empty() {
                return Err(WriteError::InvalidConditionalFormatRule(
                    "expression formula must not be empty".to_string(),
                ));
            }
            validate_xml_text("conditional format expression", formula)?;
        }
        CfRule::TopBottom { rank, percent, .. } => {
            if *rank == 0 {
                return Err(WriteError::InvalidConditionalFormatRule(
                    "top/bottom rank must be at least 1".to_string(),
                ));
            }
            if *percent && *rank > 100 {
                return Err(WriteError::InvalidConditionalFormatRule(
                    "top/bottom percent rank must be within 1..=100".to_string(),
                ));
            }
        }
        CfRule::ColorScale2 { .. }
        | CfRule::ColorScale3 { .. }
        | CfRule::DataBar { .. }
        | CfRule::AboveAverage { .. }
        | CfRule::DuplicateValues { .. } => {}
    }
    Ok(())
}

fn validate_data_validation_rule(rule: &DataValidation) -> Result<(), WriteError> {
    if rule.formula1.trim().is_empty() {
        return Err(WriteError::InvalidDataValidationRule(
            "formula1 must not be empty".to_string(),
        ));
    }
    validate_data_validation_xml_text("formula1", &rule.formula1)?;
    if let Some(formula2) = &rule.formula2 {
        validate_data_validation_xml_text("formula2", formula2)?;
    }
    if let Some((title, message)) = &rule.prompt {
        validate_data_validation_xml_text("prompt title", title)?;
        validate_data_validation_xml_text("prompt", message)?;
    }
    if let Some((title, message)) = &rule.error {
        validate_data_validation_xml_text("error title", title)?;
        validate_data_validation_xml_text("error", message)?;
    }
    if !matches!(rule.kind, DvKind::List | DvKind::Custom)
        && matches!(rule.operator, DvOp::Between | DvOp::NotBetween)
        && rule
            .formula2
            .as_deref()
            .is_none_or(|formula| formula.trim().is_empty())
    {
        return Err(WriteError::InvalidDataValidationRule(
            "between/notBetween rules require formula2".to_string(),
        ));
    }
    Ok(())
}

fn validate_data_validation_xml_text(field: &str, value: &str) -> Result<(), WriteError> {
    if is_xml_text_preserved(value) {
        Ok(())
    } else {
        Err(WriteError::InvalidDataValidationRule(format!(
            "{field} contains XML-forbidden characters"
        )))
    }
}

fn parse_chart_range_ref(reference: &str) -> Option<(u32, u16, u32, u16)> {
    let range = split_chart_sheet_range(reference)?;
    let (first, last) = range.split_once(':')?;
    if last.contains(':') {
        return None;
    }
    let (r0, c0) = parse_a1_cell(first)?;
    let (r1, c1) = parse_a1_cell(last)?;
    range_in_grid_ordered(r0, c0, r1, c1).then_some((r0, c0, r1, c1))
}

fn split_chart_sheet_range(reference: &str) -> Option<&str> {
    if reference.starts_with('\'') {
        let mut chars = reference.char_indices().peekable();
        chars.next();
        let mut sheet_name_has_chars = false;
        while let Some((idx, ch)) = chars.next() {
            if ch != '\'' {
                sheet_name_has_chars = true;
                continue;
            }
            if matches!(chars.peek(), Some((_, '\''))) {
                chars.next();
                sheet_name_has_chars = true;
                continue;
            }
            let next = idx + ch.len_utf8();
            return reference[next..]
                .strip_prefix('!')
                .filter(|range| sheet_name_has_chars && !range.is_empty());
        }
        None
    } else {
        let (sheet_name, range) = reference.split_once('!')?;
        if sheet_name.is_empty()
            || range.is_empty()
            || range.contains('!')
            || sheet_name.contains('\'')
            || sheet_name.chars().any(char::is_whitespace)
        {
            return None;
        }
        Some(range)
    }
}

fn parse_a1_cell(cell: &str) -> Option<(u32, u16)> {
    let bytes = cell.as_bytes();
    let mut i = 0;
    if bytes.get(i) == Some(&b'$') {
        i += 1;
    }
    let col_start = i;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == col_start || i - col_start > 3 {
        return None;
    }
    let col = bytes[col_start..i].iter().fold(0u32, |acc, &b| {
        acc * 26 + u32::from(b.to_ascii_uppercase() - b'A') + 1
    });
    if !(1..=u32::from(MAX_COL) + 1).contains(&col) {
        return None;
    }

    if bytes.get(i) == Some(&b'$') {
        i += 1;
    }
    let row_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == row_start || i != bytes.len() {
        return None;
    }
    let row: u32 = cell[row_start..].parse().ok()?;
    if !(1..=MAX_ROW + 1).contains(&row) {
        return None;
    }

    Some((row - 1, u16::try_from(col - 1).ok()?))
}

/// Excel sheet-name rules: non-empty, ≤ 31 chars, none of `: \ / ? * [ ]`, no
/// XML-forbidden scalars, and not surrounded by whitespace (the writer trims,
/// which would change the name).
fn is_valid_sheet_name(name: &str) -> bool {
    let count = name.chars().count();
    if count == 0 || count > 31 {
        return false;
    }
    if name.trim() != name {
        return false; // leading/trailing (or all) whitespace gets trimmed
    }
    if !is_xml_text_preserved(name) {
        return false;
    }
    !name
        .chars()
        .any(|c| matches!(c, ':' | '\\' | '/' | '?' | '*' | '[' | ']'))
}

/// Excel table-name rules, kept in lock-step with the writer's sanitizer
/// ([`table_name`](super::table::table_name)): first char a letter or `_`, the rest
/// alphanumeric or `_`, and not a cell reference (`A1`). A name the sanitizer would
/// rewrite (e.g. `bad-name` → `bad_name`) is rejected so a checked write never
/// produces a different name than described.
fn is_valid_table_name(name: &str) -> bool {
    let Some(first) = name.chars().next() else {
        return false; // empty
    };
    if !(first.is_alphabetic() || first == '_') {
        return false;
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return false;
    }
    !super::table::looks_like_cell_ref(name) && !super::table::looks_like_r1c1_ref(name)
}

fn is_valid_defined_name(name: &str) -> bool {
    let Some(first) = name.chars().next() else {
        return false;
    };
    if !(first.is_alphabetic() || first == '_' || first == '\\') {
        return false;
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '\\'))
    {
        return false;
    }
    !super::table::looks_like_cell_ref(name) && !super::table::looks_like_r1c1_ref(name)
}

#[cfg(test)]
mod tests {
    use crate::{Sheet, Table, Workbook};

    use super::WriteError;

    type DocPropertySetter = fn(&mut crate::DocProperties, &str);

    fn sheet_with_table(name: &str, tname: &str) -> Sheet {
        let mut s = Sheet::new(name);
        s.write(0, 0, "h1");
        s.write(0, 1, "h2");
        s.add_table(Table {
            range: (0, 0, 2, 1),
            name: tname.to_string(),
            columns: vec!["h1".into(), "h2".into()],
            style: None,
        });
        s
    }

    #[test]
    fn duplicate_table_name_is_rejected() {
        let wb = Workbook {
            sheets: vec![
                sheet_with_table("S1", "Sales"),
                sheet_with_table("S2", "sales"), // same name, different case
            ],
            ..Default::default()
        };
        match wb.to_xlsx_checked() {
            Err(WriteError::DuplicateTableName(_)) => {}
            other => panic!("expected DuplicateTableName, got {other:?}"),
        }
    }

    #[test]
    fn clean_workbook_is_ok() {
        let mut sales = sheet_with_table("Sheet1", "Sales");
        sales.set_table_header_format("Sales", &crate::Format::new().set_bold());
        let mut wb = Workbook {
            sheets: vec![sales, sheet_with_table("둘째", "Costs")],
            ..Default::default()
        };
        wb.define_name("Tax.Rate", "Sheet1!$A$1");
        wb.define_name("_Named_Total", "Sheet1!$B$2");
        let bytes = wb.to_xlsx_checked().expect("clean workbook validates");
        assert!(!bytes.is_empty(), "checked write produced bytes");
    }

    #[test]
    fn invalid_doc_property_timestamp_is_rejected() {
        for bad_ts in ["2024-02-31T00:00:00Z", "not-a-date"] {
            let mut wb = Workbook::new();
            wb.add_sheet("S");
            wb.properties = crate::DocProperties {
                created: Some(bad_ts.into()),
                ..Default::default()
            };

            let result = wb.to_xlsx_checked();
            assert!(
                result.is_err(),
                "invalid created timestamp {bad_ts:?} should be rejected"
            );
            let err = result.unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("invalid document property created timestamp: {bad_ts:?}")
            );
        }
    }

    #[test]
    fn doc_property_xml_text_that_would_be_dropped_is_rejected() {
        let cases: [(&str, &str, DocPropertySetter); 8] = [
            (
                "document property title",
                "title\u{1f}",
                |props: &mut crate::DocProperties, value: &str| props.title = Some(value.into()),
            ),
            (
                "document property subject",
                "subject\u{1f}",
                |props: &mut crate::DocProperties, value: &str| props.subject = Some(value.into()),
            ),
            (
                "document property creator",
                "creator\u{1f}",
                |props: &mut crate::DocProperties, value: &str| props.creator = Some(value.into()),
            ),
            (
                "document property keywords",
                "keywords\u{1f}",
                |props: &mut crate::DocProperties, value: &str| props.keywords = Some(value.into()),
            ),
            (
                "document property description",
                "description\u{1f}",
                |props: &mut crate::DocProperties, value: &str| {
                    props.description = Some(value.into())
                },
            ),
            (
                "document property lastModifiedBy",
                "last\u{1f}editor",
                |props: &mut crate::DocProperties, value: &str| {
                    props.last_modified_by = Some(value.into())
                },
            ),
            (
                "document property company",
                "company\u{1f}",
                |props: &mut crate::DocProperties, value: &str| props.company = Some(value.into()),
            ),
            (
                "document property created",
                "2024-01-02T03:04:05Z\u{1f}",
                |props: &mut crate::DocProperties, value: &str| props.created = Some(value.into()),
            ),
        ];

        for (field, value, set_value) in cases {
            let mut wb = Workbook::new();
            wb.add_sheet("S");
            set_value(&mut wb.properties, value);

            assert_invalid_xml_text(wb, field, value);
        }
    }

    #[test]
    fn invalid_and_duplicate_defined_names_are_rejected() {
        for name in ["A1", "R1C1", "bad name", "1Bad", "Bad-Name"] {
            let mut wb = Workbook::new();
            wb.add_sheet("S");
            wb.define_name(name, "S!$A$1");

            assert!(
                matches!(
                    wb.to_xlsx_checked(),
                    Err(WriteError::InvalidDefinedName(bad)) if bad == name
                ),
                "invalid defined name {name:?} should be rejected"
            );
        }

        let mut wb = Workbook::new();
        wb.add_sheet("S");
        wb.define_name("Rate", "S!$A$1");
        wb.define_name("rate", "S!$A$2");

        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::DuplicateDefinedName(name)) if name == "rate"
        ));
    }

    #[test]
    fn defined_name_formula_xml_text_that_would_be_dropped_is_rejected() {
        let mut wb = Workbook::new();
        wb.add_sheet("S");
        wb.define_name("TaxRate", "S!$A$1\u{1f}");

        assert_invalid_xml_text(wb, "defined name formula", "S!$A$1\u{1f}");
    }

    #[test]
    fn too_many_sheets_is_rejected_before_emission() {
        let mut wb = Workbook::new();
        for idx in 0..=255 {
            wb.add_sheet(format!("S{idx}"));
        }

        let err = wb.to_xlsx_checked().unwrap_err();
        assert_eq!(err, WriteError::TooManySheets);
        assert_eq!(
            err.to_string(),
            "too many sheets (Excel allows at most 255)"
        );
    }

    #[test]
    fn out_of_grid_cell_is_rejected() {
        let mut s = Sheet::new("S");
        s.write(2_000_000, 0, "way past the last row"); // row > MAX_ROW
        let wb = Workbook {
            sheets: vec![s],
            ..Default::default()
        };
        match wb.to_xlsx_checked() {
            Err(WriteError::CellOutOfGrid { row, .. }) => assert_eq!(row, 2_000_000),
            other => panic!("expected CellOutOfGrid, got {other:?}"),
        }
    }

    #[test]
    fn out_of_grid_blank_format_cells_are_rejected() {
        let format = crate::Format::new().set_bold();

        let mut row = Sheet::new("S");
        row.write_blank_with_format(1_048_576, 0, &format);
        let wb = Workbook {
            sheets: vec![row],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::CellOutOfGrid {
                row: 1_048_576,
                col: 0
            })
        ));

        let mut col = Sheet::new("S");
        col.write_blank_with_format(0, 16_384, &format);
        let wb = Workbook {
            sheets: vec![col],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::CellOutOfGrid {
                row: 0,
                col: 16_384
            })
        ));
    }

    #[test]
    fn non_finite_numeric_cells_are_rejected() {
        let mut direct = Sheet::new("S");
        direct.write(0, 0, f64::NAN);
        let wb = Workbook {
            sheets: vec![direct],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::NonFiniteNumber { row: 0, col: 0 })
        ));

        let mut formula = Sheet::new("S");
        formula.write(
            1,
            2,
            crate::Cell::Formula {
                formula: "1/0".to_string(),
                cached: Box::new(crate::Cell::Number(f64::INFINITY)),
            },
        );
        let wb = Workbook {
            sheets: vec![formula],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::NonFiniteNumber { row: 1, col: 2 })
        ));
    }

    #[test]
    fn invalid_hyperlink_targets_are_rejected() {
        for target in ["", "\u{1}"] {
            let mut sheet = Sheet::new("S");
            sheet.write_url(3, 4, target, "link");
            let wb = Workbook {
                sheets: vec![sheet],
                ..Default::default()
            };

            match wb.to_xlsx_checked() {
                Err(WriteError::InvalidHyperlinkTarget {
                    row: 3,
                    col: 4,
                    target: actual,
                }) => assert_eq!(actual, target),
                other => panic!("expected InvalidHyperlinkTarget, got {other:?}"),
            }
        }
    }

    fn assert_invalid_xml_text(wb: Workbook, field: &str, value: &str) {
        match wb.to_xlsx_checked() {
            Err(WriteError::InvalidXmlText {
                field: actual_field,
                value: actual_value,
            }) => {
                assert_eq!(actual_field, field);
                assert_eq!(actual_value, value);
            }
            Err(other) => panic!("expected InvalidXmlText for {field}, got {other:?}"),
            Ok(_) => panic!("expected InvalidXmlText for {field}, got Ok bytes"),
        }
    }

    #[test]
    fn cell_text_that_would_be_dropped_is_rejected() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("S");
        sheet.write(0, 0, "keep\u{1f}all");

        assert_invalid_xml_text(wb, "cell text", "keep\u{1f}all");
    }

    #[test]
    fn cell_error_text_that_would_be_dropped_is_rejected() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("S");
        sheet.write(0, 0, crate::Cell::Error("bad\u{1f}error".to_string()));

        assert_invalid_xml_text(wb, "cell error", "bad\u{1f}error");
    }

    #[test]
    fn formula_source_text_that_would_be_dropped_is_rejected() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("S");
        sheet.write(
            0,
            0,
            crate::Cell::Formula {
                formula: "SUM(A1:A2)\u{1f}".to_string(),
                cached: Box::new(crate::Cell::Number(3.0)),
            },
        );

        assert_invalid_xml_text(wb, "formula", "SUM(A1:A2)\u{1f}");
    }

    #[test]
    fn formula_cached_text_that_would_be_dropped_is_rejected() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("S");
        sheet.write(
            0,
            0,
            crate::Cell::Formula {
                formula: "A1".to_string(),
                cached: Box::new(crate::Cell::Text("cached\u{1f}value".to_string())),
            },
        );

        assert_invalid_xml_text(wb, "formula cached text", "cached\u{1f}value");
    }

    #[test]
    fn formula_cached_error_that_would_be_dropped_is_rejected() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("S");
        sheet.write(
            0,
            0,
            crate::Cell::Formula {
                formula: "A1".to_string(),
                cached: Box::new(crate::Cell::Error("bad\u{1f}error".to_string())),
            },
        );

        assert_invalid_xml_text(wb, "formula cached error", "bad\u{1f}error");
    }

    #[test]
    fn format_number_format_text_that_would_be_dropped_is_rejected() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("S");
        let format = crate::Format::new().set_num_format("0\u{1f}.00");
        sheet.write_number_with_format(0, 0, 12.5, &format);

        assert_invalid_xml_text(wb, "number format", "0\u{1f}.00");
    }

    #[test]
    fn rich_text_font_name_that_would_be_dropped_is_rejected() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("S");
        sheet.write_rich(
            0,
            0,
            vec![crate::TextRun::new(
                "styled",
                crate::Font {
                    name: Some("Bad\u{1f}Font".to_string()),
                    ..Default::default()
                },
            )],
        );

        assert_invalid_xml_text(wb, "font name", "Bad\u{1f}Font");
    }

    #[test]
    fn rich_text_run_text_that_would_be_dropped_is_rejected() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("S");
        sheet.write_rich(
            0,
            0,
            vec![crate::TextRun::new("bad\u{1f}run", crate::Font::default())],
        );

        assert_invalid_xml_text(wb, "rich string text", "bad\u{1f}run");
    }

    #[test]
    fn table_column_xml_text_that_would_be_dropped_is_rejected() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("S");
        sheet.add_table(Table {
            range: (0, 0, 0, 0),
            name: "Table1".into(),
            columns: vec!["bad\u{1f}header".into()],
            style: None,
        });

        assert_invalid_xml_text(wb, "table column", "bad\u{1f}header");
    }

    #[test]
    fn table_style_xml_text_that_would_be_dropped_is_rejected() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("S");
        sheet.add_table(Table {
            range: (0, 0, 0, 0),
            name: "Table1".into(),
            columns: vec!["header".into()],
            style: Some("Bad\u{1f}Style".into()),
        });

        assert_invalid_xml_text(wb, "table style", "Bad\u{1f}Style");
    }

    #[test]
    fn comment_xml_text_that_would_be_dropped_is_rejected() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("S");
        sheet.add_comment(0, 0, "keep\u{1f}all", Some("reviewer"));

        assert_invalid_xml_text(wb, "comment text", "keep\u{1f}all");
    }

    #[test]
    fn comment_author_xml_text_that_would_be_dropped_is_rejected() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("S");
        sheet.add_comment(0, 0, "review", Some("review\u{1f}er"));

        assert_invalid_xml_text(wb, "comment author", "review\u{1f}er");
    }

    fn assert_layout_metadata_rejected(sheet: Sheet) {
        let wb = Workbook {
            sheets: vec![sheet],
            ..Default::default()
        };
        match wb.to_xlsx_checked() {
            Err(WriteError::InvalidSheetLayout(_)) => {}
            other => panic!("expected InvalidSheetLayout, got {other:?}"),
        }
    }

    #[test]
    fn invalid_sheet_layout_metadata_is_rejected() {
        let mut row_height = Sheet::new("S");
        row_height.set_row_height(0, f32::NAN);
        assert_layout_metadata_rejected(row_height);

        let mut row_height = Sheet::new("S");
        row_height.set_row_height(0, -1.0);
        assert_layout_metadata_rejected(row_height);

        let mut row_height = Sheet::new("S");
        row_height.set_row_height(1_048_576, 12.0);
        assert_layout_metadata_rejected(row_height);

        let mut col_width = Sheet::new("S");
        col_width.set_col_width(0, f32::INFINITY);
        assert_layout_metadata_rejected(col_width);

        let mut col_width = Sheet::new("S");
        col_width.set_col_width(16_384, 12.0);
        assert_layout_metadata_rejected(col_width);

        let mut default_row_height = Sheet::new("S");
        default_row_height.set_default_row_height(-1.0);
        assert_layout_metadata_rejected(default_row_height);

        let mut default_col_width = Sheet::new("S");
        default_col_width.set_default_col_width(f32::NAN);
        assert_layout_metadata_rejected(default_col_width);

        let bold = crate::Format::new().set_bold();

        let mut row_format = Sheet::new("S");
        row_format.set_row_format(1_048_576, &bold);
        assert_layout_metadata_rejected(row_format);

        let mut col_format = Sheet::new("S");
        col_format.set_col_format(16_384, &bold);
        assert_layout_metadata_rejected(col_format);

        let mut row_outline = Sheet::new("S");
        row_outline.row_outline.insert(1_048_576, 1);
        assert_layout_metadata_rejected(row_outline);

        let mut col_outline = Sheet::new("S");
        col_outline.group_cols(16_384, 16_384, 1);
        assert_layout_metadata_rejected(col_outline);

        let mut outline_level = Sheet::new("S");
        outline_level.group_rows(0, 0, 8);
        assert_layout_metadata_rejected(outline_level);

        let mut collapsed_row = Sheet::new("S");
        collapsed_row.collapse_row(1_048_576);
        assert_layout_metadata_rejected(collapsed_row);
    }

    #[test]
    fn invalid_table_name_is_rejected() {
        let mut s = Sheet::new("S");
        s.write(0, 0, "h1");
        s.add_table(Table {
            range: (0, 0, 1, 0),
            name: "A1".into(), // looks like a cell reference
            columns: vec!["h1".into()],
            style: None,
        });
        let wb = Workbook {
            sheets: vec![s],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::InvalidTableName(_))
        ));
    }

    #[test]
    fn table_range_columns_mismatch_is_rejected() {
        let mut s = Sheet::new("S");
        s.write(0, 0, "h1");
        s.add_table(Table {
            range: (0, 0, 2, 1), // width 2
            name: "T".into(),
            columns: vec!["only_one".into()], // but only 1 column declared
            style: None,
        });
        let wb = Workbook {
            sheets: vec![s],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::TableRangeColumnsMismatch { .. })
        ));
    }

    #[test]
    fn unknown_table_header_format_target_is_rejected() {
        let mut s = Sheet::new("S");
        s.set_table_header_format("MissingTable", &crate::Format::new().set_bold());
        let wb = Workbook {
            sheets: vec![s],
            ..Default::default()
        };

        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::UnknownTableHeaderFormat { table })
                if table == "MissingTable"
        ));
    }

    #[test]
    fn active_sheet_out_of_range_is_rejected() {
        let mut wb = Workbook::new();
        wb.add_sheet("Only");
        wb.set_active_sheet(1);

        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::ActiveSheetOutOfRange { index, sheets })
                if index == 1 && sheets == 1
        ));
    }

    #[test]
    fn invalid_and_duplicate_sheet_names_are_rejected() {
        // Illegal char.
        let wb = Workbook {
            sheets: vec![Sheet::new("bad/name")],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::InvalidSheetName(_))
        ));

        // Case-insensitive duplicate.
        let wb = Workbook {
            sheets: vec![Sheet::new("Data"), Sheet::new("data")],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::DuplicateSheetName(_))
        ));
    }

    #[test]
    fn table_name_checks_match_the_sanitizer() {
        // Real cell refs are rejected; default-style names that only LOOK ref-ish
        // (column too long / row too large) are accepted.
        assert!(super::is_valid_table_name("Table1")); // `Table` is not a column
        assert!(super::is_valid_table_name("Sales_2024"));
        assert!(!super::is_valid_table_name("Q4")); // a real cell address
        assert!(!super::is_valid_table_name("A1"));
        assert!(!super::is_valid_table_name("bad-name")); // sanitizer would rewrite to bad_name
        assert!(!super::is_valid_table_name("2024")); // starts with a digit
    }

    #[test]
    fn defined_name_checks_match_writer_contract() {
        assert!(super::is_valid_defined_name("Tax.Rate"));
        assert!(super::is_valid_defined_name("_Named_Total"));
        assert!(super::is_valid_defined_name("\\PrintArea"));
        assert!(super::is_valid_defined_name("Table1")); // `Table` is not a column
        assert!(!super::is_valid_defined_name(""));
        assert!(!super::is_valid_defined_name("A1"));
        assert!(!super::is_valid_defined_name("R1C1"));
        assert!(!super::is_valid_defined_name("bad name"));
        assert!(!super::is_valid_defined_name("bad-name"));
        assert!(!super::is_valid_defined_name("2024"));
    }

    #[test]
    fn sheet_name_rejects_untrimmed() {
        assert!(!super::is_valid_sheet_name(" Lead")); // trimmed away by the writer
        assert!(!super::is_valid_sheet_name("Trail ")); // ditto
        assert!(!super::is_valid_sheet_name("   ")); // all whitespace -> SheetN
        assert!(super::is_valid_sheet_name("Q1 Results")); // internal space is fine
    }

    #[test]
    fn sheet_name_xml_text_that_would_be_dropped_is_rejected() {
        let wb = Workbook {
            sheets: vec![Sheet::new("Bad\u{1f}Name")],
            ..Default::default()
        };

        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::InvalidSheetName(name)) if name == "Bad\u{1f}Name"
        ));
    }

    #[test]
    fn reversed_range_and_off_grid_comment_are_rejected() {
        let mut s = Sheet::new("S");
        s.write(0, 0, "x");
        s.merge(2, 0, 0, 0); // reversed: r1 < r0
        let wb = Workbook {
            sheets: vec![s],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::MergeOutOfGrid)
        ));

        let mut s2 = Sheet::new("S");
        s2.add_comment(2_000_000, 0, "off grid", None); // row > MAX_ROW
        let wb2 = Workbook {
            sheets: vec![s2],
            ..Default::default()
        };
        assert!(matches!(
            wb2.to_xlsx_checked(),
            Err(WriteError::CellOutOfGrid { .. })
        ));
    }

    #[test]
    fn cells_hidden_by_merged_ranges_are_rejected() {
        let mut value = Sheet::new("S");
        value.merge(0, 0, 1, 1);
        value.write(0, 1, "hidden");
        let wb = Workbook {
            sheets: vec![value],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::CellUnderMergedRange { row: 0, col: 1 })
        ));

        let mut blank = Sheet::new("S");
        blank.merge(0, 0, 1, 1);
        blank.write_blank_with_format(1, 0, &crate::Format::new().set_bold());
        let wb = Workbook {
            sheets: vec![blank],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::CellUnderMergedRange { row: 1, col: 0 })
        ));
    }

    #[test]
    fn drawing_anchors_are_rejected_when_reversed_or_out_of_grid() {
        let mut image = Sheet::new("S");
        image.add_image(crate::Image {
            data: vec![1, 2, 3],
            format: crate::ImageFmt::Png,
            from: (1_048_576, 0),
            to: Some((1_048_576, 1)),
        });
        let wb = Workbook {
            sheets: vec![image],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::MergeOutOfGrid)
        ));

        let mut default_image = Sheet::new("S");
        default_image.add_image(crate::Image {
            data: vec![1, 2, 3],
            format: crate::ImageFmt::Png,
            from: (1_048_575, 16_383),
            to: None,
        });
        let wb = Workbook {
            sheets: vec![default_image],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::MergeOutOfGrid)
        ));

        let mut chart = Sheet::new("S");
        chart.add_chart(crate::Chart {
            kind: crate::ChartKind::Bar,
            title: None,
            series: vec![crate::Series {
                name: None,
                categories: None,
                values: "S!$A$1:$A$1".into(),
                bubble_sizes: None,
            }],
            legend: false,
            data_labels: false,
            x_axis_title: None,
            y_axis_title: None,
            from: (5, 5),
            to: (4, 5),
        });
        let wb = Workbook {
            sheets: vec![chart],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::MergeOutOfGrid)
        ));

        let mut repeat_cols = Sheet::new("S");
        repeat_cols.set_page_setup(crate::PageSetup {
            repeat_cols: Some((3, 1)),
            ..Default::default()
        });
        let wb = Workbook {
            sheets: vec![repeat_cols],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::MergeOutOfGrid)
        ));
    }

    #[test]
    fn simple_chart_series_references_are_accepted() {
        let mut s = Sheet::new("S");
        s.add_chart(crate::Chart {
            kind: crate::ChartKind::Bubble,
            title: None,
            series: vec![
                crate::Series {
                    name: None,
                    categories: Some("S!$A$1:$A$5".into()),
                    values: "S!$B$1:$B$5".into(),
                    bubble_sizes: Some("S!$C$1:$C$5".into()),
                },
                crate::Series {
                    name: Some("quoted sheet".into()),
                    categories: Some("'Q1 Sales'!A1:A5".into()),
                    values: "'Q1 Sales'!B1:B5".into(),
                    bubble_sizes: None,
                },
            ],
            legend: false,
            data_labels: false,
            x_axis_title: None,
            y_axis_title: None,
            from: (0, 0),
            to: (10, 5),
        });
        let wb = Workbook {
            sheets: vec![s, Sheet::new("Q1 Sales")],
            ..Default::default()
        };

        assert!(
            wb.to_xlsx_checked().is_ok(),
            "simple chart A1 references should validate"
        );
    }

    #[test]
    fn malformed_chart_series_references_are_rejected() {
        for reference in ["S!A1", "S!A:A", "S!A1:B", "S!A0:A1", "'Bad!A1:A2"] {
            let mut s = Sheet::new("S");
            s.add_chart(crate::Chart {
                kind: crate::ChartKind::Bar,
                title: None,
                series: vec![crate::Series {
                    name: None,
                    categories: None,
                    values: reference.into(),
                    bubble_sizes: None,
                }],
                legend: false,
                data_labels: false,
                x_axis_title: None,
                y_axis_title: None,
                from: (0, 0),
                to: (10, 5),
            });
            let wb = Workbook {
                sheets: vec![s],
                ..Default::default()
            };

            assert!(
                matches!(
                    wb.to_xlsx_checked(),
                    Err(WriteError::InvalidChartReference(_))
                ),
                "malformed chart reference {reference:?} should be rejected"
            );
        }
    }

    #[test]
    fn reversed_and_out_of_grid_chart_series_references_are_rejected() {
        for reference in ["S!B2:A1", "S!XFE1:XFE2", "S!A1048577:A1048578"] {
            let mut s = Sheet::new("S");
            s.add_chart(crate::Chart {
                kind: crate::ChartKind::Bubble,
                title: None,
                series: vec![crate::Series {
                    name: None,
                    categories: Some("S!$A$1:$A$5".into()),
                    values: "S!$B$1:$B$5".into(),
                    bubble_sizes: Some(reference.into()),
                }],
                legend: false,
                data_labels: false,
                x_axis_title: None,
                y_axis_title: None,
                from: (0, 0),
                to: (10, 5),
            });
            let wb = Workbook {
                sheets: vec![s],
                ..Default::default()
            };

            assert!(
                matches!(
                    wb.to_xlsx_checked(),
                    Err(WriteError::InvalidChartReference(_))
                ),
                "invalid chart reference {reference:?} should be rejected"
            );
        }
    }

    #[test]
    fn chart_text_that_would_be_dropped_is_rejected() {
        for (field, value, title, series_name, x_axis_title, y_axis_title) in [
            (
                "chart title",
                "Bad\u{1f}Title",
                Some("Bad\u{1f}Title"),
                None,
                None,
                None,
            ),
            (
                "chart series name",
                "Bad\u{1f}Series",
                None,
                Some("Bad\u{1f}Series"),
                None,
                None,
            ),
            (
                "chart x-axis title",
                "Bad\u{1f}X",
                None,
                None,
                Some("Bad\u{1f}X"),
                None,
            ),
            (
                "chart y-axis title",
                "Bad\u{1f}Y",
                None,
                None,
                None,
                Some("Bad\u{1f}Y"),
            ),
        ] {
            let mut s = Sheet::new("S");
            s.add_chart(crate::Chart {
                kind: crate::ChartKind::Line,
                title: title.map(str::to_string),
                series: vec![crate::Series {
                    name: series_name.map(str::to_string),
                    categories: Some("S!$A$1:$A$5".into()),
                    values: "S!$B$1:$B$5".into(),
                    bubble_sizes: None,
                }],
                legend: false,
                data_labels: false,
                x_axis_title: x_axis_title.map(str::to_string),
                y_axis_title: y_axis_title.map(str::to_string),
                from: (0, 0),
                to: (10, 5),
            });
            let wb = Workbook {
                sheets: vec![s],
                ..Default::default()
            };

            assert_invalid_xml_text(wb, field, value);
        }
    }

    #[test]
    fn chart_reference_text_that_would_be_dropped_is_rejected() {
        for (field, value, categories, values, bubble_sizes) in [
            (
                "chart categories reference",
                "'Bad\u{1f}Sheet'!$A$1:$A$5",
                Some("'Bad\u{1f}Sheet'!$A$1:$A$5"),
                "S!$B$1:$B$5",
                Some("S!$C$1:$C$5"),
            ),
            (
                "chart values reference",
                "'Bad\u{1f}Sheet'!$B$1:$B$5",
                Some("S!$A$1:$A$5"),
                "'Bad\u{1f}Sheet'!$B$1:$B$5",
                Some("S!$C$1:$C$5"),
            ),
            (
                "chart bubble-size reference",
                "'Bad\u{1f}Sheet'!$C$1:$C$5",
                Some("S!$A$1:$A$5"),
                "S!$B$1:$B$5",
                Some("'Bad\u{1f}Sheet'!$C$1:$C$5"),
            ),
        ] {
            let mut s = Sheet::new("S");
            s.add_chart(crate::Chart {
                kind: crate::ChartKind::Bubble,
                title: None,
                series: vec![crate::Series {
                    name: None,
                    categories: categories.map(str::to_string),
                    values: values.into(),
                    bubble_sizes: bubble_sizes.map(str::to_string),
                }],
                legend: false,
                data_labels: false,
                x_axis_title: None,
                y_axis_title: None,
                from: (0, 0),
                to: (10, 5),
            });
            let wb = Workbook {
                sheets: vec![s],
                ..Default::default()
            };

            assert_invalid_xml_text(wb, field, value);
        }
    }

    #[test]
    fn sparkline_locations_out_of_grid_are_rejected() {
        let mut s = Sheet::new("S");
        s.add_sparkline(crate::Sparkline {
            location: (0, 16_384),
            range: "S!$A$1:$A$5".into(),
            kind: crate::SparklineKind::Line,
        });
        let wb = Workbook {
            sheets: vec![s],
            ..Default::default()
        };

        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::MergeOutOfGrid)
        ));
    }

    #[test]
    fn malformed_sparkline_source_ranges_are_rejected() {
        for reference in [
            "S!A1",
            "S!A:A",
            "S!B2:A1",
            "S!XFE1:XFE2",
            "S!A1048577:A1048578",
        ] {
            let mut s = Sheet::new("S");
            s.add_sparkline(crate::Sparkline {
                location: (0, 0),
                range: reference.into(),
                kind: crate::SparklineKind::Line,
            });
            let wb = Workbook {
                sheets: vec![s],
                ..Default::default()
            };

            assert!(
                matches!(
                    wb.to_xlsx_checked(),
                    Err(WriteError::InvalidSparklineReference(_))
                ),
                "invalid sparkline reference {reference:?} should be rejected"
            );
        }
    }

    #[test]
    fn sparkline_reference_text_that_would_be_dropped_is_rejected() {
        let mut s = Sheet::new("S");
        s.add_sparkline(crate::Sparkline {
            location: (0, 0),
            range: "'Bad\u{1f}Sheet'!$A$1:$A$5".into(),
            kind: crate::SparklineKind::Line,
        });
        let wb = Workbook {
            sheets: vec![s],
            ..Default::default()
        };

        assert_invalid_xml_text(wb, "sparkline reference", "'Bad\u{1f}Sheet'!$A$1:$A$5");
    }

    #[test]
    fn clamped_authoring_ranges_are_rejected() {
        let mut cf = Sheet::new("S");
        cf.add_conditional_format(crate::CondFormat {
            sqref: (0, 0, 1_048_576, 0),
            rule: crate::CfRule::DataBar {
                color: crate::Color([0x44, 0xAA, 0x66]),
            },
        });
        let wb = Workbook {
            sheets: vec![cf],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::MergeOutOfGrid)
        ));

        let mut dv = Sheet::new("S");
        dv.add_data_validation(crate::DataValidation::list((2, 0, 1, 0), "\"A,B\""));
        let wb = Workbook {
            sheets: vec![dv],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::MergeOutOfGrid)
        ));

        let mut print_area = Sheet::new("S");
        print_area.set_page_setup(crate::PageSetup {
            print_area: Some((0, 0, 1_048_576, 0)),
            ..Default::default()
        });
        let wb = Workbook {
            sheets: vec![print_area],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::MergeOutOfGrid)
        ));

        let mut repeat_rows = Sheet::new("S");
        repeat_rows.set_page_setup(crate::PageSetup {
            repeat_rows: Some((3, 1)),
            ..Default::default()
        });
        let wb = Workbook {
            sheets: vec![repeat_rows],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::MergeOutOfGrid)
        ));
    }

    #[test]
    fn invalid_data_validation_rules_are_rejected() {
        let mut empty_list = Sheet::new("S");
        empty_list.add_data_validation(crate::DataValidation::list((0, 0, 0, 0), ""));
        let wb = Workbook {
            sheets: vec![empty_list],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::InvalidDataValidationRule(_))
        ));

        let mut missing_bound = Sheet::new("S");
        missing_bound.add_data_validation(crate::DataValidation {
            sqref: (0, 0, 0, 0),
            kind: crate::DvKind::Whole,
            operator: crate::DvOp::Between,
            formula1: "1".into(),
            formula2: None,
            allow_blank: true,
            show_input_message: false,
            show_error_message: true,
            prompt: None,
            error: None,
        });
        let wb = Workbook {
            sheets: vec![missing_bound],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::InvalidDataValidationRule(_))
        ));

        let mut invalid_formula1 = Sheet::new("S");
        invalid_formula1
            .add_data_validation(crate::DataValidation::list((0, 0, 0, 0), "\"A\u{1},B\""));
        let wb = Workbook {
            sheets: vec![invalid_formula1],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::InvalidDataValidationRule(_))
        ));

        let mut invalid_formula2 = Sheet::new("S");
        invalid_formula2.add_data_validation(crate::DataValidation {
            sqref: (0, 0, 0, 0),
            kind: crate::DvKind::Whole,
            operator: crate::DvOp::Between,
            formula1: "1".into(),
            formula2: Some("9\u{1}".into()),
            allow_blank: true,
            show_input_message: false,
            show_error_message: true,
            prompt: None,
            error: None,
        });
        let wb = Workbook {
            sheets: vec![invalid_formula2],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::InvalidDataValidationRule(_))
        ));

        let mut invalid_prompt = Sheet::new("S");
        let mut rule = crate::DataValidation::list((0, 0, 0, 0), "\"A,B\"");
        rule.prompt = Some(("Pick".into(), "Bad\u{1}prompt".into()));
        invalid_prompt.add_data_validation(rule);
        let wb = Workbook {
            sheets: vec![invalid_prompt],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::InvalidDataValidationRule(_))
        ));

        let mut invalid_error = Sheet::new("S");
        let mut rule = crate::DataValidation::list((0, 0, 0, 0), "\"A,B\"");
        rule.error = Some(("Bad\u{1}title".into(), "Choose A or B".into()));
        invalid_error.add_data_validation(rule);
        let wb = Workbook {
            sheets: vec![invalid_error],
            ..Default::default()
        };
        assert!(matches!(
            wb.to_xlsx_checked(),
            Err(WriteError::InvalidDataValidationRule(_))
        ));
    }

    #[test]
    fn invalid_conditional_format_top_bottom_ranks_are_rejected() {
        for (rank, percent) in [(0, false), (0, true), (101, true)] {
            let mut s = Sheet::new("S");
            s.add_conditional_format(crate::CondFormat {
                sqref: (0, 0, 9, 0),
                rule: crate::CfRule::TopBottom {
                    rank,
                    bottom: false,
                    percent,
                    fill: crate::Color([0x44, 0xAA, 0x66]),
                },
            });
            let wb = Workbook {
                sheets: vec![s],
                ..Default::default()
            };

            assert!(
                matches!(
                    wb.to_xlsx_checked(),
                    Err(WriteError::InvalidConditionalFormatRule(_))
                ),
                "invalid TopBottom rank {rank} (percent={percent}) should be rejected"
            );
        }
    }

    #[test]
    fn invalid_conditional_format_formulas_are_rejected() {
        for rule in [
            crate::CfRule::CellIs {
                op: crate::DvOp::GreaterThan,
                formula1: String::new(),
                formula2: None,
                fill: crate::Color([0x44, 0xAA, 0x66]),
            },
            crate::CfRule::CellIs {
                op: crate::DvOp::Between,
                formula1: "1".into(),
                formula2: None,
                fill: crate::Color([0x44, 0xAA, 0x66]),
            },
            crate::CfRule::CellIs {
                op: crate::DvOp::NotBetween,
                formula1: "1".into(),
                formula2: Some(String::new()),
                fill: crate::Color([0x44, 0xAA, 0x66]),
            },
            crate::CfRule::Expression {
                formula: String::new(),
                fill: crate::Color([0x44, 0xAA, 0x66]),
            },
        ] {
            let mut s = Sheet::new("S");
            s.add_conditional_format(crate::CondFormat {
                sqref: (0, 0, 9, 0),
                rule,
            });
            let wb = Workbook {
                sheets: vec![s],
                ..Default::default()
            };

            assert!(matches!(
                wb.to_xlsx_checked(),
                Err(WriteError::InvalidConditionalFormatRule(_))
            ));
        }
    }

    #[test]
    fn conditional_format_formula_xml_text_that_would_be_dropped_is_rejected() {
        let cases = [
            (
                "conditional format formula1",
                "A1>0\u{1f}",
                crate::CfRule::CellIs {
                    op: crate::DvOp::GreaterThan,
                    formula1: "A1>0\u{1f}".into(),
                    formula2: None,
                    fill: crate::Color([0x44, 0xAA, 0x66]),
                },
            ),
            (
                "conditional format formula2",
                "10\u{1f}",
                crate::CfRule::CellIs {
                    op: crate::DvOp::Between,
                    formula1: "1".into(),
                    formula2: Some("10\u{1f}".into()),
                    fill: crate::Color([0x44, 0xAA, 0x66]),
                },
            ),
            (
                "conditional format expression",
                "ISNUMBER(A1)\u{1f}",
                crate::CfRule::Expression {
                    formula: "ISNUMBER(A1)\u{1f}".into(),
                    fill: crate::Color([0x44, 0xAA, 0x66]),
                },
            ),
        ];

        for (field, value, rule) in cases {
            let mut sheet = Sheet::new("S");
            sheet.add_conditional_format(crate::CondFormat {
                sqref: (0, 0, 9, 0),
                rule,
            });
            let wb = Workbook {
                sheets: vec![sheet],
                ..Default::default()
            };

            assert_invalid_xml_text(wb, field, value);
        }
    }

    #[test]
    fn clamped_page_setup_scale_is_rejected() {
        for scale in [9, 401] {
            let mut sheet = Sheet::new("S");
            sheet.set_page_setup(crate::PageSetup {
                scale: Some(scale),
                ..Default::default()
            });
            let wb = Workbook {
                sheets: vec![sheet],
                ..Default::default()
            };

            assert!(matches!(
                wb.to_xlsx_checked(),
                Err(WriteError::InvalidPageSetupScale { value }) if value == scale
            ));
        }
    }

    #[test]
    fn invalid_page_setup_margins_are_rejected() {
        for margin in [-0.1, f64::NAN] {
            let mut sheet = Sheet::new("S");
            sheet.set_page_setup(crate::PageSetup {
                margins: Some((margin, 0.7, 0.75, 0.75, 0.3, 0.3)),
                ..Default::default()
            });
            let wb = Workbook {
                sheets: vec![sheet],
                ..Default::default()
            };

            assert!(matches!(
                wb.to_xlsx_checked(),
                Err(WriteError::InvalidPageSetupMargins)
            ));
        }
    }

    #[test]
    fn page_setup_header_footer_xml_text_that_would_be_dropped_is_rejected() {
        let cases = [
            ("page setup header", "&CReport\u{1f}", true),
            ("page setup footer", "&RPage &P\u{1f}", false),
        ];

        for (field, value, is_header) in cases {
            let mut sheet = Sheet::new("S");
            let mut page_setup = crate::PageSetup::default();
            if is_header {
                page_setup.header = Some(value.into());
            } else {
                page_setup.footer = Some(value.into());
            }
            sheet.set_page_setup(page_setup);
            let wb = Workbook {
                sheets: vec![sheet],
                ..Default::default()
            };

            assert_invalid_xml_text(wb, field, value);
        }
    }

    #[test]
    fn clamped_sheet_view_zoom_is_rejected() {
        for zoom in [9, 401] {
            let mut sheet = Sheet::new("S");
            sheet.set_zoom(zoom);
            let wb = Workbook {
                sheets: vec![sheet],
                ..Default::default()
            };

            assert!(matches!(
                wb.to_xlsx_checked(),
                Err(WriteError::InvalidSheetViewZoom { value }) if value == zoom
            ));
        }
    }

    #[test]
    fn write_error_is_nameable_at_crate_root() {
        // The publication blocker fix: WriteError must be reachable as crate::WriteError.
        let _e: crate::WriteError = WriteError::TooManySheets;
    }

    #[test]
    fn write_error_is_std_error() {
        // Display is non-empty and the type is usable as a boxed std error.
        let e = WriteError::TooManySheets;
        let s = e.to_string();
        assert!(!s.is_empty());
        let _boxed: Box<dyn std::error::Error> = Box::new(e);
    }
}
