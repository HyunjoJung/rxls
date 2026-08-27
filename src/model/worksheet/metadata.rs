//! Worksheet and sheet-view metadata.

use std::collections::{BTreeMap, BTreeSet};

use super::super::{Color, Dimensions, PageSetup};
use super::{
    Chart, Comment, CondFormat, DataValidation, Image, ProtectionOptions, Sparkline, Table,
};

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
