//! Deterministic worksheet print pagination over the backend-neutral scene.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rxls::{PageSetup, PrintLossKind, PrintPageOrder, Sheet, Workbook};
use sha2::{Digest, Sha256};

use crate::error::{LimitKind, RenderError};
use crate::layout::{
    absolute_drawings_intersect_range, build_auxiliary_text_node, build_sheet_scene,
    build_sheet_scene_with_geometry, cell_drawings_intersect_prepared_range,
    cell_style_has_visible_blank_paint, external_render_dependency_cells,
    measure_sheet_axes_for_ranges, prepared_drawing_geometry_extent, render_used_print_range,
    render_used_scene_range, CellCoordinate, MeasuredAxisSlot, RenderLimits, RenderOptions,
    RenderRange, RenderReport, RenderSelection, SheetGeometryOverride, SparseDisplayCellIndex,
    WarningCode, MAX_WORKSHEET_COLUMN, MAX_WORKSHEET_ROW,
};
use crate::scene::{
    ClipGroupNode, Fixed, PathCommand, Rect, RectNode, Rgb, Scene, SceneNode, TextAnchor,
    TextBaseline, TextStyle, FIXED_UNITS_PER_PIXEL,
};

const DEFAULT_LEFT_RIGHT_INCHES: f64 = 0.7;
const DEFAULT_TOP_BOTTOM_INCHES: f64 = 0.75;
const DEFAULT_HEADER_FOOTER_INCHES: f64 = 0.3;
const ROW_HEADING_WIDTH: Fixed = Fixed::from_pixels(32);
const COLUMN_HEADING_HEIGHT: Fixed = Fixed::from_pixels(20);
const HEADER_FOOTER_FONT_SIZE: Fixed = Fixed::from_pixels(12);
const SINGLE_PAGE_CUSTOM_PAPER_CODE: u16 = 0;
const MIN_PRINT_SCALE_PERMILLE: u16 = 100;
const MAX_PRINT_SCALE_PERMILLE: u16 = 4_000;

/// Hard ceilings specific to pagination and multi-backend output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintLimits {
    /// Maximum logical page slots before sparse-page omission.
    pub max_logical_pages: u64,
    /// Maximum emitted pages after sparse-page omission.
    pub max_pages: u64,
    /// Maximum total scene nodes across every emitted page.
    pub max_total_scene_nodes: u64,
    /// Maximum vector commands consumed by one backend operation.
    pub max_backend_commands: u64,
    /// Maximum complete PDF byte length.
    pub max_pdf_bytes: u64,
    /// Maximum width or height of one raster page.
    pub max_raster_dimension: u32,
    /// Maximum pixels in one raster page.
    pub max_raster_pixels: u64,
    /// Maximum encoded PNG byte length for one page.
    pub max_png_bytes_per_page: u64,
}

impl Default for PrintLimits {
    fn default() -> Self {
        Self {
            max_logical_pages: 16_384,
            max_pages: 4_096,
            max_total_scene_nodes: 8_000_000,
            max_backend_commands: 16_000_000,
            max_pdf_bytes: 256 << 20,
            max_raster_dimension: 32_768,
            max_raster_pixels: 100_000_000,
            max_png_bytes_per_page: 64 << 20,
        }
    }
}

/// Print-layout options layered on the ordinary worksheet renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintOptions {
    /// Cell/style/typography policy shared with whole-sheet SVG rendering.
    pub render: RenderOptions,
    /// Omit logical page slots whose body contains no cell, format-only blank,
    /// or merge. Scale selection still sees the complete print area.
    pub omit_sparse_pages: bool,
    /// Ignore authored print-page settings and emit the selected visible sheet
    /// scene at 100% on one content-sized page.
    pub single_page_sheets: bool,
    /// Pagination and backend resource ceilings.
    pub limits: PrintLimits,
}

impl Default for PrintOptions {
    fn default() -> Self {
        Self {
            render: RenderOptions::default(),
            omit_sparse_pages: true,
            single_page_sheets: false,
            limits: PrintLimits::default(),
        }
    }
}

/// An explicit print-layout policy that replaced authored pagination settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrintLayoutOverride {
    /// Fit the selected worksheet range to exactly one paper page.
    SinglePageSheets,
}

impl PrintLayoutOverride {
    /// Stable machine-readable identifier.
    pub const fn code(self) -> &'static str {
        match self {
            Self::SinglePageSheets => "single_page_sheets",
        }
    }
}

/// Stable typed print approximation or recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PrintWarningCode {
    /// A source print-area reference was malformed and omitted by the reader.
    SourceInvalidPrintArea,
    /// A source manual page break was malformed and omitted by the reader.
    SourceInvalidPageBreak,
    /// Source print metadata referenced a missing definition or relationship.
    SourcePrintReferenceMissing,
    /// A source print property could not be represented by the reader.
    SourcePrintPropertyUnsupported,
    /// Malformed source header/footer content could not be retained exactly.
    SourceHeaderFooterMalformed,
    /// A source or rxls print-metadata retention limit was reached.
    SourcePrintMetadataLimitExceeded,
    /// An unknown paper code used the deterministic Letter fallback.
    UnknownPaperSizeFallback,
    /// Invalid margins used deterministic Excel-compatible defaults.
    InvalidMarginsFallback,
    /// An explicit percentage was outside 10-400% and was clamped.
    PrintScaleClamped,
    /// Fit-to-page could not satisfy its target at the minimum scale.
    FitTargetUnreachable,
    /// A repeated-title range was reversed, outside the grid, or unusable.
    InvalidPrintTitlesIgnored,
    /// A repeated-title range occurs in the middle of the print area and is
    /// therefore repeated without being removed from the body.
    MidAreaPrintTitlesDuplicated,
    /// A merge larger than the available body box forced one page to overflow.
    MergeExpandedPage,
    /// Repeated title axes expanded so a merge was never split at the title/body
    /// seam.
    PrintTitlesExpandedForMerge,
    /// A manual page break was moved to a merge boundary so the merge remained
    /// indivisible.
    ManualBreakShiftedForMerge,
    /// One or more indivisible rows, columns, or merges exceed the printable
    /// body box at the selected scale.
    PageContentOverflow,
    /// Logical blank pages were omitted after scale and breaks were finalized.
    SparsePagesOmitted,
    /// A volatile header/footer date or time field was omitted.
    VolatileHeaderFooterFieldOmitted,
    /// Header/footer font/style control codes were flattened.
    HeaderFooterFormattingSimplified,
    /// A filename/path/picture field could not be resolved from the public
    /// workbook model and was omitted.
    HeaderFooterSourceFieldOmitted,
    /// Multiple authored print areas selected different fit scales; page-map
    /// entries retain each exact scale.
    MultipleAreaScales,
}

impl PrintWarningCode {
    /// Stable machine-readable identifier.
    pub const fn code(self) -> &'static str {
        match self {
            Self::SourceInvalidPrintArea => "source_invalid_print_area",
            Self::SourceInvalidPageBreak => "source_invalid_page_break",
            Self::SourcePrintReferenceMissing => "source_print_reference_missing",
            Self::SourcePrintPropertyUnsupported => "source_print_property_unsupported",
            Self::SourceHeaderFooterMalformed => "source_header_footer_malformed",
            Self::SourcePrintMetadataLimitExceeded => "source_print_metadata_limit_exceeded",
            Self::UnknownPaperSizeFallback => "unknown_paper_size_fallback",
            Self::InvalidMarginsFallback => "invalid_margins_fallback",
            Self::PrintScaleClamped => "print_scale_clamped",
            Self::FitTargetUnreachable => "fit_target_unreachable",
            Self::InvalidPrintTitlesIgnored => "invalid_print_titles_ignored",
            Self::MidAreaPrintTitlesDuplicated => "mid_area_print_titles_duplicated",
            Self::MergeExpandedPage => "merge_expanded_page",
            Self::PrintTitlesExpandedForMerge => "print_titles_expanded_for_merge",
            Self::ManualBreakShiftedForMerge => "manual_break_shifted_for_merge",
            Self::PageContentOverflow => "page_content_overflow",
            Self::SparsePagesOmitted => "sparse_pages_omitted",
            Self::VolatileHeaderFooterFieldOmitted => "volatile_header_footer_field_omitted",
            Self::HeaderFooterFormattingSimplified => "header_footer_formatting_simplified",
            Self::HeaderFooterSourceFieldOmitted => "header_footer_source_field_omitted",
            Self::MultipleAreaScales => "multiple_area_scales",
        }
    }
}

/// Aggregated print warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintWarning {
    /// Warning category.
    pub code: PrintWarningCode,
    /// Occurrence count.
    pub occurrences: u64,
}

/// Deterministic paper geometry in fixed-point CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaperGeometry {
    /// Source paper code after fallback. Zero denotes the content-sized canvas
    /// used by [`PrintLayoutOverride::SinglePageSheets`].
    pub paper_code: u16,
    /// Page width after orientation.
    pub width: Fixed,
    /// Page height after orientation.
    pub height: Fixed,
}

/// One exact page-map entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageMapEntry {
    /// Zero-based position in the emitted page list.
    pub output_index: usize,
    /// One-based physical page number including `first_page_number`.
    pub displayed_page_number: u64,
    /// Zero-based authored print-area index.
    pub area_index: usize,
    /// Zero-based horizontal page slot.
    pub horizontal_index: usize,
    /// Zero-based vertical page slot.
    pub vertical_index: usize,
    /// Whether an authored manual column break begins this page column.
    pub manual_col_break_before: bool,
    /// Whether an authored manual row break begins this page row.
    pub manual_row_break_before: bool,
    /// Body range on this page.
    pub body_range: RenderRange,
    /// Repeated rows, when active.
    pub repeat_rows: Option<(u32, u32)>,
    /// Repeated columns, when active.
    pub repeat_cols: Option<(u16, u16)>,
    /// Percentage in permille (1000 = 100%).
    pub scale_permille: u16,
}

/// One laid-out print page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintPage {
    /// Exact page-map entry.
    pub map: PageMapEntry,
    /// Shared backend-neutral page scene.
    pub scene: Scene,
}

/// Machine-readable pagination report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Source whole-range render report.
    pub source: RenderReport,
    /// Source render reports for every authored print area, in authored order.
    /// `source` remains the first entry for compatibility.
    pub sources: Vec<RenderReport>,
    /// Chosen paper geometry.
    pub paper: PaperGeometry,
    /// Printable content rectangle after margins.
    pub content_rect: Rect,
    /// Explicit layout override, when authored scale/fit settings were ignored.
    pub layout_override: Option<PrintLayoutOverride>,
    /// Effective authored traversal order, or `None` when a layout override
    /// ignored source pagination.
    pub page_order: Option<PrintPageOrder>,
    /// Effective retained manual row breaks. Empty when a layout override
    /// ignored source pagination.
    pub manual_row_breaks: Vec<u32>,
    /// Effective retained manual column breaks. Empty when a layout override
    /// ignored source pagination.
    pub manual_col_breaks: Vec<u16>,
    /// Final percentage in permille (1000 = 100%). Zero means authored print
    /// areas selected distinct fit scales; exact values remain in `pages`.
    pub scale_permille: u16,
    /// Logical page slots before sparse omission.
    pub logical_pages: u64,
    /// Number of omitted blank logical slots.
    pub sparse_pages_omitted: u64,
    /// Exact emitted page map in paint/output order.
    pub pages: Vec<PageMapEntry>,
    /// Typed print warnings in code order.
    pub warnings: Vec<PrintWarning>,
}

impl PrintReport {
    /// Serialize a path-neutral stable compact JSON report.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"schema_version\":");
        out.push_str(&self.schema_version.to_string());
        out.push_str(",\"sheet_index\":");
        out.push_str(&self.source.sheet_index.to_string());
        out.push_str(",\"sheet_name\":\"");
        push_json_escaped(&mut out, &self.source.sheet_name);
        out.push_str("\",\"source_report\":");
        out.push_str(&self.source.to_json());
        out.push_str(",\"source_reports\":[");
        for (index, source) in self.sources.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            out.push_str(&source.to_json());
        }
        out.push(']');
        out.push_str(",\"paper\":{\"code\":");
        out.push_str(&self.paper.paper_code.to_string());
        out.push_str(",\"width_raw\":");
        out.push_str(&self.paper.width.raw().to_string());
        out.push_str(",\"height_raw\":");
        out.push_str(&self.paper.height.raw().to_string());
        out.push_str("},\"content_rect\":");
        push_rect_json(&mut out, self.content_rect);
        if let Some(layout_override) = self.layout_override {
            out.push_str(",\"layout_override\":\"");
            out.push_str(layout_override.code());
            out.push('"');
        }
        if let Some(page_order) = self.page_order {
            out.push_str(",\"page_order\":\"");
            out.push_str(print_page_order_code(page_order));
            out.push('"');
        }
        out.push_str(",\"manual_row_breaks\":[");
        for (index, row) in self.manual_row_breaks.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            out.push_str(&row.to_string());
        }
        out.push_str("],\"manual_col_breaks\":[");
        for (index, column) in self.manual_col_breaks.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            out.push_str(&column.to_string());
        }
        out.push(']');
        out.push_str(",\"scale_permille\":");
        out.push_str(&self.scale_permille.to_string());
        out.push_str(",\"logical_pages\":");
        out.push_str(&self.logical_pages.to_string());
        out.push_str(",\"sparse_pages_omitted\":");
        out.push_str(&self.sparse_pages_omitted.to_string());
        out.push_str(",\"pages\":[");
        for (index, page) in self.pages.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            push_page_map_json(&mut out, page);
        }
        out.push_str("],\"warnings\":[");
        for (index, warning) in self.warnings.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            out.push_str("{\"code\":\"");
            out.push_str(warning.code.code());
            out.push_str("\",\"occurrences\":");
            out.push_str(&warning.occurrences.to_string());
            out.push('}');
        }
        out.push_str("]}");
        out
    }
}

/// Complete deterministic print document for one worksheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintDocument {
    /// Pages in exact output order.
    pub pages: Vec<PrintPage>,
    /// Pagination report and map.
    pub report: PrintReport,
    /// Backend safety limits captured with the document.
    pub limits: PrintLimits,
}

/// Bounded pagination state with an exact report and no retained page scenes.
///
/// Build this once with [`prepare_print_document`] (or its sheet variant), then
/// materialize a requested page with [`build_print_page`]. The state owns all
/// options and verified font data required by later page construction, so it
/// never borrows caller memory and is suitable for WebAssembly request paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPrintDocument {
    /// Exact pagination report and emitted page map.
    pub report: PrintReport,
    /// Backend safety limits captured with the plan.
    pub limits: PrintLimits,
    render_limits: RenderLimits,
    source_identity: [u8; 32],
    source_dependencies: Vec<CellCoordinate>,
    tracks_used_extent: bool,
    state: PreparedPrintState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreparedPrintState {
    SinglePage {
        render_options: RenderOptions,
    },
    Paginated {
        behavior: PrintBehavior,
        paper: PaperGeometry,
        content_rect: Rect,
        header_y: Fixed,
        footer_y: Fixed,
        areas: Vec<PreparedArea>,
        total_pages: u64,
    },
}

#[derive(Default)]
struct PrintWarnings(BTreeMap<PrintWarningCode, u64>);

impl PrintWarnings {
    fn add(&mut self, code: PrintWarningCode) {
        self.add_count(code, 1);
    }

    fn add_count(&mut self, code: PrintWarningCode, count: u64) {
        if count != 0 {
            let entry = self.0.entry(code).or_default();
            *entry = entry.saturating_add(count);
        }
    }

    fn finish(self) -> Vec<PrintWarning> {
        self.0
            .into_iter()
            .map(|(code, occurrences)| PrintWarning { code, occurrences })
            .collect()
    }
}

#[derive(Debug)]
struct DecodedMediaBudget {
    limit: u64,
    retained: u64,
}

impl DecodedMediaBudget {
    fn new(limit: u64) -> Self {
        Self { limit, retained: 0 }
    }

    fn build_sheet_scene(
        &mut self,
        sheet: &Sheet,
        sheet_index: usize,
        base_options: &RenderOptions,
        geometry: Option<SheetGeometryOverride<'_>>,
    ) -> Result<Scene, RenderError> {
        let mut options = base_options.clone();
        options.limits.max_decoded_media_bytes = self
            .limit
            .checked_sub(self.retained)
            .ok_or(RenderError::CoordinateOverflow)?;
        let build = match geometry {
            Some(geometry) => {
                build_sheet_scene_with_geometry(sheet, sheet_index, &options, geometry)
            }
            None => build_sheet_scene(sheet, sheet_index, &options),
        };
        let build = match build {
            Ok(build) => build,
            Err(RenderError::LimitExceeded {
                kind: LimitKind::DecodedMediaBytes,
                actual,
                ..
            }) => {
                let actual = self
                    .retained
                    .checked_add(actual)
                    .ok_or(RenderError::CoordinateOverflow)?;
                return Err(RenderError::LimitExceeded {
                    kind: LimitKind::DecodedMediaBytes,
                    limit: self.limit,
                    actual,
                });
            }
            Err(error) => return Err(error),
        };
        let retained = self
            .retained
            .checked_add(scene_decoded_media_bytes(&build.scene)?)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce_print_limit(LimitKind::DecodedMediaBytes, self.limit, retained)?;
        self.retained = retained;
        Ok(build.scene)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AxisSegment<I> {
    first: I,
    last: I,
    size: Fixed,
    manual_break_before: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PageSlot {
    horizontal_index: usize,
    vertical_index: usize,
    rows: AxisSegment<u32>,
    columns: AxisSegment<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeaderFooterPolicy {
    odd_header: Option<String>,
    odd_footer: Option<String>,
    even_header: Option<String>,
    even_footer: Option<String>,
    first_header: Option<String>,
    first_footer: Option<String>,
    different_odd_even: bool,
    different_first: bool,
    scale_with_document: bool,
    align_with_margins: bool,
}

impl HeaderFooterPolicy {
    fn select(&self, output_index: usize, displayed_page_number: u64) -> RunningText<'_> {
        if output_index == 0 && self.different_first {
            RunningText {
                header: self.first_header.as_deref(),
                footer: self.first_footer.as_deref(),
            }
        } else if displayed_page_number % 2 == 0 && self.different_odd_even {
            RunningText {
                header: self.even_header.as_deref(),
                footer: self.even_footer.as_deref(),
            }
        } else {
            RunningText {
                header: self.odd_header.as_deref(),
                footer: self.odd_footer.as_deref(),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RunningText<'a> {
    header: Option<&'a str>,
    footer: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrintBehavior {
    page_order: PrintPageOrder,
    gridlines: bool,
    headings: bool,
    center_horizontally: bool,
    center_vertically: bool,
    header_footer: HeaderFooterPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedArea {
    area_index: usize,
    source_options: RenderOptions,
    source: RenderReport,
    measured_rows: Vec<MeasuredAxisSlot<u32>>,
    measured_columns: Vec<MeasuredAxisSlot<u16>>,
    render_ranges: Vec<RenderRange>,
    measurement_ranges: Vec<RenderRange>,
    repeat_rows: Option<(u32, u32)>,
    repeat_cols: Option<(u16, u16)>,
    repeated_height: Fixed,
    repeated_width: Fixed,
    headings_height: Fixed,
    headings_width: Fixed,
    scale_permille: u16,
    logical_pages: u64,
    sparse_pages_omitted: u64,
    slots: Vec<PageSlot>,
}

fn effective_print_behavior(sheet: &Sheet, setup: &PageSetup) -> PrintBehavior {
    let metadata = sheet.print_metadata();
    let header_footer = metadata.header_footer();
    PrintBehavior {
        page_order: metadata
            .page_order()
            .unwrap_or(PrintPageOrder::DownThenOver),
        gridlines: metadata
            .print_gridlines()
            .unwrap_or_else(|| sheet.print_gridlines()),
        headings: metadata
            .print_headings()
            .unwrap_or_else(|| sheet.print_headings()),
        center_horizontally: metadata
            .center_horizontally()
            .unwrap_or(setup.center_horizontally),
        center_vertically: metadata
            .center_vertically()
            .unwrap_or(setup.center_vertically),
        header_footer: HeaderFooterPolicy {
            odd_header: header_footer
                .odd_header()
                .map(str::to_owned)
                .or_else(|| setup.header.clone()),
            odd_footer: header_footer
                .odd_footer()
                .map(str::to_owned)
                .or_else(|| setup.footer.clone()),
            even_header: header_footer.even_header().map(str::to_owned),
            even_footer: header_footer.even_footer().map(str::to_owned),
            first_header: header_footer.first_header().map(str::to_owned),
            first_footer: header_footer.first_footer().map(str::to_owned),
            different_odd_even: header_footer.different_odd_even().unwrap_or(false),
            different_first: header_footer.different_first().unwrap_or(false),
            scale_with_document: header_footer.scale_with_document().unwrap_or(false),
            align_with_margins: header_footer.align_with_margins().unwrap_or(false),
        },
    }
}

fn add_source_print_losses(sheet: &Sheet, warnings: &mut PrintWarnings) {
    for loss in sheet.print_metadata().losses() {
        let code = match loss.kind {
            PrintLossKind::InvalidPrintArea => PrintWarningCode::SourceInvalidPrintArea,
            PrintLossKind::InvalidPageBreak => PrintWarningCode::SourceInvalidPageBreak,
            PrintLossKind::MissingReference => PrintWarningCode::SourcePrintReferenceMissing,
            PrintLossKind::UnsupportedProperty => PrintWarningCode::SourcePrintPropertyUnsupported,
            PrintLossKind::MalformedHeaderFooter => PrintWarningCode::SourceHeaderFooterMalformed,
            PrintLossKind::LimitExceeded => PrintWarningCode::SourcePrintMetadataLimitExceeded,
            _ => PrintWarningCode::SourcePrintPropertyUnsupported,
        };
        warnings.add_count(code, u64::from(loss.occurrences));
    }
}

fn choose_print_ranges(
    sheet: &Sheet,
    setup: &PageSetup,
    options: &RenderOptions,
) -> Result<Vec<RenderRange>, RenderError> {
    match options.selection {
        RenderSelection::Range(range) => Ok(vec![validate_range(range)?]),
        RenderSelection::Used => {
            let source_areas = sheet.print_metadata().print_areas();
            if !source_areas.is_empty() {
                source_areas
                    .iter()
                    .copied()
                    .map(RenderRange::from)
                    .map(validate_range)
                    .collect()
            } else {
                let range = match setup.print_area {
                    Some(range) => RenderRange::from(range),
                    None => render_used_print_range(sheet, options)?,
                };
                Ok(vec![validate_range(range)?])
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_print_area(
    sheet: &Sheet,
    sheet_index: usize,
    area_index: usize,
    range: RenderRange,
    setup: &PageSetup,
    behavior: &PrintBehavior,
    repeat_rows: Option<(u32, u32)>,
    repeat_cols: Option<(u16, u16)>,
    content_rect: Rect,
    options: &PrintOptions,
    warnings: &mut PrintWarnings,
) -> Result<PreparedArea, RenderError> {
    let mut source_options = options.render.clone();
    source_options.selection = RenderSelection::Range(range);
    source_options.gridlines &= behavior.gridlines;

    let mut body_first_row = range.first_row;
    let mut body_first_col = range.first_col;
    if let Some((first, last)) = repeat_rows {
        if first <= range.first_row && last >= range.first_row {
            body_first_row = last.saturating_add(1);
        } else if first <= range.last_row && last >= range.first_row {
            warnings.add(PrintWarningCode::MidAreaPrintTitlesDuplicated);
        }
    }
    if let Some((first, last)) = repeat_cols {
        if first <= range.first_col && last >= range.first_col {
            body_first_col = last.saturating_add(1);
        } else if first <= range.last_col && last >= range.first_col {
            warnings.add(PrintWarningCode::MidAreaPrintTitlesDuplicated);
        }
    }

    // Capture the exact disjoint body/title rectangles and reconcile their
    // axes. A Cartesian bounding box can charge millions of cells between a
    // distant print area and title band even though those cells are never
    // rendered.
    let mut render_ranges = vec![range];
    if let Some((first, last)) = repeat_rows {
        render_ranges.push(RenderRange::new(
            first,
            range.first_col,
            last,
            range.last_col,
        ));
    }
    if let Some((first, last)) = repeat_cols {
        render_ranges.push(RenderRange::new(
            range.first_row,
            first,
            range.last_row,
            last,
        ));
    }
    if let (Some((first_row, last_row)), Some((first_col, last_col))) = (repeat_rows, repeat_cols) {
        render_ranges.push(RenderRange::new(first_row, first_col, last_row, last_col));
    }
    render_ranges.sort_by_key(|range| {
        (
            range.first_row,
            range.first_col,
            range.last_row,
            range.last_col,
        )
    });
    render_ranges.dedup();

    // Cell-anchored drawings retain one sheet-space destination rectangle even
    // when a print area selects only its middle or one edge. Measure sparse
    // row/column bands across each relevant anchor without charging the dense
    // Cartesian rectangle between them.
    let (drawing_extents, has_absolute_drawings) =
        prepared_drawing_geometry_extent(sheet, &render_ranges)?;
    let mut measurement_ranges = render_ranges.clone();
    for extent in drawing_extents {
        measurement_ranges.push(RenderRange::new(
            extent.first_row,
            extent.first_col,
            extent.last_row,
            extent.first_col,
        ));
        measurement_ranges.push(RenderRange::new(
            extent.first_row,
            extent.first_col,
            extent.first_row,
            extent.last_col,
        ));
    }
    if has_absolute_drawings {
        let last_row = render_ranges
            .iter()
            .map(|range| range.last_row)
            .max()
            .unwrap_or(range.last_row);
        measurement_ranges.push(RenderRange::new(
            0,
            range.first_col,
            last_row,
            range.first_col,
        ));
    }
    measurement_ranges.sort_by_key(|range| {
        (
            range.first_row,
            range.first_col,
            range.last_row,
            range.last_col,
        )
    });
    measurement_ranges.dedup();
    enforce_measurement_union_limits(&measurement_ranges, &source_options.limits)?;
    let mut row_sizes = BTreeMap::new();
    let mut column_sizes = BTreeMap::new();
    for (rows, columns) in
        measure_sheet_axes_for_ranges(sheet, &measurement_ranges, &source_options)?
    {
        merge_measured_axis(&mut row_sizes, &rows);
        merge_measured_axis(&mut column_sizes, &columns);
    }
    let measured_rows = measured_axis_slots(row_sizes)?;
    let measured_columns = measured_axis_slots(column_sizes)?;
    let mut source = build_sheet_scene_with_geometry(
        sheet,
        sheet_index,
        &source_options,
        SheetGeometryOverride::new(&measured_rows, &measured_columns),
    )?
    .report;
    source
        .warnings
        .retain(|warning| warning.code != WarningCode::PaginationDeferred);
    let body_rows = if body_first_row <= range.last_row {
        measured_axis_range(&measured_rows, body_first_row, range.last_row)
    } else {
        Vec::new()
    };
    let body_columns = if body_first_col <= range.last_col {
        measured_axis_range(&measured_columns, body_first_col, range.last_col)
    } else {
        Vec::new()
    };
    let repeated_rows = match repeat_rows {
        Some((first, last)) => measured_axis_range(&measured_rows, first, last),
        None => Vec::new(),
    };
    let repeated_columns = match repeat_cols {
        Some((first, last)) => measured_axis_range(&measured_columns, first, last),
        None => Vec::new(),
    };
    let repeated_height = axis_total(&repeated_rows)?;
    let repeated_width = axis_total(&repeated_columns)?;
    let headings_width = if behavior.headings {
        ROW_HEADING_WIDTH
    } else {
        Fixed::ZERO
    };
    let headings_height = if behavior.headings {
        COLUMN_HEADING_HEIGHT
    } else {
        Fixed::ZERO
    };
    let row_merges = merge_intervals_rows(sheet, body_first_row, range.last_row);
    let col_merges = merge_intervals_columns(sheet, body_first_col, range.last_col);
    let row_breaks = sheet.print_metadata().manual_row_breaks();
    let col_breaks = sheet.print_metadata().manual_col_breaks();

    let scale_permille = choose_scale(
        setup,
        &body_rows,
        &body_columns,
        repeated_height,
        repeated_width,
        headings_height,
        headings_width,
        content_rect,
        &row_merges,
        &col_merges,
        row_breaks,
        col_breaks,
        warnings,
    )?;
    let body_capacity_height = unscaled_capacity(
        content_rect.height,
        scale_permille,
        repeated_height,
        headings_height,
    )?;
    let body_capacity_width = unscaled_capacity(
        content_rect.width,
        scale_permille,
        repeated_width,
        headings_width,
    )?;
    let (row_segments, row_expansions, row_break_shifts) =
        partition_axis_with_breaks(&body_rows, body_capacity_height, &row_merges, row_breaks)?;
    let (column_segments, column_expansions, column_break_shifts) =
        partition_axis_with_breaks(&body_columns, body_capacity_width, &col_merges, col_breaks)?;
    warnings.add_count(
        PrintWarningCode::MergeExpandedPage,
        row_expansions.saturating_add(column_expansions),
    );
    warnings.add_count(
        PrintWarningCode::ManualBreakShiftedForMerge,
        row_break_shifts.saturating_add(column_break_shifts),
    );
    warnings.add_count(
        PrintWarningCode::PageContentOverflow,
        row_segments
            .iter()
            .filter(|segment| segment.size > body_capacity_height)
            .count() as u64
            + column_segments
                .iter()
                .filter(|segment| segment.size > body_capacity_width)
                .count() as u64
            + u64::from(
                scale_fixed(
                    repeated_height
                        .checked_add(headings_height)
                        .ok_or(RenderError::CoordinateOverflow)?,
                    scale_permille,
                )? > content_rect.height,
            )
            + u64::from(
                scale_fixed(
                    repeated_width
                        .checked_add(headings_width)
                        .ok_or(RenderError::CoordinateOverflow)?,
                    scale_permille,
                )? > content_rect.width,
            ),
    );

    let row_segments = ensure_row_segment(row_segments, range);
    let column_segments = ensure_column_segment(column_segments, range);
    let logical_pages = (row_segments.len() as u64)
        .checked_mul(column_segments.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce_print_limit(
        LimitKind::LogicalPages,
        options.limits.max_logical_pages,
        logical_pages,
    )?;

    let mut slots = Vec::new();
    match behavior.page_order {
        PrintPageOrder::OverThenDown => {
            for (vertical_index, rows) in row_segments.iter().copied().enumerate() {
                for (horizontal_index, columns) in column_segments.iter().copied().enumerate() {
                    let slot = PageSlot {
                        horizontal_index,
                        vertical_index,
                        rows,
                        columns,
                    };
                    if !options.omit_sparse_pages
                        || page_has_content(
                            sheet,
                            slot,
                            range,
                            &source_options,
                            SheetGeometryOverride::new(&measured_rows, &measured_columns),
                        )?
                    {
                        slots.push(slot);
                    }
                }
            }
        }
        _ => {
            for (horizontal_index, columns) in column_segments.iter().copied().enumerate() {
                for (vertical_index, rows) in row_segments.iter().copied().enumerate() {
                    let slot = PageSlot {
                        horizontal_index,
                        vertical_index,
                        rows,
                        columns,
                    };
                    if !options.omit_sparse_pages
                        || page_has_content(
                            sheet,
                            slot,
                            range,
                            &source_options,
                            SheetGeometryOverride::new(&measured_rows, &measured_columns),
                        )?
                    {
                        slots.push(slot);
                    }
                }
            }
        }
    }
    if slots.is_empty() {
        slots.push(PageSlot {
            horizontal_index: 0,
            vertical_index: 0,
            rows: row_segments[0],
            columns: column_segments[0],
        });
    }
    let sparse_pages_omitted = logical_pages.saturating_sub(slots.len() as u64);
    warnings.add_count(PrintWarningCode::SparsePagesOmitted, sparse_pages_omitted);
    enforce_print_limit(
        LimitKind::Pages,
        options.limits.max_pages,
        slots.len() as u64,
    )?;

    Ok(PreparedArea {
        area_index,
        source_options,
        source,
        measured_rows,
        measured_columns,
        render_ranges,
        measurement_ranges,
        repeat_rows,
        repeat_cols,
        repeated_height,
        repeated_width,
        headings_height,
        headings_width,
        scale_permille,
        logical_pages,
        sparse_pages_omitted,
        slots,
    })
}

fn prepare_single_page_sheet_document(
    sheet: &Sheet,
    sheet_index: usize,
    options: &PrintOptions,
) -> Result<PreparedPrintDocument, RenderError> {
    enforce_print_limit(LimitKind::LogicalPages, options.limits.max_logical_pages, 1)?;
    enforce_print_limit(LimitKind::Pages, options.limits.max_pages, 1)?;

    let mut render_options = options.render.clone();
    let metadata = sheet.print_metadata();
    render_options.gridlines &= metadata
        .print_gridlines()
        .unwrap_or_else(|| sheet.print_gridlines());
    let build = build_sheet_scene(sheet, sheet_index, &render_options)?;
    let scene = build.scene;
    let mut source = build.report;
    source
        .warnings
        .retain(|warning| warning.code != WarningCode::PaginationDeferred);
    let scene_nodes = scene_node_count(&scene.nodes)?;
    enforce_print_limit(
        LimitKind::PageSceneNodes,
        options
            .limits
            .max_total_scene_nodes
            .min(u64::from(u32::MAX)),
        scene_nodes,
    )?;
    enforce_print_limit(
        LimitKind::TotalSceneNodes,
        options.limits.max_total_scene_nodes,
        scene_nodes,
    )?;
    enforce_print_limit(
        LimitKind::BackendCommands,
        options.limits.max_backend_commands,
        backend_command_count(&scene.nodes)?,
    )?;

    let paper = PaperGeometry {
        paper_code: SINGLE_PAGE_CUSTOM_PAPER_CODE,
        width: scene.width,
        height: scene.height,
    };
    let content_rect = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: scene.width,
        height: scene.height,
    };
    let map = PageMapEntry {
        output_index: 0,
        displayed_page_number: 1,
        area_index: 0,
        horizontal_index: 0,
        vertical_index: 0,
        manual_col_break_before: false,
        manual_row_break_before: false,
        body_range: source.range,
        repeat_rows: None,
        repeat_cols: None,
        scale_permille: 1_000,
    };
    let sources = vec![source.clone()];
    let report = PrintReport {
        schema_version: 2,
        source,
        sources,
        paper,
        content_rect,
        layout_override: Some(PrintLayoutOverride::SinglePageSheets),
        page_order: None,
        manual_row_breaks: Vec::new(),
        manual_col_breaks: Vec::new(),
        scale_permille: 1_000,
        logical_pages: 1,
        sparse_pages_omitted: 0,
        pages: vec![map.clone()],
        warnings: Vec::new(),
    };
    let source_dependencies =
        external_render_dependency_cells(sheet, &[report.source.range], &render_options, None)?;
    let source_identity = print_source_identity(
        sheet,
        sheet_index,
        &[report.source.range],
        &[report.source.range],
        &options.render.limits,
        &source_dependencies,
    )?;
    Ok(PreparedPrintDocument {
        report,
        limits: options.limits.clone(),
        render_limits: options.render.limits.clone(),
        source_identity,
        source_dependencies,
        tracks_used_extent: matches!(options.render.selection, RenderSelection::Used),
        state: PreparedPrintState::SinglePage { render_options },
    })
}

/// Prepare an exact print report and page map without retaining page scenes.
pub fn prepare_print_document(
    workbook: &Workbook,
    sheet_index: usize,
    options: &PrintOptions,
) -> Result<PreparedPrintDocument, RenderError> {
    let sheet = workbook
        .sheets
        .get(sheet_index)
        .ok_or(RenderError::SheetIndexOutOfRange {
            requested: sheet_index,
            sheet_count: workbook.sheets.len(),
        })?;
    prepare_sheet_print_document(sheet, sheet_index, options)
}

/// Prepare an exact print report and page map without an owning workbook.
pub fn prepare_sheet_print_document(
    sheet: &Sheet,
    sheet_index: usize,
    options: &PrintOptions,
) -> Result<PreparedPrintDocument, RenderError> {
    if options.single_page_sheets {
        return prepare_single_page_sheet_document(sheet, sheet_index, options);
    }

    let mut warnings = PrintWarnings::default();
    add_source_print_losses(sheet, &mut warnings);
    let setup = sheet.page_setup().cloned().unwrap_or_default();
    let behavior = effective_print_behavior(sheet, &setup);
    let paper = paper_geometry(setup.paper_size, setup.landscape, &mut warnings)?;
    let (content_rect, header_y, footer_y) = content_geometry(paper, setup.margins, &mut warnings)?;
    let repeat_rows = expand_repeat_rows_for_merges(
        sheet,
        normalize_repeat_rows(setup.repeat_rows, &mut warnings),
        &mut warnings,
    );
    let repeat_cols = expand_repeat_cols_for_merges(
        sheet,
        normalize_repeat_cols(setup.repeat_cols, &mut warnings),
        &mut warnings,
    );
    let tracks_used_extent = matches!(options.render.selection, RenderSelection::Used)
        && sheet.print_metadata().print_areas().is_empty()
        && setup.print_area.is_none();

    let ranges = choose_print_ranges(sheet, &setup, &options.render)?;
    let mut areas = Vec::with_capacity(ranges.len());
    let mut logical_pages = 0_u64;
    let mut sparse_pages_omitted = 0_u64;
    let mut emitted_pages = 0_u64;
    for (area_index, range) in ranges.into_iter().enumerate() {
        let area = prepare_print_area(
            sheet,
            sheet_index,
            area_index,
            range,
            &setup,
            &behavior,
            repeat_rows,
            repeat_cols,
            content_rect,
            options,
            &mut warnings,
        )?;
        logical_pages = logical_pages
            .checked_add(area.logical_pages)
            .ok_or(RenderError::CoordinateOverflow)?;
        sparse_pages_omitted = sparse_pages_omitted
            .checked_add(area.sparse_pages_omitted)
            .ok_or(RenderError::CoordinateOverflow)?;
        emitted_pages = emitted_pages
            .checked_add(area.slots.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce_print_limit(
            LimitKind::LogicalPages,
            options.limits.max_logical_pages,
            logical_pages,
        )?;
        enforce_print_limit(LimitKind::Pages, options.limits.max_pages, emitted_pages)?;
        areas.push(area);
    }

    let mut scales = areas
        .iter()
        .map(|area| area.scale_permille)
        .collect::<BTreeSet<_>>();
    let scale_permille = if scales.len() == 1 {
        scales.pop_first().unwrap_or(1_000)
    } else {
        warnings.add(PrintWarningCode::MultipleAreaScales);
        0
    };
    let first_page_number = u64::from(setup.first_page_number.unwrap_or(1));
    let total_pages = emitted_pages;
    let mut page_map = Vec::with_capacity(emitted_pages as usize);
    let mut output_index = 0_usize;
    for area in &areas {
        for &slot in &area.slots {
            let displayed_page_number = first_page_number.saturating_add(output_index as u64);
            let map = PageMapEntry {
                output_index,
                displayed_page_number,
                area_index: area.area_index,
                horizontal_index: slot.horizontal_index,
                vertical_index: slot.vertical_index,
                manual_col_break_before: slot.columns.manual_break_before,
                manual_row_break_before: slot.rows.manual_break_before,
                body_range: RenderRange::new(
                    slot.rows.first,
                    slot.columns.first,
                    slot.rows.last,
                    slot.columns.last,
                ),
                repeat_rows: area.repeat_rows,
                repeat_cols: area.repeat_cols,
                scale_permille: area.scale_permille,
            };
            collect_running_text_warnings(
                behavior
                    .header_footer
                    .select(output_index, displayed_page_number),
                &sheet.name,
                displayed_page_number,
                total_pages,
                &mut warnings,
            );
            page_map.push(map);
            output_index = output_index
                .checked_add(1)
                .ok_or(RenderError::CoordinateOverflow)?;
        }
    }

    let sources = areas
        .iter()
        .map(|area| area.source.clone())
        .collect::<Vec<_>>();
    let source = sources
        .first()
        .cloned()
        .ok_or(RenderError::CoordinateOverflow)?;
    let report = PrintReport {
        schema_version: 2,
        source,
        sources,
        paper,
        content_rect,
        layout_override: None,
        page_order: Some(behavior.page_order),
        manual_row_breaks: sheet.print_metadata().manual_row_breaks().to_vec(),
        manual_col_breaks: sheet.print_metadata().manual_col_breaks().to_vec(),
        scale_permille,
        logical_pages,
        sparse_pages_omitted,
        pages: page_map,
        warnings: warnings.finish(),
    };
    let identity_ranges = areas
        .iter()
        .flat_map(|area| area.measurement_ranges.iter().copied())
        .collect::<Vec<_>>();
    let style_identity_ranges = areas
        .iter()
        .flat_map(|area| area.render_ranges.iter().copied())
        .collect::<Vec<_>>();
    let mut source_dependencies = BTreeSet::new();
    for area in &areas {
        source_dependencies.extend(external_render_dependency_cells(
            sheet,
            &area.render_ranges,
            &area.source_options,
            Some(SheetGeometryOverride::new(
                &area.measured_rows,
                &area.measured_columns,
            )),
        )?);
    }
    let source_dependencies = source_dependencies.into_iter().collect::<Vec<_>>();
    let source_identity = print_source_identity(
        sheet,
        sheet_index,
        &identity_ranges,
        &style_identity_ranges,
        &options.render.limits,
        &source_dependencies,
    )?;
    Ok(PreparedPrintDocument {
        report,
        limits: options.limits.clone(),
        render_limits: options.render.limits.clone(),
        source_identity,
        source_dependencies,
        tracks_used_extent,
        state: PreparedPrintState::Paginated {
            behavior,
            paper,
            content_rect,
            header_y,
            footer_y,
            areas,
            total_pages,
        },
    })
}

fn collect_running_text_warnings(
    running_text: RunningText<'_>,
    sheet_name: &str,
    page_number: u64,
    total_pages: u64,
    warnings: &mut PrintWarnings,
) {
    for source in [running_text.header, running_text.footer]
        .into_iter()
        .flatten()
        .filter(|text| !text.is_empty())
    {
        let _ = expand_running_text(source, sheet_name, page_number, total_pages, warnings);
    }
}

struct Sha256DebugWriter<'a> {
    digest: &'a mut Sha256,
}

impl fmt::Write for Sha256DebugWriter<'_> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.digest.update(value.as_bytes());
        Ok(())
    }
}

fn update_debug_streaming<T: fmt::Debug + ?Sized>(digest: &mut Sha256, value: &T) {
    {
        let mut writer = Sha256DebugWriter { digest };
        fmt::write(&mut writer, format_args!("{value:?}"))
            .expect("SHA-256 debug writer is infallible");
    }
    // Rust Debug output is UTF-8, so 0xff is an unambiguous structural record
    // terminator without buffering the complete sidecar.
    digest.update([0xff]);
}

fn merged_identity_row_bands(ranges: &[RenderRange]) -> Vec<(u32, u32)> {
    let mut bands = ranges
        .iter()
        .map(|range| (range.first_row, range.last_row))
        .collect::<Vec<_>>();
    bands.sort_unstable();
    let mut merged = Vec::<(u32, u32)>::with_capacity(bands.len());
    for (first_row, last_row) in bands {
        if first_row > last_row {
            continue;
        }
        if let Some((_, previous_last_row)) = merged.last_mut() {
            if first_row <= previous_last_row.saturating_add(1) {
                *previous_last_row = (*previous_last_row).max(last_row);
                continue;
            }
        }
        merged.push((first_row, last_row));
    }
    merged
}

fn print_source_identity(
    sheet: &Sheet,
    sheet_index: usize,
    ranges: &[RenderRange],
    style_ranges: &[RenderRange],
    limits: &RenderLimits,
    source_dependencies: &[CellCoordinate],
) -> Result<[u8; 32], RenderError> {
    fn update_bytes(digest: &mut Sha256, bytes: &[u8]) {
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }

    fn update_debug<T: fmt::Debug + ?Sized>(digest: &mut Sha256, value: &T) {
        update_bytes(digest, format!("{value:?}").as_bytes());
    }

    let mut digest = Sha256::new();
    update_bytes(&mut digest, b"rxls.prepared-print-source.v6");
    digest.update((sheet_index as u64).to_le_bytes());
    update_bytes(&mut digest, sheet.name.as_bytes());

    // Geometry and print metadata are all ordered maps/sets or stable typed
    // values, so their Debug representations are deterministic within this
    // private identity version. Image payloads are streamed separately.
    update_debug(&mut digest, &sheet.sheet_view());
    update_debug(&mut digest, &sheet.page_setup());
    update_debug(&mut digest, sheet.print_metadata());
    update_debug(&mut digest, &sheet.default_cell_style());
    update_debug(&mut digest, &sheet.style_fidelity());
    update_debug(&mut digest, sheet.style_losses());
    update_debug(&mut digest, sheet.column_styles());
    update_debug(&mut digest, sheet.row_styles());
    update_debug(&mut digest, sheet.blank_cell_styles());
    update_debug(&mut digest, sheet.tables());
    update_debug(&mut digest, sheet.table_header_styles());
    update_debug(&mut digest, sheet.column_widths());
    update_debug(&mut digest, sheet.physical_column_widths());
    update_debug(&mut digest, &sheet.default_column_width());
    update_debug(&mut digest, &sheet.implicit_ooxml_column_width());
    update_debug(&mut digest, sheet.row_heights());
    update_debug(&mut digest, &sheet.default_row_height());
    update_debug(&mut digest, &sheet.has_implicit_ooxml_row_height());
    update_debug(&mut digest, sheet.hidden_columns());
    update_debug(&mut digest, sheet.hidden_rows());
    update_debug(&mut digest, &sheet.default_hidden_row_exceptions());
    update_debug(&mut digest, sheet.merged_ranges());
    update_debug(&mut digest, sheet.conditional_formats());
    update_debug(&mut digest, sheet.conditional_format_metadata());
    let mut style_ranges = style_ranges.to_vec();
    for &(first_row, first_col, last_row, last_col) in sheet.merged_ranges() {
        if first_row <= last_row
            && first_col <= last_col
            && style_ranges.iter().any(|range| {
                first_row <= range.last_row
                    && last_row >= range.first_row
                    && first_col <= range.last_col
                    && last_col >= range.first_col
            })
        {
            style_ranges.push(RenderRange::new(first_row, first_col, first_row, first_col));
        }
    }
    style_ranges.sort_by_key(|range| {
        (
            range.first_row,
            range.first_col,
            range.last_row,
            range.last_col,
        )
    });
    style_ranges.dedup();
    update_debug(&mut digest, &style_ranges);
    let style_ranges = style_ranges
        .iter()
        .map(|range| {
            (
                range.first_row,
                range.first_col,
                range.last_row,
                range.last_col,
            )
        })
        .collect::<Vec<_>>();
    let style_sidecar_entries = sheet.render_style_sidecar_entry_count(&style_ranges);
    if style_sidecar_entries > limits.max_cells {
        return Err(RenderError::LimitExceeded {
            kind: LimitKind::Cells,
            limit: limits.max_cells,
            actual: style_sidecar_entries,
        });
    }
    update_bytes(&mut digest, b"rxls.render-style-sidecar.v2");
    update_debug_streaming(
        &mut digest,
        &sheet.render_style_sidecar_identity(&style_ranges),
    );
    update_debug(&mut digest, sheet.charts());
    update_debug(&mut digest, sheet.sparklines());
    update_debug(&mut digest, sheet.drawing_metadata());
    let mut media_bytes = 0_u64;
    for image in sheet.images() {
        update_debug(&mut digest, &image.format);
        update_debug(&mut digest, &image.from);
        update_debug(&mut digest, &image.to);
        media_bytes = media_bytes
            .checked_add(image.data.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        if media_bytes > limits.max_media_bytes {
            return Err(RenderError::LimitExceeded {
                kind: LimitKind::MediaBytes,
                limit: limits.max_media_bytes,
                actual: media_bytes,
            });
        }
        update_bytes(&mut digest, &image.data);
    }

    let mut ranges = ranges.to_vec();
    ranges.sort_by_key(|range| {
        (
            range.first_row,
            range.first_col,
            range.last_row,
            range.last_col,
        )
    });
    ranges.dedup();
    update_debug(&mut digest, &ranges);

    let display_cell_index = SparseDisplayCellIndex::new(sheet);
    let mut selected_cells = BTreeMap::new();
    for (first_row, last_row) in merged_identity_row_bands(&ranges) {
        for cell in display_cell_index.range((first_row, 0, last_row, MAX_WORKSHEET_COLUMN)) {
            selected_cells.insert((cell.row, cell.col), cell);
            if selected_cells.len() as u64 > limits.max_cells {
                return Err(RenderError::LimitExceeded {
                    kind: LimitKind::Cells,
                    limit: limits.max_cells,
                    actual: selected_cells.len() as u64,
                });
            }
        }
    }
    for &(r0, c0, r1, c1) in sheet.merged_ranges() {
        if ranges.iter().any(|range| {
            r0 <= r1
                && c0 <= c1
                && r0 <= range.last_row
                && r1 >= range.first_row
                && c0 <= range.last_col
                && c1 >= range.first_col
        }) {
            for cell in display_cell_index.range((r0, c0, r0, c0)) {
                selected_cells.insert((r0, c0), cell);
                if selected_cells.len() as u64 > limits.max_cells {
                    return Err(RenderError::LimitExceeded {
                        kind: LimitKind::Cells,
                        limit: limits.max_cells,
                        actual: selected_cells.len() as u64,
                    });
                }
            }
        }
    }
    for ((row, col), cell) in selected_cells {
        digest.update(row.to_le_bytes());
        digest.update(col.to_le_bytes());
        update_debug(&mut digest, cell.value);
        update_bytes(&mut digest, cell.formatted.as_bytes());
        update_debug(&mut digest, &cell.explicit_style);
        update_debug(&mut digest, &cell.rich_text);
        update_debug(&mut digest, &cell.hyperlink);
        update_debug(&mut digest, &sheet.resolved_cell_style(row, col));
    }
    update_bytes(&mut digest, b"rxls.indirect-scene-dependencies.v1");
    for coordinate in source_dependencies {
        digest.update(coordinate.row.to_le_bytes());
        digest.update(coordinate.col.to_le_bytes());
        update_debug(&mut digest, &sheet.cell(coordinate.row, coordinate.col));
        update_debug(
            &mut digest,
            &sheet.formatted(coordinate.row, coordinate.col),
        );
    }
    Ok(digest.finalize().into())
}

/// Materialize exactly one requested page from a prepared page map.
pub fn build_print_page(
    workbook: &Workbook,
    prepared: &PreparedPrintDocument,
    page_index: usize,
) -> Result<PrintPage, RenderError> {
    let sheet_index = prepared.report.source.sheet_index;
    let sheet = workbook
        .sheets
        .get(sheet_index)
        .ok_or(RenderError::SheetIndexOutOfRange {
            requested: sheet_index,
            sheet_count: workbook.sheets.len(),
        })?;
    let mut media_budget = DecodedMediaBudget::new(prepared.render_limits.max_decoded_media_bytes);
    build_sheet_print_page_with_budget(sheet, prepared, page_index, &mut media_budget, true)
}

/// Materialize exactly one requested page without an owning workbook.
pub fn build_sheet_print_page(
    sheet: &Sheet,
    prepared: &PreparedPrintDocument,
    page_index: usize,
) -> Result<PrintPage, RenderError> {
    let mut media_budget = DecodedMediaBudget::new(prepared.render_limits.max_decoded_media_bytes);
    build_sheet_print_page_with_budget(sheet, prepared, page_index, &mut media_budget, true)
}

fn build_sheet_print_page_with_budget(
    sheet: &Sheet,
    prepared: &PreparedPrintDocument,
    page_index: usize,
    media_budget: &mut DecodedMediaBudget,
    verify_source: bool,
) -> Result<PrintPage, RenderError> {
    if verify_source {
        if prepared.tracks_used_extent {
            let mut options = match &prepared.state {
                PreparedPrintState::SinglePage { render_options } => render_options.clone(),
                PreparedPrintState::Paginated { areas, .. } => areas
                    .first()
                    .map(|area| area.source_options.clone())
                    .ok_or(RenderError::Backend {
                        reason: "prepared_print_state_invalid",
                    })?,
            };
            options.selection = RenderSelection::Used;
            let current_range = match &prepared.state {
                PreparedPrintState::SinglePage { .. } => render_used_scene_range(sheet, &options)?,
                PreparedPrintState::Paginated { .. } => render_used_print_range(sheet, &options)?,
            };
            if current_range != prepared.report.source.range {
                return Err(RenderError::Backend {
                    reason: "prepared_print_source_changed",
                });
            }
        }
        let identity_ranges = match &prepared.state {
            PreparedPrintState::SinglePage { .. } => vec![prepared.report.source.range],
            PreparedPrintState::Paginated { areas, .. } => areas
                .iter()
                .flat_map(|area| area.measurement_ranges.iter().copied())
                .collect(),
        };
        let style_identity_ranges = match &prepared.state {
            PreparedPrintState::SinglePage { .. } => vec![prepared.report.source.range],
            PreparedPrintState::Paginated { areas, .. } => areas
                .iter()
                .flat_map(|area| area.render_ranges.iter().copied())
                .collect(),
        };
        if print_source_identity(
            sheet,
            prepared.report.source.sheet_index,
            &identity_ranges,
            &style_identity_ranges,
            &prepared.render_limits,
            &prepared.source_dependencies,
        )? != prepared.source_identity
        {
            return Err(RenderError::Backend {
                reason: "prepared_print_source_changed",
            });
        }
    }
    let map = prepared
        .report
        .pages
        .get(page_index)
        .filter(|map| map.output_index == page_index)
        .cloned()
        .ok_or(RenderError::Backend {
            reason: "print_page_index_out_of_range",
        })?;
    let sheet_index = prepared.report.source.sheet_index;
    let scene = match &prepared.state {
        PreparedPrintState::SinglePage { render_options } => {
            let scene = media_budget.build_sheet_scene(sheet, sheet_index, render_options, None)?;
            let scene_nodes = scene_node_count(&scene.nodes)?;
            enforce_print_limit(
                LimitKind::PageSceneNodes,
                prepared
                    .limits
                    .max_total_scene_nodes
                    .min(u64::from(u32::MAX)),
                scene_nodes,
            )?;
            enforce_print_limit(
                LimitKind::TotalSceneNodes,
                prepared.limits.max_total_scene_nodes,
                scene_nodes,
            )?;
            enforce_print_limit(
                LimitKind::BackendCommands,
                prepared.limits.max_backend_commands,
                backend_command_count(&scene.nodes)?,
            )?;
            scene
        }
        PreparedPrintState::Paginated {
            behavior,
            paper,
            content_rect,
            header_y,
            footer_y,
            areas,
            total_pages,
        } => {
            let area = areas
                .iter()
                .find(|area| area.area_index == map.area_index)
                .ok_or(RenderError::Backend {
                    reason: "prepared_print_state_invalid",
                })?;
            let slot = area
                .slots
                .iter()
                .copied()
                .find(|slot| {
                    slot.horizontal_index == map.horizontal_index
                        && slot.vertical_index == map.vertical_index
                        && slot.rows.first == map.body_range.first_row
                        && slot.rows.last == map.body_range.last_row
                        && slot.columns.first == map.body_range.first_col
                        && slot.columns.last == map.body_range.last_col
                })
                .ok_or(RenderError::Backend {
                    reason: "prepared_print_state_invalid",
                })?;
            let running_text = behavior
                .header_footer
                .select(map.output_index, map.displayed_page_number);
            let mut discarded_warnings = PrintWarnings::default();
            build_page_scene(
                sheet,
                sheet_index,
                &area.source_options,
                &area.measured_rows,
                &area.measured_columns,
                behavior,
                running_text,
                *paper,
                *content_rect,
                *header_y,
                *footer_y,
                slot,
                area.repeat_rows,
                area.repeat_cols,
                area.repeated_height,
                area.repeated_width,
                area.headings_height,
                area.headings_width,
                area.scale_permille,
                map.displayed_page_number,
                *total_pages,
                &mut discarded_warnings,
                &prepared.limits,
                media_budget,
            )?
        }
    };
    Ok(PrintPage { map, scene })
}

/// Build and retain every page for native multi-page backends.
pub fn build_print_document(
    workbook: &Workbook,
    sheet_index: usize,
    options: &PrintOptions,
) -> Result<PrintDocument, RenderError> {
    let prepared = prepare_print_document(workbook, sheet_index, options)?;
    let sheet_index = prepared.report.source.sheet_index;
    let sheet = workbook
        .sheets
        .get(sheet_index)
        .ok_or(RenderError::SheetIndexOutOfRange {
            requested: sheet_index,
            sheet_count: workbook.sheets.len(),
        })?;
    let mut pages = Vec::with_capacity(prepared.report.pages.len());
    let mut total_scene_nodes = 0_u64;
    let mut media_budget = DecodedMediaBudget::new(prepared.render_limits.max_decoded_media_bytes);
    for page_index in 0..prepared.report.pages.len() {
        let page = build_sheet_print_page_with_budget(
            sheet,
            &prepared,
            page_index,
            &mut media_budget,
            false,
        )?;
        total_scene_nodes = total_scene_nodes
            .checked_add(scene_node_count(&page.scene.nodes)?)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce_print_limit(
            LimitKind::TotalSceneNodes,
            prepared.limits.max_total_scene_nodes,
            total_scene_nodes,
        )?;
        pages.push(page);
    }
    Ok(PrintDocument {
        pages,
        report: prepared.report,
        limits: prepared.limits,
    })
}

/// Build and retain every page without requiring an owning workbook.
pub fn build_sheet_print_document(
    sheet: &Sheet,
    sheet_index: usize,
    options: &PrintOptions,
) -> Result<PrintDocument, RenderError> {
    let prepared = prepare_sheet_print_document(sheet, sheet_index, options)?;
    let mut pages = Vec::with_capacity(prepared.report.pages.len());
    let mut total_scene_nodes = 0_u64;
    let mut media_budget = DecodedMediaBudget::new(prepared.render_limits.max_decoded_media_bytes);
    for page_index in 0..prepared.report.pages.len() {
        let page = build_sheet_print_page_with_budget(
            sheet,
            &prepared,
            page_index,
            &mut media_budget,
            false,
        )?;
        total_scene_nodes = total_scene_nodes
            .checked_add(scene_node_count(&page.scene.nodes)?)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce_print_limit(
            LimitKind::TotalSceneNodes,
            prepared.limits.max_total_scene_nodes,
            total_scene_nodes,
        )?;
        pages.push(page);
    }
    Ok(PrintDocument {
        pages,
        report: prepared.report,
        limits: prepared.limits,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_page_scene(
    sheet: &Sheet,
    sheet_index: usize,
    source_options: &RenderOptions,
    measured_rows: &[MeasuredAxisSlot<u32>],
    measured_columns: &[MeasuredAxisSlot<u16>],
    behavior: &PrintBehavior,
    running_text: RunningText<'_>,
    paper: PaperGeometry,
    content_rect: Rect,
    header_y: Fixed,
    footer_y: Fixed,
    slot: PageSlot,
    repeat_rows: Option<(u32, u32)>,
    repeat_cols: Option<(u16, u16)>,
    repeated_height: Fixed,
    repeated_width: Fixed,
    headings_height: Fixed,
    headings_width: Fixed,
    scale_permille: u16,
    displayed_page_number: u64,
    total_pages: u64,
    warnings: &mut PrintWarnings,
    limits: &PrintLimits,
    media_budget: &mut DecodedMediaBudget,
) -> Result<Scene, RenderError> {
    let unscaled_width = headings_width
        .checked_add(repeated_width)
        .and_then(|value| value.checked_add(slot.columns.size))
        .ok_or(RenderError::CoordinateOverflow)?;
    let unscaled_height = headings_height
        .checked_add(repeated_height)
        .and_then(|value| value.checked_add(slot.rows.size))
        .ok_or(RenderError::CoordinateOverflow)?;
    let grid_width = scale_fixed(unscaled_width, scale_permille)?;
    let grid_height = scale_fixed(unscaled_height, scale_permille)?;
    let x_center = if behavior.center_horizontally {
        positive_half_difference(content_rect.width, grid_width)
    } else {
        Fixed::ZERO
    };
    let y_center = if behavior.center_vertically {
        positive_half_difference(content_rect.height, grid_height)
    } else {
        Fixed::ZERO
    };
    let grid_x = content_rect
        .x
        .checked_add(x_center)
        .ok_or(RenderError::CoordinateOverflow)?;
    let grid_y = content_rect
        .y
        .checked_add(y_center)
        .ok_or(RenderError::CoordinateOverflow)?;
    let mut nodes = Vec::new();
    let right_to_left = sheet.sheet_view().right_to_left;

    if behavior.headings {
        append_headings(
            &mut nodes,
            sheet,
            source_options,
            measured_rows,
            measured_columns,
            slot,
            repeat_rows,
            repeat_cols,
            grid_x,
            grid_y,
            repeated_height,
            repeated_width,
            headings_height,
            headings_width,
            scale_permille,
            limits,
        )?;
    }

    let body_x = if right_to_left {
        grid_x
    } else {
        grid_x
            .checked_add(scale_fixed(
                headings_width
                    .checked_add(repeated_width)
                    .ok_or(RenderError::CoordinateOverflow)?,
                scale_permille,
            )?)
            .ok_or(RenderError::CoordinateOverflow)?
    };
    let body_y = grid_y
        .checked_add(scale_fixed(
            headings_height
                .checked_add(repeated_height)
                .ok_or(RenderError::CoordinateOverflow)?,
            scale_permille,
        )?)
        .ok_or(RenderError::CoordinateOverflow)?;
    let repeat_x = if right_to_left {
        grid_x
            .checked_add(scale_fixed(slot.columns.size, scale_permille)?)
            .ok_or(RenderError::CoordinateOverflow)?
    } else {
        grid_x
            .checked_add(scale_fixed(headings_width, scale_permille)?)
            .ok_or(RenderError::CoordinateOverflow)?
    };
    let repeat_y = grid_y
        .checked_add(scale_fixed(headings_height, scale_permille)?)
        .ok_or(RenderError::CoordinateOverflow)?;

    if slot.rows.size.raw() > 0 && slot.columns.size.raw() > 0 {
        append_block(
            &mut nodes,
            sheet,
            sheet_index,
            source_options,
            measured_rows,
            measured_columns,
            RenderRange::new(
                slot.rows.first,
                slot.columns.first,
                slot.rows.last,
                slot.columns.last,
            ),
            body_x,
            body_y,
            slot.columns.size,
            slot.rows.size,
            content_rect,
            scale_permille,
            limits,
            media_budget,
        )?;
    }
    if let Some((first_row, last_row)) = repeat_rows.filter(|_| slot.columns.size.raw() > 0) {
        append_block(
            &mut nodes,
            sheet,
            sheet_index,
            source_options,
            measured_rows,
            measured_columns,
            RenderRange::new(first_row, slot.columns.first, last_row, slot.columns.last),
            body_x,
            repeat_y,
            slot.columns.size,
            repeated_height,
            content_rect,
            scale_permille,
            limits,
            media_budget,
        )?;
    }
    if let Some((first_col, last_col)) = repeat_cols.filter(|_| slot.rows.size.raw() > 0) {
        append_block(
            &mut nodes,
            sheet,
            sheet_index,
            source_options,
            measured_rows,
            measured_columns,
            RenderRange::new(slot.rows.first, first_col, slot.rows.last, last_col),
            repeat_x,
            body_y,
            repeated_width,
            slot.rows.size,
            content_rect,
            scale_permille,
            limits,
            media_budget,
        )?;
    }
    if let (Some((first_row, last_row)), Some((first_col, last_col))) = (repeat_rows, repeat_cols) {
        append_block(
            &mut nodes,
            sheet,
            sheet_index,
            source_options,
            measured_rows,
            measured_columns,
            RenderRange::new(first_row, first_col, last_row, last_col),
            repeat_x,
            repeat_y,
            repeated_width,
            repeated_height,
            content_rect,
            scale_permille,
            limits,
            media_budget,
        )?;
    }

    let running_x = if behavior.header_footer.align_with_margins {
        content_rect.x
    } else {
        Fixed::ZERO
    };
    let running_width = if behavior.header_footer.align_with_margins {
        content_rect.width
    } else {
        paper.width
    };
    let running_scale = if behavior.header_footer.scale_with_document {
        scale_permille
    } else {
        1_000
    };
    append_running_text(
        &mut nodes,
        running_text.header,
        sheet,
        source_options,
        displayed_page_number,
        total_pages,
        running_x,
        running_width,
        header_y,
        running_scale,
        false,
        warnings,
        limits,
    )?;
    append_running_text(
        &mut nodes,
        running_text.footer,
        sheet,
        source_options,
        displayed_page_number,
        total_pages,
        running_x,
        running_width,
        footer_y,
        running_scale,
        true,
        warnings,
        limits,
    )?;

    enforce_print_limit(
        LimitKind::PageSceneNodes,
        limits.max_total_scene_nodes.min(u64::from(u32::MAX)),
        scene_node_count(&nodes)?,
    )?;
    let backend_commands = backend_command_count(&nodes)?;
    enforce_print_limit(
        LimitKind::BackendCommands,
        limits.max_backend_commands,
        backend_commands,
    )?;
    Ok(Scene {
        title: format!("{} - Page {displayed_page_number}", sheet.name),
        width: paper.width,
        height: paper.height,
        background: Rgb::WHITE,
        nodes,
    })
}

fn backend_command_count(nodes: &[SceneNode]) -> Result<u64, RenderError> {
    nodes.iter().try_fold(0_u64, |sum, node| {
        sum.checked_add(match node {
            SceneNode::ClipGroup(group) => backend_command_count(&group.nodes)?
                .checked_add(2)
                .ok_or(RenderError::CoordinateOverflow)?,
            SceneNode::Rect(_) | SceneNode::Text(_) => 1,
            SceneNode::Line(_) => 2,
            SceneNode::Path(node) => node.commands.len() as u64,
            SceneNode::Image(_) => 1,
            SceneNode::GlyphRun(node) => {
                node.commands.len() as u64 + node.decorations.len() as u64 * 2
            }
        })
        .ok_or(RenderError::CoordinateOverflow)
    })
}

fn scene_decoded_media_bytes(scene: &Scene) -> Result<u64, RenderError> {
    decoded_media_bytes(&scene.nodes)
}

fn decoded_media_bytes(nodes: &[SceneNode]) -> Result<u64, RenderError> {
    nodes.iter().try_fold(0_u64, |total, node| {
        let bytes = match node {
            SceneNode::ClipGroup(group) => decoded_media_bytes(&group.nodes)?,
            SceneNode::Image(image) => image.rgba.len() as u64,
            _ => 0,
        };
        total
            .checked_add(bytes)
            .ok_or(RenderError::CoordinateOverflow)
    })
}

fn scene_node_count(nodes: &[SceneNode]) -> Result<u64, RenderError> {
    nodes.iter().try_fold(0_u64, |total, node| {
        let descendants = match node {
            SceneNode::ClipGroup(group) => scene_node_count(&group.nodes)?,
            _ => 0,
        };
        total
            .checked_add(1)
            .and_then(|total| total.checked_add(descendants))
            .ok_or(RenderError::CoordinateOverflow)
    })
}

#[allow(clippy::too_many_arguments)]
fn append_block(
    output: &mut Vec<SceneNode>,
    sheet: &Sheet,
    sheet_index: usize,
    base_options: &RenderOptions,
    measured_rows: &[MeasuredAxisSlot<u32>],
    measured_columns: &[MeasuredAxisSlot<u16>],
    range: RenderRange,
    x: Fixed,
    y: Fixed,
    width: Fixed,
    height: Fixed,
    content_rect: Rect,
    scale_permille: u16,
    limits: &PrintLimits,
    media_budget: &mut DecodedMediaBudget,
) -> Result<(), RenderError> {
    let block_clip = Rect {
        x,
        y,
        width: scale_fixed(width, scale_permille)?,
        height: scale_fixed(height, scale_permille)?,
    };
    let Some(clip) = intersect_rect(block_clip, content_rect)? else {
        return Ok(());
    };
    let mut options = base_options.clone();
    options.selection = RenderSelection::Range(range);
    let scene = media_budget.build_sheet_scene(
        sheet,
        sheet_index,
        &options,
        Some(SheetGeometryOverride::new(measured_rows, measured_columns)),
    )?;
    let mut nodes = Vec::with_capacity(scene.nodes.len());
    for node in scene.nodes {
        let transformed = transform_node(node, x, y, scale_permille)?;
        nodes.push(transformed);
    }
    if nodes.is_empty() {
        return Ok(());
    }
    output.push(SceneNode::ClipGroup(ClipGroupNode { clip, nodes }));
    enforce_print_limit(
        LimitKind::PageSceneNodes,
        limits.max_total_scene_nodes.min(u64::from(u32::MAX)),
        scene_node_count(output)?,
    )?;
    Ok(())
}

fn intersect_rect(left: Rect, right: Rect) -> Result<Option<Rect>, RenderError> {
    let left_right = left
        .x
        .checked_add(left.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let left_bottom = left
        .y
        .checked_add(left.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let right_right = right
        .x
        .checked_add(right.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let right_bottom = right
        .y
        .checked_add(right.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let end_x = left_right.min(right_right);
    let end_y = left_bottom.min(right_bottom);
    if end_x <= x || end_y <= y {
        return Ok(None);
    }
    Ok(Some(Rect {
        x,
        y,
        width: end_x
            .checked_sub(x)
            .ok_or(RenderError::CoordinateOverflow)?,
        height: end_y
            .checked_sub(y)
            .ok_or(RenderError::CoordinateOverflow)?,
    }))
}

#[allow(clippy::too_many_arguments)]
fn append_headings(
    output: &mut Vec<SceneNode>,
    sheet: &Sheet,
    options: &RenderOptions,
    measured_rows: &[MeasuredAxisSlot<u32>],
    measured_columns: &[MeasuredAxisSlot<u16>],
    slot: PageSlot,
    repeat_rows: Option<(u32, u32)>,
    repeat_cols: Option<(u16, u16)>,
    grid_x: Fixed,
    grid_y: Fixed,
    repeated_height: Fixed,
    repeated_width: Fixed,
    heading_height: Fixed,
    heading_width: Fixed,
    scale_permille: u16,
    limits: &PrintLimits,
) -> Result<(), RenderError> {
    let scaled_heading_width = scale_fixed(heading_width, scale_permille)?;
    let scaled_heading_height = scale_fixed(heading_height, scale_permille)?;
    let right_to_left = sheet.sheet_view().right_to_left;
    let row_heading_x = if right_to_left {
        let scaled_body_width = scale_fixed(slot.columns.size, scale_permille)?;
        let scaled_repeated_width = scale_fixed(repeated_width, scale_permille)?;
        grid_x
            .checked_add(scaled_body_width)
            .and_then(|value| value.checked_add(scaled_repeated_width))
            .ok_or(RenderError::CoordinateOverflow)?
    } else {
        grid_x
    };
    append_heading_cell(
        output,
        "",
        Rect {
            x: row_heading_x,
            y: grid_y,
            width: scaled_heading_width,
            height: scaled_heading_height,
        },
        options,
        limits,
    )?;
    let body_rows = if slot.rows.size.raw() > 0 {
        measured_axis_range(measured_rows, slot.rows.first, slot.rows.last)
    } else {
        Vec::new()
    };
    let body_columns = if slot.columns.size.raw() > 0 {
        measured_axis_range(measured_columns, slot.columns.first, slot.columns.last)
    } else {
        Vec::new()
    };
    let title_rows = match repeat_rows {
        Some((first, last)) => measured_axis_range(measured_rows, first, last),
        None => Vec::new(),
    };
    let title_columns = match repeat_cols {
        Some((first, last)) => measured_axis_range(measured_columns, first, last),
        None => Vec::new(),
    };
    let mut x = if right_to_left {
        grid_x
    } else {
        grid_x
            .checked_add(scaled_heading_width)
            .ok_or(RenderError::CoordinateOverflow)?
    };
    let visual_columns = if right_to_left {
        body_columns
            .iter()
            .rev()
            .chain(title_columns.iter().rev())
            .collect::<Vec<_>>()
    } else {
        title_columns
            .iter()
            .chain(&body_columns)
            .collect::<Vec<_>>()
    };
    for axis in visual_columns {
        let width = scale_fixed(axis.size, scale_permille)?;
        append_heading_cell(
            output,
            &column_label(axis.index),
            Rect {
                x,
                y: grid_y,
                width,
                height: scaled_heading_height,
            },
            options,
            limits,
        )?;
        x = x
            .checked_add(width)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    let mut y = grid_y
        .checked_add(scaled_heading_height)
        .ok_or(RenderError::CoordinateOverflow)?;
    for axis in title_rows.iter().chain(&body_rows) {
        let height = scale_fixed(axis.size, scale_permille)?;
        append_heading_cell(
            output,
            &(u64::from(axis.index) + 1).to_string(),
            Rect {
                x: row_heading_x,
                y,
                width: scaled_heading_width,
                height,
            },
            options,
            limits,
        )?;
        y = y
            .checked_add(height)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    let expected_width = scale_fixed(
        heading_width
            .checked_add(repeated_width)
            .and_then(|value| value.checked_add(slot.columns.size))
            .ok_or(RenderError::CoordinateOverflow)?,
        scale_permille,
    )?;
    let expected_height = scale_fixed(
        heading_height
            .checked_add(repeated_height)
            .and_then(|value| value.checked_add(slot.rows.size))
            .ok_or(RenderError::CoordinateOverflow)?,
        scale_permille,
    )?;
    debug_assert!(
        x.raw()
            <= grid_x
                .raw()
                .saturating_add(expected_width.raw())
                .saturating_add(2)
    );
    debug_assert!(
        y.raw()
            <= grid_y
                .raw()
                .saturating_add(expected_height.raw())
                .saturating_add(2)
    );
    Ok(())
}

fn append_heading_cell(
    output: &mut Vec<SceneNode>,
    text: &str,
    rect: Rect,
    options: &RenderOptions,
    limits: &PrintLimits,
) -> Result<(), RenderError> {
    output.push(SceneNode::Rect(RectNode {
        rect,
        fill: Some(Rgb::new(242, 242, 242)),
        stroke: Some(Rgb::new(166, 166, 166)),
        stroke_width: Fixed::from_pixels(1),
    }));
    if !text.is_empty() {
        output.push(build_auxiliary_text_node(
            text.to_string(),
            rect,
            Fixed::from_pixels(2),
            TextStyle {
                family: options.default_font_family.clone(),
                size: HEADER_FOOTER_FONT_SIZE,
                color: Rgb::BLACK,
                bold: false,
                italic: false,
                underline: false,
                strikethrough: false,
                anchor: TextAnchor::Middle,
                baseline: TextBaseline::Middle,
                rotation_degrees: 0,
            },
            options,
        )?);
    }
    enforce_print_limit(
        LimitKind::PageSceneNodes,
        limits.max_total_scene_nodes.min(u64::from(u32::MAX)),
        output.len() as u64,
    )
}

#[allow(clippy::too_many_arguments)]
fn append_running_text(
    output: &mut Vec<SceneNode>,
    source: Option<&str>,
    sheet: &Sheet,
    options: &RenderOptions,
    page_number: u64,
    total_pages: u64,
    page_x: Fixed,
    page_width: Fixed,
    base_y: Fixed,
    text_scale_permille: u16,
    bottom_aligned: bool,
    warnings: &mut PrintWarnings,
    limits: &PrintLimits,
) -> Result<(), RenderError> {
    let Some(source) = source.filter(|text| !text.is_empty()) else {
        return Ok(());
    };
    let sections = expand_running_text(source, &sheet.name, page_number, total_pages, warnings);
    let third = Fixed::from_raw(page_width.raw() / 3);
    let height = scale_fixed(Fixed::from_pixels(16), text_scale_permille)?;
    let padding = scale_fixed(Fixed::from_pixels(4), text_scale_permille)?;
    let font_size = scale_fixed(HEADER_FOOTER_FONT_SIZE, text_scale_permille)?;
    let y = if bottom_aligned {
        base_y
            .checked_add(Fixed::from_pixels(16))
            .and_then(|value| value.checked_sub(height))
            .ok_or(RenderError::CoordinateOverflow)?
    } else {
        base_y
    };
    for (index, text) in sections.into_iter().enumerate() {
        if text.is_empty() {
            continue;
        }
        let section_x = Fixed::from_raw(third.raw().saturating_mul(index as i64));
        let width = if index == 2 {
            page_width
                .checked_sub(section_x)
                .ok_or(RenderError::CoordinateOverflow)?
        } else {
            third
        };
        let bounds = Rect {
            x: page_x
                .checked_add(section_x)
                .ok_or(RenderError::CoordinateOverflow)?,
            y,
            width,
            height,
        };
        output.push(build_auxiliary_text_node(
            text,
            bounds,
            padding,
            TextStyle {
                family: options.default_font_family.clone(),
                size: font_size,
                color: Rgb::BLACK,
                bold: false,
                italic: false,
                underline: false,
                strikethrough: false,
                anchor: match index {
                    0 => TextAnchor::Start,
                    1 => TextAnchor::Middle,
                    _ => TextAnchor::End,
                },
                baseline: TextBaseline::Middle,
                rotation_degrees: 0,
            },
            options,
        )?);
        enforce_print_limit(
            LimitKind::PageSceneNodes,
            limits.max_total_scene_nodes.min(u64::from(u32::MAX)),
            output.len() as u64,
        )?;
    }
    Ok(())
}

fn expand_running_text(
    source: &str,
    sheet_name: &str,
    page_number: u64,
    total_pages: u64,
    warnings: &mut PrintWarnings,
) -> [String; 3] {
    let chars: Vec<char> = source.chars().collect();
    let mut sections = [String::new(), String::new(), String::new()];
    let mut section = 1_usize;
    let mut index = 0_usize;
    while index < chars.len() {
        if chars[index] != '&' {
            sections[section].push(chars[index]);
            index += 1;
            continue;
        }
        index += 1;
        let Some(&code) = chars.get(index) else {
            sections[section].push('&');
            break;
        };
        index += 1;
        match code {
            '&' => sections[section].push('&'),
            'L' | 'l' => section = 0,
            'C' | 'c' => section = 1,
            'R' | 'r' => section = 2,
            'P' | 'p' => sections[section].push_str(&page_number.to_string()),
            'N' | 'n' => sections[section].push_str(&total_pages.to_string()),
            'A' | 'a' => sections[section].push_str(sheet_name),
            'D' | 'd' | 'T' | 't' => {
                warnings.add(PrintWarningCode::VolatileHeaderFooterFieldOmitted);
            }
            '"' => {
                while index < chars.len() && chars[index] != '"' {
                    index += 1;
                }
                if index < chars.len() {
                    index += 1;
                }
                warnings.add(PrintWarningCode::HeaderFooterFormattingSimplified);
            }
            value if value.is_ascii_digit() => {
                while index < chars.len() && chars[index].is_ascii_digit() {
                    index += 1;
                }
                warnings.add(PrintWarningCode::HeaderFooterFormattingSimplified);
            }
            'K' | 'k' => {
                for _ in 0..6 {
                    if index < chars.len() && chars[index].is_ascii_alphanumeric() {
                        index += 1;
                    }
                }
                warnings.add(PrintWarningCode::HeaderFooterFormattingSimplified);
            }
            'B' | 'b' | 'I' | 'i' | 'U' | 'u' | 'E' | 'e' | 'S' | 's' | 'X' | 'x' | 'Y' | 'y'
            | '+' | '-' | 'O' | 'o' | 'H' | 'h' => {
                warnings.add(PrintWarningCode::HeaderFooterFormattingSimplified)
            }
            'F' | 'f' | 'Z' | 'z' | 'G' | 'g' => {
                warnings.add(PrintWarningCode::HeaderFooterSourceFieldOmitted);
            }
            other => {
                sections[section].push('&');
                sections[section].push(other);
            }
        }
    }
    sections
}

fn validate_range(range: RenderRange) -> Result<RenderRange, RenderError> {
    if range.first_row > range.last_row || range.first_col > range.last_col {
        return Err(RenderError::InvalidRange {
            first_row: range.first_row,
            first_col: range.first_col,
            last_row: range.last_row,
            last_col: range.last_col,
        });
    }
    if range.last_row > MAX_WORKSHEET_ROW || range.last_col > MAX_WORKSHEET_COLUMN {
        return Err(RenderError::RangeOutsideGrid {
            last_row: range.last_row,
            last_col: range.last_col,
            max_row: MAX_WORKSHEET_ROW,
            max_col: MAX_WORKSHEET_COLUMN,
        });
    }
    Ok(range)
}

fn normalize_repeat_rows(
    range: Option<(u32, u32)>,
    warnings: &mut PrintWarnings,
) -> Option<(u32, u32)> {
    match range {
        Some((first, last)) if first <= last && last <= MAX_WORKSHEET_ROW => Some((first, last)),
        Some(_) => {
            warnings.add(PrintWarningCode::InvalidPrintTitlesIgnored);
            None
        }
        None => None,
    }
}

fn normalize_repeat_cols(
    range: Option<(u16, u16)>,
    warnings: &mut PrintWarnings,
) -> Option<(u16, u16)> {
    match range {
        Some((first, last)) if first <= last && last <= MAX_WORKSHEET_COLUMN => Some((first, last)),
        Some(_) => {
            warnings.add(PrintWarningCode::InvalidPrintTitlesIgnored);
            None
        }
        None => None,
    }
}

fn expand_repeat_rows_for_merges(
    sheet: &Sheet,
    range: Option<(u32, u32)>,
    warnings: &mut PrintWarnings,
) -> Option<(u32, u32)> {
    let (mut first, mut last) = range?;
    loop {
        let previous = (first, last);
        for &(merge_first, _, merge_last, _) in sheet.merged_ranges() {
            if merge_first <= last && merge_last >= first {
                first = first.min(merge_first);
                last = last.max(merge_last);
            }
        }
        if (first, last) == previous {
            break;
        }
        warnings.add(PrintWarningCode::PrintTitlesExpandedForMerge);
    }
    Some((first, last))
}

fn expand_repeat_cols_for_merges(
    sheet: &Sheet,
    range: Option<(u16, u16)>,
    warnings: &mut PrintWarnings,
) -> Option<(u16, u16)> {
    let (mut first, mut last) = range?;
    loop {
        let previous = (first, last);
        for &(_, merge_first, _, merge_last) in sheet.merged_ranges() {
            if merge_first <= last && merge_last >= first {
                first = first.min(merge_first);
                last = last.max(merge_last);
            }
        }
        if (first, last) == previous {
            break;
        }
        warnings.add(PrintWarningCode::PrintTitlesExpandedForMerge);
    }
    Some((first, last))
}

fn paper_geometry(
    code: Option<u16>,
    landscape: bool,
    warnings: &mut PrintWarnings,
) -> Result<PaperGeometry, RenderError> {
    let requested = code.unwrap_or(1);
    let (resolved, width_in, height_in) = match requested {
        1 => (1, 8.5, 11.0),
        2 => (2, 8.5, 11.0),
        3 => (3, 11.0, 17.0),
        4 => (4, 17.0, 11.0),
        5 => (5, 8.5, 14.0),
        6 => (6, 5.5, 8.5),
        7 => (7, 7.25, 10.5),
        8 => (8, 11.692_913_385_8, 16.535_433_070_9),
        9 => (9, 8.267_716_535_4, 11.692_913_385_8),
        10 => (10, 8.267_716_535_4, 11.692_913_385_8),
        11 => (11, 5.826_771_653_5, 8.267_716_535_4),
        12 => (12, mm_to_inches(257.0), mm_to_inches(364.0)),
        13 => (13, mm_to_inches(182.0), mm_to_inches(257.0)),
        14 => (14, 8.5, 13.0),
        15 => (15, mm_to_inches(215.0), mm_to_inches(275.0)),
        16 => (16, 10.0, 14.0),
        17 => (17, 11.0, 17.0),
        18 => (18, 8.5, 11.0),
        19 => (19, 3.875, 8.875),
        20 => (20, 4.125, 9.5),
        21 => (21, 4.5, 10.375),
        22 => (22, 4.75, 11.0),
        23 => (23, 5.0, 11.5),
        27 => (27, mm_to_inches(110.0), mm_to_inches(220.0)),
        28 => (28, mm_to_inches(162.0), mm_to_inches(229.0)),
        29 => (29, mm_to_inches(324.0), mm_to_inches(458.0)),
        30 => (30, mm_to_inches(229.0), mm_to_inches(324.0)),
        31 => (31, mm_to_inches(114.0), mm_to_inches(162.0)),
        42 => (42, mm_to_inches(250.0), mm_to_inches(353.0)),
        43 => (43, mm_to_inches(100.0), mm_to_inches(148.0)),
        66 => (66, mm_to_inches(420.0), mm_to_inches(594.0)),
        70 => (70, mm_to_inches(105.0), mm_to_inches(148.0)),
        _ => {
            warnings.add(PrintWarningCode::UnknownPaperSizeFallback);
            (1, 8.5, 11.0)
        }
    };
    let mut width = inches_to_fixed(width_in)?;
    let mut height = inches_to_fixed(height_in)?;
    if landscape {
        std::mem::swap(&mut width, &mut height);
    }
    Ok(PaperGeometry {
        paper_code: resolved,
        width,
        height,
    })
}

const fn mm_to_inches(millimeters: f64) -> f64 {
    millimeters / 25.4
}

fn content_geometry(
    paper: PaperGeometry,
    margins: Option<(f64, f64, f64, f64, f64, f64)>,
    warnings: &mut PrintWarnings,
) -> Result<(Rect, Fixed, Fixed), RenderError> {
    let defaults = (
        DEFAULT_LEFT_RIGHT_INCHES,
        DEFAULT_LEFT_RIGHT_INCHES,
        DEFAULT_TOP_BOTTOM_INCHES,
        DEFAULT_TOP_BOTTOM_INCHES,
        DEFAULT_HEADER_FOOTER_INCHES,
        DEFAULT_HEADER_FOOTER_INCHES,
    );
    let valid = margins.filter(|values| {
        let all = [values.0, values.1, values.2, values.3, values.4, values.5];
        all.iter().all(|value| value.is_finite() && *value >= 0.0)
            && (values.0 + values.1) * 96.0
                < paper.width.raw() as f64 / FIXED_UNITS_PER_PIXEL as f64
            && (values.2 + values.3) * 96.0
                < paper.height.raw() as f64 / FIXED_UNITS_PER_PIXEL as f64
            && values.4 * 96.0 < paper.height.raw() as f64 / FIXED_UNITS_PER_PIXEL as f64
            && values.5 * 96.0 < paper.height.raw() as f64 / FIXED_UNITS_PER_PIXEL as f64
    });
    if margins.is_some() && valid.is_none() {
        warnings.add(PrintWarningCode::InvalidMarginsFallback);
    }
    let (left, right, top, bottom, header, footer) = valid.unwrap_or(defaults);
    let left = inches_to_fixed(left)?;
    let right = inches_to_fixed(right)?;
    let top = inches_to_fixed(top)?;
    let bottom = inches_to_fixed(bottom)?;
    let header = inches_to_fixed(header)?;
    let footer = inches_to_fixed(footer)?;
    let width = paper
        .width
        .checked_sub(left)
        .and_then(|value| value.checked_sub(right))
        .ok_or(RenderError::CoordinateOverflow)?;
    let height = paper
        .height
        .checked_sub(top)
        .and_then(|value| value.checked_sub(bottom))
        .ok_or(RenderError::CoordinateOverflow)?;
    let header_y = header;
    let footer_y = paper
        .height
        .checked_sub(footer)
        .and_then(|value| value.checked_sub(Fixed::from_pixels(16)))
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok((
        Rect {
            x: left,
            y: top,
            width,
            height,
        },
        header_y,
        footer_y,
    ))
}

#[allow(clippy::too_many_arguments)]
fn choose_scale(
    setup: &PageSetup,
    rows: &[MeasuredAxisSlot<u32>],
    columns: &[MeasuredAxisSlot<u16>],
    repeated_height: Fixed,
    repeated_width: Fixed,
    headings_height: Fixed,
    headings_width: Fixed,
    content: Rect,
    row_merges: &[(u32, u32)],
    col_merges: &[(u16, u16)],
    row_breaks: &[u32],
    col_breaks: &[u16],
    warnings: &mut PrintWarnings,
) -> Result<u16, RenderError> {
    let fit_width = setup.fit_to_width.filter(|value| *value != 0);
    let fit_height = setup.fit_to_height.filter(|value| *value != 0);
    if fit_width.is_none() && fit_height.is_none() {
        let requested = setup.scale.unwrap_or(100);
        let clamped = requested.clamp(10, 400);
        if requested != clamped {
            warnings.add(PrintWarningCode::PrintScaleClamped);
        }
        return Ok(clamped.saturating_mul(10));
    }
    let fits = |scale| -> Result<bool, RenderError> {
        if scale_fixed(
            repeated_height
                .checked_add(headings_height)
                .ok_or(RenderError::CoordinateOverflow)?,
            scale,
        )? > content.height
            || scale_fixed(
                repeated_width
                    .checked_add(headings_width)
                    .ok_or(RenderError::CoordinateOverflow)?,
                scale,
            )? > content.width
        {
            return Ok(false);
        }
        let row_capacity =
            unscaled_capacity(content.height, scale, repeated_height, headings_height)?;
        let col_capacity = unscaled_capacity(content.width, scale, repeated_width, headings_width)?;
        let (row_pages, _, _) =
            partition_axis_with_breaks(rows, row_capacity, row_merges, row_breaks)?;
        let (col_pages, _, _) =
            partition_axis_with_breaks(columns, col_capacity, col_merges, col_breaks)?;
        Ok(row_pages.iter().all(|page| page.size <= row_capacity)
            && col_pages.iter().all(|page| page.size <= col_capacity)
            && fit_height.is_none_or(|target| row_pages.len() <= usize::from(target))
            && fit_width.is_none_or(|target| col_pages.len() <= usize::from(target)))
    };
    if !fits(MIN_PRINT_SCALE_PERMILLE)? {
        warnings.add(PrintWarningCode::FitTargetUnreachable);
        return Ok(MIN_PRINT_SCALE_PERMILLE);
    }
    let mut low = MIN_PRINT_SCALE_PERMILLE;
    let mut high = MAX_PRINT_SCALE_PERMILLE;
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if fits(middle)? {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Ok(low)
}

fn unscaled_capacity(
    page_capacity: Fixed,
    scale_permille: u16,
    repeated: Fixed,
    headings: Fixed,
) -> Result<Fixed, RenderError> {
    let raw = i128::from(page_capacity.raw())
        .checked_mul(1_000)
        .and_then(|value| value.checked_div(i128::from(scale_permille)))
        .ok_or(RenderError::CoordinateOverflow)?;
    let raw = i64::try_from(raw).map_err(|_| RenderError::CoordinateOverflow)?;
    Ok(Fixed::from_raw(raw)
        .checked_sub(repeated)
        .and_then(|value| value.checked_sub(headings))
        .unwrap_or(Fixed::from_raw(1))
        .max(Fixed::from_raw(1)))
}

fn partition_axis<I>(
    slots: &[MeasuredAxisSlot<I>],
    capacity: Fixed,
    merges: &[(I, I)],
) -> Result<(Vec<AxisSegment<I>>, u64), RenderError>
where
    I: Copy + Ord,
{
    if slots.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let mut output = Vec::new();
    let mut position = 0_usize;
    let mut expansions = 0_u64;
    while position < slots.len() {
        let mut next = position;
        let mut size = Fixed::ZERO;
        while next < slots.len() {
            let candidate = size
                .checked_add(slots[next].size)
                .ok_or(RenderError::CoordinateOverflow)?;
            if next > position && candidate > capacity {
                break;
            }
            size = candidate;
            next += 1;
        }
        if next == position {
            next += 1;
            size = slots[position].size;
        }
        if next < slots.len() {
            let boundary = slots[next].index;
            if let Some(&(merge_first, merge_last)) = merges
                .iter()
                .find(|(first, last)| *first < boundary && boundary <= *last)
            {
                let merge_start = slots
                    .iter()
                    .position(|slot| slot.index >= merge_first)
                    .unwrap_or(position);
                if merge_start > position {
                    next = merge_start;
                } else {
                    let merge_end = slots
                        .iter()
                        .rposition(|slot| slot.index <= merge_last)
                        .map_or(next, |index| index + 1);
                    if merge_end > next {
                        next = merge_end;
                        expansions = expansions.saturating_add(1);
                    }
                }
                size = axis_slice_total(&slots[position..next])?;
            }
        }
        output.push(AxisSegment {
            first: slots[position].index,
            last: slots[next - 1].index,
            size,
            manual_break_before: false,
        });
        position = next;
    }
    Ok((output, expansions))
}

fn partition_axis_with_breaks<I>(
    slots: &[MeasuredAxisSlot<I>],
    capacity: Fixed,
    merges: &[(I, I)],
    manual_breaks: &[I],
) -> Result<(Vec<AxisSegment<I>>, u64, u64), RenderError>
where
    I: Copy + Ord,
{
    if slots.is_empty() {
        return Ok((Vec::new(), 0, 0));
    }

    let mut cuts = vec![(0_usize, false)];
    let mut shifted_breaks = 0_u64;
    for &manual_break in manual_breaks {
        let position = slots.partition_point(|slot| slot.index < manual_break);
        if position == 0 || position == slots.len() {
            continue;
        }
        let boundary = slots[position].index;
        let previous = cuts.last().map_or(0, |(position, _)| *position);
        let mut adjusted = position;
        if let Some(&(merge_first, merge_last)) = merges
            .iter()
            .find(|(first, last)| *first < boundary && boundary <= *last)
        {
            let merge_start = slots.partition_point(|slot| slot.index < merge_first);
            let merge_end = slots.partition_point(|slot| slot.index <= merge_last);
            adjusted = if merge_start > previous {
                merge_start
            } else {
                merge_end
            };
            shifted_breaks = shifted_breaks.saturating_add(1);
        }
        if adjusted <= previous || adjusted >= slots.len() {
            continue;
        }
        cuts.push((adjusted, true));
    }
    cuts.push((slots.len(), false));

    let mut output = Vec::new();
    let mut expansions = 0_u64;
    for window in cuts.windows(2) {
        let (start, manual_break_before) = window[0];
        let end = window[1].0;
        if start >= end {
            continue;
        }
        let (mut segments, slice_expansions) =
            partition_axis(&slots[start..end], capacity, merges)?;
        if let Some(first) = segments.first_mut() {
            first.manual_break_before = manual_break_before;
        }
        expansions = expansions.saturating_add(slice_expansions);
        output.extend(segments);
    }
    Ok((output, expansions, shifted_breaks))
}

fn ensure_row_segment(
    segments: Vec<AxisSegment<u32>>,
    range: RenderRange,
) -> Vec<AxisSegment<u32>> {
    if segments.is_empty() {
        vec![AxisSegment {
            first: range.first_row,
            last: range.first_row,
            size: Fixed::ZERO,
            manual_break_before: false,
        }]
    } else {
        segments
    }
}

fn ensure_column_segment(
    segments: Vec<AxisSegment<u16>>,
    range: RenderRange,
) -> Vec<AxisSegment<u16>> {
    if segments.is_empty() {
        vec![AxisSegment {
            first: range.first_col,
            last: range.first_col,
            size: Fixed::ZERO,
            manual_break_before: false,
        }]
    } else {
        segments
    }
}

fn page_has_content(
    sheet: &Sheet,
    slot: PageSlot,
    print_range: RenderRange,
    options: &RenderOptions,
    geometry: SheetGeometryOverride<'_>,
) -> Result<bool, RenderError> {
    let first_row = slot.rows.first.max(print_range.first_row);
    let last_row = slot.rows.last.min(print_range.last_row);
    let first_col = slot.columns.first.max(print_range.first_col);
    let last_col = slot.columns.last.min(print_range.last_col);
    let contains = |row: u32, col: u16| {
        row >= slot.rows.first
            && row <= slot.rows.last
            && col >= slot.columns.first
            && col <= slot.columns.last
            && row >= print_range.first_row
            && row <= print_range.last_row
            && col >= print_range.first_col
            && col <= print_range.last_col
    };
    if first_row <= last_row && first_col <= last_col {
        if sheet
            .display_cells_in_range(first_row, first_col, last_row, last_col)
            .next()
            .is_some()
            || sheet
                .blank_cell_styles()
                .range((first_row, 0)..=(last_row, MAX_WORKSHEET_COLUMN))
                .any(|(&(row, col), _)| contains(row, col))
        {
            return Ok(true);
        }
        if sheet
            .default_cell_style()
            .is_some_and(cell_style_has_visible_blank_paint)
            || sheet
                .row_styles()
                .range(first_row..=last_row)
                .any(|(_, style)| cell_style_has_visible_blank_paint(style))
            || sheet
                .column_styles()
                .range(first_col..=last_col)
                .any(|(_, style)| cell_style_has_visible_blank_paint(style))
            || sheet.tables().iter().any(|table| {
                let (r0, c0, r1, c1) = table.range;
                r0 <= r1
                    && c0 <= c1
                    && r0 <= last_row
                    && r1 >= first_row
                    && c0 <= last_col
                    && c1 >= first_col
            })
            || sheet.conditional_formats().iter().any(|conditional| {
                let (r0, c0, r1, c1) = conditional.sqref;
                r0 <= r1
                    && c0 <= c1
                    && r0 <= last_row
                    && r1 >= first_row
                    && c0 <= last_col
                    && c1 >= first_col
            })
        {
            return Ok(true);
        }
    }
    if sheet.merged_ranges().iter().any(|&(r0, c0, r1, c1)| {
        r0 <= slot.rows.last
            && r1 >= slot.rows.first
            && c0 <= slot.columns.last
            && c1 >= slot.columns.first
    }) {
        return Ok(true);
    }

    if cell_drawings_intersect_prepared_range(
        sheet,
        RenderRange::new(
            slot.rows.first,
            slot.columns.first,
            slot.rows.last,
            slot.columns.last,
        ),
        geometry,
    )? {
        return Ok(true);
    }
    if sheet
        .sparklines()
        .iter()
        .any(|sparkline| contains(sparkline.location.0, sparkline.location.1))
    {
        return Ok(true);
    }
    absolute_drawings_intersect_range(
        sheet,
        RenderRange::new(
            slot.rows.first,
            slot.columns.first,
            slot.rows.last,
            slot.columns.last,
        ),
        slot.columns.size,
        slot.rows.size,
        options,
        geometry,
    )
}

fn merge_intervals_rows(sheet: &Sheet, first: u32, last: u32) -> Vec<(u32, u32)> {
    if first > last {
        return Vec::new();
    }
    let mut values = BTreeSet::new();
    for &(r0, _, r1, _) in sheet.merged_ranges() {
        if r0 < r1 && r0 <= last && r1 >= first {
            values.insert((r0.max(first), r1.min(last)));
        }
    }
    values.into_iter().collect()
}

fn merge_intervals_columns(sheet: &Sheet, first: u16, last: u16) -> Vec<(u16, u16)> {
    if first > last {
        return Vec::new();
    }
    let mut values = BTreeSet::new();
    for &(_, c0, _, c1) in sheet.merged_ranges() {
        if c0 < c1 && c0 <= last && c1 >= first {
            values.insert((c0.max(first), c1.min(last)));
        }
    }
    values.into_iter().collect()
}

fn measured_axis_range<I>(
    slots: &[MeasuredAxisSlot<I>],
    first: I,
    last: I,
) -> Vec<MeasuredAxisSlot<I>>
where
    I: Copy + Ord,
{
    slots
        .iter()
        .copied()
        .filter(|slot| slot.index >= first && slot.index <= last)
        .collect()
}

fn merge_measured_axis<I>(sizes: &mut BTreeMap<I, Fixed>, slots: &[MeasuredAxisSlot<I>])
where
    I: Copy + Ord,
{
    for slot in slots {
        sizes
            .entry(slot.index)
            .and_modify(|size| *size = (*size).max(slot.size))
            .or_insert(slot.size);
    }
}

fn measured_axis_slots<I>(
    sizes: BTreeMap<I, Fixed>,
) -> Result<Vec<MeasuredAxisSlot<I>>, RenderError>
where
    I: Copy + Ord,
{
    let mut offset = Fixed::ZERO;
    let mut slots = Vec::with_capacity(sizes.len());
    for (index, size) in sizes {
        slots.push(MeasuredAxisSlot {
            index,
            offset,
            size,
        });
        offset = offset
            .checked_add(size)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(slots)
}

fn enforce_measurement_union_limits(
    ranges: &[RenderRange],
    limits: &RenderLimits,
) -> Result<(), RenderError> {
    let rows = interval_union_count(
        ranges
            .iter()
            .map(|range| (u64::from(range.first_row), u64::from(range.last_row) + 1)),
    )?;
    let columns = interval_union_count(
        ranges
            .iter()
            .map(|range| (u64::from(range.first_col), u64::from(range.last_col) + 1)),
    )?;
    let cells = rectangle_union_count(ranges)?;
    for (kind, limit, actual) in [
        (LimitKind::Rows, limits.max_rows, rows),
        (LimitKind::Columns, limits.max_columns, columns),
        (LimitKind::Cells, limits.max_cells, cells),
    ] {
        if actual > limit {
            return Err(RenderError::LimitExceeded {
                kind,
                limit,
                actual,
            });
        }
    }
    Ok(())
}

fn interval_union_count(
    intervals: impl IntoIterator<Item = (u64, u64)>,
) -> Result<u64, RenderError> {
    let mut intervals = intervals.into_iter().collect::<Vec<_>>();
    intervals.sort_unstable();
    let mut total = 0_u64;
    let mut current: Option<(u64, u64)> = None;
    for (start, end) in intervals {
        if start >= end {
            continue;
        }
        match current {
            Some((current_start, current_end)) if start <= current_end => {
                current = Some((current_start, current_end.max(end)));
            }
            Some((current_start, current_end)) => {
                total = total
                    .checked_add(current_end - current_start)
                    .ok_or(RenderError::CoordinateOverflow)?;
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    if let Some((start, end)) = current {
        total = total
            .checked_add(end - start)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(total)
}

fn rectangle_union_count(ranges: &[RenderRange]) -> Result<u64, RenderError> {
    let mut row_boundaries = ranges
        .iter()
        .flat_map(|range| [u64::from(range.first_row), u64::from(range.last_row) + 1])
        .collect::<Vec<_>>();
    row_boundaries.sort_unstable();
    row_boundaries.dedup();
    let mut total = 0_u64;
    for band in row_boundaries.windows(2) {
        let first = band[0];
        let last = band[1];
        let columns = interval_union_count(ranges.iter().filter_map(|range| {
            let range_first = u64::from(range.first_row);
            let range_last = u64::from(range.last_row) + 1;
            (range_first < last && range_last > first)
                .then_some((u64::from(range.first_col), u64::from(range.last_col) + 1))
        }))?;
        total = total
            .checked_add(
                (last - first)
                    .checked_mul(columns)
                    .ok_or(RenderError::CoordinateOverflow)?,
            )
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(total)
}

fn axis_total<I>(slots: &[MeasuredAxisSlot<I>]) -> Result<Fixed, RenderError> {
    axis_slice_total(slots)
}

fn axis_slice_total<I>(slots: &[MeasuredAxisSlot<I>]) -> Result<Fixed, RenderError> {
    slots.iter().try_fold(Fixed::ZERO, |sum, slot| {
        sum.checked_add(slot.size)
            .ok_or(RenderError::CoordinateOverflow)
    })
}

fn transform_node(
    node: SceneNode,
    x: Fixed,
    y: Fixed,
    scale_permille: u16,
) -> Result<SceneNode, RenderError> {
    transform_node_inner(node, x, y, scale_permille, 0)
}

fn transform_node_inner(
    node: SceneNode,
    x: Fixed,
    y: Fixed,
    scale_permille: u16,
    depth: u16,
) -> Result<SceneNode, RenderError> {
    if depth > 64 {
        return Err(RenderError::Backend {
            reason: "scene_nesting_too_deep",
        });
    }
    Ok(match node {
        SceneNode::ClipGroup(mut group) => {
            group.clip = transform_rect(group.clip, x, y, scale_permille)?;
            group.nodes = group
                .nodes
                .into_iter()
                .map(|node| {
                    transform_node_inner(node, x, y, scale_permille, depth.saturating_add(1))
                })
                .collect::<Result<Vec<_>, _>>()?;
            SceneNode::ClipGroup(group)
        }
        SceneNode::Rect(mut node) => {
            node.rect = transform_rect(node.rect, x, y, scale_permille)?;
            node.stroke_width = scale_fixed(node.stroke_width, scale_permille)?;
            SceneNode::Rect(node)
        }
        SceneNode::Line(mut node) => {
            node.x1 = transform_coordinate(node.x1, x, scale_permille)?;
            node.x2 = transform_coordinate(node.x2, x, scale_permille)?;
            node.y1 = transform_coordinate(node.y1, y, scale_permille)?;
            node.y2 = transform_coordinate(node.y2, y, scale_permille)?;
            node.width = scale_fixed(node.width, scale_permille)?;
            SceneNode::Line(node)
        }
        SceneNode::Path(mut node) => {
            for command in &mut node.commands {
                *command = transform_path_command(*command, x, y, scale_permille)?;
            }
            node.stroke_width = scale_fixed(node.stroke_width, scale_permille)?;
            SceneNode::Path(node)
        }
        SceneNode::Image(mut node) => {
            node.rect = transform_rect(node.rect, x, y, scale_permille)?;
            SceneNode::Image(node)
        }
        SceneNode::Text(mut node) => {
            node.bounds = transform_rect(node.bounds, x, y, scale_permille)?;
            node.clip_bounds = transform_rect(node.clip_bounds, x, y, scale_permille)?;
            node.horizontal_padding = scale_fixed(node.horizontal_padding, scale_permille)?;
            node.style.size = scale_fixed(node.style.size, scale_permille)?;
            SceneNode::Text(node)
        }
        SceneNode::GlyphRun(mut node) => {
            node.clip_bounds = transform_rect(node.clip_bounds, x, y, scale_permille)?;
            node.pivot_x = transform_coordinate(node.pivot_x, x, scale_permille)?;
            node.pivot_y = transform_coordinate(node.pivot_y, y, scale_permille)?;
            for command in &mut node.commands {
                *command = transform_path_command(*command, x, y, scale_permille)?;
            }
            for line in &mut node.decorations {
                line.x1 = transform_coordinate(line.x1, x, scale_permille)?;
                line.x2 = transform_coordinate(line.x2, x, scale_permille)?;
                line.y1 = transform_coordinate(line.y1, y, scale_permille)?;
                line.y2 = transform_coordinate(line.y2, y, scale_permille)?;
                line.width = scale_fixed(line.width, scale_permille)?;
            }
            SceneNode::GlyphRun(node)
        }
    })
}

fn transform_path_command(
    command: PathCommand,
    x: Fixed,
    y: Fixed,
    scale_permille: u16,
) -> Result<PathCommand, RenderError> {
    let point = |px, py| {
        Ok::<_, RenderError>((
            transform_coordinate(px, x, scale_permille)?,
            transform_coordinate(py, y, scale_permille)?,
        ))
    };
    Ok(match command {
        PathCommand::MoveTo { x: px, y: py } => {
            let (x, y) = point(px, py)?;
            PathCommand::MoveTo { x, y }
        }
        PathCommand::LineTo { x: px, y: py } => {
            let (x, y) = point(px, py)?;
            PathCommand::LineTo { x, y }
        }
        PathCommand::QuadraticTo {
            control_x,
            control_y,
            x: px,
            y: py,
        } => {
            let (control_x, control_y) = point(control_x, control_y)?;
            let (x, y) = point(px, py)?;
            PathCommand::QuadraticTo {
                control_x,
                control_y,
                x,
                y,
            }
        }
        PathCommand::CubicTo {
            control1_x,
            control1_y,
            control2_x,
            control2_y,
            x: px,
            y: py,
        } => {
            let (control1_x, control1_y) = point(control1_x, control1_y)?;
            let (control2_x, control2_y) = point(control2_x, control2_y)?;
            let (x, y) = point(px, py)?;
            PathCommand::CubicTo {
                control1_x,
                control1_y,
                control2_x,
                control2_y,
                x,
                y,
            }
        }
        PathCommand::Close => PathCommand::Close,
    })
}

fn transform_rect(
    rect: Rect,
    x: Fixed,
    y: Fixed,
    scale_permille: u16,
) -> Result<Rect, RenderError> {
    Ok(Rect {
        x: transform_coordinate(rect.x, x, scale_permille)?,
        y: transform_coordinate(rect.y, y, scale_permille)?,
        width: scale_fixed(rect.width, scale_permille)?,
        height: scale_fixed(rect.height, scale_permille)?,
    })
}

fn transform_coordinate(
    value: Fixed,
    offset: Fixed,
    scale_permille: u16,
) -> Result<Fixed, RenderError> {
    offset
        .checked_add(scale_fixed(value, scale_permille)?)
        .ok_or(RenderError::CoordinateOverflow)
}

fn scale_fixed(value: Fixed, scale_permille: u16) -> Result<Fixed, RenderError> {
    let product = i128::from(value.raw())
        .checked_mul(i128::from(scale_permille))
        .ok_or(RenderError::CoordinateOverflow)?;
    let adjusted = if product >= 0 {
        product + 500
    } else {
        product - 500
    };
    let raw = adjusted / 1_000;
    Ok(Fixed::from_raw(
        i64::try_from(raw).map_err(|_| RenderError::CoordinateOverflow)?,
    ))
}

fn positive_half_difference(container: Fixed, content: Fixed) -> Fixed {
    Fixed::from_raw(container.raw().saturating_sub(content.raw()).max(0) / 2)
}

fn inches_to_fixed(inches: f64) -> Result<Fixed, RenderError> {
    let raw = (inches * 96.0 * FIXED_UNITS_PER_PIXEL as f64).round();
    if !raw.is_finite() || raw < 0.0 || raw > i64::MAX as f64 {
        return Err(RenderError::CoordinateOverflow);
    }
    Ok(Fixed::from_raw(raw as i64))
}

fn column_label(column: u16) -> String {
    let mut value = u32::from(column) + 1;
    let mut reversed = Vec::new();
    while value != 0 {
        value -= 1;
        reversed.push((b'A' + (value % 26) as u8) as char);
        value /= 26;
    }
    reversed.into_iter().rev().collect()
}

fn print_page_order_code(order: PrintPageOrder) -> &'static str {
    match order {
        PrintPageOrder::DownThenOver => "down_then_over",
        PrintPageOrder::OverThenDown => "over_then_down",
        _ => "unknown",
    }
}

fn enforce_print_limit(kind: LimitKind, limit: u64, actual: u64) -> Result<(), RenderError> {
    if actual > limit {
        Err(RenderError::LimitExceeded {
            kind,
            limit,
            actual,
        })
    } else {
        Ok(())
    }
}

fn push_page_map_json(out: &mut String, page: &PageMapEntry) {
    out.push_str("{\"output_index\":");
    out.push_str(&page.output_index.to_string());
    out.push_str(",\"displayed_page_number\":");
    out.push_str(&page.displayed_page_number.to_string());
    out.push_str(",\"area_index\":");
    out.push_str(&page.area_index.to_string());
    out.push_str(",\"horizontal_index\":");
    out.push_str(&page.horizontal_index.to_string());
    out.push_str(",\"vertical_index\":");
    out.push_str(&page.vertical_index.to_string());
    out.push_str(",\"manual_col_break_before\":");
    out.push_str(if page.manual_col_break_before {
        "true"
    } else {
        "false"
    });
    out.push_str(",\"manual_row_break_before\":");
    out.push_str(if page.manual_row_break_before {
        "true"
    } else {
        "false"
    });
    out.push_str(",\"body_range\":{\"first_row\":");
    out.push_str(&page.body_range.first_row.to_string());
    out.push_str(",\"first_col\":");
    out.push_str(&page.body_range.first_col.to_string());
    out.push_str(",\"last_row\":");
    out.push_str(&page.body_range.last_row.to_string());
    out.push_str(",\"last_col\":");
    out.push_str(&page.body_range.last_col.to_string());
    out.push_str("},\"repeat_rows\":");
    push_optional_u32_pair(out, page.repeat_rows);
    out.push_str(",\"repeat_cols\":");
    push_optional_u16_pair(out, page.repeat_cols);
    out.push_str(",\"scale_permille\":");
    out.push_str(&page.scale_permille.to_string());
    out.push('}');
}

fn push_optional_u32_pair(out: &mut String, value: Option<(u32, u32)>) {
    match value {
        Some((first, last)) => {
            out.push('[');
            out.push_str(&first.to_string());
            out.push(',');
            out.push_str(&last.to_string());
            out.push(']');
        }
        None => out.push_str("null"),
    }
}

fn push_optional_u16_pair(out: &mut String, value: Option<(u16, u16)>) {
    match value {
        Some((first, last)) => {
            out.push('[');
            out.push_str(&first.to_string());
            out.push(',');
            out.push_str(&last.to_string());
            out.push(']');
        }
        None => out.push_str("null"),
    }
}

fn push_rect_json(out: &mut String, rect: Rect) {
    out.push_str("{\"x_raw\":");
    out.push_str(&rect.x.raw().to_string());
    out.push_str(",\"y_raw\":");
    out.push_str(&rect.y.raw().to_string());
    out.push_str(",\"width_raw\":");
    out.push_str(&rect.width.raw().to_string());
    out.push_str(",\"height_raw\":");
    out.push_str(&rect.height.raw().to_string());
    out.push('}');
}

fn push_json_escaped(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch < '\u{20}' => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use rxls::{CellStyle, Chart, ChartKind, Color, PageSetup, Workbook};
    use zip::write::SimpleFileOptions;

    use super::*;

    #[test]
    fn prepared_identity_merges_256_column_area_row_scans_once() {
        let ranges = (0..256_u16)
            .map(|column| RenderRange::new(0, column, 249_999, column))
            .collect::<Vec<_>>();
        assert_eq!(merged_identity_row_bands(&ranges), [(0, 249_999)]);

        let disjoint = [
            RenderRange::new(0, 0, 9, 0),
            RenderRange::new(5, 10, 20, 10),
            RenderRange::new(30, 20, 39, 20),
            RenderRange::new(40, 30, 49, 30),
        ];
        assert_eq!(merged_identity_row_bands(&disjoint), [(0, 20), (30, 49)]);
    }

    fn zip_parts(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for &(path, body) in parts {
            writer
                .start_file(path, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn default_hidden_print_workbook(default_hidden: bool) -> Workbook {
        let worksheet = format!(
            r#"<worksheet><sheetFormatPr defaultRowHeight="15" zeroHeight="{}"/><sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="3"><c r="A3"><v>3</v></c></row><row r="5" hidden="1"><c r="A5"><v>5</v></c></row></sheetData></worksheet>"#,
            u8::from(default_hidden)
        );
        let mut workbook = Workbook::open(&zip_parts(&[
            (
                "xl/workbook.xml",
                br#"<workbook><sheets><sheet name="Rows" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
        ]))
        .unwrap();
        workbook.sheets[0].set_page_setup(
            PageSetup::new()
                .with_print_area((0, 0, 4, 0))
                .with_scale(100),
        );
        workbook
    }

    fn solid_rgba_png() -> Vec<u8> {
        let mut output = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut output, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[17, 89, 143, 255]).unwrap();
        }
        output
    }

    fn absolute_image_workbook(right_to_left: bool) -> Workbook {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="Absolute"><table:shapes><draw:frame draw:name="absolute" text:anchor-type="page" svg:x="0in" svg:y="0.2604166666667in" svg:width="0.1041666666667in" svg:height="0.1041666666667in"><draw:image xlink:href="Pictures/page.png"/></draw:frame></table:shapes><table:table-row><table:table-cell/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let image = solid_rgba_png();
        let mut workbook = Workbook::open(&zip_parts(&[
            (
                "mimetype",
                b"application/vnd.oasis.opendocument.spreadsheet",
            ),
            ("content.xml", content.as_bytes()),
            ("Pictures/page.png", &image),
        ]))
        .unwrap();
        workbook.sheets[0].set_right_to_left(right_to_left);
        workbook
    }

    fn absolute_print_options(omit_sparse_pages: bool) -> PrintOptions {
        PrintOptions {
            omit_sparse_pages,
            render: RenderOptions {
                font_pack: Some(crate::font::synthetic_test_pack()),
                default_font_family: "Wide Sans".to_string(),
                ..RenderOptions::default()
            },
            ..PrintOptions::default()
        }
    }

    fn image_rects(nodes: &[SceneNode], output: &mut Vec<Rect>) {
        for node in nodes {
            match node {
                SceneNode::ClipGroup(group) => image_rects(&group.nodes, output),
                SceneNode::Image(image) => output.push(image.rect),
                _ => {}
            }
        }
    }

    fn page_image_rects(page: &PrintPage) -> Vec<Rect> {
        let mut output = Vec::new();
        image_rects(&page.scene.nodes, &mut output);
        output
    }

    fn assert_full_and_streamed_absolute_images(
        workbook: &Workbook,
        options: &PrintOptions,
        expected_body_rows: &[(u32, u32)],
        expected_image_counts: &[usize],
    ) {
        let prepared = prepare_print_document(workbook, 0, options).unwrap();
        assert_eq!(prepared.report.pages.len(), expected_body_rows.len());
        let streamed = (0..prepared.report.pages.len())
            .map(|index| build_print_page(workbook, &prepared, index).unwrap())
            .collect::<Vec<_>>();
        let full = build_print_document(workbook, 0, options).unwrap();
        assert_eq!(full.report.pages, prepared.report.pages);
        assert_eq!(
            full.pages
                .iter()
                .map(|page| { (page.map.body_range.first_row, page.map.body_range.last_row,) })
                .collect::<Vec<_>>(),
            expected_body_rows
        );
        for ((full_page, streamed_page), &expected_count) in
            full.pages.iter().zip(&streamed).zip(expected_image_counts)
        {
            let full_rects = page_image_rects(full_page);
            assert_eq!(full_rects.len(), expected_count, "{:?}", full_page.map);
            assert_eq!(page_image_rects(streamed_page), full_rects);
        }
    }

    #[test]
    fn column_labels_cover_excel_boundaries() {
        assert_eq!(column_label(0), "A");
        assert_eq!(column_label(25), "Z");
        assert_eq!(column_label(26), "AA");
        assert_eq!(column_label(16_383), "XFD");
    }

    #[test]
    fn running_text_sections_and_fields_are_stable() {
        let mut warnings = PrintWarnings::default();
        assert_eq!(
            expand_running_text("&L&& &A&C&P/&N&R&D", "Data", 3, 9, &mut warnings),
            ["& Data", "3/9", ""]
        );
        assert_eq!(
            warnings.finish(),
            vec![PrintWarning {
                code: PrintWarningCode::VolatileHeaderFooterFieldOmitted,
                occurrences: 1,
            }]
        );
    }

    #[test]
    fn clip_groups_count_both_backend_boundary_operations() {
        let clip = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(10),
            height: Fixed::from_pixels(10),
        };
        let nodes = vec![SceneNode::ClipGroup(crate::scene::ClipGroupNode {
            clip,
            nodes: vec![
                SceneNode::ClipGroup(crate::scene::ClipGroupNode {
                    clip,
                    nodes: Vec::new(),
                }),
                SceneNode::Rect(RectNode {
                    rect: clip,
                    fill: Some(Rgb::WHITE),
                    stroke: None,
                    stroke_width: Fixed::ZERO,
                }),
            ],
        })];

        assert_eq!(backend_command_count(&nodes).unwrap(), 5);
    }

    #[test]
    fn manual_breaks_are_exact_and_move_only_to_merge_boundaries() {
        let slots = (0_u32..9)
            .map(|index| MeasuredAxisSlot {
                index,
                offset: Fixed::from_pixels(i64::from(index) * 10),
                size: Fixed::from_pixels(10),
            })
            .collect::<Vec<_>>();
        let (segments, expansions, shifted) =
            partition_axis_with_breaks(&slots, Fixed::from_pixels(25), &[(2, 4)], &[3, 7]).unwrap();

        assert_eq!(shifted, 1);
        assert_eq!(expansions, 1);
        assert_eq!(
            segments
                .iter()
                .map(|segment| (segment.first, segment.last, segment.manual_break_before))
                .collect::<Vec<_>>(),
            [(0, 1, false), (2, 4, true), (5, 6, false), (7, 8, true)]
        );
    }

    #[test]
    fn first_even_and_odd_running_text_selection_uses_displayed_page_numbers() {
        let policy = HeaderFooterPolicy {
            odd_header: Some("odd".to_string()),
            odd_footer: Some("odd-f".to_string()),
            even_header: Some("even".to_string()),
            even_footer: Some("even-f".to_string()),
            first_header: Some("first".to_string()),
            first_footer: Some("first-f".to_string()),
            different_odd_even: true,
            different_first: true,
            scale_with_document: true,
            align_with_margins: true,
        };

        assert_eq!(policy.select(0, 3).header, Some("first"));
        assert_eq!(policy.select(1, 4).header, Some("even"));
        assert_eq!(policy.select(2, 5).header, Some("odd"));
    }

    fn multipage_document() -> PrintDocument {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("서울 보고서");
        for row in 0..10 {
            sheet.set_row_height(row, 100.0);
            for column in 0..6 {
                sheet.set_col_width(column, 40.0);
                sheet.write(row, column, format!("한글 {row}:{column}"));
            }
        }
        sheet.set_print_gridlines();
        sheet.set_print_headings();
        sheet.merge(2, 1, 4, 1);
        sheet.set_page_setup(
            PageSetup::new()
                .with_print_area((0, 0, 9, 5))
                .with_repeat_rows(0, 0)
                .with_repeat_cols(0, 0)
                .with_header("&L&A&C&P / &N")
                .with_footer("&R검증"),
        );
        let options = PrintOptions {
            omit_sparse_pages: false,
            render: RenderOptions {
                font_pack: Some(crate::font::synthetic_test_pack()),
                default_font_family: "Wide Sans".to_string(),
                ..RenderOptions::default()
            },
            ..PrintOptions::default()
        };
        build_print_document(&workbook, 0, &options).unwrap()
    }

    #[test]
    fn ordered_pages_pdf_and_png_share_one_scene_deterministically() {
        let document = multipage_document();
        assert!(document.pages.len() >= 4);
        assert_eq!(document.pages[0].map.horizontal_index, 0);
        assert_eq!(document.pages[0].map.vertical_index, 0);
        assert_eq!(document.pages[1].map.horizontal_index, 0);
        assert_eq!(document.pages[1].map.vertical_index, 1);
        let pdf = crate::render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, crate::render_print_document_pdf(&document).unwrap());
        assert!(pdf.starts_with(b"%PDF-1.7"));
        assert!(pdf.windows(7).any(|bytes| bytes == b"/Actual"));
        assert!(String::from_utf8_lossy(&pdf).contains("/MediaBox [0 0 612 792]"));
        let svg = crate::render_scene_svg(&document.pages[0].scene, 64 << 20).unwrap();
        assert!(svg.contains("width=\"816\" height=\"1056\""));
        let png = crate::render_print_page_png(&document.pages[0], 96, &document).unwrap();
        assert_eq!(
            png,
            crate::render_print_page_png(&document.pages[0], 96, &document).unwrap()
        );
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 816);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 1_056);
    }

    #[test]
    fn single_page_sheets_honors_source_print_gridlines_and_caller_disable() {
        fn has_gridline(nodes: &[SceneNode]) -> bool {
            nodes.iter().any(|node| match node {
                SceneNode::Line(line) => line.color == Rgb::GRIDLINE,
                SceneNode::ClipGroup(group) => has_gridline(&group.nodes),
                _ => false,
            })
        }

        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("gridlines");
        sheet.write(0, 0, "A");
        sheet.write(1, 1, "B");
        sheet.set_print_gridlines();
        let options = PrintOptions {
            single_page_sheets: true,
            render: RenderOptions {
                font_pack: Some(crate::font::synthetic_test_pack()),
                default_font_family: "Wide Sans".to_string(),
                ..RenderOptions::default()
            },
            ..PrintOptions::default()
        };
        let document = build_print_document(&workbook, 0, &options).unwrap();
        assert!(has_gridline(&document.pages[0].scene.nodes));

        let mut disabled = options;
        disabled.render.gridlines = false;
        let document = build_print_document(&workbook, 0, &disabled).unwrap();
        assert!(!has_gridline(&document.pages[0].scene.nodes));
    }

    #[test]
    fn default_hidden_rows_drive_pagination_and_prepared_print_identity() {
        fn prepared_rows(prepared: &PreparedPrintDocument) -> Vec<(u32, Fixed)> {
            let PreparedPrintState::Paginated { areas, .. } = &prepared.state else {
                panic!("expected paginated state");
            };
            areas[0]
                .measured_rows
                .iter()
                .map(|row| (row.index, row.size))
                .collect()
        }

        let hidden = default_hidden_print_workbook(true);
        let visible = default_hidden_print_workbook(false);
        let options = PrintOptions {
            omit_sparse_pages: false,
            render: RenderOptions {
                gridlines: false,
                font_pack: Some(crate::font::synthetic_test_pack()),
                default_font_family: "Wide Sans".to_string(),
                ..RenderOptions::default()
            },
            ..PrintOptions::default()
        };

        let prepared = prepare_print_document(&hidden, 0, &options).unwrap();
        assert_eq!(
            prepared_rows(&prepared),
            [(0, Fixed::from_pixels(20)), (2, Fixed::from_pixels(20))]
        );
        build_sheet_print_page(&hidden.sheets[0], &prepared, 0).unwrap();

        let visible_prepared = prepare_print_document(&visible, 0, &options).unwrap();
        assert_eq!(
            prepared_rows(&visible_prepared),
            [
                (0, Fixed::from_pixels(20)),
                (1, Fixed::from_pixels(20)),
                (2, Fixed::from_pixels(20)),
                (3, Fixed::from_pixels(20))
            ]
        );
        assert!(matches!(
            build_sheet_print_page(&visible.sheets[0], &prepared, 0),
            Err(RenderError::Backend {
                reason: "prepared_print_source_changed"
            })
        ));

        let include_hidden = PrintOptions {
            render: RenderOptions {
                include_hidden: true,
                ..options.render.clone()
            },
            ..options
        };
        let include_hidden_prepared = prepare_print_document(&hidden, 0, &include_hidden).unwrap();
        assert_eq!(
            prepared_rows(&include_hidden_prepared),
            (0..=4)
                .map(|row| (row, Fixed::from_pixels(20)))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn paginated_body_replays_prepared_nonlocal_auto_row_geometry() {
        fn filled_rect(nodes: &[SceneNode], color: Rgb) -> Option<Rect> {
            nodes.iter().find_map(|node| match node {
                SceneNode::ClipGroup(group) => filled_rect(&group.nodes, color),
                SceneNode::Rect(node) if node.fill == Some(color) => Some(node.rect),
                _ => None,
            })
        }

        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("prepared-print-geometry");
        sheet.set_col_width(0, 100.0);
        sheet.set_col_width(1, 2.0);
        sheet.write_styled(
            0,
            0,
            "plain",
            &CellStyle::new().fill(Color::rgb(17, 34, 51)),
        );
        sheet.write_styled(0, 1, "한글中文한글中文", &CellStyle::new().wrap());
        sheet.set_page_setup(
            PageSetup::new()
                .with_print_area((0, 0, 0, 1))
                .with_scale(100),
        );
        let options = PrintOptions {
            omit_sparse_pages: false,
            render: RenderOptions {
                font_pack: Some(crate::font::synthetic_test_pack()),
                default_font_family: "Wide Sans".to_string(),
                ..RenderOptions::default()
            },
            ..PrintOptions::default()
        };

        let document = build_print_document(&workbook, 0, &options).unwrap();
        assert!(document.pages.len() >= 2);
        let first_body = document.pages[0]
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::ClipGroup(group)
                    if filled_rect(&group.nodes, Rgb::new(17, 34, 51)).is_some() =>
                {
                    Some((
                        group.clip,
                        filled_rect(&group.nodes, Rgb::new(17, 34, 51)).unwrap(),
                    ))
                }
                _ => None,
            })
            .expect("first paginated body clip and filled cell");
        assert_eq!(first_body.1.height, first_body.0.height);
    }

    #[test]
    fn absolute_drawing_uses_prepared_auto_height_origins_in_full_and_streamed_pages() {
        for right_to_left in [false, true] {
            for omit_sparse_pages in [false, true] {
                let mut workbook = absolute_image_workbook(right_to_left);
                let sheet = &mut workbook.sheets[0];
                sheet.write(
                    0,
                    0,
                    (0..30)
                        .map(|index| format!("line-{index}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                sheet.write(1, 0, "row-two");
                sheet.set_page_setup(
                    PageSetup::new()
                        .with_print_area((0, 0, 1, 0))
                        .with_paper_size(1)
                        .with_scale(400),
                );
                let options = absolute_print_options(omit_sparse_pages);
                assert_full_and_streamed_absolute_images(
                    &workbook,
                    &options,
                    &[(0, 0), (1, 1)],
                    &[1, 0],
                );

                let document = build_print_document(&workbook, 0, &options).unwrap();
                let rect = page_image_rects(&document.pages[0])[0];
                assert_eq!(
                    rect.y.raw() - document.report.content_rect.y.raw(),
                    Fixed::from_pixels(100).raw()
                );
            }
        }
    }

    #[test]
    fn absolute_drawing_respects_explicit_and_hidden_preceding_row_geometry() {
        for right_to_left in [false, true] {
            for omit_sparse_pages in [false, true] {
                let options = absolute_print_options(omit_sparse_pages);

                let mut explicit = absolute_image_workbook(right_to_left);
                let sheet = &mut explicit.sheets[0];
                sheet.write(0, 0, "explicit-height");
                sheet.write(1, 0, "row-two");
                sheet.set_row_height(0, 300.0);
                sheet.set_page_setup(
                    PageSetup::new()
                        .with_print_area((0, 0, 1, 0))
                        .with_paper_size(1)
                        .with_scale(400),
                );
                assert_full_and_streamed_absolute_images(
                    &explicit,
                    &options,
                    &[(0, 0), (1, 1)],
                    &[1, 0],
                );

                let mut hidden = absolute_image_workbook(right_to_left);
                let sheet = &mut hidden.sheets[0];
                sheet.write(0, 0, "hidden-row");
                sheet.write(1, 0, "visible-row");
                sheet.hide_row(0);
                sheet.set_row_height(1, 75.0);
                sheet.set_page_setup(
                    PageSetup::new()
                        .with_print_area((0, 0, 1, 0))
                        .with_paper_size(1)
                        .with_scale(400),
                );
                assert_full_and_streamed_absolute_images(&hidden, &options, &[(1, 1)], &[1]);
            }
        }
    }

    #[test]
    fn partial_anchor_identity_covers_off_area_content_derived_row_height() {
        fn prepared_row_zero_height(prepared: &PreparedPrintDocument) -> Fixed {
            let PreparedPrintState::Paginated { areas, .. } = &prepared.state else {
                panic!("expected paginated state");
            };
            areas[0]
                .measured_rows
                .iter()
                .find(|row| row.index == 0)
                .map(|row| row.size)
                .expect("off-area anchor row geometry")
        }

        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Identity");
        sheet.set_col_width(5, 2.0);
        sheet.write_styled(0, 5, "short", &CellStyle::new().wrap());
        sheet.write(1, 1, "printed");
        sheet.add_chart(Chart::new(ChartKind::Line, (0, 0), (2, 2)));
        sheet.set_page_setup(
            PageSetup::new()
                .with_print_area((1, 1, 1, 1))
                .with_paper_size(1)
                .with_scale(100),
        );
        let options = PrintOptions {
            render: RenderOptions {
                font_pack: Some(crate::font::synthetic_test_pack()),
                default_font_family: "Wide Sans".to_string(),
                ..RenderOptions::default()
            },
            ..PrintOptions::default()
        };

        let prepared = prepare_print_document(&workbook, 0, &options).unwrap();
        let original_height = prepared_row_zero_height(&prepared);
        build_print_page(&workbook, &prepared, 0).unwrap();

        workbook.sheets[0].write_styled(
            0,
            5,
            (0..20)
                .map(|index| format!("mandatory-line-{index}"))
                .collect::<Vec<_>>()
                .join("\n"),
            &CellStyle::new().wrap(),
        );
        let changed = prepare_print_document(&workbook, 0, &options).unwrap();
        assert!(
            prepared_row_zero_height(&changed) > original_height,
            "off-area content must expand the row used by the partial anchor"
        );
        assert!(matches!(
            build_print_page(&workbook, &prepared, 0),
            Err(RenderError::Backend {
                reason: "prepared_print_source_changed"
            })
        ));
    }

    #[test]
    fn backend_limits_fail_before_output_is_returned() {
        let mut document = multipage_document();
        let expected_pdf = crate::render_print_document_pdf(&document).unwrap();
        document.limits.max_pdf_bytes = expected_pdf.len() as u64;
        assert_eq!(
            crate::render_print_document_pdf(&document).unwrap(),
            expected_pdf
        );
        document.limits.max_pdf_bytes = expected_pdf.len() as u64 - 1;
        assert!(matches!(
            crate::render_print_document_pdf(&document),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::PdfBytes,
                ..
            })
        ));
        document.limits.max_pdf_bytes = 64;
        assert!(matches!(
            crate::render_print_document_pdf(&document),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::PdfBytes,
                limit: 64,
                ..
            })
        ));
        document.limits.max_pdf_bytes = 256 << 20;
        let expected_png = crate::render_print_page_png(&document.pages[0], 96, &document).unwrap();
        document.limits.max_png_bytes_per_page = expected_png.len() as u64;
        assert_eq!(
            crate::render_print_page_png(&document.pages[0], 96, &document).unwrap(),
            expected_png
        );
        document.limits.max_png_bytes_per_page = expected_png.len() as u64 - 1;
        assert!(matches!(
            crate::render_print_page_png(&document.pages[0], 96, &document),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::PngBytes,
                ..
            })
        ));
        document.limits.max_png_bytes_per_page = 64 << 20;
        document.limits.max_raster_pixels = 1;
        assert!(matches!(
            crate::render_print_page_png(&document.pages[0], 96, &document),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::RasterPixels,
                limit: 1,
                ..
            })
        ));
    }
}
