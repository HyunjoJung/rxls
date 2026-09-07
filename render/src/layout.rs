//! Bounded worksheet-to-scene layout.
//!
//! Scene orchestration and shared layout state live here. Private submodules
//! own axis geometry, conditional rules, charts, cell typography, and borders.

mod charts;
mod conditional;
mod edges;
mod geometry;
mod text;

use charts::{a1_range_points, resolve_numeric_a1_range, try_push_chart_with_layout};
use conditional::{
    calc_line_layout_available, conditional_reference_coordinate,
    conditional_style_affects_text_layout, conditional_style_is_geometry_safe_color_only,
    has_conditional_text_layout_overlay, numeric_bounds, parse_conditional_expression,
    parse_conditional_operand, push_data_bar, resolve_conditional_layout_cells,
    resolve_conditional_paints, ConditionalOperand,
};
use edges::{
    compose_edges, push_composed_edges, push_print_gridline_leading_frame,
    remap_calc_metafile_grid_edges, EdgeClaimKind, EdgeCompositionMode,
};
use geometry::{
    apply_axis_geometry, apply_calc_single_page_axis_fit,
    apply_calc_single_page_metafile_grid_axis_fit, apply_source_native_axis_endpoints,
    calc_inclusive_rectangle_extent, calc_twips_position_to_fixed, column_width,
    empty_used_column_width, fallback_row_height, imported_column_axis_measure,
    imported_row_axis_measure, maximum_digit_width, points_to_fixed, reflect_rect_horizontally,
    reflected_x, row_height, source_axis_contribution_twips, source_native_column_prefix,
    source_native_row_prefix, verified_calc_cell_font_size_pt, verified_ooxml_cell_font_size_pt,
    verified_ooxml_normal_font_size, visual_column_slots, SourceAxisCursor,
};
use text::{
    build_glyph_run, calc_cell_text_layout_bounds, calc_edit_engine_uses_only_complex_role,
    calc_script_class_summary_bounded, charge_automatic_text_bytes, has_mixed_calc_script_classes,
    inner_width, map_font_error, measure_automatic_cell_height, multiply_fixed,
    outlined_horizontal_padding, scale_ratio, shape_text_with_kerning, shaped_width, text_style,
    CalcCellScriptAnalysis, CalcScriptClassSummary,
};

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use rxls::{
    BorderStyle, Cell, CellStyle, CfRule, ChartKind, Color, DisplayCell, DrawingAnchorBehavior,
    DrawingMetadata, DrawingObjectKind, Font, FormatPattern, FormatScript, ImportedAxisMeasure,
    OoxmlImplicitRowHeight, Sheet, Sparkline, SparklineKind, StyleFidelity, Workbook,
};

use crate::error::{LimitKind, RenderError};
use crate::font::{BaseDirection, FontPack, FontPackError, FontRequest};
use crate::interaction::CellInteractionRegion;
use crate::media::decode_image;
use crate::scene::{
    ClipGroupNode, Fixed, ImageNode, LineNode, Rect, RectNode, Rgb, Scene, SceneNode, TextAnchor,
    TextNode, TextStyle, FIXED_UNITS_PER_PIXEL,
};
use crate::typography::CellLineLayoutPolicy;

/// Largest supported zero-based worksheet row (Excel row 1,048,576).
pub const MAX_WORKSHEET_ROW: u32 = 1_048_575;
/// Largest supported zero-based worksheet column (Excel column XFD).
pub const MAX_WORKSHEET_COLUMN: u16 = 16_383;
/// Deterministic default character count used with verified font metrics.
const DEFAULT_COLUMN_CHARACTERS: f32 = 10.0;
/// LibreOffice Calc's import geometry adds two device pixels to explicit
/// Excel character widths. Missing-width fallback keeps the ECMA five-pixel
/// allowance because it is derived from the verified default font instead.
const IMPORTED_COLUMN_PADDING_PIXELS: u16 = 2;
const DEFAULT_COLUMN_PADDING_PIXELS: u16 = 5;
/// Calc's 8.5-character application default when an OOXML worksheet omits
/// sheet-format width metadata, encoded in 1/256 digit-width units.
const OOXML_APPLICATION_DEFAULT_COLUMN_WIDTH_256: u32 =
    XLSB_DIGIT_WIDTH_SCALE * 8 + XLSB_DIGIT_WIDTH_SCALE / 2;
/// Calc's fixed application default for BIFF worksheets that omit both
/// `STANDARDWIDTH` and `DEFCOLWIDTH`: 64 points at 96 CSS pixels per inch.
const BIFF_APPLICATION_DEFAULT_COLUMN_WIDTH: Fixed = Fixed::from_raw(87_381);
/// Calc's fixed application default for BIFF worksheets that omit
/// `DEFAULTROWHEIGHT`: 255 twips, or 12.75 points / 17 CSS pixels.
const BIFF_APPLICATION_DEFAULT_ROW_HEIGHT: Fixed = Fixed::from_pixels(17);
/// The pinned Calc oracle resolves an OOXML worksheet without an authoritative
/// default row height to 0.5 cm (14.173228 points / 18.897638 CSS pixels).
/// Fixed-point layout rounds that imported-only value to the nearest 1/1024 px.
/// This remains the conservative fallback when the workbook's Normal font
/// cannot be resolved exactly from a verified pack.
const OOXML_APPLICATION_DEFAULT_ROW_HEIGHT: Fixed = Fixed::from_raw(19_351);
/// Calc keeps cell-anchored drawing endpoints on the imported OOXML standard
/// row track (12.8 pt) even when the worksheet's text layout later chooses a
/// taller automatic row.  Drawing anchors therefore need a separate, stable
/// track model instead of borrowing the cell paint height.
pub(crate) const CALC_OOXML_DRAWING_DEFAULT_ROW_HEIGHT: Fixed = Fixed::from_raw(17_476);
/// LibreOffice 26.2.3.2
/// `sc/source/core/data/column2.cxx::lcl_GetAttribHeight` derives an automatic
/// row from 118% of the pattern font's integer-twip height, then adds the two
/// default 20-twip margins and subtracts its 23-twip standard-row adjustment.
const CALC_NORMAL_ROW_HEIGHT_PERCENT: i128 = 118;
const CALC_NORMAL_ROW_HEIGHT_PERCENT_DENOMINATOR: i128 = 100;
const CALC_NORMAL_ROW_HEIGHT_ADJUSTMENT_TWIPS: i128 = 17;
const TWIPS_PER_POINT: i128 = 20;
/// OOXML `baseColWidth` excludes the four margin pixels and one gridline pixel
/// included when deriving a default column width.
const OOXML_BASE_COLUMN_EXTRA_PADDING_PIXELS: u16 = 5;
/// XLSB `coldx` and `dxGCol` store 256 units per standard-font digit.
const XLSB_DIGIT_WIDTH_SCALE: u32 = 256;
/// Calc converts XLSB digit widths through integer twips at 96 CSS pixels/inch.
const TWIPS_PER_CSS_PIXEL: i128 = 15;
/// Calc's non-printer optimal-row path samples 1,000 twips through a 96-DPI
/// virtual device. `LogicToPixel(1000 twips)` rounds to 67 pixels, so its
/// stored pixels-per-twip value is 67/1000 rather than the exact 1/15.
const CALC_OPTIMAL_HEIGHT_SAMPLE_TWIPS: u64 = 1_000;
const CALC_OPTIMAL_HEIGHT_SAMPLE_PIXELS: u64 = 67;
const CALC_DEVICE_DPI: u64 = 96;
const MM100_PER_INCH: u64 = 2_540;
/// Calc's English `CTL_SPREADSHEET` default starts with Tahoma. The verified
/// render font configuration declares the deterministic substitute; packs
/// without either an exact Tahoma face or that retained alias fail closed.
const CALC_CTL_LOGICAL_FAMILY: &str = "Tahoma";
/// `sc/source/filter/oox/stylesbuffer.cxx::Font::finalizeImport` assigns the
/// requested family to Calc's complex-script role when its resolved face has
/// any one of these exact sentinel glyphs. Otherwise that role retains the
/// document-pool default above.
const CALC_CTL_FONT_PROBES: [&str; 8] = [
    "\u{05d1}", "\u{0631}", "\u{0721}", "\u{0911}", "\u{0e01}", "\u{fb21}", "\u{fb51}", "\u{fe71}",
];
/// `cchDefColWidth` excludes the four margins and one gridline screen pixel.
const XLSB_BASE_COLUMN_SCREEN_PIXELS: u16 = 5;
/// Calc's default 20-twip top and bottom margins each truncate to one device
/// pixel through the 67/1000 optimal-height scale.
const AUTO_ROW_VERTICAL_PADDING_PIXELS: i64 = 2;
/// Calc's default left and right EditEngine cell margins are each 20 twips.
const CALC_CELL_HORIZONTAL_MARGIN_TWIPS: u64 = 20;
/// Calc's `sc/source/ui/view/output2.cxx` offsets top-aligned text by
/// `ATTR_MARGIN`'s top value and bottom-aligned text by its bottom value.
const CALC_CELL_VERTICAL_MARGIN_TWIPS: i128 = 20;
/// Calc's BIFF importer assigns a 40-twip `ATTR_MARGIN` to the imported default
/// cell XF. Other imported formats retain Calc's ordinary 20-twip default.
const CALC_BIFF_CELL_VERTICAL_MARGIN_TWIPS: i128 = 40;
const CALC_CELL_VERTICAL_MARGIN: Fixed = Fixed::from_raw(
    ((CALC_CELL_VERTICAL_MARGIN_TWIPS * FIXED_UNITS_PER_PIXEL as i128 + TWIPS_PER_CSS_PIXEL / 2)
        / TWIPS_PER_CSS_PIXEL) as i64,
);
const CALC_BIFF_CELL_VERTICAL_MARGIN: Fixed = Fixed::from_raw(
    ((CALC_BIFF_CELL_VERTICAL_MARGIN_TWIPS * FIXED_UNITS_PER_PIXEL as i128
        + TWIPS_PER_CSS_PIXEL / 2)
        / TWIPS_PER_CSS_PIXEL) as i64,
);
/// Calc removes one additional device pixel for the cell grid line before
/// converting the wrapping paper size to Map100thMM.
const CALC_CELL_GRID_PIXELS: u64 = 1;
const CALC_WORKSHEET_KERNING: bool = false;

/// Inclusive zero-based worksheet rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RenderRange {
    /// First row.
    pub first_row: u32,
    /// First column.
    pub first_col: u16,
    /// Last row.
    pub last_row: u32,
    /// Last column.
    pub last_col: u16,
}

impl RenderRange {
    /// Construct an inclusive range.
    pub const fn new(first_row: u32, first_col: u16, last_row: u32, last_col: u16) -> Self {
        Self {
            first_row,
            first_col,
            last_row,
            last_col,
        }
    }

    fn validate(self) -> Result<Self, RenderError> {
        if self.first_row > self.last_row || self.first_col > self.last_col {
            return Err(RenderError::InvalidRange {
                first_row: self.first_row,
                first_col: self.first_col,
                last_row: self.last_row,
                last_col: self.last_col,
            });
        }
        if self.last_row > MAX_WORKSHEET_ROW || self.last_col > MAX_WORKSHEET_COLUMN {
            return Err(RenderError::RangeOutsideGrid {
                last_row: self.last_row,
                last_col: self.last_col,
                max_row: MAX_WORKSHEET_ROW,
                max_col: MAX_WORKSHEET_COLUMN,
            });
        }
        Ok(self)
    }
}

/// Worksheet extent selected independently from future print pagination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RenderSelection {
    /// Render values, visibly painted format-only blanks, content-bearing
    /// merges, and public drawing anchors in the visual used range.
    #[default]
    Used,
    /// Render one explicit inclusive worksheet rectangle.
    Range(RenderRange),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsedDrawingTerminalColumnPolicy {
    Indexed,
    CalcOoxmlSinglePage,
}

#[derive(Debug)]
struct CalcOoxmlSinglePageColumnBounds {
    cumulative_twips: Vec<u64>,
}

/// Hard resource ceilings applied before and during layout and serialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderLimits {
    /// Maximum source rows selected before hidden-row filtering.
    pub max_rows: u64,
    /// Maximum source columns selected before hidden-column filtering.
    pub max_columns: u64,
    /// Maximum rectangular source cell count.
    pub max_cells: u64,
    /// Maximum conditional-formatting rules retained for one worksheet.
    pub max_conditional_rules: u64,
    /// Maximum cell/rule evaluations used to resolve conditional formatting.
    pub max_conditional_evaluations: u64,
    /// Maximum image, chart, shape, and sparkline objects retained per sheet.
    pub max_drawing_objects: u64,
    /// Maximum aggregate embedded image payload bytes inspected per sheet.
    pub max_media_bytes: u64,
    /// Maximum decoded width or height of one embedded image.
    pub max_image_dimension: u64,
    /// Maximum decoded pixels in one embedded image.
    pub max_image_pixels: u64,
    /// Maximum aggregate decoded RGBA bytes retained per sheet.
    pub max_decoded_media_bytes: u64,
    /// Maximum aggregate chart series retained per sheet.
    pub max_chart_series: u64,
    /// Maximum aggregate chart and sparkline source points resolved per sheet.
    pub max_chart_points: u64,
    /// Maximum accumulated UTF-8 cell display-text bytes.
    pub max_text_bytes: u64,
    /// Maximum Unicode scalar values passed to text backends.
    pub max_glyphs: u64,
    /// Maximum visual runs produced by bidirectional shaping.
    pub max_text_runs: u64,
    /// Maximum laid-out lines after explicit and automatic wrapping.
    pub max_text_lines: u64,
    /// Maximum vector commands expanded from shaped glyph outlines.
    pub max_path_commands: u64,
    /// Maximum backend-neutral scene operations.
    pub max_scene_nodes: u64,
    /// Maximum canvas width or height in raw 1/1024-pixel units.
    pub max_dimension_raw: u64,
    /// Maximum serialized SVG size.
    pub max_output_bytes: u64,
}

impl Default for RenderLimits {
    fn default() -> Self {
        Self {
            max_rows: 4_096,
            max_columns: 512,
            max_cells: 250_000,
            max_conditional_rules: 4_096,
            max_conditional_evaluations: 1_000_000,
            max_drawing_objects: 4_096,
            max_media_bytes: 64 << 20,
            max_image_dimension: 16_384,
            max_image_pixels: 100_000_000,
            max_decoded_media_bytes: 256 << 20,
            max_chart_series: 256,
            max_chart_points: 1_000_000,
            max_text_bytes: 16 << 20,
            max_glyphs: 2_000_000,
            max_text_runs: 1_000_000,
            max_text_lines: 500_000,
            max_path_commands: 8_000_000,
            max_scene_nodes: 4_000_000,
            max_dimension_raw: 10_000_000 * FIXED_UNITS_PER_PIXEL as u64,
            max_output_bytes: 64 << 20,
        }
    }
}

/// Rendering policy for one worksheet range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOptions {
    /// Visual used range or explicit rectangular grid selection.
    pub selection: RenderSelection,
    /// Request worksheet gridlines. Ordinary rendering also honors the source
    /// sheet-view flag; print rendering instead combines this request with the
    /// source print-gridline setting.
    pub gridlines: bool,
    /// Give hidden rows and columns their normal geometry instead of omitting them.
    pub include_hidden: bool,
    /// Canvas background.
    pub background: Rgb,
    /// Fallback pixel width when the workbook has no width metadata or verified font.
    pub default_column_width: Fixed,
    /// Caller fallback row height for authored and non-OOXML sheets without
    /// retained height metadata. Imported format-retained application defaults
    /// take precedence.
    pub default_row_height: Fixed,
    /// Horizontal text padding inside a cell.
    pub horizontal_padding: Fixed,
    /// Fallback font family.
    pub default_font_family: String,
    /// Fallback font size in CSS pixels.
    pub default_font_size: Fixed,
    /// Smallest font size that shrink-to-fit may select.
    pub min_shrink_font_size: Fixed,
    /// Explicit verified font pack used for deterministic shaping and outlines.
    pub font_pack: Option<crate::FontPack>,
    /// Resource ceilings.
    pub limits: RenderLimits,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            selection: RenderSelection::Used,
            gridlines: true,
            include_hidden: false,
            background: Rgb::WHITE,
            default_column_width: Fixed::from_pixels(64),
            default_row_height: Fixed::from_pixels(20),
            horizontal_padding: Fixed::from_pixels(3),
            default_font_family: "Liberation Sans".to_string(),
            default_font_size: Fixed::from_raw(15_019),
            min_shrink_font_size: Fixed::from_raw(2_731),
            font_pack: None,
            limits: RenderLimits::default(),
        }
    }
}

impl RenderOptions {
    /// Layer a verified fallback after the current caller font pack.
    ///
    /// If no primary pack is configured, the fallback becomes the sole pack.
    /// Exact families remain caller-first, aliases are considered only after
    /// exact matches, and the resulting stack never discovers host fonts.
    pub fn with_fallback_font_pack(mut self, fallback: &FontPack) -> Result<Self, FontPackError> {
        self.font_pack = Some(match self.font_pack.as_ref() {
            Some(caller) => caller.with_fallback(fallback)?,
            None => fallback.clone(),
        });
        Ok(self)
    }
}

/// One zero-based worksheet coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellCoordinate {
    /// Row index.
    pub row: u32,
    /// Column index.
    pub col: u16,
}

/// Renderer-owned sparse coordinate index built from the frozen public
/// `Sheet::display_cells` surface. One operation builds it once, then each
/// rectangular query visits only requested rows and retained cells.
pub(crate) struct SparseDisplayCellIndex<'a> {
    sheet: &'a Sheet,
}

impl<'a> SparseDisplayCellIndex<'a> {
    pub(crate) fn new(sheet: &'a Sheet) -> Self {
        Self { sheet }
    }

    pub(crate) fn range(
        &self,
        range: (u32, u16, u32, u16),
    ) -> impl Iterator<Item = DisplayCell<'a>> + '_ {
        self.sheet
            .display_cells_in_range(range.0, range.1, range.2, range.3)
    }
}

/// Stable warning category for a deliberate rendering approximation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WarningCode {
    /// Text advance uses the bounded approximate fallback because no verified font pack was supplied.
    ApproximateTextMetrics,
    /// The requested font family was replaced by a verified pack fallback.
    FontFamilySubstituted,
    /// No verified font in the pack contained one or more requested glyphs.
    MissingGlyph,
    /// A non-solid spreadsheet fill was reduced to one color.
    PatternFillSimplified,
    /// Rich runs were flattened because exact outlined typography was unavailable or their text was inconsistent.
    RichTextFlattened,
    /// Wrapping was reduced to backend clipping in the fontless approximate fallback.
    TextWrappingSimplified,
    /// Shrink-to-fit was not applied in the fontless approximate fallback.
    ShrinkToFitIgnored,
    /// Superscript or subscript was not applied in the fontless approximate fallback.
    FontScriptIgnored,
    /// A double border was represented as one thick line.
    DoubleBorderSimplified,
    /// A malformed or overlapping merge was skipped.
    MergeSkipped,
    /// The merge anchor was hidden or outside the selected rectangle.
    MergeAnchorOutsideVisibleRange,
    /// A non-finite or non-positive workbook dimension used the configured fallback.
    InvalidGeometryFallback,
    /// An XML-forbidden character was replaced with U+FFFD.
    InvalidXmlCharacterReplaced,
    /// A hyperlink with a non-allowlisted URI scheme was omitted.
    UnsafeHyperlinkDropped,
    /// A retained conditional-format rule was outside the bounded painted subset.
    ConditionalFormattingDeferred,
    /// A gradient data bar was represented by a deterministic solid bar.
    ConditionalDataBarSimplified,
    /// A numeric or date display that does not fit was replaced by hash marks.
    NumericOverflowHashed,
    /// An unsupported embedded image was represented by a bounded geometric placeholder.
    ImagePlaceholder,
    /// An unsupported chart or series was represented by a bounded geometric placeholder.
    ChartPlaceholder,
    /// Unsupported chart-series metadata used a deterministic visual fallback.
    ChartMetadataSimplified,
    /// An unsupported sparkline source was represented by a bounded geometric placeholder.
    SparklinePlaceholder,
    /// An unsupported drawing shape was represented by a bounded geometric placeholder.
    ShapePlaceholder,
    /// A drawing could not be located inside the selected visible axes.
    DrawingAnchorUnavailable,
    /// Source shape metadata lacked a public anchor and was not painted.
    ShapeAnchorUnavailable,
    /// Print pagination is separate from the current whole-sheet scene.
    PaginationDeferred,
    /// Formula display currently uses the retained cached/formatted value.
    CachedFormulaDisplay,
    /// The reader retained a documented subset of source style information.
    SourceStylesPartial,
    /// The reader did not retain source style information for this sheet.
    SourceStylesUnavailable,
}

impl WarningCode {
    /// Stable machine-readable identifier.
    pub const fn code(self) -> &'static str {
        match self {
            Self::ApproximateTextMetrics => "approximate_text_metrics",
            Self::FontFamilySubstituted => "font_family_substituted",
            Self::MissingGlyph => "missing_glyph",
            Self::PatternFillSimplified => "pattern_fill_simplified",
            Self::RichTextFlattened => "rich_text_flattened",
            Self::TextWrappingSimplified => "text_wrapping_simplified",
            Self::ShrinkToFitIgnored => "shrink_to_fit_ignored",
            Self::FontScriptIgnored => "font_script_ignored",
            Self::DoubleBorderSimplified => "double_border_simplified",
            Self::MergeSkipped => "merge_skipped",
            Self::MergeAnchorOutsideVisibleRange => "merge_anchor_outside_visible_range",
            Self::InvalidGeometryFallback => "invalid_geometry_fallback",
            Self::InvalidXmlCharacterReplaced => "invalid_xml_character_replaced",
            Self::UnsafeHyperlinkDropped => "unsafe_hyperlink_dropped",
            Self::ConditionalFormattingDeferred => "conditional_formatting_deferred",
            Self::ConditionalDataBarSimplified => "conditional_data_bar_simplified",
            Self::NumericOverflowHashed => "numeric_overflow_hashed",
            Self::ImagePlaceholder => "image_placeholder",
            Self::ChartPlaceholder => "chart_placeholder",
            Self::ChartMetadataSimplified => "chart_metadata_simplified",
            Self::SparklinePlaceholder => "sparkline_placeholder",
            Self::ShapePlaceholder => "shape_placeholder",
            Self::DrawingAnchorUnavailable => "drawing_anchor_unavailable",
            Self::ShapeAnchorUnavailable => "shape_anchor_unavailable",
            Self::PaginationDeferred => "pagination_deferred",
            Self::CachedFormulaDisplay => "cached_formula_display",
            Self::SourceStylesPartial => "source_styles_partial",
            Self::SourceStylesUnavailable => "source_styles_unavailable",
        }
    }
}

/// Aggregated warning with deterministic first-occurrence provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderWarning {
    /// Warning category.
    pub code: WarningCode,
    /// Number of occurrences.
    pub occurrences: u64,
    /// First affected cell, if the warning is cell-scoped.
    pub first_cell: Option<CellCoordinate>,
}

/// One path-free verified font face selected by layout or text shaping.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RenderedFontFace {
    /// SHA-256 of the source verified font pack containing this face.
    pub source_pack_sha256: String,
    /// SHA-256 of the complete selected OpenType face bytes.
    pub face_sha256: String,
    /// Declared actual family, never a proprietary alias label.
    pub family: String,
    /// Selected CSS-style numeric weight.
    pub weight: u16,
    /// Whether the selected face is italic.
    pub italic: bool,
    /// Whether at least one use substituted this face for another family.
    pub substituted: bool,
}

/// Machine-readable statistics and approximations for one render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Zero-based source sheet index.
    pub sheet_index: usize,
    /// Source sheet name.
    pub sheet_name: String,
    /// Inclusive source rectangle.
    pub range: RenderRange,
    /// Source rows before hidden-row filtering.
    pub rows_considered: u64,
    /// Source columns before hidden-column filtering.
    pub columns_considered: u64,
    /// Rectangular source cells before hidden-axis filtering.
    pub cells_considered: u64,
    /// Visible (or explicitly included hidden) rows.
    pub visible_rows: u64,
    /// Visible (or explicitly included hidden) columns.
    pub visible_columns: u64,
    /// Rendered cell or merged-cell regions.
    pub rendered_regions: u64,
    /// Hidden rows omitted from the selected range.
    pub hidden_rows_skipped: u64,
    /// Hidden columns omitted from the selected range.
    pub hidden_columns_skipped: u64,
    /// Non-overlapping merged regions represented in the scene.
    pub merged_regions: u64,
    /// Accumulated UTF-8 display-text bytes.
    pub text_bytes: u64,
    /// Unicode scalar values passed to the text backend.
    pub glyphs: u64,
    /// Scene node count.
    pub scene_nodes: u64,
    /// Serialized SVG bytes, or zero before SVG serialization.
    pub svg_bytes: u64,
    /// SHA-256 of the effective verified pack or caller-first pack stack.
    pub font_pack_sha256: Option<String>,
    /// Every selected verified face, sorted by path-free identity.
    pub font_faces: Vec<RenderedFontFace>,
    /// Deterministically ordered warnings.
    pub warnings: Vec<RenderWarning>,
}

impl RenderReport {
    /// Serialize this report to stable compact JSON without environment data.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"schema_version\":");
        out.push_str(&self.schema_version.to_string());
        out.push_str(",\"sheet_index\":");
        out.push_str(&self.sheet_index.to_string());
        out.push_str(",\"sheet_name\":\"");
        push_json_escaped(&mut out, &self.sheet_name);
        out.push_str("\",\"range\":{\"first_row\":");
        out.push_str(&self.range.first_row.to_string());
        out.push_str(",\"first_col\":");
        out.push_str(&self.range.first_col.to_string());
        out.push_str(",\"last_row\":");
        out.push_str(&self.range.last_row.to_string());
        out.push_str(",\"last_col\":");
        out.push_str(&self.range.last_col.to_string());
        out.push_str("},\"rows_considered\":");
        out.push_str(&self.rows_considered.to_string());
        out.push_str(",\"columns_considered\":");
        out.push_str(&self.columns_considered.to_string());
        out.push_str(",\"cells_considered\":");
        out.push_str(&self.cells_considered.to_string());
        out.push_str(",\"visible_rows\":");
        out.push_str(&self.visible_rows.to_string());
        out.push_str(",\"visible_columns\":");
        out.push_str(&self.visible_columns.to_string());
        out.push_str(",\"rendered_regions\":");
        out.push_str(&self.rendered_regions.to_string());
        out.push_str(",\"hidden_rows_skipped\":");
        out.push_str(&self.hidden_rows_skipped.to_string());
        out.push_str(",\"hidden_columns_skipped\":");
        out.push_str(&self.hidden_columns_skipped.to_string());
        out.push_str(",\"merged_regions\":");
        out.push_str(&self.merged_regions.to_string());
        out.push_str(",\"text_bytes\":");
        out.push_str(&self.text_bytes.to_string());
        out.push_str(",\"glyphs\":");
        out.push_str(&self.glyphs.to_string());
        out.push_str(",\"scene_nodes\":");
        out.push_str(&self.scene_nodes.to_string());
        out.push_str(",\"svg_bytes\":");
        out.push_str(&self.svg_bytes.to_string());
        out.push_str(",\"font_pack_sha256\":");
        match &self.font_pack_sha256 {
            Some(digest) => {
                out.push('"');
                out.push_str(digest);
                out.push('"');
            }
            None => out.push_str("null"),
        }
        out.push_str(",\"font_faces\":[");
        for (index, face) in self.font_faces.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            out.push_str("{\"source_pack_sha256\":\"");
            out.push_str(&face.source_pack_sha256);
            out.push_str("\",\"face_sha256\":\"");
            out.push_str(&face.face_sha256);
            out.push_str("\",\"family\":\"");
            push_json_escaped(&mut out, &face.family);
            out.push_str("\",\"weight\":");
            out.push_str(&face.weight.to_string());
            out.push_str(",\"italic\":");
            out.push_str(if face.italic { "true" } else { "false" });
            out.push_str(",\"substituted\":");
            out.push_str(if face.substituted { "true" } else { "false" });
            out.push('}');
        }
        out.push(']');
        out.push_str(",\"warnings\":[");
        for (index, warning) in self.warnings.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            out.push_str("{\"code\":\"");
            out.push_str(warning.code.code());
            out.push_str("\",\"occurrences\":");
            out.push_str(&warning.occurrences.to_string());
            out.push_str(",\"first_cell\":");
            match warning.first_cell {
                Some(cell) => {
                    out.push_str("{\"row\":");
                    out.push_str(&cell.row.to_string());
                    out.push_str(",\"col\":");
                    out.push_str(&cell.col.to_string());
                    out.push('}');
                }
                None => out.push_str("null"),
            }
            out.push('}');
        }
        out.push_str("]}");
        out
    }
}

/// Result of bounded layout before backend serialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneBuild {
    /// Backend-neutral scene.
    pub scene: Scene,
    /// Layout report; `svg_bytes` remains zero until SVG serialization.
    pub report: RenderReport,
}

/// One measured worksheet axis entry shared with print pagination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MeasuredAxisSlot<I> {
    pub(crate) index: I,
    pub(crate) offset: Fixed,
    pub(crate) size: Fixed,
}

type AxisSlot<I> = MeasuredAxisSlot<I>;

pub(crate) type MeasuredAxes = (Vec<MeasuredAxisSlot<u32>>, Vec<MeasuredAxisSlot<u16>>);

/// Prepared worksheet geometry reused while materializing paginated print
/// tiles. The print planner measures the complete body/title union once; each
/// tile must replay those exact axis sizes instead of deriving a smaller,
/// tile-local automatic-row-height model. The complete axes also establish one
/// stable sheet-space coordinate system for cell-anchored drawings: a print
/// tile clips and translates that geometry instead of resizing the object to
/// the tile-local anchor fragment.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SheetGeometryOverride<'a> {
    rows: &'a [MeasuredAxisSlot<u32>],
    columns: &'a [MeasuredAxisSlot<u16>],
}

impl<'a> SheetGeometryOverride<'a> {
    pub(crate) fn new(
        rows: &'a [MeasuredAxisSlot<u32>],
        columns: &'a [MeasuredAxisSlot<u16>],
    ) -> Self {
        Self { rows, columns }
    }
}

struct AxisMeasurement {
    rows: Vec<MeasuredAxisSlot<u32>>,
    columns: Vec<MeasuredAxisSlot<u16>>,
    source_native_twips: Option<SourceNativeAxisTwips>,
    maximum_digit_width: Fixed,
    typography: TypographyStats,
    conditional_evaluations: u64,
}

struct SourceNativeAxisTwips {
    rows: Vec<i128>,
    columns: Vec<i128>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AxisEndpointPolicy {
    PerTrackFixed,
    SourceNative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridlinePolicy {
    WorksheetView,
    AuthoredPrint,
    CalcSinglePagePrint,
}

// Calc paints print gridlines as black 0.1-point hairlines. The scene uses
// 1/1024 CSS-pixel units and the PDF backend applies 3/4 point per CSS pixel,
// so 137 raw units is the closest backend-neutral representation.
pub(crate) const PRINT_GRIDLINE_WIDTH: Fixed = Fixed::from_raw(137);
const PRINT_GRIDLINE_FRAME_TOP_INSET: Fixed = Fixed::from_raw(39);
const PRINT_GRIDLINE_FRAME_LEFT_INSET: Fixed = Fixed::from_raw(155);
const PRINT_GRIDLINE_FRAME_TRAILING_INSET: Fixed = Fixed::from_raw(1_355);

// Calc fits the cell layer into a SinglePageSheets page against an extra
// 20-twip extent. Drawing objects retain the unfitted sheet-space axis.
const CALC_SINGLE_PAGE_FIT_PADDING_TWIPS: i128 = 20;

/// Immutable, per-operation effective-style capture. Every selected grid
/// coordinate (plus an intersecting merge anchor) resolves worksheet, axis,
/// table-region, and direct-cell layers exactly once before typography
/// measurement or scene painting begins. Conditional overlays remain a later
/// bounded paint step.
struct RenderStyleSnapshot {
    default_style: Option<Arc<CellStyle>>,
    styles: BTreeMap<CellCoordinate, Option<Arc<CellStyle>>>,
    interned: HashMap<CellStyle, Arc<CellStyle>>,
}

impl RenderStyleSnapshot {
    fn new(sheet: &Sheet) -> Self {
        let mut snapshot = Self {
            default_style: None,
            styles: BTreeMap::new(),
            interned: HashMap::new(),
        };
        snapshot.default_style = snapshot.intern(sheet.default_cell_style().cloned());
        snapshot
    }

    fn intern(&mut self, style: Option<CellStyle>) -> Option<Arc<CellStyle>> {
        let style = style?;
        if let Some(interned) = self.interned.get(&style) {
            return Some(Arc::clone(interned));
        }
        let interned = Arc::new(style.clone());
        self.interned.insert(style, Arc::clone(&interned));
        Some(interned)
    }

    fn capture_coordinate(&mut self, sheet: &Sheet, coordinate: CellCoordinate) {
        if self.styles.contains_key(&coordinate) {
            return;
        }
        let style = self.intern(sheet.resolved_cell_style(coordinate.row, coordinate.col));
        self.styles.insert(coordinate, style);
    }

    fn capture_sparse_visual_candidates(
        &mut self,
        sheet: &Sheet,
        options: &RenderOptions,
    ) -> Result<(), RenderError> {
        // Reject dense source models before resolving or interning a style for
        // every cell. This keeps the materialized-cell ceiling useful as a
        // wall/RSS guard even when a reader legitimately exposes millions of
        // populated cells.
        let mut display_cell_count = 0_u64;
        for _ in sheet.display_cells() {
            display_cell_count = display_cell_count
                .checked_add(1)
                .ok_or(RenderError::CoordinateOverflow)?;
            enforce(
                LimitKind::Cells,
                options.limits.max_cells,
                display_cell_count,
            )?;
        }
        let mut blank_style_count = 0_u64;
        for _ in sheet.blank_cell_styles().keys() {
            blank_style_count = blank_style_count
                .checked_add(1)
                .ok_or(RenderError::CoordinateOverflow)?;
            enforce(
                LimitKind::Cells,
                options.limits.max_cells,
                blank_style_count,
            )?;
        }
        for cell in sheet.display_cells() {
            self.capture_coordinate(
                sheet,
                CellCoordinate {
                    row: cell.row,
                    col: cell.col,
                },
            );
            enforce(
                LimitKind::Cells,
                options.limits.max_cells,
                self.styles.len() as u64,
            )?;
        }
        for &(row, col) in sheet.blank_cell_styles().keys() {
            self.capture_coordinate(sheet, CellCoordinate { row, col });
            enforce(
                LimitKind::Cells,
                options.limits.max_cells,
                self.styles.len() as u64,
            )?;
        }
        Ok(())
    }

    fn capture_range(
        &mut self,
        sheet: &Sheet,
        range: RenderRange,
        options: &RenderOptions,
    ) -> Result<(), RenderError> {
        for row in range.first_row..=range.last_row {
            for col in range.first_col..=range.last_col {
                self.capture_coordinate(sheet, CellCoordinate { row, col });
            }
        }
        for &(r0, c0, r1, c1) in sheet.merged_ranges() {
            if r0 <= r1
                && c0 <= c1
                && r0 <= range.last_row
                && r1 >= range.first_row
                && c0 <= range.last_col
                && c1 >= range.first_col
            {
                self.capture_coordinate(sheet, CellCoordinate { row: r0, col: c0 });
            }
        }
        enforce(
            LimitKind::Cells,
            options.limits.max_cells,
            self.styles.len() as u64,
        )
    }

    fn style(&self, coordinate: CellCoordinate) -> Option<&CellStyle> {
        self.styles.get(&coordinate).and_then(Option::as_deref)
    }

    fn owned_style(&self, coordinate: CellCoordinate) -> Option<CellStyle> {
        self.style(coordinate).cloned()
    }

    fn default_style(&self) -> Option<&CellStyle> {
        self.default_style.as_deref()
    }
}

/// Measure row and column geometry with exactly the same conversion rules used
/// by worksheet layout. Print pagination consumes this instead of maintaining a
/// second, subtly different width/height model.
#[cfg(test)]
pub(crate) fn measure_sheet_axes(
    sheet: &Sheet,
    range: RenderRange,
    options: &RenderOptions,
) -> Result<MeasuredAxes, RenderError> {
    measure_sheet_axes_for_ranges(sheet, &[range], options)?
        .pop()
        .ok_or(RenderError::CoordinateOverflow)
}

/// Measure several disjoint print rectangles against one sparse display-cell
/// candidate index. Body and title planning therefore never rebuilds or walks
/// the complete worksheet cell set once per rectangle.
pub(crate) fn measure_sheet_axes_for_ranges(
    sheet: &Sheet,
    ranges: &[RenderRange],
    options: &RenderOptions,
) -> Result<Vec<MeasuredAxes>, RenderError> {
    measure_sheet_axes_for_ranges_with_policy(
        sheet,
        ranges,
        options,
        AxisEndpointPolicy::PerTrackFixed,
    )
}

fn measure_sheet_axes_for_ranges_with_policy(
    sheet: &Sheet,
    ranges: &[RenderRange],
    options: &RenderOptions,
    endpoint_policy: AxisEndpointPolicy,
) -> Result<Vec<MeasuredAxes>, RenderError> {
    let mut validated = Vec::with_capacity(ranges.len());
    for &range in ranges {
        let range = range.validate()?;
        let cells = (u64::from(range.last_row) - u64::from(range.first_row) + 1)
            .checked_mul(u64::from(range.last_col) - u64::from(range.first_col) + 1)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(LimitKind::Cells, options.limits.max_cells, cells)?;
        validated.push(range);
    }

    // Automatic row height is a worksheet-row property: content outside the
    // painted columns can still establish the row's height. Merge the body and
    // title row bands first so each retained candidate is indexed and filtered
    // exactly once, rather than once per measurement rectangle.
    let mut row_bands = validated
        .iter()
        .map(|range| (range.first_row, range.last_row))
        .collect::<Vec<_>>();
    row_bands.sort_unstable();
    let mut merged_row_bands = Vec::<(u32, u32)>::new();
    for (first, last) in row_bands {
        if let Some((_, previous_last)) = merged_row_bands.last_mut() {
            if first <= previous_last.saturating_add(1) {
                *previous_last = (*previous_last).max(last);
                continue;
            }
        }
        merged_row_bands.push((first, last));
    }
    let display_cell_index = SparseDisplayCellIndex::new(sheet);
    let mut candidates = BTreeMap::<CellCoordinate, DisplayCell<'_>>::new();
    for (first_row, last_row) in merged_row_bands {
        for cell in display_cell_index.range((first_row, 0, last_row, MAX_WORKSHEET_COLUMN)) {
            candidates.insert(
                CellCoordinate {
                    row: cell.row,
                    col: cell.col,
                },
                cell,
            );
            enforce(
                LimitKind::Cells,
                options.limits.max_cells,
                candidates.len() as u64,
            )?;
        }
    }
    // A merge anchor may sit outside the clipped row/column rectangle while
    // covered cells remain visible inside it. Add only intersecting anchors.
    for &(r0, c0, r1, c1) in sheet.merged_ranges() {
        if validated
            .iter()
            .any(|range| r0 <= r1 && c0 <= c1 && r0 <= range.last_row && r1 >= range.first_row)
        {
            for cell in display_cell_index.range((r0, c0, r0, c0)) {
                candidates.insert(CellCoordinate { row: r0, col: c0 }, cell);
                enforce(
                    LimitKind::Cells,
                    options.limits.max_cells,
                    candidates.len() as u64,
                )?;
            }
        }
    }
    let candidates = candidates.into_values().collect::<Vec<_>>();
    let mut measurements = Vec::with_capacity(validated.len());
    let mut conditional_evaluations = 0_u64;
    for range in validated {
        let mut style_snapshot = RenderStyleSnapshot::new(sheet);
        style_snapshot.capture_range(sheet, range, options)?;
        let mut warnings = Warnings::default();
        let measured = measure_sheet_axes_inner_with_policy(
            sheet,
            range,
            &style_snapshot,
            options,
            Some(&candidates),
            &mut warnings,
            endpoint_policy,
            conditional_evaluations,
        )?;
        conditional_evaluations = measured.conditional_evaluations;
        measurements.push((measured.rows, measured.columns));
    }
    Ok(measurements)
}

#[cfg(test)]
fn measure_sheet_axes_inner(
    sheet: &Sheet,
    range: RenderRange,
    style_snapshot: &RenderStyleSnapshot,
    options: &RenderOptions,
    automatic_candidates: Option<&[DisplayCell<'_>]>,
    warnings: &mut Warnings,
) -> Result<AxisMeasurement, RenderError> {
    measure_sheet_axes_inner_with_policy(
        sheet,
        range,
        style_snapshot,
        options,
        automatic_candidates,
        warnings,
        AxisEndpointPolicy::PerTrackFixed,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
fn measure_sheet_axes_inner_with_policy(
    sheet: &Sheet,
    range: RenderRange,
    style_snapshot: &RenderStyleSnapshot,
    options: &RenderOptions,
    automatic_candidates: Option<&[DisplayCell<'_>]>,
    warnings: &mut Warnings,
    endpoint_policy: AxisEndpointPolicy,
    initial_conditional_evaluations: u64,
) -> Result<AxisMeasurement, RenderError> {
    let range = range.validate()?;
    let row_count = u64::from(range.last_row) - u64::from(range.first_row) + 1;
    let column_count = u64::from(range.last_col) - u64::from(range.first_col) + 1;
    enforce(LimitKind::Rows, options.limits.max_rows, row_count)?;
    enforce(LimitKind::Columns, options.limits.max_columns, column_count)?;
    let mut typography = TypographyStats::default();
    let mut conditional_evaluations = initial_conditional_evaluations;
    let maximum_digit_width =
        maximum_digit_width(style_snapshot, options, warnings, &mut typography)?;
    let source_native_columns = endpoint_policy == AxisEndpointPolicy::SourceNative;
    let mut columns = Vec::new();
    let mut column_widths = BTreeMap::new();
    let mut x = Fixed::ZERO;
    for column in range.first_col..=range.last_col {
        if !options.include_hidden && sheet.hidden_columns().contains(&column) {
            continue;
        }
        let size = column_width(sheet, column, maximum_digit_width, options, warnings);
        column_widths.insert(column, size);
        columns.push(MeasuredAxisSlot {
            index: column,
            offset: x,
            size,
        });
        if !source_native_columns {
            x = x.checked_add(size).ok_or(RenderError::CoordinateOverflow)?;
            enforce_dimension(x, options)?;
        }
    }
    let source_native_column_twips = if source_native_columns {
        let prefix = source_native_column_prefix(
            sheet,
            range.first_col,
            maximum_digit_width,
            options,
            warnings,
        )?;
        let contributions = apply_source_native_axis_endpoints(
            &mut columns,
            prefix,
            maximum_digit_width,
            options,
            |column| imported_column_axis_measure(sheet, column, options),
        )?;
        for slot in &columns {
            column_widths.insert(slot.index, slot.size);
        }
        Some(contributions)
    } else {
        None
    };

    let mut row_sizes = BTreeMap::new();
    for row in range.first_row..=range.last_row {
        if !options.include_hidden && row_is_hidden(sheet, row) {
            continue;
        }
        row_sizes.insert(row, row_height(sheet, row, options, warnings));
    }
    let mut source_native_rows = None;
    if endpoint_policy == AxisEndpointPolicy::SourceNative {
        let prefix = source_native_row_prefix(
            sheet,
            range.first_row,
            maximum_digit_width,
            options,
            warnings,
        )?;
        let mut native_rows = row_sizes
            .iter()
            .map(|(&row, &size)| MeasuredAxisSlot {
                index: row,
                offset: Fixed::ZERO,
                size,
            })
            .collect::<Vec<_>>();
        let native_contributions = apply_source_native_axis_endpoints(
            &mut native_rows,
            prefix,
            maximum_digit_width,
            options,
            |row| imported_row_axis_measure(sheet, row, options),
        )?;
        let contributions: BTreeMap<_, _> = native_rows
            .iter()
            .zip(native_contributions)
            .map(|(slot, twips)| (slot.index, twips))
            .collect();
        row_sizes = native_rows
            .into_iter()
            .map(|slot| (slot.index, slot.size))
            .collect();
        source_native_rows = Some((prefix, contributions, row_sizes.clone()));
    }
    expand_automatic_row_heights(
        sheet,
        range,
        style_snapshot,
        maximum_digit_width,
        options,
        warnings,
        &mut column_widths,
        &mut row_sizes,
        &mut typography,
        &mut conditional_evaluations,
        automatic_candidates,
    )?;

    let mut rows = Vec::with_capacity(row_sizes.len());
    let mut source_native_row_twips = None;
    if let Some((prefix, native_contributions, native_baselines)) = source_native_rows {
        // Automatic-height measurement operates in the renderer's Fixed
        // domain. Calc persists the resulting track as integer twips before
        // SinglePageSheets converts cumulative endpoints to Map100thMM, so
        // preserve untouched source measures and requantize only grown rows.
        let mut cursor = SourceAxisCursor::new(prefix)?;
        let mut final_contributions = Vec::with_capacity(row_sizes.len());
        for (row, size) in row_sizes {
            let contribution = if native_baselines.get(&row) == Some(&size) {
                native_contributions
                    .get(&row)
                    .copied()
                    .ok_or(RenderError::CoordinateOverflow)?
            } else {
                source_axis_contribution_twips(None, size, maximum_digit_width)
                    .ok_or(RenderError::CoordinateOverflow)?
            };
            final_contributions.push(contribution);
            let (offset, size, boundary) = cursor.advance(contribution)?;
            rows.push(MeasuredAxisSlot {
                index: row,
                offset,
                size,
            });
            enforce_dimension(boundary, options)?;
        }
        source_native_row_twips = Some(final_contributions);
    } else {
        let mut y = Fixed::ZERO;
        for (row, size) in row_sizes {
            rows.push(MeasuredAxisSlot {
                index: row,
                offset: y,
                size,
            });
            y = y.checked_add(size).ok_or(RenderError::CoordinateOverflow)?;
            enforce_dimension(y, options)?;
        }
    }
    let source_native_twips = match (source_native_row_twips, source_native_column_twips) {
        (Some(rows), Some(columns)) => Some(SourceNativeAxisTwips { rows, columns }),
        (None, None) => None,
        _ => return Err(RenderError::CoordinateOverflow),
    };
    Ok(AxisMeasurement {
        rows,
        columns,
        source_native_twips,
        maximum_digit_width,
        typography,
        conditional_evaluations,
    })
}

/// Shape auxiliary page text (headings, headers, and footers) through the same
/// verified-font outline pipeline as cell text. Without a font pack this
/// deliberately returns the same approximate `Text` node used by sheet layout.
pub(crate) fn build_auxiliary_text_node(
    text: String,
    bounds: Rect,
    horizontal_padding: Fixed,
    style: TextStyle,
    options: &RenderOptions,
) -> Result<SceneNode, RenderError> {
    build_auxiliary_text_node_with_kerning(text, bounds, horizontal_padding, style, true, options)
}

fn build_auxiliary_text_node_with_kerning(
    text: String,
    bounds: Rect,
    horizontal_padding: Fixed,
    style: TextStyle,
    kerning: bool,
    options: &RenderOptions,
) -> Result<SceneNode, RenderError> {
    build_auxiliary_text_node_with_clip_and_kerning(
        text,
        bounds,
        bounds,
        horizontal_padding,
        style,
        kerning,
        options,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_auxiliary_text_node_with_clip_and_kerning(
    text: String,
    bounds: Rect,
    clip_bounds: Rect,
    horizontal_padding: Fixed,
    style: TextStyle,
    kerning: bool,
    options: &RenderOptions,
) -> Result<SceneNode, RenderError> {
    let Some(font_pack) = options.font_pack.as_ref() else {
        return Ok(SceneNode::Text(TextNode {
            text,
            bounds,
            clip_bounds,
            horizontal_padding,
            style,
            hyperlink: None,
        }));
    };
    let region = Region {
        source: CellCoordinate { row: 0, col: 0 },
        rect: bounds,
        is_merged: false,
        line_layout_policy: CellLineLayoutPolicy::Native,
        line_placement_policy: CalcLinePlacementPolicy::Native,
        calc_wrap_space: None,
        style: None,
        conditional: ConditionalPaint::default(),
        text,
        rich_text: None,
        hyperlink: None,
        numeric_default: false,
        text_can_overflow: false,
        fixed_height_row: false,
        ods_fixed_height_row: false,
        print_vertical_overflow: false,
        vertical_margin: CALC_CELL_VERTICAL_MARGIN,
    };
    let mut auxiliary_options = options.clone();
    auxiliary_options.horizontal_padding = horizontal_padding;
    let mut statistics = TypographyStats::default();
    let mut warnings = Warnings::default();
    Ok(SceneNode::GlyphRun(build_glyph_run(
        font_pack,
        &region,
        bounds,
        clip_bounds,
        None,
        &style,
        false,
        kerning,
        false,
        &auxiliary_options,
        &mut statistics,
        &mut warnings,
    )?))
}

#[derive(Debug, Clone)]
struct MergeLayout {
    owner: CellCoordinate,
    anchor: CellCoordinate,
    rect: Rect,
    has_adjustable_row: bool,
    calc_wrap_space: Option<CalcWrapSpace>,
}

/// Calc EditEngine's wrapping coordinate space for an exactly recoverable
/// OOXML column span.
///
/// The paper width is deliberately typed and retained in Map100thMM instead
/// of being mixed with physical `Fixed` pixels used for glyph painting.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct CalcWrapSpace {
    paper_width_mm100: u64,
}

#[derive(Debug, Clone, Copy)]
struct CalcLineLayoutEvidence {
    is_plain_text: bool,
    has_adjustable_row: bool,
    wrap_space_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CalcImportProvenance {
    Ods,
    Biff,
    Xlsb,
    Xlsx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CalcLinePlacementPolicy {
    Native,
    #[cfg(test)]
    RequestedFace,
    Imported(CalcImportProvenance),
}

impl CalcLinePlacementPolicy {
    fn uses_placement_ascent(self) -> bool {
        self != Self::Native
    }

    fn uses_placement_line_height(self) -> bool {
        !matches!(
            self,
            Self::Native | Self::Imported(CalcImportProvenance::Biff)
        )
    }
}

#[derive(Debug, Clone)]
struct Region {
    source: CellCoordinate,
    rect: Rect,
    is_merged: bool,
    line_layout_policy: CellLineLayoutPolicy,
    line_placement_policy: CalcLinePlacementPolicy,
    calc_wrap_space: Option<CalcWrapSpace>,
    style: Option<CellStyle>,
    conditional: ConditionalPaint,
    text: String,
    rich_text: Option<Vec<rxls::TextRun>>,
    hyperlink: Option<String>,
    numeric_default: bool,
    text_can_overflow: bool,
    fixed_height_row: bool,
    ods_fixed_height_row: bool,
    print_vertical_overflow: bool,
    vertical_margin: Fixed,
}

#[derive(Debug, Clone, Default)]
struct ConditionalPaint {
    style: Option<CellStyle>,
    data_bar: Option<DataBarPaint>,
}

/// One conditional-format rule's resolved outcome for a single cell.
///
/// Grouped so [`apply_conditional_paint`] takes the rule result as one value
/// instead of four positional flags that are easy to transpose at a call site.
struct ConditionalOutcome {
    style: Option<CellStyle>,
    data_bar: Option<DataBarPaint>,
    stop_if_true: bool,
    text_measurement_unresolved: bool,
}

#[derive(Debug, Clone, Copy)]
struct DataBarPaint {
    color: Rgb,
    width_ppm: u32,
}

#[derive(Debug, Clone, Copy)]
enum DrawingPlaceholderKind {
    Image(usize),
    Chart(usize, ChartKind),
    Sparkline(usize, SparklineKind),
    Shape,
}

#[derive(Debug, Clone, Copy)]
struct DrawingPlaceholder {
    kind: DrawingPlaceholderKind,
    rect: Rect,
    z_order: i64,
    ordinal: u64,
    source: CellCoordinate,
    clip: Option<Rect>,
}

enum DrawingPlacement {
    Placed(Rect),
    OutsideViewport,
    Unavailable,
}

#[derive(Clone, Default)]
struct TypographyStats {
    text_bytes: u64,
    shaped_glyphs: u64,
    text_work: u64,
    shaped_runs: u64,
    text_lines: u64,
    path_commands: u64,
    font_faces: BTreeMap<(String, String, String, u16, bool), bool>,
}

impl TypographyStats {
    fn record_face(
        &mut self,
        pack: &FontPack,
        font_id: crate::font::FontId,
        substituted: bool,
    ) -> Result<(), RenderError> {
        let identity = pack
            .selected_face_identity(font_id)
            .map_err(map_font_error)?;
        self.font_faces
            .entry((
                identity.source_pack_sha256.to_string(),
                identity.face_sha256.to_string(),
                identity.family.to_string(),
                identity.weight,
                identity.italic,
            ))
            .and_modify(|seen_substitution| *seen_substitution |= substituted)
            .or_insert(substituted);
        Ok(())
    }

    fn finish_font_faces(self) -> Vec<RenderedFontFace> {
        self.font_faces
            .into_iter()
            .map(
                |((source_pack_sha256, face_sha256, family, weight, italic), substituted)| {
                    RenderedFontFace {
                        source_pack_sha256,
                        face_sha256,
                        family,
                        weight,
                        italic,
                        substituted,
                    }
                },
            )
            .collect()
    }
}

#[derive(Clone, Default)]
struct Warnings(BTreeMap<WarningCode, (u64, Option<CellCoordinate>)>);

impl Warnings {
    fn add(&mut self, code: WarningCode, cell: Option<CellCoordinate>) {
        let entry = self.0.entry(code).or_insert((0, cell));
        entry.0 = entry.0.saturating_add(1);
    }

    fn add_count(&mut self, code: WarningCode, count: u64, cell: Option<CellCoordinate>) {
        if count == 0 {
            return;
        }
        let entry = self.0.entry(code).or_insert((0, cell));
        entry.0 = entry.0.saturating_add(count);
    }

    fn finish(self) -> Vec<RenderWarning> {
        self.0
            .into_iter()
            .map(|(code, (occurrences, first_cell))| RenderWarning {
                code,
                occurrences,
                first_cell,
            })
            .collect()
    }
}

/// Lay out one workbook sheet as a backend-neutral fixed-point scene.
pub fn build_scene(
    workbook: &Workbook,
    sheet_index: usize,
    options: &RenderOptions,
) -> Result<SceneBuild, RenderError> {
    let sheet = workbook
        .sheets
        .get(sheet_index)
        .ok_or(RenderError::SheetIndexOutOfRange {
            requested: sheet_index,
            sheet_count: workbook.sheets.len(),
        })?;
    build_sheet_scene(sheet, sheet_index, options)
}

pub(crate) fn build_scene_with_interaction(
    workbook: &Workbook,
    sheet_index: usize,
    options: &RenderOptions,
) -> Result<(SceneBuild, Vec<CellInteractionRegion>), RenderError> {
    let sheet = workbook
        .sheets
        .get(sheet_index)
        .ok_or(RenderError::SheetIndexOutOfRange {
            requested: sheet_index,
            sheet_count: workbook.sheets.len(),
        })?;
    let mut cells = Vec::new();
    let build = build_sheet_scene_inner_with_interaction(
        sheet,
        sheet_index,
        options,
        None,
        UsedDrawingTerminalColumnPolicy::Indexed,
        AxisEndpointPolicy::PerTrackFixed,
        GridlinePolicy::WorksheetView,
        Some(&mut cells),
    )?;
    Ok((build, cells))
}

/// Lay out one sheet without requiring its owning workbook.
pub fn build_sheet_scene(
    sheet: &Sheet,
    sheet_index: usize,
    options: &RenderOptions,
) -> Result<SceneBuild, RenderError> {
    build_sheet_scene_inner(
        sheet,
        sheet_index,
        options,
        None,
        UsedDrawingTerminalColumnPolicy::Indexed,
        AxisEndpointPolicy::PerTrackFixed,
        GridlinePolicy::WorksheetView,
    )
}

#[cfg(test)]
pub(crate) fn build_single_page_sheet_scene(
    sheet: &Sheet,
    sheet_index: usize,
    options: &RenderOptions,
) -> Result<SceneBuild, RenderError> {
    build_sheet_scene_inner(
        sheet,
        sheet_index,
        options,
        None,
        UsedDrawingTerminalColumnPolicy::CalcOoxmlSinglePage,
        AxisEndpointPolicy::SourceNative,
        GridlinePolicy::WorksheetView,
    )
}

pub(crate) fn build_single_page_sheet_scene_for_print(
    sheet: &Sheet,
    sheet_index: usize,
    options: &RenderOptions,
) -> Result<SceneBuild, RenderError> {
    build_sheet_scene_inner(
        sheet,
        sheet_index,
        options,
        None,
        UsedDrawingTerminalColumnPolicy::CalcOoxmlSinglePage,
        AxisEndpointPolicy::SourceNative,
        GridlinePolicy::CalcSinglePagePrint,
    )
}

#[cfg(test)]
pub(crate) fn build_sheet_scene_with_geometry(
    sheet: &Sheet,
    sheet_index: usize,
    options: &RenderOptions,
    geometry: SheetGeometryOverride<'_>,
) -> Result<SceneBuild, RenderError> {
    build_sheet_scene_inner(
        sheet,
        sheet_index,
        options,
        Some(geometry),
        UsedDrawingTerminalColumnPolicy::Indexed,
        AxisEndpointPolicy::PerTrackFixed,
        GridlinePolicy::WorksheetView,
    )
}

pub(crate) fn build_sheet_scene_with_geometry_for_print(
    sheet: &Sheet,
    sheet_index: usize,
    options: &RenderOptions,
    geometry: SheetGeometryOverride<'_>,
) -> Result<SceneBuild, RenderError> {
    build_sheet_scene_inner(
        sheet,
        sheet_index,
        options,
        Some(geometry),
        UsedDrawingTerminalColumnPolicy::Indexed,
        AxisEndpointPolicy::PerTrackFixed,
        GridlinePolicy::AuthoredPrint,
    )
}

pub(crate) fn build_sheet_scene_for_print(
    sheet: &Sheet,
    sheet_index: usize,
    options: &RenderOptions,
) -> Result<SceneBuild, RenderError> {
    build_sheet_scene_inner(
        sheet,
        sheet_index,
        options,
        None,
        UsedDrawingTerminalColumnPolicy::Indexed,
        AxisEndpointPolicy::PerTrackFixed,
        GridlinePolicy::AuthoredPrint,
    )
}

fn calc_cell_vertical_margin(sheet: &Sheet) -> Fixed {
    let imported_biff = sheet.biff_uses_application_default_column_width()
        || sheet.biff_uses_application_default_row_height()
        || matches!(
            sheet.imported_default_column_axis_measure(),
            Some(ImportedAxisMeasure::CharacterWidth256(_))
        )
        || sheet
            .imported_column_axis_measures()
            .values()
            .any(|measure| matches!(measure, ImportedAxisMeasure::CharacterWidth256(_)));
    if imported_biff {
        CALC_BIFF_CELL_VERTICAL_MARGIN
    } else {
        CALC_CELL_VERTICAL_MARGIN
    }
}

fn build_sheet_scene_inner(
    sheet: &Sheet,
    sheet_index: usize,
    options: &RenderOptions,
    geometry: Option<SheetGeometryOverride<'_>>,
    terminal_column_policy: UsedDrawingTerminalColumnPolicy,
    endpoint_policy: AxisEndpointPolicy,
    gridline_policy: GridlinePolicy,
) -> Result<SceneBuild, RenderError> {
    build_sheet_scene_inner_with_interaction(
        sheet,
        sheet_index,
        options,
        geometry,
        terminal_column_policy,
        endpoint_policy,
        gridline_policy,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_sheet_scene_inner_with_interaction(
    sheet: &Sheet,
    sheet_index: usize,
    options: &RenderOptions,
    geometry: Option<SheetGeometryOverride<'_>>,
    terminal_column_policy: UsedDrawingTerminalColumnPolicy,
    endpoint_policy: AxisEndpointPolicy,
    gridline_policy: GridlinePolicy,
    interaction: Option<&mut Vec<CellInteractionRegion>>,
) -> Result<SceneBuild, RenderError> {
    let mut style_snapshot = RenderStyleSnapshot::new(sheet);
    let used_selection = matches!(options.selection, RenderSelection::Used);
    let used_extent = match options.selection {
        RenderSelection::Used => {
            style_snapshot.capture_sparse_visual_candidates(sheet, options)?;
            Some(render_used_extent(
                sheet,
                &style_snapshot,
                options,
                terminal_column_policy,
                endpoint_policy,
            )?)
        }
        RenderSelection::Range(_) => None,
    };
    let empty_used_selection = used_extent
        .as_ref()
        .is_some_and(|extent| extent.range.is_none());
    let range = match options.selection {
        RenderSelection::Used => used_extent
            .as_ref()
            .and_then(|extent| extent.range)
            .unwrap_or_else(|| RenderRange::new(0, 0, 0, 0)),
        RenderSelection::Range(range) => range,
    }
    .validate()?;

    let rows_considered = u64::from(range.last_row) - u64::from(range.first_row) + 1;
    let columns_considered = u64::from(range.last_col) - u64::from(range.first_col) + 1;
    enforce(LimitKind::Rows, options.limits.max_rows, rows_considered)?;
    enforce(
        LimitKind::Columns,
        options.limits.max_columns,
        columns_considered,
    )?;
    let cells_considered = rows_considered
        .checked_mul(columns_considered)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(LimitKind::Cells, options.limits.max_cells, cells_considered)?;
    style_snapshot.capture_range(sheet, range, options)?;
    enforce(
        LimitKind::ConditionalRules,
        options.limits.max_conditional_rules,
        sheet.conditional_formats().len() as u64,
    )?;
    let calc_line_layout_available = calc_line_layout_available(sheet, options);
    let vertical_margin = calc_cell_vertical_margin(sheet);
    let print_vertical_overflow_available = gridline_policy != GridlinePolicy::WorksheetView
        && !has_conditional_text_layout_overlay(sheet);
    let ods_native_sheet = matches!(
        sheet.imported_default_row_axis_measure(),
        Some(ImportedAxisMeasure::MillimeterHundredths(_))
    );
    let unsupported_shapes = sheet
        .drawing_metadata()
        .iter()
        .filter(|metadata| matches!(metadata.kind, DrawingObjectKind::Shape))
        .count() as u64;
    let drawing_objects = (sheet.images().len() as u64)
        .checked_add(sheet.charts().len() as u64)
        .and_then(|count| count.checked_add(sheet.sparklines().len() as u64))
        .and_then(|count| count.checked_add(unsupported_shapes))
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::DrawingObjects,
        options.limits.max_drawing_objects,
        drawing_objects,
    )?;
    let chart_series = sheet.charts().iter().try_fold(0_u64, |total, chart| {
        total
            .checked_add(chart.series.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)
    })?;
    enforce(
        LimitKind::ChartSeries,
        options.limits.max_chart_series,
        chart_series,
    )?;
    let media_bytes = sheet.images().iter().try_fold(0_u64, |total, image| {
        total
            .checked_add(image.data.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)
    })?;
    enforce(
        LimitKind::MediaBytes,
        options.limits.max_media_bytes,
        media_bytes,
    )?;

    let mut warnings = Warnings::default();
    match sheet.style_fidelity() {
        StyleFidelity::Partial => warnings.add(WarningCode::SourceStylesPartial, None),
        StyleFidelity::Unavailable => {
            warnings.add(WarningCode::SourceStylesUnavailable, None);
        }
        _ => {}
    }
    if empty_used_selection {
        let height = Fixed::from_pixels(1);
        let mut typography = TypographyStats::default();
        let width = empty_used_column_width(
            sheet,
            &style_snapshot,
            options,
            &mut warnings,
            &mut typography,
        )?;
        enforce_dimension(width, options)?;
        enforce_dimension(height, options)?;
        let shapes_without_cell_geometry = sheet
            .drawing_metadata()
            .iter()
            .filter(|metadata| {
                metadata.kind == DrawingObjectKind::Shape && metadata.from_cell.is_none()
            })
            .count() as u64;
        warnings.add_count(
            WarningCode::ShapeAnchorUnavailable,
            shapes_without_cell_geometry,
            None,
        );
        add_empty_absolute_anchor_warnings(sheet, &mut warnings)?;
        if sheet.page_setup().is_some() {
            warnings.add(WarningCode::PaginationDeferred, None);
        }
        return Ok(SceneBuild {
            scene: Scene {
                title: sheet.name.clone(),
                width,
                height,
                background: options.background,
                nodes: Vec::new(),
            },
            report: RenderReport {
                schema_version: 2,
                sheet_index,
                sheet_name: sheet.name.clone(),
                range,
                rows_considered,
                columns_considered,
                cells_considered,
                visible_rows: 0,
                visible_columns: 0,
                rendered_regions: 0,
                hidden_rows_skipped: 0,
                hidden_columns_skipped: 0,
                merged_regions: 0,
                text_bytes: 0,
                glyphs: 0,
                scene_nodes: 0,
                svg_bytes: 0,
                font_pack_sha256: options
                    .font_pack
                    .as_ref()
                    .map(|pack| pack.pack_sha256().to_string()),
                font_faces: typography.finish_font_faces(),
                warnings: warnings.finish(),
            },
        });
    }
    let measured = measure_sheet_axes_inner_with_policy(
        sheet,
        range,
        &style_snapshot,
        options,
        None,
        &mut warnings,
        endpoint_policy,
        0,
    )?;
    let mut row_slots = measured.rows;
    let mut col_slots = measured.columns;
    let source_native_twips = measured.source_native_twips;
    if let Some(geometry) = geometry {
        apply_axis_geometry(&mut row_slots, geometry.rows)?;
        apply_axis_geometry(&mut col_slots, geometry.columns)?;
    }
    let maximum_digit_width = measured.maximum_digit_width;
    let mut typography_stats = measured.typography;
    let mut conditional_evaluations = measured.conditional_evaluations;
    let hidden_rows_skipped = rows_considered.saturating_sub(row_slots.len() as u64);
    let hidden_columns_skipped = columns_considered.saturating_sub(col_slots.len() as u64);
    let mut y = axis_slots_end(&row_slots)?;
    let mut x = axis_slots_end(&col_slots)?;
    let mut drawing_row_slots = None;
    let mut drawing_col_slots = None;
    let mut metafile_grid_row_slots = None;
    let mut metafile_grid_col_slots = None;
    if let Some(source_native_twips) = source_native_twips.as_ref() {
        debug_assert_eq!(endpoint_policy, AxisEndpointPolicy::SourceNative);
        debug_assert!(geometry.is_none());
        x = calc_inclusive_rectangle_extent(x).ok_or(RenderError::CoordinateOverflow)?;
        y = calc_inclusive_rectangle_extent(y).ok_or(RenderError::CoordinateOverflow)?;
        drawing_row_slots = Some(row_slots.clone());
        drawing_col_slots = Some(col_slots.clone());
        if gridline_policy == GridlinePolicy::CalcSinglePagePrint {
            let mut grid_rows = row_slots.clone();
            let mut grid_columns = col_slots.clone();
            apply_calc_single_page_metafile_grid_axis_fit(
                &mut grid_rows,
                &source_native_twips.rows,
            )?;
            apply_calc_single_page_metafile_grid_axis_fit(
                &mut grid_columns,
                &source_native_twips.columns,
            )?;
            metafile_grid_row_slots = Some(grid_rows);
            metafile_grid_col_slots = Some(grid_columns);
        }
        apply_calc_single_page_axis_fit(&mut row_slots, &source_native_twips.rows, y, options)?;
        apply_calc_single_page_axis_fit(&mut col_slots, &source_native_twips.columns, x, options)?;
    }
    let rtl_fit_slack = if source_native_twips.is_some() {
        x.checked_sub(axis_slots_end(&col_slots)?)
            .ok_or(RenderError::CoordinateOverflow)?
    } else {
        Fixed::ZERO
    };
    let viewport_rows = drawing_row_slots.as_deref().unwrap_or(&row_slots);
    let viewport = drawing_layout_viewport(
        sheet,
        range,
        viewport_rows,
        x,
        y,
        maximum_digit_width,
        used_selection,
        options,
        geometry,
        endpoint_policy,
        &mut warnings,
    )?;
    offset_axis_slots(&mut col_slots, viewport.cell.x)?;
    offset_axis_slots(&mut row_slots, viewport.cell.y)?;
    if let Some(slots) = drawing_col_slots.as_mut() {
        offset_axis_slots(slots, viewport.cell.x)?;
    }
    if let Some(slots) = drawing_row_slots.as_mut() {
        offset_axis_slots(slots, viewport.cell.y)?;
    }
    if let Some(slots) = metafile_grid_col_slots.as_mut() {
        offset_axis_slots(slots, viewport.cell.x)?;
    }
    if let Some(slots) = metafile_grid_row_slots.as_mut() {
        offset_axis_slots(slots, viewport.cell.y)?;
    }
    let canvas_width = viewport.sheet.width.max(Fixed::from_pixels(1));
    let canvas_height = viewport.sheet.height.max(Fixed::from_pixels(1));
    enforce_dimension(canvas_width, options)?;
    enforce_dimension(canvas_height, options)?;
    let sheet_right_to_left = sheet.sheet_view().right_to_left;
    let reflection_width = if sheet_right_to_left {
        canvas_width
            .checked_sub(rtl_fit_slack)
            .ok_or(RenderError::CoordinateOverflow)?
    } else {
        canvas_width
    };
    let reflected_col_slots =
        visual_column_slots(&col_slots, reflection_width, sheet_right_to_left)?;
    let visual_col_slots = reflected_col_slots.as_deref().unwrap_or(&col_slots);
    let reflected_metafile_grid_col_slots = if let Some(slots) = &metafile_grid_col_slots {
        visual_column_slots(slots, reflection_width, sheet_right_to_left)?
    } else {
        None
    };
    let visual_metafile_grid_col_slots = reflected_metafile_grid_col_slots
        .as_deref()
        .or(metafile_grid_col_slots.as_deref());

    let mut merge_cover = BTreeMap::<CellCoordinate, usize>::new();
    let mut merge_layouts = Vec::<MergeLayout>::new();

    for &(r0, c0, r1, c1) in sheet.merged_ranges() {
        if used_extent
            .as_ref()
            .is_some_and(|extent| !extent.active_merges.contains(&(r0, c0, r1, c1)))
        {
            continue;
        }
        if r0 > r1 || c0 > c1 {
            warnings.add(
                WarningCode::MergeSkipped,
                Some(CellCoordinate { row: r0, col: c0 }),
            );
            continue;
        }
        let first_row = r0.max(range.first_row);
        let last_row = r1.min(range.last_row);
        let first_col = c0.max(range.first_col);
        let last_col = c1.min(range.last_col);
        if first_row > last_row || first_col > last_col {
            continue;
        }
        let merge_rows: Vec<_> = row_slots
            .iter()
            .copied()
            .filter(|slot| slot.index >= first_row && slot.index <= last_row)
            .collect();
        let merge_cols: Vec<_> = visual_col_slots
            .iter()
            .copied()
            .filter(|slot| slot.index >= first_col && slot.index <= last_col)
            .collect();
        let (Some(first_visible_row), Some(first_visible_col)) =
            (merge_rows.first(), merge_cols.first())
        else {
            continue;
        };
        let covered: Vec<_> = merge_rows
            .iter()
            .flat_map(|row| {
                merge_cols.iter().map(move |col| CellCoordinate {
                    row: row.index,
                    col: col.index,
                })
            })
            .collect();
        if covered.iter().any(|cell| merge_cover.contains_key(cell)) {
            warnings.add(
                WarningCode::MergeSkipped,
                Some(CellCoordinate { row: r0, col: c0 }),
            );
            continue;
        }
        let width = sum_fixed(merge_cols.iter().map(|slot| slot.size))?;
        let height = sum_fixed(merge_rows.iter().map(|slot| slot.size))?;
        let merge_x = merge_cols
            .iter()
            .map(|slot| slot.offset)
            .min()
            .ok_or(RenderError::CoordinateOverflow)?;
        let layout_index = merge_layouts.len();
        let layout = MergeLayout {
            owner: CellCoordinate {
                row: first_visible_row.index,
                col: first_visible_col.index,
            },
            anchor: CellCoordinate { row: r0, col: c0 },
            rect: Rect {
                x: merge_x,
                y: first_visible_row.offset,
                width,
                height,
            },
            has_adjustable_row: merge_rows
                .iter()
                .any(|slot| !effective_row_height_is_manual(sheet, slot.index)),
            calc_wrap_space: if calc_line_layout_available {
                calc_ooxml_merge_wrap_space(sheet, c0, c1, maximum_digit_width, options)?
            } else {
                None
            },
        };
        if layout.anchor != layout.owner {
            warnings.add(
                WarningCode::MergeAnchorOutsideVisibleRange,
                Some(layout.anchor),
            );
        }
        for cell in covered {
            merge_cover.insert(cell, layout_index);
        }
        merge_layouts.push(layout);
    }

    let display_cell_index = SparseDisplayCellIndex::new(sheet);
    let mut display_cells = display_cell_index
        .range((
            range.first_row,
            range.first_col,
            range.last_row,
            range.last_col,
        ))
        .map(|cell| {
            (
                CellCoordinate {
                    row: cell.row,
                    col: cell.col,
                },
                cell,
            )
        })
        .collect::<BTreeMap<_, _>>();
    for merge in &merge_layouts {
        if display_cells.contains_key(&merge.anchor) {
            continue;
        }
        for cell in display_cell_index.range((
            merge.anchor.row,
            merge.anchor.col,
            merge.anchor.row,
            merge.anchor.col,
        )) {
            display_cells.insert(merge.anchor, cell);
        }
    }
    let mut regions = Vec::new();
    for row in &row_slots {
        for col in visual_col_slots {
            let coordinate = CellCoordinate {
                row: row.index,
                col: col.index,
            };
            let (source, rect, is_merged, has_adjustable_row, calc_wrap_space) =
                if let Some(&merge_index) = merge_cover.get(&coordinate) {
                    let merge = &merge_layouts[merge_index];
                    if coordinate != merge.owner {
                        continue;
                    }
                    (
                        merge.anchor,
                        merge.rect,
                        true,
                        merge.has_adjustable_row,
                        merge.calc_wrap_space,
                    )
                } else {
                    (
                        coordinate,
                        Rect {
                            x: col.offset,
                            y: row.offset,
                            width: col.size,
                            height: row.size,
                        },
                        false,
                        !effective_row_height_is_manual(sheet, coordinate.row),
                        if calc_line_layout_available {
                            calc_ooxml_cell_wrap_space(
                                sheet,
                                col.index,
                                maximum_digit_width,
                                options,
                            )?
                        } else {
                            None
                        },
                    )
                };
            let display_cell = display_cells.get(&source);
            let raw_text = display_cell.map_or("", |cell| cell.formatted);
            let (text, replaced) = sanitize_xml_text(raw_text);
            warnings.add_count(
                WarningCode::InvalidXmlCharacterReplaced,
                replaced,
                Some(source),
            );
            let source_rich_text = display_cell.and_then(|cell| cell.rich_text);
            let rich_text = source_rich_text.and_then(|runs| {
                let sanitized = sanitize_rich_text(runs);
                let matches_display = sanitized
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
                    == text;
                if options.font_pack.is_some() && matches_display {
                    Some(sanitized)
                } else {
                    warnings.add(WarningCode::RichTextFlattened, Some(source));
                    None
                }
            });
            if display_cell.is_some_and(|cell| matches!(cell.value, Cell::Formula { .. })) {
                warnings.add(WarningCode::CachedFormulaDisplay, Some(source));
            }
            let style = style_snapshot.owned_style(source);
            collect_style_warnings(
                style.as_ref(),
                source,
                options.font_pack.is_none(),
                &mut warnings,
            );
            let numeric_default =
                display_cell.is_some_and(|cell| cell_defaults_to_right_alignment(cell.value));
            let text_can_overflow =
                display_cell.is_some_and(|cell| cell_allows_horizontal_overflow(cell.value));
            let hyperlink = display_cell
                .and_then(|cell| cell.hyperlink)
                .and_then(|target| {
                    if is_safe_hyperlink(target) {
                        Some(target.to_string())
                    } else {
                        warnings.add(WarningCode::UnsafeHyperlinkDropped, Some(source));
                        None
                    }
                });
            let is_plain_text =
                display_cell.is_some_and(|cell| matches!(cell.value, Cell::Text(_)));
            let line_layout_policy = cell_line_layout_policy(
                sheet,
                source,
                style.as_ref(),
                rich_text.as_deref(),
                CalcLineLayoutEvidence {
                    is_plain_text,
                    has_adjustable_row,
                    wrap_space_available: calc_line_layout_available && calc_wrap_space.is_some(),
                },
                options,
            );
            let line_placement_policy = calc_line_placement_policy(
                sheet,
                source,
                style.as_ref(),
                rich_text.as_deref(),
                is_plain_text,
                options,
            );
            regions.push(Region {
                source,
                rect,
                is_merged,
                line_layout_policy,
                line_placement_policy,
                calc_wrap_space: (line_layout_policy == CellLineLayoutPolicy::CalcEditEngine)
                    .then_some(calc_wrap_space)
                    .flatten(),
                style,
                conditional: ConditionalPaint::default(),
                text,
                rich_text,
                hyperlink,
                numeric_default,
                text_can_overflow,
                fixed_height_row: !has_adjustable_row,
                ods_fixed_height_row: ods_native_sheet && !has_adjustable_row,
                print_vertical_overflow: print_vertical_overflow_available && has_adjustable_row,
                vertical_margin,
            });
        }
    }

    if let Some(cells) = interaction {
        enforce(
            LimitKind::Cells,
            options.limits.max_cells,
            regions.len() as u64,
        )?;
        for region in &regions {
            let rect = region.rect;
            if rect.width <= Fixed::ZERO || rect.height <= Fixed::ZERO {
                continue;
            }
            let right = rect
                .x
                .checked_add(rect.width)
                .ok_or(RenderError::CoordinateOverflow)?;
            let bottom = rect
                .y
                .checked_add(rect.height)
                .ok_or(RenderError::CoordinateOverflow)?;
            if rect.x < Fixed::ZERO
                || rect.y < Fixed::ZERO
                || right > canvas_width
                || bottom > canvas_height
            {
                return Err(RenderError::CoordinateOverflow);
            }
            cells.push(CellInteractionRegion {
                source: region.source,
                rect,
            });
        }
    }

    apply_numeric_overflow(
        &mut regions,
        &display_cells,
        options,
        sheet.sheet_view().right_to_left,
        &mut typography_stats,
        &mut warnings,
    )?;
    let mut text_bytes = 0_u64;
    let mut glyphs = 0_u64;
    for region in &regions {
        text_bytes = text_bytes
            .checked_add(region.text.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(
            LimitKind::TextBytes,
            options.limits.max_text_bytes,
            text_bytes,
        )?;
        glyphs = glyphs
            .checked_add(region.text.chars().count() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(LimitKind::Glyphs, options.limits.max_glyphs, glyphs)?;
        if !region.text.is_empty() && options.font_pack.is_none() {
            warnings.add(WarningCode::ApproximateTextMetrics, Some(region.source));
        }
    }

    let mut nodes = Vec::new();
    let row_regions = regions_by_visual_row(&regions)?;
    let show_gridlines = options.gridlines
        && (gridline_policy != GridlinePolicy::WorksheetView || !sheet.sheet_view().hide_gridlines);
    let _ = resolve_conditional_paints(
        sheet,
        &display_cells,
        &mut regions,
        options,
        &mut warnings,
        &mut conditional_evaluations,
        false,
    )?;
    let mut suppresses_gridlines = Vec::with_capacity(regions.len());
    for region in &regions {
        let fill = resolve_fill(region.style.as_ref(), region.source, &mut warnings);
        suppresses_gridlines.push(fill.is_some());
        if fill.is_some() {
            push_node(
                &mut nodes,
                SceneNode::Rect(RectNode {
                    rect: region.rect,
                    fill,
                    stroke: None,
                    stroke_width: Fixed::ZERO,
                }),
                options,
            )?;
        }
    }
    let composed_edges = compose_edges(
        &regions,
        &suppresses_gridlines,
        show_gridlines,
        gridline_policy,
        EdgeCompositionMode::Combined,
        options,
    )?;
    let calc_metafile_grid_composed_edges = (gridline_policy
        == GridlinePolicy::CalcSinglePagePrint)
        .then(|| {
            compose_edges(
                &regions,
                &suppresses_gridlines,
                show_gridlines,
                gridline_policy,
                EdgeCompositionMode::CalcMetafileGrid,
                options,
            )
        })
        .transpose()?;
    let metafile_grid_edges = match (
        metafile_grid_row_slots.as_deref(),
        visual_metafile_grid_col_slots,
    ) {
        (Some(grid_rows), Some(grid_columns)) => Some(remap_calc_metafile_grid_edges(
            calc_metafile_grid_composed_edges
                .as_deref()
                .unwrap_or(&composed_edges),
            &row_slots,
            visual_col_slots,
            grid_rows,
            grid_columns,
        )?),
        (None, None) => None,
        _ => return Err(RenderError::CoordinateOverflow),
    };
    let scene_bounds = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: canvas_width,
        height: canvas_height,
    };
    let cell_output_left = col_slots
        .first()
        .map_or(viewport.cell.x, |slot| slot.offset);
    let cell_output_right = axis_slots_end(&col_slots)?;
    let cell_output_bounds = Rect {
        x: cell_output_left,
        y: viewport.cell.y,
        width: cell_output_right
            .checked_sub(cell_output_left)
            .ok_or(RenderError::CoordinateOverflow)?,
        height: viewport.cell.height,
    };
    if gridline_policy == GridlinePolicy::WorksheetView {
        push_composed_edges(
            &mut nodes,
            &composed_edges,
            EdgeClaimKind::Gridline,
            gridline_policy,
            viewport.cell,
            scene_bounds,
            sheet_right_to_left,
            options,
        )?;
    }
    for region in &regions {
        if let Some(bar) = region.conditional.data_bar {
            push_data_bar(&mut nodes, region.rect, bar, options)?;
        }
    }
    for (region_index, region) in regions.iter().enumerate() {
        if region.text.is_empty() {
            continue;
        }
        let style = text_style(region, options);
        let clip_bounds =
            text_clip_bounds(region_index, &regions, &row_regions, &style, scene_bounds)?;
        let layout_bounds =
            calc_cell_text_layout_bounds(region.rect, style.baseline, region.vertical_margin)?;
        let node = match options.font_pack.as_ref() {
            Some(font_pack) => SceneNode::GlyphRun(build_glyph_run(
                font_pack,
                region,
                layout_bounds,
                clip_bounds,
                Some(cell_output_bounds),
                &style,
                sheet_right_to_left,
                CALC_WORKSHEET_KERNING,
                gridline_policy == GridlinePolicy::CalcSinglePagePrint,
                options,
                &mut typography_stats,
                &mut warnings,
            )?),
            None => SceneNode::Text(TextNode {
                text: region.text.clone(),
                bounds: layout_bounds,
                clip_bounds,
                horizontal_padding: options.horizontal_padding,
                style,
                hyperlink: region.hyperlink.clone(),
            }),
        };
        push_node(&mut nodes, node, options)?;
    }
    push_composed_edges(
        &mut nodes,
        &composed_edges,
        EdgeClaimKind::Explicit,
        gridline_policy,
        viewport.cell,
        scene_bounds,
        sheet_right_to_left,
        options,
    )?;
    if show_gridlines && gridline_policy != GridlinePolicy::WorksheetView {
        // Calc paints print gridlines above cell content and borders but below
        // anchored drawing objects such as images and charts.
        if gridline_policy != GridlinePolicy::CalcSinglePagePrint || !sheet_right_to_left {
            push_composed_edges(
                &mut nodes,
                metafile_grid_edges.as_deref().unwrap_or(&composed_edges),
                EdgeClaimKind::Gridline,
                gridline_policy,
                viewport.cell,
                scene_bounds,
                sheet_right_to_left,
                options,
            )?;
        }
        push_print_gridline_leading_frame(
            &mut nodes,
            viewport.cell,
            scene_bounds,
            sheet_right_to_left,
            options,
        )?;
    }
    push_drawing_placeholders(
        &mut nodes,
        sheet,
        drawing_row_slots.as_deref().unwrap_or(&row_slots),
        drawing_col_slots.as_deref().unwrap_or(&col_slots),
        geometry,
        viewport.cell,
        viewport.sheet,
        canvas_width,
        canvas_height,
        sheet_right_to_left,
        gridline_policy == GridlinePolicy::CalcSinglePagePrint,
        &mut text_bytes,
        &mut glyphs,
        &mut typography_stats,
        options,
        &mut warnings,
    )?;
    if sheet.page_setup().is_some() {
        warnings.add(WarningCode::PaginationDeferred, None);
    }

    let report = RenderReport {
        schema_version: 2,
        sheet_index,
        sheet_name: sheet.name.clone(),
        range,
        rows_considered,
        columns_considered,
        cells_considered,
        visible_rows: row_slots.len() as u64,
        visible_columns: col_slots.len() as u64,
        rendered_regions: regions.len() as u64,
        hidden_rows_skipped,
        hidden_columns_skipped,
        merged_regions: merge_layouts.len() as u64,
        text_bytes,
        glyphs,
        scene_nodes: scene_node_count(&nodes)?,
        svg_bytes: 0,
        font_pack_sha256: options
            .font_pack
            .as_ref()
            .map(|pack| pack.pack_sha256().to_string()),
        font_faces: typography_stats.finish_font_faces(),
        warnings: warnings.finish(),
    };
    Ok(SceneBuild {
        scene: Scene {
            title: sheet.name.clone(),
            width: canvas_width,
            height: canvas_height,
            background: options.background,
            nodes,
        },
        report,
    })
}

#[derive(Default)]
struct UsedRenderExtent {
    range: Option<RenderRange>,
    active_merges: BTreeSet<(u32, u16, u32, u16)>,
}

/// Resolve Calc-compatible visual content for [`RenderSelection::Used`].
///
/// Value cells and format-only blanks with paint establish the cell extent. A
/// merge expands that extent only when it covers one of those retained cells;
/// detached empty merges and font/alignment/number-format-only blanks therefore
/// cannot create giant canvases. Public image, chart, and sparkline anchors are
/// included independently because they carry visible sheet content.
fn render_used_extent(
    sheet: &Sheet,
    style_snapshot: &RenderStyleSnapshot,
    options: &RenderOptions,
    terminal_column_policy: UsedDrawingTerminalColumnPolicy,
    endpoint_policy: AxisEndpointPolicy,
) -> Result<UsedRenderExtent, RenderError> {
    let mut extent = UsedRenderExtent::default();
    let mut retained_cells = BTreeMap::<u32, BTreeSet<u16>>::new();
    let metadata_index = DrawingMetadataIndex::new(sheet);
    let maximum_digit_width =
        drawing_extent_maximum_digit_width(sheet, style_snapshot, options, terminal_column_policy)?;
    let single_page_column_bounds =
        if terminal_column_policy == UsedDrawingTerminalColumnPolicy::CalcOoxmlSinglePage {
            maximum_digit_width.and_then(|maximum_digit_width| {
                calc_ooxml_single_page_column_bounds(sheet, maximum_digit_width, options)
            })
        } else {
            None
        };

    for (row, col, _) in sheet.cells() {
        include_render_coordinate(&mut extent.range, row, col);
        retained_cells.entry(row).or_default().insert(col);
    }
    for &(row, col) in sheet.blank_cell_styles().keys() {
        if style_snapshot
            .style(CellCoordinate { row, col })
            .is_some_and(cell_style_has_visible_blank_paint)
        {
            include_render_coordinate(&mut extent.range, row, col);
            retained_cells.entry(row).or_default().insert(col);
        }
    }
    for &(r0, c0, r1, c1) in sheet.merged_ranges() {
        if r0 > r1 || c0 > c1 {
            continue;
        }
        let intersects_retained_cell = retained_cells
            .range(r0..=r1)
            .any(|(_, columns)| columns.range(c0..=c1).next().is_some());
        if intersects_retained_cell {
            include_render_coordinate(&mut extent.range, r0, c0);
            include_render_coordinate(&mut extent.range, r1, c1);
            extent.active_merges.insert((r0, c0, r1, c1));
        }
    }
    for (index, image) in sheet.images().iter().enumerate() {
        let metadata = metadata_index.get(DrawingObjectKind::Image, index);
        if is_sheet_absolute_metadata(metadata) {
            if absolute_drawing_paint_bounds(DrawingObjectKind::Image, metadata)?
                .is_some_and(rect_intersects_positive_sheet)
            {
                include_render_coordinate(&mut extent.range, 0, 0);
            }
            continue;
        }
        include_render_coordinate(
            &mut extent.range,
            image.from.0.min(MAX_WORKSHEET_ROW),
            image.from.1.min(MAX_WORKSHEET_COLUMN),
        );
        let to = image.to.unwrap_or((
            image.from.0.saturating_add(10),
            image.from.1.saturating_add(4),
        ));
        let to = drawing_used_to(
            sheet,
            image.from,
            to,
            metadata,
            maximum_digit_width,
            single_page_column_bounds.as_ref(),
            options,
            endpoint_policy,
        )?;
        include_render_coordinate(
            &mut extent.range,
            to.0.min(MAX_WORKSHEET_ROW),
            to.1.min(MAX_WORKSHEET_COLUMN),
        );
    }
    for (index, chart) in sheet.charts().iter().enumerate() {
        let metadata = metadata_index.get(DrawingObjectKind::Chart, index);
        if is_sheet_absolute_metadata(metadata) {
            if absolute_drawing_paint_bounds(DrawingObjectKind::Chart, metadata)?
                .is_some_and(rect_intersects_positive_sheet)
            {
                include_render_coordinate(&mut extent.range, 0, 0);
            }
            continue;
        }
        include_render_coordinate(
            &mut extent.range,
            chart.from.0.min(MAX_WORKSHEET_ROW),
            chart.from.1.min(MAX_WORKSHEET_COLUMN),
        );
        let to = drawing_used_to(
            sheet,
            chart.from,
            chart.to,
            metadata,
            maximum_digit_width,
            single_page_column_bounds.as_ref(),
            options,
            endpoint_policy,
        )?;
        include_render_coordinate(
            &mut extent.range,
            to.0.min(MAX_WORKSHEET_ROW),
            to.1.min(MAX_WORKSHEET_COLUMN),
        );
    }
    for metadata in sheet
        .drawing_metadata()
        .iter()
        .filter(|metadata| metadata.kind == DrawingObjectKind::Shape)
    {
        let Some(from) = metadata.from_cell else {
            continue;
        };
        include_render_coordinate(
            &mut extent.range,
            from.0.min(MAX_WORKSHEET_ROW),
            from.1.min(MAX_WORKSHEET_COLUMN),
        );
        let to = drawing_used_to(
            sheet,
            from,
            metadata.to_cell.unwrap_or(from),
            Some(metadata),
            maximum_digit_width,
            single_page_column_bounds.as_ref(),
            options,
            endpoint_policy,
        )?;
        include_render_coordinate(
            &mut extent.range,
            to.0.min(MAX_WORKSHEET_ROW),
            to.1.min(MAX_WORKSHEET_COLUMN),
        );
    }
    for sparkline in sheet.sparklines() {
        include_render_coordinate(
            &mut extent.range,
            sparkline.location.0.min(MAX_WORKSHEET_ROW),
            sparkline.location.1.min(MAX_WORKSHEET_COLUMN),
        );
    }
    Ok(extent)
}

fn add_empty_absolute_anchor_warnings(
    sheet: &Sheet,
    warnings: &mut Warnings,
) -> Result<(), RenderError> {
    let metadata_index = DrawingMetadataIndex::new(sheet);
    for (kind, anchors) in [
        (
            DrawingObjectKind::Image,
            sheet
                .images()
                .iter()
                .map(|image| image.from)
                .collect::<Vec<_>>(),
        ),
        (
            DrawingObjectKind::Chart,
            sheet
                .charts()
                .iter()
                .map(|chart| chart.from)
                .collect::<Vec<_>>(),
        ),
    ] {
        for (object_index, anchor) in anchors.into_iter().enumerate() {
            let metadata = metadata_index.get(kind, object_index);
            if is_sheet_absolute_metadata(metadata) && absolute_drawing_bounds(metadata)?.is_none()
            {
                warnings.add(
                    WarningCode::DrawingAnchorUnavailable,
                    Some(CellCoordinate {
                        row: anchor.0,
                        col: anchor.1,
                    }),
                );
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn drawing_used_to(
    sheet: &Sheet,
    from: (u32, u16),
    to: (u32, u16),
    metadata: Option<&DrawingMetadata>,
    maximum_digit_width: Option<Fixed>,
    single_page_column_bounds: Option<&CalcOoxmlSinglePageColumnBounds>,
    options: &RenderOptions,
    endpoint_policy: AxisEndpointPolicy,
) -> Result<(u32, u16), RenderError> {
    if let Some((width, height)) = metadata
        .filter(|metadata| {
            metadata.behavior != DrawingAnchorBehavior::MoveAndSize && metadata.from_cell.is_some()
        })
        .and_then(|metadata| metadata.absolute_size_emu)
    {
        let maximum_digit_width = maximum_digit_width.ok_or(RenderError::CoordinateOverflow)?;
        let (from_column_offset, from_row_offset) = metadata
            .and_then(|metadata| metadata.from_offset_emu)
            .unwrap_or((0, 0));
        return Ok((
            fixed_size_used_row(
                sheet,
                from.0,
                from_row_offset,
                height,
                maximum_digit_width,
                options,
                endpoint_policy,
            )?,
            fixed_size_used_column(
                sheet,
                from.1,
                from_column_offset,
                width,
                maximum_digit_width,
                options,
                endpoint_policy,
            )?,
        ));
    }
    let Some((column_offset, row_offset)) = metadata.and_then(|metadata| metadata.to_offset_emu)
    else {
        return Ok(to);
    };
    let terminal_column = if column_offset == 0 {
        single_page_column_bounds
            .and_then(|bounds| bounds.terminal_column(from.1, to.1))
            .unwrap_or_else(|| terminal_used_column(sheet, from.1, to.1, column_offset, options))
    } else {
        terminal_used_column(sheet, from.1, to.1, column_offset, options)
    };
    Ok((
        terminal_used_row(sheet, from.0, to.0, row_offset, options),
        terminal_column,
    ))
}

fn drawing_extent_maximum_digit_width(
    sheet: &Sheet,
    style_snapshot: &RenderStyleSnapshot,
    options: &RenderOptions,
    terminal_column_policy: UsedDrawingTerminalColumnPolicy,
) -> Result<Option<Fixed>, RenderError> {
    let requires_single_page_terminal_width = terminal_column_policy
        == UsedDrawingTerminalColumnPolicy::CalcOoxmlSinglePage
        && options.font_pack.is_some()
        && sheet.implicit_ooxml_column_width() == Some(None)
        && sheet.xlsb_default_column_width().is_none()
        && sheet.xlsb_column_widths_256().is_empty();
    if !requires_single_page_terminal_width
        && !sheet.drawing_metadata().iter().any(|metadata| {
            metadata.behavior != DrawingAnchorBehavior::MoveAndSize
                && metadata.from_cell.is_some()
                && metadata.absolute_size_emu.is_some()
        })
    {
        return Ok(None);
    }
    let mut warnings = Warnings::default();
    let mut typography = TypographyStats::default();
    maximum_digit_width(style_snapshot, options, &mut warnings, &mut typography).map(Some)
}

fn fixed_size_used_column(
    sheet: &Sheet,
    from: u16,
    from_offset_emu: i64,
    width_emu: u64,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    endpoint_policy: AxisEndpointPolicy,
) -> Result<u16, RenderError> {
    let target = emu_to_fixed(from_offset_emu)?
        .checked_add(emu_size_to_fixed(width_emu)?)
        .ok_or(RenderError::CoordinateOverflow)?;
    if target <= Fixed::ZERO {
        return Ok(from);
    }
    let mut boundary = Fixed::ZERO;
    let mut candidate = from;
    let mut last = from;
    let mut warnings = Warnings::default();
    let mut native_cursor = if endpoint_policy == AxisEndpointPolicy::SourceNative {
        Some(SourceAxisCursor::new(source_native_column_prefix(
            sheet,
            from,
            maximum_digit_width,
            options,
            &mut warnings,
        )?)?)
    } else {
        None
    };
    while let Some(column) = next_visible_column(sheet, candidate, options) {
        enforce(
            LimitKind::Columns,
            options.limits.max_columns,
            u64::from(column) - u64::from(from) + 1,
        )?;
        if boundary >= target {
            break;
        }
        last = column;
        let fallback = column_width(sheet, column, maximum_digit_width, options, &mut warnings);
        boundary = if let Some(cursor) = native_cursor.as_mut() {
            let contribution = source_axis_contribution_twips(
                imported_column_axis_measure(sheet, column, options),
                fallback,
                maximum_digit_width,
            )
            .ok_or(RenderError::CoordinateOverflow)?;
            cursor.advance(contribution)?.2
        } else {
            boundary
                .checked_add(fallback)
                .ok_or(RenderError::CoordinateOverflow)?
        };
        if boundary >= target || column == MAX_WORKSHEET_COLUMN {
            break;
        }
        candidate = column + 1;
    }
    Ok(last)
}

fn fixed_size_used_row(
    sheet: &Sheet,
    from: u32,
    from_offset_emu: i64,
    height_emu: u64,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    endpoint_policy: AxisEndpointPolicy,
) -> Result<u32, RenderError> {
    let target = emu_to_fixed(from_offset_emu)?
        .checked_add(emu_size_to_fixed(height_emu)?)
        .ok_or(RenderError::CoordinateOverflow)?;
    if target <= Fixed::ZERO {
        return Ok(from);
    }
    let mut boundary = Fixed::ZERO;
    let mut candidate = from;
    let mut last = from;
    let mut warnings = Warnings::default();
    let mut native_cursor = if endpoint_policy == AxisEndpointPolicy::SourceNative {
        Some(SourceAxisCursor::new(source_native_row_prefix(
            sheet,
            from,
            maximum_digit_width,
            options,
            &mut warnings,
        )?)?)
    } else {
        None
    };
    while let Some(row) = next_visible_row(sheet, candidate, options) {
        enforce(
            LimitKind::Rows,
            options.limits.max_rows,
            u64::from(row) - u64::from(from) + 1,
        )?;
        if boundary >= target {
            break;
        }
        last = row;
        let fallback = row_height(sheet, row, options, &mut warnings);
        boundary = if let Some(cursor) = native_cursor.as_mut() {
            let contribution = source_axis_contribution_twips(
                imported_row_axis_measure(sheet, row, options),
                fallback,
                maximum_digit_width,
            )
            .ok_or(RenderError::CoordinateOverflow)?;
            cursor.advance(contribution)?.2
        } else {
            boundary
                .checked_add(fallback)
                .ok_or(RenderError::CoordinateOverflow)?
        };
        if boundary >= target || row == MAX_WORKSHEET_ROW {
            break;
        }
        candidate = row + 1;
    }
    Ok(last)
}

fn calc_ooxml_single_page_column_bounds(
    sheet: &Sheet,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
) -> Option<CalcOoxmlSinglePageColumnBounds> {
    if options.font_pack.is_none()
        || sheet.implicit_ooxml_column_width() != Some(None)
        || sheet.xlsb_default_column_width().is_some()
        || !sheet.xlsb_column_widths_256().is_empty()
    {
        return None;
    }
    let Ok(maximum_digit_width_raw) = u64::try_from(maximum_digit_width.raw()) else {
        return None;
    };
    let scaled_digit_width = maximum_digit_width_raw.checked_mul(15)?;
    let digit_twips = scaled_digit_width / 1_024;
    if digit_twips == 0 {
        return None;
    }

    // Marker coordinates are absolute from column A. Build one schema-bounded
    // prefix table per render, then resolve every drawing endpoint by binary
    // search instead of rescanning the grid for each retained anchor.
    let mut cumulative = 0_u64;
    let mut cumulative_twips =
        Vec::with_capacity(usize::from(MAX_WORKSHEET_COLUMN).saturating_add(1));
    for column in 0..=MAX_WORKSHEET_COLUMN {
        if options.include_hidden || !sheet.hidden_columns().contains(&column) {
            cumulative = cumulative.checked_add(calc_ooxml_wrap_column_twips(
                sheet,
                column,
                digit_twips,
            )?)?;
        }
        cumulative_twips.push(cumulative);
    }
    Some(CalcOoxmlSinglePageColumnBounds { cumulative_twips })
}

impl CalcOoxmlSinglePageColumnBounds {
    fn terminal_column(&self, from: u16, to: u16) -> Option<u16> {
        let to = usize::from(to);
        if to <= usize::from(from) || to > self.cumulative_twips.len() {
            return None;
        }
        let marker_boundary_twips = self.cumulative_twips[to.checked_sub(1)?];
        let marker_boundary_mm100 = round_unsigned_ratio(marker_boundary_twips, 127, 72)?;
        let closed_boundary_mm100 = marker_boundary_mm100.checked_sub(1)?;
        let closed_boundary_twips = round_unsigned_ratio(closed_boundary_mm100, 72, 127)?;
        let terminal = self
            .cumulative_twips
            .partition_point(|boundary| *boundary <= closed_boundary_twips);
        if terminal >= self.cumulative_twips.len() {
            return None;
        }
        u16::try_from(terminal).ok().map(|column| column.max(from))
    }
}

fn round_unsigned_ratio(value: u64, numerator: u64, denominator: u64) -> Option<u64> {
    let rounded = u128::from(value)
        .checked_mul(u128::from(numerator))?
        .checked_add(u128::from(denominator / 2))?
        .checked_div(u128::from(denominator))?;
    u64::try_from(rounded).ok()
}

fn calc_wrap_space_from_column_twips(
    widths: impl IntoIterator<Item = u64>,
) -> Result<Option<CalcWrapSpace>, RenderError> {
    let mut document_pixels = 0_u64;
    let mut columns = 0_u64;
    for twips in widths {
        if twips == 0 {
            return Ok(None);
        }
        let Some(column_pixels) = twips
            .checked_mul(CALC_OPTIMAL_HEIGHT_SAMPLE_PIXELS)
            .and_then(|value| value.checked_div(CALC_OPTIMAL_HEIGHT_SAMPLE_TWIPS))
        else {
            return Ok(None);
        };
        if column_pixels == 0 {
            return Ok(None);
        }
        let Some(next_pixels) = document_pixels.checked_add(column_pixels) else {
            return Ok(None);
        };
        document_pixels = next_pixels;
        let Some(next_columns) = columns.checked_add(1) else {
            return Ok(None);
        };
        columns = next_columns;
    }
    if columns == 0 {
        return Ok(None);
    }
    let Some(margin_pixels) = CALC_CELL_HORIZONTAL_MARGIN_TWIPS
        .checked_mul(CALC_OPTIMAL_HEIGHT_SAMPLE_PIXELS)
        .and_then(|value| value.checked_div(CALC_OPTIMAL_HEIGHT_SAMPLE_TWIPS))
    else {
        return Ok(None);
    };
    let Some(inset_pixels) = margin_pixels
        .checked_mul(2)
        .and_then(|value| value.checked_add(CALC_CELL_GRID_PIXELS))
    else {
        return Ok(None);
    };
    let Some(paper_pixels) = document_pixels.checked_sub(inset_pixels) else {
        return Ok(None);
    };
    if paper_pixels == 0 {
        return Ok(None);
    }
    let Some(paper_width_mm100) =
        round_unsigned_ratio(paper_pixels, MM100_PER_INCH, CALC_DEVICE_DPI)
    else {
        return Ok(None);
    };
    Ok(
        (paper_width_mm100 > 0 && i64::try_from(paper_width_mm100).is_ok())
            .then_some(CalcWrapSpace { paper_width_mm100 }),
    )
}

fn calc_ooxml_wrap_digit_twips(maximum_digit_width: Fixed) -> Option<u64> {
    let raw = u64::try_from(maximum_digit_width.raw()).ok()?;
    raw.checked_mul(u64::try_from(TWIPS_PER_CSS_PIXEL).ok()?)
        .and_then(|value| value.checked_div(FIXED_UNITS_PER_PIXEL as u64))
        .filter(|width| *width > 0)
}

fn calc_ooxml_wrap_column_twips(sheet: &Sheet, column: u16, digit_twips: u64) -> Option<u64> {
    if sheet.physical_column_widths().contains_key(&column)
        || sheet.xlsb_column_widths_256().contains_key(&column)
        || sheet.xlsb_default_column_width().is_some()
        || sheet.default_column_width().is_some()
        || sheet.implicit_ooxml_column_width() != Some(None)
    {
        return None;
    }
    if let Some(characters) = sheet.column_widths().get(&column).copied() {
        if !characters.is_finite() || characters <= 0.0 {
            return None;
        }
        let character_twips = (f64::from(characters) * digit_twips as f64).round();
        if !character_twips.is_finite()
            || character_twips <= 0.0
            || character_twips > u64::MAX as f64
        {
            return None;
        }
        Some(character_twips as u64)
    } else if sheet.ooxml_uses_defaulted_base_column_width() {
        digit_twips.checked_mul(8).and_then(|value| {
            u64::try_from(TWIPS_PER_CSS_PIXEL)
                .ok()?
                .checked_mul(5)
                .and_then(|padding| value.checked_add(padding))
        })
    } else {
        digit_twips
            .checked_mul(17)
            .and_then(|value| value.checked_add(1))
            .map(|value| value / 2)
            .filter(|width| *width > 0)
    }
}

fn calc_ooxml_wrap_space(
    sheet: &Sheet,
    columns: impl IntoIterator<Item = u16>,
    maximum_digit_width: Fixed,
) -> Result<Option<CalcWrapSpace>, RenderError> {
    let Some(digit_twips) = calc_ooxml_wrap_digit_twips(maximum_digit_width) else {
        return Ok(None);
    };
    let widths = columns
        .into_iter()
        .map(|column| calc_ooxml_wrap_column_twips(sheet, column, digit_twips))
        .collect::<Option<Vec<_>>>();
    let Some(widths) = widths else {
        return Ok(None);
    };
    calc_wrap_space_from_column_twips(widths)
}

fn calc_ooxml_cell_wrap_space(
    sheet: &Sheet,
    column: u16,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
) -> Result<Option<CalcWrapSpace>, RenderError> {
    if options.include_hidden && sheet.hidden_columns().contains(&column) {
        return Ok(None);
    }
    calc_ooxml_wrap_space(sheet, [column], maximum_digit_width)
}

fn calc_ooxml_merge_wrap_space(
    sheet: &Sheet,
    first: u16,
    last: u16,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
) -> Result<Option<CalcWrapSpace>, RenderError> {
    if first > last || first > MAX_WORKSHEET_COLUMN {
        return Ok(None);
    }
    let last = last.min(MAX_WORKSHEET_COLUMN);
    if sheet.hidden_columns().contains(&first)
        || (options.include_hidden && sheet.hidden_columns().range(first..=last).next().is_some())
    {
        // Calc's optimal-height path uses the hidden anchor's original
        // width, while cell painting uses its current zero width. The exact
        // shared wrapper cannot represent that split. Likewise, include_hidden
        // is a renderer-only view that has no Calc-equivalent paper width.
        return Ok(None);
    }
    calc_ooxml_wrap_space(
        sheet,
        (first..=last).filter(|column| !sheet.hidden_columns().contains(column)),
        maximum_digit_width,
    )
}

impl CalcWrapSpace {
    /// Return an opaque width for the shared bounded wrapper. Both the paper
    /// and candidate widths use Map100thMM raw units at this API seam.
    fn line_width(self) -> Result<Fixed, RenderError> {
        i64::try_from(self.paper_width_mm100)
            .map(Fixed::from_raw)
            .map_err(|_| RenderError::CoordinateOverflow)
    }

    fn physical_width_mm100(width: Fixed) -> Result<Fixed, RenderError> {
        let raw = u64::try_from(width.raw()).map_err(|_| RenderError::CoordinateOverflow)?;
        let denominator = CALC_DEVICE_DPI
            .checked_mul(FIXED_UNITS_PER_PIXEL as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        let width_mm100 = round_unsigned_ratio(raw, MM100_PER_INCH, denominator)
            .ok_or(RenderError::CoordinateOverflow)?;
        i64::try_from(width_mm100)
            .map(Fixed::from_raw)
            .map_err(|_| RenderError::CoordinateOverflow)
    }

    fn measure_physical_width(width: Fixed, font_size: Fixed) -> Result<Fixed, RenderError> {
        if font_size.raw() <= 0 {
            return Err(RenderError::Typography {
                reason: "invalid_calc_wrap_font_size",
            });
        }
        let font_raw =
            u64::try_from(font_size.raw()).map_err(|_| RenderError::CoordinateOverflow)?;
        let device_em_pixels = round_unsigned_ratio(font_raw, 1, FIXED_UNITS_PER_PIXEL as u64)
            .filter(|pixels| *pixels > 0)
            .ok_or(RenderError::CoordinateOverflow)?;
        let device_em_raw = device_em_pixels
            .checked_mul(FIXED_UNITS_PER_PIXEL as u64)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or(RenderError::CoordinateOverflow)?;
        let device_width = scale_ratio(width, device_em_raw, font_size.raw())?;
        Self::physical_width_mm100(device_width)
    }
}

fn round_signed_ratio(value: i128, numerator: i128, denominator: i128) -> Result<i64, RenderError> {
    if numerator < 0 || denominator <= 0 {
        return Err(RenderError::CoordinateOverflow);
    }
    let scaled = value
        .checked_mul(numerator)
        .ok_or(RenderError::CoordinateOverflow)?;
    let magnitude = scaled.unsigned_abs();
    let denominator = denominator as u128;
    let rounded = magnitude
        .checked_add(denominator / 2)
        .and_then(|value| value.checked_div(denominator))
        .and_then(|value| i128::try_from(value).ok())
        .and_then(|value| {
            if scaled < 0 {
                value.checked_neg()
            } else {
                Some(value)
            }
        })
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(rounded)
}

fn terminal_used_column(
    sheet: &Sheet,
    from: u16,
    to: u16,
    offset_emu: i64,
    options: &RenderOptions,
) -> u16 {
    if offset_emu > 0 {
        return next_visible_column(sheet, to.max(from), options).unwrap_or_else(|| {
            previous_visible_column(sheet, MAX_WORKSHEET_COLUMN, from, options).unwrap_or(from)
        });
    }
    if to <= from {
        return from;
    }
    previous_visible_column(sheet, to - 1, from, options).unwrap_or(from)
}

fn terminal_used_row(
    sheet: &Sheet,
    from: u32,
    to: u32,
    offset_emu: i64,
    options: &RenderOptions,
) -> u32 {
    if offset_emu > 0 {
        return next_visible_row(sheet, to.max(from), options).unwrap_or_else(|| {
            previous_visible_row(sheet, MAX_WORKSHEET_ROW, from, options).unwrap_or(from)
        });
    }
    if to <= from {
        return from;
    }
    previous_visible_row(sheet, to - 1, from, options).unwrap_or(from)
}

fn next_visible_column(sheet: &Sheet, start: u16, options: &RenderOptions) -> Option<u16> {
    if options.include_hidden {
        return Some(start);
    }
    let mut candidate = start;
    for &hidden in sheet.hidden_columns().range(start..) {
        if hidden > candidate {
            break;
        }
        if hidden == candidate {
            if candidate == MAX_WORKSHEET_COLUMN {
                return None;
            }
            candidate += 1;
        }
    }
    Some(candidate)
}

fn previous_visible_column(
    sheet: &Sheet,
    start: u16,
    minimum: u16,
    options: &RenderOptions,
) -> Option<u16> {
    if start < minimum {
        return None;
    }
    if options.include_hidden {
        return Some(start);
    }
    let mut candidate = start;
    for &hidden in sheet.hidden_columns().range(minimum..=start).rev() {
        if hidden < candidate {
            break;
        }
        if hidden == candidate {
            if candidate == minimum {
                return None;
            }
            candidate -= 1;
        }
    }
    Some(candidate)
}

fn next_visible_row(sheet: &Sheet, start: u32, options: &RenderOptions) -> Option<u32> {
    if options.include_hidden {
        return Some(start);
    }
    if let Some(visible_rows) = sheet.default_hidden_row_exceptions() {
        return visible_rows
            .range(start..)
            .copied()
            .find(|row| !sheet.hidden_rows().contains(row));
    }
    let mut candidate = start;
    for &hidden in sheet.hidden_rows().range(start..) {
        if hidden > candidate {
            break;
        }
        if hidden == candidate {
            if candidate == MAX_WORKSHEET_ROW {
                return None;
            }
            candidate += 1;
        }
    }
    Some(candidate)
}

fn previous_visible_row(
    sheet: &Sheet,
    start: u32,
    minimum: u32,
    options: &RenderOptions,
) -> Option<u32> {
    if start < minimum {
        return None;
    }
    if options.include_hidden {
        return Some(start);
    }
    if let Some(visible_rows) = sheet.default_hidden_row_exceptions() {
        return visible_rows
            .range(minimum..=start)
            .rev()
            .copied()
            .find(|row| !sheet.hidden_rows().contains(row));
    }
    let mut candidate = start;
    for &hidden in sheet.hidden_rows().range(minimum..=start).rev() {
        if hidden < candidate {
            break;
        }
        if hidden == candidate {
            if candidate == minimum {
                return None;
            }
            candidate -= 1;
        }
    }
    Some(candidate)
}

fn anchor_range_intersects_render_ranges(
    from: (u32, u16),
    to: (u32, u16),
    ranges: &[RenderRange],
) -> bool {
    from.0 <= to.0
        && from.1 <= to.1
        && ranges.iter().any(|range| {
            from.0 <= range.last_row
                && to.0 >= range.first_row
                && from.1 <= range.last_col
                && to.1 >= range.first_col
        })
}

/// Return the complete cell-anchor extent needed to paint drawings that touch
/// any selected print rectangle. Sheet-absolute drawings are reported
/// separately because their Y placement depends on every preceding prepared
/// row, including content-derived automatic heights.
pub(crate) fn prepared_drawing_geometry_extent(
    sheet: &Sheet,
    ranges: &[RenderRange],
    options: &RenderOptions,
) -> Result<(Vec<RenderRange>, bool), RenderError> {
    let metadata_index = DrawingMetadataIndex::new(sheet);
    let mut extents = Vec::new();
    let mut has_absolute = false;
    let style_snapshot = RenderStyleSnapshot::new(sheet);
    let maximum_digit_width = drawing_extent_maximum_digit_width(
        sheet,
        &style_snapshot,
        options,
        UsedDrawingTerminalColumnPolicy::Indexed,
    )?;
    let mut include = |from: (u32, u16), to: (u32, u16)| {
        extents.push(RenderRange::new(
            from.0.min(MAX_WORKSHEET_ROW),
            from.1.min(MAX_WORKSHEET_COLUMN),
            to.0.min(MAX_WORKSHEET_ROW),
            to.1.min(MAX_WORKSHEET_COLUMN),
        ));
    };

    for (index, image) in sheet.images().iter().enumerate() {
        let metadata = metadata_index.get(DrawingObjectKind::Image, index);
        if is_sheet_absolute_metadata(metadata) {
            has_absolute |= absolute_drawing_paint_bounds(DrawingObjectKind::Image, metadata)?
                .is_some_and(rect_intersects_positive_sheet);
            continue;
        }
        let to = drawing_used_to(
            sheet,
            image.from,
            image.to.unwrap_or((
                image.from.0.saturating_add(10),
                image.from.1.saturating_add(4),
            )),
            metadata,
            maximum_digit_width,
            None,
            options,
            AxisEndpointPolicy::PerTrackFixed,
        )?;
        if anchor_range_intersects_render_ranges(image.from, to, ranges) {
            include(image.from, to);
        }
    }
    for (index, chart) in sheet.charts().iter().enumerate() {
        let metadata = metadata_index.get(DrawingObjectKind::Chart, index);
        if is_sheet_absolute_metadata(metadata) {
            has_absolute |= absolute_drawing_paint_bounds(DrawingObjectKind::Chart, metadata)?
                .is_some_and(rect_intersects_positive_sheet);
            continue;
        }
        let to = drawing_used_to(
            sheet,
            chart.from,
            chart.to,
            metadata,
            maximum_digit_width,
            None,
            options,
            AxisEndpointPolicy::PerTrackFixed,
        )?;
        if anchor_range_intersects_render_ranges(chart.from, to, ranges) {
            include(chart.from, to);
        }
    }
    for metadata in sheet
        .drawing_metadata()
        .iter()
        .filter(|metadata| metadata.kind == DrawingObjectKind::Shape)
    {
        let Some(from) = metadata.from_cell else {
            continue;
        };
        let to = drawing_used_to(
            sheet,
            from,
            metadata.to_cell.unwrap_or(from),
            Some(metadata),
            maximum_digit_width,
            None,
            options,
            AxisEndpointPolicy::PerTrackFixed,
        )?;
        if anchor_range_intersects_render_ranges(from, to, ranges) {
            include(from, to);
        }
    }
    extents.sort_by_key(|range| {
        (
            range.first_row,
            range.first_col,
            range.last_row,
            range.last_col,
        )
    });
    extents.dedup();
    Ok((extents, has_absolute))
}

fn include_render_coordinate(range: &mut Option<RenderRange>, row: u32, col: u16) {
    *range = Some(match *range {
        Some(range) => RenderRange::new(
            range.first_row.min(row),
            range.first_col.min(col),
            range.last_row.max(row),
            range.last_col.max(col),
        ),
        None => RenderRange::new(row, col, row, col),
    });
}

pub(crate) fn cell_style_has_visible_blank_paint(style: &CellStyle) -> bool {
    let has_fill = match style.pattern_fill {
        Some(fill) if fill.pattern == FormatPattern::None => style.fill.is_some(),
        Some(fill) => {
            fill.foreground.is_some() || fill.background.is_some() || style.fill.is_some()
        }
        None => style.fill.is_some(),
    };
    let has_border = style.border.as_ref().is_some_and(|border| {
        [border.left, border.right, border.top, border.bottom]
            .into_iter()
            .any(|edge| edge != BorderStyle::None)
    });
    has_fill || has_border
}

pub(crate) fn render_single_page_used_scene_range(
    sheet: &Sheet,
    options: &RenderOptions,
) -> Result<RenderRange, RenderError> {
    let mut style_snapshot = RenderStyleSnapshot::new(sheet);
    style_snapshot.capture_sparse_visual_candidates(sheet, options)?;
    let extent = render_used_extent(
        sheet,
        &style_snapshot,
        options,
        UsedDrawingTerminalColumnPolicy::CalcOoxmlSinglePage,
        AxisEndpointPolicy::SourceNative,
    )?;
    Ok(extent.range.unwrap_or_else(|| RenderRange::new(0, 0, 0, 0)))
}

/// Resolve the cell range needed to paginate all used visual content.
///
/// A sheet-absolute drawing is positioned in physical sheet coordinates, so
/// representing it only as A1 is sufficient for a single expanded scene but
/// not for cell-partitioned print pages. Extend the fallback print range to
/// the row and column whose persisted geometry reaches the drawing bounds.
pub(crate) fn render_used_print_range(
    sheet: &Sheet,
    options: &RenderOptions,
) -> Result<RenderRange, RenderError> {
    let mut style_snapshot = RenderStyleSnapshot::new(sheet);
    style_snapshot.capture_sparse_visual_candidates(sheet, options)?;
    let mut range = render_used_extent(
        sheet,
        &style_snapshot,
        options,
        UsedDrawingTerminalColumnPolicy::Indexed,
        AxisEndpointPolicy::PerTrackFixed,
    )?
    .range
    .unwrap_or_else(|| RenderRange::new(0, 0, 0, 0));
    let Some((absolute_right, absolute_bottom)) = absolute_drawing_positive_extent(sheet)? else {
        return Ok(range);
    };

    let mut warnings = Warnings::default();
    let mut typography = TypographyStats::default();
    let maximum_digit_width =
        maximum_digit_width(&style_snapshot, options, &mut warnings, &mut typography)?;
    range.last_col = range.last_col.max(print_column_for_absolute_extent(
        sheet,
        absolute_right,
        maximum_digit_width,
        options,
        &mut warnings,
    )?);
    range.last_row = range.last_row.max(print_row_for_absolute_extent(
        sheet,
        absolute_bottom,
        maximum_digit_width,
        options,
        &mut warnings,
    )?);
    Ok(range)
}

fn print_column_for_absolute_extent(
    sheet: &Sheet,
    target_right: Fixed,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<u16, RenderError> {
    let mut right = Fixed::ZERO;
    for column in 0..=MAX_WORKSHEET_COLUMN {
        if options.include_hidden || !sheet.hidden_columns().contains(&column) {
            right = right
                .checked_add(column_width(
                    sheet,
                    column,
                    maximum_digit_width,
                    options,
                    warnings,
                ))
                .ok_or(RenderError::CoordinateOverflow)?;
        }
        if right >= target_right {
            return Ok(column);
        }
    }
    Ok(MAX_WORKSHEET_COLUMN)
}

fn print_row_for_absolute_extent(
    sheet: &Sheet,
    target_bottom: Fixed,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<u32, RenderError> {
    let mut first = 0_u32;
    let mut last = MAX_WORKSHEET_ROW;
    while first < last {
        let middle = first + (last - first) / 2;
        let next_row = middle
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        let (_, bottom) = sheet_grid_origin(
            sheet,
            RenderRange::new(next_row, 0, next_row, 0),
            maximum_digit_width,
            options,
            warnings,
        )?;
        if bottom >= target_bottom {
            last = middle;
        } else {
            first = middle.saturating_add(1);
        }
    }
    Ok(first)
}

impl From<(u32, u16, u32, u16)> for RenderRange {
    fn from(value: (u32, u16, u32, u16)) -> Self {
        Self::new(value.0, value.1, value.2, value.3)
    }
}

fn enforce(kind: LimitKind, limit: u64, actual: u64) -> Result<(), RenderError> {
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

fn enforce_dimension(value: Fixed, options: &RenderOptions) -> Result<(), RenderError> {
    let actual = u64::try_from(value.raw()).map_err(|_| RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::Dimension,
        options.limits.max_dimension_raw,
        actual,
    )
}

fn push_node(
    nodes: &mut Vec<SceneNode>,
    node: SceneNode,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let actual = nodes.len() as u64 + 1;
    enforce(
        LimitKind::SceneNodes,
        options.limits.max_scene_nodes,
        actual,
    )?;
    nodes.push(node);
    Ok(())
}

fn sum_fixed(values: impl IntoIterator<Item = Fixed>) -> Result<Fixed, RenderError> {
    values.into_iter().try_fold(Fixed::ZERO, |sum, value| {
        sum.checked_add(value)
            .ok_or(RenderError::CoordinateOverflow)
    })
}

fn axis_slots_end<I>(slots: &[MeasuredAxisSlot<I>]) -> Result<Fixed, RenderError> {
    let Some(last) = slots.last() else {
        return Ok(Fixed::ZERO);
    };
    last.offset
        .checked_add(last.size)
        .ok_or(RenderError::CoordinateOverflow)
}

fn cell_has_auto_filter_button(sheet: &Sheet, source: CellCoordinate) -> bool {
    let is_header = |(first_row, first_col, _last_row, last_col)| {
        source.row == first_row && source.col >= first_col && source.col <= last_col
    };
    sheet.autofilter_range().is_some_and(is_header)
        || sheet.tables().iter().any(|table| is_header(table.range))
}

fn cell_line_layout_policy(
    sheet: &Sheet,
    source: CellCoordinate,
    style: Option<&CellStyle>,
    rich_text: Option<&[rxls::TextRun]>,
    evidence: CalcLineLayoutEvidence,
    options: &RenderOptions,
) -> CellLineLayoutPolicy {
    let verified = || {
        if !evidence.is_plain_text
            || !evidence.has_adjustable_row
            || rich_text.is_some()
            || !evidence.wrap_space_available
            || cell_has_auto_filter_button(sheet, source)
        {
            return None;
        }
        let style = style?;
        let alignment = style.align.as_ref()?;
        if !alignment.wrap
            || alignment.rotation != 0
            || alignment.shrink_to_fit
            || alignment.indent != 0
        {
            return None;
        }
        verified_calc_cell_font_size_pt(sheet, source, style, options).map(|_| ())
    };
    if verified().is_some() {
        CellLineLayoutPolicy::CalcEditEngine
    } else if matches!(
        sheet.imported_default_row_axis_measure(),
        Some(ImportedAxisMeasure::MillimeterHundredths(_))
    ) {
        CellLineLayoutPolicy::OdsNative
    } else {
        CellLineLayoutPolicy::Native
    }
}

fn calc_import_provenance(sheet: &Sheet) -> Option<CalcImportProvenance> {
    match sheet.implicit_ooxml_row_height_source() {
        Some(OoxmlImplicitRowHeight::XlsxApplicationDefault) => {
            return Some(CalcImportProvenance::Xlsx);
        }
        Some(OoxmlImplicitRowHeight::XlsbApplicationDefault) => {
            return Some(CalcImportProvenance::Xlsb);
        }
        Some(OoxmlImplicitRowHeight::None) | None => {}
    }
    if sheet.biff_uses_application_default_row_height()
        || matches!(
            sheet.imported_default_row_axis_measure(),
            Some(ImportedAxisMeasure::Twips(_))
        )
    {
        return Some(CalcImportProvenance::Biff);
    }
    matches!(
        sheet.imported_default_row_axis_measure(),
        Some(ImportedAxisMeasure::MillimeterHundredths(_))
    )
    .then_some(CalcImportProvenance::Ods)
}

fn calc_line_placement_policy(
    sheet: &Sheet,
    source: CellCoordinate,
    style: Option<&CellStyle>,
    rich_text: Option<&[rxls::TextRun]>,
    is_plain_text: bool,
    options: &RenderOptions,
) -> CalcLinePlacementPolicy {
    let verified = || {
        if !is_plain_text || rich_text.is_some() || has_conditional_text_layout_overlay(sheet) {
            return None;
        }
        let provenance = calc_import_provenance(sheet)?;
        let style = style?;
        let font = style.font.as_ref()?;
        if font.size_pt.is_none() || font.script != FormatScript::None {
            return None;
        }
        if style.align.as_ref().is_some_and(|alignment| {
            alignment.rotation != 0 || alignment.shrink_to_fit || alignment.indent != 0
        }) {
            return None;
        }
        let resolution = options.font_pack.as_ref()?.resolve(FontRequest {
            family: font.name.as_deref()?,
            weight: if font.bold { 700 } else { 400 },
            italic: font.italic,
        });
        if !(resolution.exact_family || resolution.declared_alias) || !resolution.exact_style {
            return None;
        }
        if matches!(
            provenance,
            CalcImportProvenance::Xlsx | CalcImportProvenance::Xlsb
        ) && verified_calc_cell_font_size_pt(sheet, source, style, options).is_none()
        {
            return None;
        }
        Some(CalcLinePlacementPolicy::Imported(provenance))
    };
    verified().unwrap_or(CalcLinePlacementPolicy::Native)
}

fn calc_ooxml_implicit_row_height(sheet: &Sheet, options: &RenderOptions) -> Option<Fixed> {
    if !sheet.has_implicit_ooxml_row_height() {
        return None;
    }
    let (points, _) = verified_ooxml_normal_font_size(sheet, options)?;
    calc_ooxml_row_height_from_points(points)
}

fn calc_ooxml_row_height_twips_from_points(points: u16) -> Option<u32> {
    let font_twips = i128::from(points).checked_mul(TWIPS_PER_POINT)?;
    let row_twips = font_twips
        .checked_mul(CALC_NORMAL_ROW_HEIGHT_PERCENT)?
        .checked_div(CALC_NORMAL_ROW_HEIGHT_PERCENT_DENOMINATOR)?
        .checked_add(CALC_NORMAL_ROW_HEIGHT_ADJUSTMENT_TWIPS)?;
    u32::try_from(row_twips).ok().filter(|twips| *twips > 0)
}

fn calc_ooxml_row_height_from_points(points: u16) -> Option<Fixed> {
    let row_twips = calc_ooxml_row_height_twips_from_points(points)?;
    let raw = i128::from(row_twips)
        .checked_mul(i128::from(FIXED_UNITS_PER_PIXEL))?
        .checked_add(TWIPS_PER_CSS_PIXEL / 2)?
        .checked_div(TWIPS_PER_CSS_PIXEL)?;
    i64::try_from(raw)
        .ok()
        .filter(|raw| *raw > 0)
        .map(Fixed::from_raw)
}

fn same_row_height_font(left: &Font, right: &Font) -> bool {
    left.name == right.name
        && left.size_pt == right.size_pt
        && left.bold == right.bold
        && left.italic == right.italic
        && left.script == right.script
}

fn row_is_hidden(sheet: &Sheet, row: u32) -> bool {
    sheet.hidden_rows().contains(&row)
        || sheet
            .default_hidden_row_exceptions()
            .is_some_and(|visible_rows| !visible_rows.contains(&row))
}

fn effective_row_height_is_manual(sheet: &Sheet, row: u32) -> bool {
    if sheet.row_heights().contains_key(&row) {
        sheet.row_height_is_manual(row)
    } else {
        sheet.default_row_height_is_manual()
    }
}

fn automatic_candidate_adjustable_row(
    sheet: &Sheet,
    range: RenderRange,
    row_sizes: &BTreeMap<u32, Fixed>,
    merge_anchors: &BTreeMap<CellCoordinate, (u32, u16, u32, u16)>,
    source: CellCoordinate,
    options: &RenderOptions,
) -> Option<u32> {
    if let Some(&(r0, c0, r1, c1)) = merge_anchors.get(&source) {
        let last_col = c1.min(MAX_WORKSHEET_COLUMN);
        let span = usize::from(last_col.checked_sub(c0)?) + 1;
        if !options.include_hidden && sheet.hidden_columns().range(c0..=last_col).count() >= span {
            return None;
        }
        let first_row = r0.max(range.first_row);
        let last_row = r1.min(range.last_row);
        if first_row > last_row {
            return None;
        }
        row_sizes
            .range(first_row..=last_row)
            .map(|(&row, _)| row)
            .find(|row| !effective_row_height_is_manual(sheet, *row))
    } else {
        (row_sizes.contains_key(&source.row)
            && !effective_row_height_is_manual(sheet, source.row)
            && (options.include_hidden || !sheet.hidden_columns().contains(&source.col)))
        .then_some(source.row)
    }
}

#[derive(Debug)]
struct AutoMergeHeight {
    rows: Vec<u32>,
    adjustable_row: u32,
    required: Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CalcAutomaticMetricSource {
    RequestedFont,
    PreparedAsianOrRequested,
    CalcComplexRole,
}

fn calc_automatic_metric_source(
    source: Option<OoxmlImplicitRowHeight>,
    requires_individual_plain: bool,
    has_verified_points: bool,
    row_script_summary: Option<&CalcScriptClassSummary>,
    cell_script_analysis: Option<&CalcCellScriptAnalysis>,
) -> Option<CalcAutomaticMetricSource> {
    if !requires_individual_plain || !has_verified_points {
        return None;
    }
    // This precedes the XLSX/XLSB split because both importers hand the same
    // mixed RTL text to EditEngine and both use the imported CTL role for the
    // resulting COMPLEX metric portion.
    if cell_script_analysis.is_some_and(|analysis| analysis.edit_engine_uses_only_complex_role) {
        return Some(CalcAutomaticMetricSource::CalcComplexRole);
    }
    let row_is_mixed = row_script_summary.is_some_and(|summary| summary.mixed);
    match source {
        Some(OoxmlImplicitRowHeight::XlsxApplicationDefault) => {
            Some(CalcAutomaticMetricSource::RequestedFont)
        }
        Some(OoxmlImplicitRowHeight::XlsbApplicationDefault)
            if row_is_mixed && row_script_summary.is_some_and(|summary| summary.has_asian) =>
        {
            Some(CalcAutomaticMetricSource::PreparedAsianOrRequested)
        }
        Some(OoxmlImplicitRowHeight::XlsbApplicationDefault) if !row_is_mixed => {
            Some(CalcAutomaticMetricSource::RequestedFont)
        }
        Some(OoxmlImplicitRowHeight::XlsbApplicationDefault) => None,
        Some(OoxmlImplicitRowHeight::None) | None => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn expand_automatic_row_heights(
    sheet: &Sheet,
    range: RenderRange,
    style_snapshot: &RenderStyleSnapshot,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    warnings: &mut Warnings,
    column_widths: &mut BTreeMap<u16, Fixed>,
    row_sizes: &mut BTreeMap<u32, Fixed>,
    typography: &mut TypographyStats,
    conditional_evaluations: &mut u64,
    automatic_candidates: Option<&[DisplayCell<'_>]>,
) -> Result<(), RenderError> {
    let Some(pack) = options.font_pack.as_ref() else {
        return Ok(());
    };
    let verified_normal_font = verified_ooxml_normal_font_size(sheet, options)
        .and_then(|_| sheet.default_cell_style()?.font.as_ref());
    let verified_implicit_ooxml =
        sheet.has_implicit_ooxml_row_height() && verified_normal_font.is_some();
    // Painting conservatively disables Calc's wrapper whenever retained
    // conditional metadata can change text geometry. Automatic-row
    // measurement must make the same decision even when the affected rule is
    // outside the rendered subset; otherwise the row is measured with Calc's
    // paper and painted with the native wrapper.
    let calc_line_layout_available = calc_line_layout_available(sheet, options);

    // Values in merged cells belong to the top-left anchor. Indexing anchors,
    // rather than every covered coordinate, keeps even whole-sheet merges
    // sparse and bounded.
    let merge_anchors = sheet
        .merged_ranges()
        .iter()
        .filter_map(|&(r0, c0, r1, c1)| {
            (r0 <= r1
                && c0 <= c1
                && r0 <= MAX_WORKSHEET_ROW
                && c0 <= MAX_WORKSHEET_COLUMN
                && r0 <= range.last_row
                && r1 >= range.first_row)
                .then_some((CellCoordinate { row: r0, col: c0 }, (r0, c0, r1, c1)))
        })
        .collect::<BTreeMap<_, _>>();

    let mut single_row_requirements = BTreeMap::<u32, Fixed>::new();
    let mut merged_requirements = Vec::<AutoMergeHeight>::new();
    let mut automatic_cells = 0_u64;

    let local_candidates = if automatic_candidates.is_none() {
        let display_cell_index = SparseDisplayCellIndex::new(sheet);
        let mut candidates = BTreeMap::new();
        for cell in
            display_cell_index.range((range.first_row, 0, range.last_row, MAX_WORKSHEET_COLUMN))
        {
            candidates.insert((cell.row, cell.col), cell);
            enforce(
                LimitKind::Cells,
                options.limits.max_cells,
                candidates.len() as u64,
            )?;
        }
        for coordinate in merge_anchors.keys() {
            for cell in display_cell_index.range((
                coordinate.row,
                coordinate.col,
                coordinate.row,
                coordinate.col,
            )) {
                candidates.insert((cell.row, cell.col), cell);
                enforce(
                    LimitKind::Cells,
                    options.limits.max_cells,
                    candidates.len() as u64,
                )?;
            }
        }
        Some(candidates.into_values().collect::<Vec<_>>())
    } else {
        None
    };
    let candidates = automatic_candidates
        .or(local_candidates.as_deref())
        .unwrap_or(&[]);
    let mut automatic_candidate_rows = BTreeMap::new();
    let mut layout_candidates = Vec::new();
    for &cell in candidates {
        if cell.formatted.is_empty()
            || cell.row > MAX_WORKSHEET_ROW
            || cell.col > MAX_WORKSHEET_COLUMN
        {
            continue;
        }
        let source = CellCoordinate {
            row: cell.row,
            col: cell.col,
        };
        let Some(adjustable_row) = automatic_candidate_adjustable_row(
            sheet,
            range,
            row_sizes,
            &merge_anchors,
            source,
            options,
        ) else {
            continue;
        };
        automatic_candidate_rows.insert(source, adjustable_row);
        layout_candidates.push(cell);
    }
    let conditional_layout_cells = resolve_conditional_layout_cells(
        sheet,
        &layout_candidates,
        style_snapshot,
        options,
        conditional_evaluations,
    )?;
    let mut row_script_classes = BTreeMap::<u32, CalcScriptClassSummary>::new();
    let mut cell_script_classes = BTreeMap::<CellCoordinate, CalcCellScriptAnalysis>::new();
    if calc_line_layout_available {
        for &cell in &layout_candidates {
            let source = CellCoordinate {
                row: cell.row,
                col: cell.col,
            };
            let adjustable_row = automatic_candidate_rows[&source];
            let summary = calc_script_class_summary_bounded(cell.formatted, options, typography)?;
            let edit_engine_uses_only_complex_role =
                calc_edit_engine_uses_only_complex_role(cell.formatted, summary, options)?;
            cell_script_classes.insert(
                source,
                CalcCellScriptAnalysis {
                    edit_engine_uses_only_complex_role,
                },
            );
            row_script_classes
                .entry(adjustable_row)
                .and_modify(|row| row.merge(summary))
                .or_insert(summary);
        }
    }
    for &cell in candidates {
        if cell.formatted.is_empty()
            || cell.row > MAX_WORKSHEET_ROW
            || cell.col > MAX_WORKSHEET_COLUMN
        {
            continue;
        }
        let source = CellCoordinate {
            row: cell.row,
            col: cell.col,
        };
        let merged = merge_anchors.get(&source).copied();
        if merged.is_none() && (cell.row < range.first_row || cell.row > range.last_row) {
            continue;
        }
        let (visible_rows, adjustable_row, width, is_merged, calc_wrap_space) =
            if let Some((r0, c0, r1, c1)) = merged {
                let visible_rows = row_sizes
                    .range(r0.max(range.first_row)..=r1.min(range.last_row))
                    .map(|(&row, _)| row)
                    .collect::<Vec<_>>();
                let Some(adjustable_row) = visible_rows
                    .iter()
                    .copied()
                    .find(|row| !effective_row_height_is_manual(sheet, *row))
                else {
                    continue;
                };
                let Some(width) = visible_column_span_width(
                    sheet,
                    c0,
                    c1,
                    maximum_digit_width,
                    options,
                    warnings,
                    column_widths,
                )?
                else {
                    continue;
                };
                let calc_wrap_space = if calc_line_layout_available {
                    calc_ooxml_merge_wrap_space(sheet, c0, c1, maximum_digit_width, options)?
                } else {
                    None
                };
                (visible_rows, adjustable_row, width, true, calc_wrap_space)
            } else {
                if !row_sizes.contains_key(&cell.row)
                    || effective_row_height_is_manual(sheet, cell.row)
                    || (!options.include_hidden && sheet.hidden_columns().contains(&cell.col))
                {
                    continue;
                }
                let width = cached_column_width(
                    sheet,
                    cell.col,
                    maximum_digit_width,
                    options,
                    warnings,
                    column_widths,
                );
                (
                    vec![cell.row],
                    cell.row,
                    width,
                    false,
                    if calc_line_layout_available {
                        calc_ooxml_cell_wrap_space(sheet, cell.col, maximum_digit_width, options)?
                    } else {
                        None
                    },
                )
            };

        let conditional_layout = conditional_layout_cells.get(&source);
        let style = conditional_layout
            .and_then(|cell| cell.effective_style.clone())
            .or_else(|| style_snapshot.owned_style(source))
            .or_else(|| sheet.resolved_cell_style(source.row, source.col));
        let active_conditional_style =
            conditional_layout.and_then(|cell| cell.active_style.as_ref());
        let active_color_only = active_conditional_style
            .is_some_and(|style| conditional_style_is_geometry_safe_color_only(style, false));
        let active_layout_style = active_conditional_style
            .is_some_and(|style| conditional_style_affects_text_layout(style, false));
        let alignment = style.as_ref().and_then(|style| style.align.as_ref());
        let font_size = style
            .as_ref()
            .and_then(|style| style.font.as_ref())
            .and_then(|font| font.size_pt)
            .and_then(|points| points_to_fixed(points as f32))
            .unwrap_or(options.default_font_size);
        let rich_text = cell.rich_text.filter(|runs| !runs.is_empty());
        let default_plain_font = verified_normal_font.is_some_and(|normal_font| {
            style
                .as_ref()
                .and_then(|style| style.font.as_ref())
                .is_some_and(|font| same_row_height_font(font, normal_font))
        });
        automatic_cells = automatic_cells
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(LimitKind::Cells, options.limits.max_cells, automatic_cells)?;
        charge_automatic_text_bytes(cell.formatted, options, typography)?;
        let plain_single_line = alignment
            .is_none_or(|alignment| !alignment.wrap && alignment.rotation == 0)
            && !contains_mandatory_line_break(cell.formatted)
            && rich_text.is_none();
        let effective_script = style
            .as_ref()
            .and_then(|style| style.font.as_ref())
            .map_or(FormatScript::None, |font| font.script);
        let ordinary_implicit_plain =
            verified_implicit_ooxml && plain_single_line && effective_script == FormatScript::None;
        if !verified_implicit_ooxml
            && plain_single_line
            && !active_layout_style
            && (default_plain_font
                || (verified_normal_font.is_none() && font_size <= options.default_font_size))
        {
            continue;
        }

        let effective_font = style.as_ref().and_then(|style| style.font.as_ref());
        let retained_font = cell.explicit_style.and_then(|style| style.font.as_ref());
        let declared_points = ordinary_implicit_plain
            .then(|| verified_ooxml_cell_font_size_pt(sheet, cell.row, cell.col))
            .flatten()
            .filter(|points| {
                effective_font.and_then(|font| font.size_pt) == Some(*points)
                    && match sheet.implicit_ooxml_row_height_source() {
                        Some(OoxmlImplicitRowHeight::XlsxApplicationDefault) => {
                            effective_font == retained_font
                        }
                        Some(OoxmlImplicitRowHeight::XlsbApplicationDefault) => true,
                        Some(OoxmlImplicitRowHeight::None) | None => false,
                    }
            });
        let verified_calc_points = ordinary_implicit_plain
            .then(|| {
                style.as_ref().and_then(|style| {
                    verified_calc_cell_font_size_pt(sheet, source, style, options)
                })
            })
            .flatten();
        let row_script_summary = row_script_classes.get(&adjustable_row);
        let cell_script_analysis = cell_script_classes.get(&source);
        let row_is_mixed = row_script_summary.is_some_and(|summary| summary.mixed);
        let requires_individual_plain = ordinary_implicit_plain
            && calc_line_layout_available
            && !cell_has_auto_filter_button(sheet, source)
            && (row_is_mixed || active_color_only || active_layout_style);
        let calc_metric_source = calc_automatic_metric_source(
            sheet.implicit_ooxml_row_height_source(),
            requires_individual_plain,
            verified_calc_points.is_some(),
            row_script_summary,
            cell_script_analysis,
        );
        // Calc sizes an automatic row from the *pattern* font height
        // (`lcl_GetAttribHeight`: 118% of the pattern font's integer-twip
        // height plus the standard margin/row adjustments) rather than from the
        // shaped run's own face metrics. The two only diverge when the cell
        // genuinely forces Calc off the pattern: text that mixes script classes
        // inside one cell selects a taller face for part of the run, and an
        // active conditional format re-resolves the cell's own appearance.
        // A row that is "mixed" only because *different* cells carry different
        // scripts does not qualify -- each of those cells is internally uniform,
        // so Calc keeps every one of them on the pattern height, which is why a
        // western/Asian or western/complex heading row stays exactly as tall as
        // the same sheet without it.
        let calc_pattern_points = verified_calc_points.filter(|_| {
            !active_color_only
                && !active_layout_style
                && !has_mixed_calc_script_classes(cell.formatted)
        });
        let declared_plain_height = if requires_individual_plain {
            None
        } else if let Some(points) = declared_points {
            calc_ooxml_row_height_from_points(points)
        } else {
            None
        };

        let required = if let Some(required) = declared_plain_height {
            required
        } else {
            let (text, _) = sanitize_xml_text(cell.formatted);
            let rich_text = rich_text.and_then(|runs| {
                let sanitized = sanitize_rich_text(runs);
                (sanitized
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
                    == text)
                    .then_some(sanitized)
            });
            let line_layout_policy = if calc_metric_source.is_some() && calc_wrap_space.is_some() {
                CellLineLayoutPolicy::CalcEditEngine
            } else {
                cell_line_layout_policy(
                    sheet,
                    source,
                    style.as_ref(),
                    rich_text.as_deref(),
                    CalcLineLayoutEvidence {
                        is_plain_text: matches!(cell.value, Cell::Text(_)),
                        has_adjustable_row: true,
                        wrap_space_available: calc_line_layout_available
                            && calc_wrap_space.is_some(),
                    },
                    options,
                )
            };
            let line_placement_policy = calc_line_placement_policy(
                sheet,
                source,
                style.as_ref(),
                rich_text.as_deref(),
                matches!(cell.value, Cell::Text(_)),
                options,
            );
            let region = Region {
                source,
                rect: Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width,
                    height: Fixed::from_raw(1),
                },
                is_merged,
                line_layout_policy,
                line_placement_policy,
                calc_wrap_space: (line_layout_policy == CellLineLayoutPolicy::CalcEditEngine)
                    .then_some(calc_wrap_space)
                    .flatten(),
                style,
                conditional: ConditionalPaint::default(),
                text,
                rich_text,
                hyperlink: None,
                numeric_default: false,
                text_can_overflow: false,
                fixed_height_row: false,
                ods_fixed_height_row: false,
                print_vertical_overflow: false,
                vertical_margin: calc_cell_vertical_margin(sheet),
            };
            measure_automatic_cell_height(
                pack,
                &region,
                sheet.sheet_view().right_to_left,
                options,
                typography,
                calc_metric_source,
                calc_pattern_points,
            )?
        };
        if is_merged {
            merged_requirements.push(AutoMergeHeight {
                rows: visible_rows,
                adjustable_row,
                required,
            });
        } else {
            single_row_requirements
                .entry(adjustable_row)
                .and_modify(|height| *height = (*height).max(required))
                .or_insert(required);
        }
    }

    // Resolve ordinary cells before merged constraints so a merged block only
    // receives the remaining deficit after its constituent rows have grown.
    for (row, required) in single_row_requirements {
        if let Some(height) = row_sizes.get_mut(&row) {
            *height = (*height).max(required);
        }
    }
    for constraint in merged_requirements {
        let total = sum_fixed(
            constraint
                .rows
                .iter()
                .filter_map(|row| row_sizes.get(row).copied()),
        )?;
        if constraint.required <= total {
            continue;
        }
        let deficit = constraint
            .required
            .checked_sub(total)
            .ok_or(RenderError::CoordinateOverflow)?;
        let height = row_sizes
            .get_mut(&constraint.adjustable_row)
            .ok_or(RenderError::CoordinateOverflow)?;
        *height = height
            .checked_add(deficit)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn visible_column_span_width(
    sheet: &Sheet,
    first: u16,
    last: u16,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    warnings: &mut Warnings,
    column_widths: &mut BTreeMap<u16, Fixed>,
) -> Result<Option<Fixed>, RenderError> {
    if first > last || first > MAX_WORKSHEET_COLUMN {
        return Ok(None);
    }
    let mut width = Fixed::ZERO;
    let mut found = false;
    for column in first..=last.min(MAX_WORKSHEET_COLUMN) {
        if !options.include_hidden && sheet.hidden_columns().contains(&column) {
            continue;
        }
        found = true;
        width = width
            .checked_add(cached_column_width(
                sheet,
                column,
                maximum_digit_width,
                options,
                warnings,
                column_widths,
            ))
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(found.then_some(width))
}

fn cached_column_width(
    sheet: &Sheet,
    column: u16,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    warnings: &mut Warnings,
    column_widths: &mut BTreeMap<u16, Fixed>,
) -> Fixed {
    if let Some(width) = column_widths.get(&column) {
        return *width;
    }
    let width = column_width(sheet, column, maximum_digit_width, options, warnings);
    column_widths.insert(column, width);
    width
}

fn contains_mandatory_line_break(text: &str) -> bool {
    text.chars()
        .any(|ch| matches!(ch, '\r' | '\n' | '\u{0085}' | '\u{2028}' | '\u{2029}'))
}

fn is_safe_hyperlink(target: &str) -> bool {
    if target.is_empty() || target.trim() != target || target.chars().any(|ch| ch.is_control()) {
        return false;
    }
    let Some((scheme, remainder)) = target.split_once(':') else {
        return false;
    };
    !remainder.is_empty()
        && ["http", "https", "mailto"]
            .iter()
            .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
}

fn apply_numeric_overflow(
    regions: &mut [Region],
    display_cells: &BTreeMap<CellCoordinate, DisplayCell<'_>>,
    options: &RenderOptions,
    sheet_right_to_left: bool,
    stats: &mut TypographyStats,
    warnings: &mut Warnings,
) -> Result<(), RenderError> {
    for region in regions.iter_mut() {
        let Some(display_cell) = display_cells.get(&region.source) else {
            continue;
        };
        if region.text.is_empty() || !cell_defaults_to_right_alignment(display_cell.value) {
            continue;
        }
        let alignment = region.style.as_ref().and_then(|style| style.align.as_ref());
        if alignment.is_some_and(|alignment| {
            alignment.wrap || alignment.shrink_to_fit || alignment.rotation != 0
        }) {
            continue;
        }
        let style = text_style(region, options);
        let (available, text_width, hash_width) = if let Some(pack) = options.font_pack.as_ref() {
            let font = region.style.as_ref().and_then(|style| style.font.as_ref());
            let request = FontRequest {
                family: &style.family,
                weight: if style.bold { 700 } else { 400 },
                italic: style.italic,
            };
            let font_size = match font.map_or(FormatScript::None, |font| font.script) {
                FormatScript::None => style.size,
                FormatScript::Superscript | FormatScript::Subscript => {
                    scale_ratio(style.size, 13, 20)?
                }
            };
            let padding = outlined_horizontal_padding(pack, request, font_size, region)?;
            let available = inner_width(region.rect.width, padding)?;
            let direction = if sheet_right_to_left {
                BaseDirection::RightToLeft
            } else {
                BaseDirection::Auto
            };
            let text_width = measured_shaped_width(
                pack,
                &region.text,
                request,
                direction,
                font_size,
                options,
                stats,
            )?;
            let hash_width =
                measured_shaped_width(pack, "#", request, direction, font_size, options, stats)?
                    .max(Fixed::from_raw(1));
            (available, text_width, hash_width)
        } else {
            let available = inner_width(region.rect.width, options.horizontal_padding)?;
            let unit = Fixed::from_raw((style.size.raw() / 2).max(1));
            let scalar_count = i64::try_from(region.text.chars().count())
                .map_err(|_| RenderError::CoordinateOverflow)?;
            let text_width = multiply_fixed(unit, scalar_count)?;
            (available, text_width, unit)
        };
        if text_width <= available {
            continue;
        }
        // Calc renders an overflowing date/time with its fixed three-hash
        // indicator. Other numeric values retain the width-filling behavior,
        // so this parity rule cannot silently change ordinary numeric output.
        let count = if cell_is_date_or_time(display_cell.value, region.style.as_ref()) {
            3
        } else {
            let count = available.raw().max(1) / hash_width.raw().max(1);
            usize::try_from(count.max(1)).map_err(|_| RenderError::CoordinateOverflow)?
        };
        enforce(LimitKind::Glyphs, options.limits.max_glyphs, count as u64)?;
        region.text = "#".repeat(count);
        region.rich_text = None;
        region.text_can_overflow = false;
        warnings.add(WarningCode::NumericOverflowHashed, Some(region.source));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn measured_shaped_width(
    pack: &FontPack,
    text: &str,
    request: FontRequest<'_>,
    direction: BaseDirection,
    font_size: Fixed,
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<Fixed, RenderError> {
    let shaped = shape_text_with_kerning(
        pack,
        text,
        request,
        direction,
        CALC_WORKSHEET_KERNING,
        options,
    )?;
    stats.shaped_glyphs = stats
        .shaped_glyphs
        .checked_add(shaped.glyph_count as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::Glyphs,
        options.limits.max_glyphs,
        stats.shaped_glyphs,
    )?;
    stats.shaped_runs = stats
        .shaped_runs
        .checked_add(shaped.runs.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::TextRuns,
        options.limits.max_text_runs,
        stats.shaped_runs,
    )?;
    shaped_width(pack, &shaped, font_size)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct A1Reference {
    sheet: Option<String>,
    row: u32,
    col: u16,
    row_absolute: bool,
    col_absolute: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct A1RangeReference {
    sheet: Option<String>,
    first_row: u32,
    first_col: u16,
    last_row: u32,
    last_col: u16,
}

fn parse_a1_reference(value: &str) -> Option<A1Reference> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (sheet, cell) = split_sheet_qualifier(value)?;
    let bytes = cell.as_bytes();
    let mut cursor = 0_usize;
    let col_absolute = bytes.get(cursor) == Some(&b'$');
    cursor += usize::from(col_absolute);
    let col_start = cursor;
    while bytes
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_alphabetic())
    {
        cursor += 1;
    }
    if cursor == col_start {
        return None;
    }
    let row_absolute = bytes.get(cursor) == Some(&b'$');
    cursor += usize::from(row_absolute);
    let row_start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    if cursor == row_start || cursor != bytes.len() {
        return None;
    }

    let mut column = 0_u32;
    for byte in &bytes[col_start..if row_absolute {
        row_start - 1
    } else {
        row_start
    }] {
        let digit = u32::from(byte.to_ascii_uppercase() - b'A') + 1;
        column = column.checked_mul(26)?.checked_add(digit)?;
    }
    if column == 0 || column > u32::from(MAX_WORKSHEET_COLUMN) + 1 {
        return None;
    }
    let row = cell[row_start..cursor].parse::<u32>().ok()?;
    if row == 0 || row > MAX_WORKSHEET_ROW + 1 {
        return None;
    }
    Some(A1Reference {
        sheet,
        row: row - 1,
        col: u16::try_from(column - 1).ok()?,
        row_absolute,
        col_absolute,
    })
}

fn parse_a1_range(value: &str) -> Option<A1RangeReference> {
    let value = value.trim().strip_prefix('=').unwrap_or(value.trim());
    let separator = find_unquoted_separator(value, b':');
    let (first, second) = match separator {
        Some(separator) => (
            parse_a1_reference(&value[..separator])?,
            parse_a1_reference(&value[separator + 1..])?,
        ),
        None => {
            let reference = parse_a1_reference(value)?;
            (reference.clone(), reference)
        }
    };
    let sheet = match (first.sheet, second.sheet) {
        (Some(first), Some(second)) if same_sheet_name(&first, &second) => Some(first),
        (Some(first), None) => Some(first),
        (None, Some(second)) => Some(second),
        (None, None) => None,
        (Some(_), Some(_)) => return None,
    };
    Some(A1RangeReference {
        sheet,
        first_row: first.row.min(second.row),
        first_col: first.col.min(second.col),
        last_row: first.row.max(second.row),
        last_col: first.col.max(second.col),
    })
}

fn find_unquoted_separator(value: &str, separator: u8) -> Option<usize> {
    let bytes = value.as_bytes();
    let mut quoted = false;
    let mut found = None;
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\'' {
            if quoted && bytes.get(cursor + 1) == Some(&b'\'') {
                cursor += 2;
                continue;
            }
            quoted = !quoted;
        } else if !quoted && bytes[cursor] == separator {
            if found.is_some() {
                return None;
            }
            found = Some(cursor);
        }
        cursor += 1;
    }
    (!quoted).then_some(found).flatten()
}

fn same_sheet_name(first: &str, second: &str) -> bool {
    first == second || (first.is_ascii() && second.is_ascii() && first.eq_ignore_ascii_case(second))
}

fn range_belongs_to_sheet(range: &A1RangeReference, sheet: &Sheet) -> bool {
    range
        .sheet
        .as_deref()
        .is_none_or(|name| same_sheet_name(name, &sheet.name))
}

fn split_sheet_qualifier(value: &str) -> Option<(Option<String>, &str)> {
    let bytes = value.as_bytes();
    let mut quoted = false;
    let mut separator = None;
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' if quoted && bytes.get(cursor + 1) == Some(&b'\'') => cursor += 2,
            b'\'' => {
                quoted = !quoted;
                cursor += 1;
            }
            b'!' if !quoted => {
                separator = Some(cursor);
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    if quoted {
        return None;
    }
    let Some(separator) = separator else {
        return Some((None, value));
    };
    let raw_sheet = value[..separator].trim();
    let cell = value[separator + 1..].trim();
    if raw_sheet.is_empty() || cell.is_empty() {
        return None;
    }
    let sheet = if raw_sheet.starts_with('\'') {
        if !raw_sheet.ends_with('\'') || raw_sheet.len() < 2 {
            return None;
        }
        let inner = &raw_sheet[1..raw_sheet.len() - 1];
        let mut name = String::with_capacity(inner.len());
        let mut chars = inner.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\'' {
                if chars.next() != Some('\'') {
                    return None;
                }
                name.push('\'');
            } else {
                name.push(ch);
            }
        }
        name
    } else {
        if raw_sheet.contains('\'') || raw_sheet.chars().any(char::is_whitespace) {
            return None;
        }
        raw_sheet.to_string()
    };
    (!sheet.is_empty()).then_some((Some(sheet), cell))
}

fn render_range_intersection(
    left: RenderRange,
    right: (u32, u16, u32, u16),
) -> Option<RenderRange> {
    let intersection = RenderRange::new(
        left.first_row.max(right.0),
        left.first_col.max(right.1),
        left.last_row.min(right.2),
        left.last_col.min(right.3),
    );
    (intersection.first_row <= intersection.last_row
        && intersection.first_col <= intersection.last_col)
        .then_some(intersection)
}

fn add_a1_dependency_range(
    sheet: &Sheet,
    source: &str,
    chart_points: &mut u64,
    dependencies: &mut BTreeSet<CellCoordinate>,
    limits: &RenderLimits,
    aggregate_limit: u64,
) -> Result<(), RenderError> {
    let Some(range) = parse_a1_range(source) else {
        return Ok(());
    };
    if !range_belongs_to_sheet(&range, sheet)
        || (range.first_row != range.last_row && range.first_col != range.last_col)
    {
        return Ok(());
    }
    let points = a1_range_points(&range).ok_or(RenderError::CoordinateOverflow)?;
    let actual = chart_points
        .checked_add(points)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(LimitKind::ChartPoints, limits.max_chart_points, actual)?;
    *chart_points = actual;
    for row in range.first_row..=range.last_row {
        for col in range.first_col..=range.last_col {
            dependencies.insert(CellCoordinate { row, col });
            enforce(LimitKind::Cells, aggregate_limit, dependencies.len() as u64)?;
        }
    }
    Ok(())
}

fn data_renderable_chart_indices(
    sheet: &Sheet,
    selected_ranges: &[RenderRange],
    options: &RenderOptions,
    geometry: Option<SheetGeometryOverride<'_>>,
    endpoint_policy: AxisEndpointPolicy,
) -> Result<BTreeSet<usize>, RenderError> {
    let mut ranges = selected_ranges.to_vec();
    ranges.sort_by_key(|range| {
        (
            range.first_row,
            range.first_col,
            range.last_row,
            range.last_col,
        )
    });
    ranges.dedup();
    if ranges.is_empty() || sheet.charts().is_empty() {
        return Ok(BTreeSet::new());
    }

    // Reuse the exact axis, viewport, anchor, and minimum-size rules used by
    // scene construction. Merely intersecting a chart anchor is insufficient:
    // a clipped or tiny chart paints a placeholder without reading its source
    // series and therefore must not consume the chart-point budget here.
    let measurements =
        measure_sheet_axes_for_ranges_with_policy(sheet, &ranges, options, endpoint_policy)?;
    let style_snapshot = RenderStyleSnapshot::new(sheet);
    let mut warnings = Warnings::default();
    let mut typography = TypographyStats::default();
    let maximum_digit_width =
        maximum_digit_width(&style_snapshot, options, &mut warnings, &mut typography)?;
    let metadata_index = DrawingMetadataIndex::new(sheet);
    let right_to_left = sheet.sheet_view().right_to_left;
    let used_selection = matches!(options.selection, RenderSelection::Used);
    let mut renderable = BTreeSet::new();

    for (range, (mut row_slots, mut col_slots)) in ranges.into_iter().zip(measurements) {
        if let Some(geometry) = geometry {
            apply_axis_geometry(&mut row_slots, geometry.rows)?;
            apply_axis_geometry(&mut col_slots, geometry.columns)?;
        }
        let grid_width = axis_slots_end(&col_slots)?;
        let grid_height = axis_slots_end(&row_slots)?;
        let viewport = drawing_layout_viewport(
            sheet,
            range,
            &row_slots,
            grid_width,
            grid_height,
            maximum_digit_width,
            used_selection,
            options,
            geometry,
            endpoint_policy,
            &mut warnings,
        )?;
        offset_axis_slots(&mut col_slots, viewport.cell.x)?;
        offset_axis_slots(&mut row_slots, viewport.cell.y)?;
        let scene_width = viewport.sheet.width.max(Fixed::from_pixels(1));

        for (chart_index, chart) in sheet.charts().iter().enumerate() {
            if renderable.contains(&chart_index) {
                continue;
            }
            let metadata = metadata_index.get(DrawingObjectKind::Chart, chart_index);
            if chart.series.is_empty()
                || metadata.is_some_and(|metadata| !metadata.chart_unsupported_reasons.is_empty())
            {
                continue;
            }
            if let DrawingPlacement::Placed(rect) = drawing_rect(
                &row_slots,
                &col_slots,
                viewport.cell,
                viewport.sheet,
                scene_width,
                DrawingObjectKind::Chart,
                chart.from,
                chart.to,
                metadata,
                right_to_left,
                geometry,
            )? {
                if rect.width >= Fixed::from_pixels(120) && rect.height >= Fixed::from_pixels(80) {
                    renderable.insert(chart_index);
                }
            }
        }
    }
    Ok(renderable)
}

/// Resolve every same-sheet cell that can indirectly change a selected scene.
///
/// Prepared print documents fingerprint this bounded dependency closure so
/// conditional expressions, charts, and sparklines cannot mutate behind a
/// previously prepared page map.
pub(crate) fn external_render_dependency_cells(
    sheet: &Sheet,
    selected_ranges: &[RenderRange],
    options: &RenderOptions,
    geometry: Option<SheetGeometryOverride<'_>>,
    single_page_source_native: bool,
) -> Result<Vec<CellCoordinate>, RenderError> {
    let limits = &options.limits;
    let conditional_limit = limits
        .max_conditional_evaluations
        .checked_mul(2)
        .ok_or(RenderError::CoordinateOverflow)?;
    let aggregate_limit = conditional_limit
        .checked_add(limits.max_chart_points)
        .ok_or(RenderError::CoordinateOverflow)?;
    let mut dependencies = BTreeSet::new();
    let mut conditional_targets = 0_u64;

    for conditional in sheet.conditional_formats() {
        let references = match &conditional.rule {
            CfRule::CellIs {
                formula1, formula2, ..
            } => [Some(formula1.as_str()), formula2.as_deref()]
                .into_iter()
                .flatten()
                .filter_map(parse_conditional_operand)
                .filter_map(|operand| match operand {
                    ConditionalOperand::Reference(reference) => Some(reference),
                    ConditionalOperand::Literal(_) => None,
                })
                .collect::<Vec<_>>(),
            CfRule::Expression { formula, .. } => parse_conditional_expression(formula)
                .into_iter()
                .flat_map(|expression| [expression.left, expression.right])
                .filter_map(|operand| match operand {
                    ConditionalOperand::Reference(reference) => Some(reference),
                    ConditionalOperand::Literal(_) => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        if references.is_empty() {
            continue;
        }
        let mut rule_targets = BTreeSet::new();
        for &selected in selected_ranges {
            let Some(targets) = render_range_intersection(selected, conditional.sqref) else {
                continue;
            };
            for row in targets.first_row..=targets.last_row {
                for col in targets.first_col..=targets.last_col {
                    if rule_targets.insert(CellCoordinate { row, col }) {
                        conditional_targets = conditional_targets
                            .checked_add(1)
                            .ok_or(RenderError::CoordinateOverflow)?;
                        enforce(
                            LimitKind::ConditionalEvaluations,
                            limits.max_conditional_evaluations,
                            conditional_targets,
                        )?;
                    }
                }
            }
        }
        for target in rule_targets {
            for reference in &references {
                if let Some(coordinate) =
                    conditional_reference_coordinate(reference, sheet, target, conditional.sqref)
                {
                    dependencies.insert(coordinate);
                    enforce(LimitKind::Cells, aggregate_limit, dependencies.len() as u64)?;
                }
            }
        }
    }

    let mut chart_points = 0_u64;
    let metadata_index = DrawingMetadataIndex::new(sheet);
    let endpoint_policy = if single_page_source_native {
        AxisEndpointPolicy::SourceNative
    } else {
        AxisEndpointPolicy::PerTrackFixed
    };
    let renderable_charts =
        data_renderable_chart_indices(sheet, selected_ranges, options, geometry, endpoint_policy)?;
    for (chart_index, chart) in sheet.charts().iter().enumerate() {
        let metadata = metadata_index.get(DrawingObjectKind::Chart, chart_index);
        if chart.series.is_empty()
            || metadata.is_some_and(|metadata| !metadata.chart_unsupported_reasons.is_empty())
            || !renderable_charts.contains(&chart_index)
        {
            continue;
        }
        for series in &chart.series {
            add_a1_dependency_range(
                sheet,
                &series.values,
                &mut chart_points,
                &mut dependencies,
                limits,
                aggregate_limit,
            )?;
            if let Some(source) = series.categories.as_deref() {
                add_a1_dependency_range(
                    sheet,
                    source,
                    &mut chart_points,
                    &mut dependencies,
                    limits,
                    aggregate_limit,
                )?;
            }
            if let Some(source) = series.bubble_sizes.as_deref() {
                add_a1_dependency_range(
                    sheet,
                    source,
                    &mut chart_points,
                    &mut dependencies,
                    limits,
                    aggregate_limit,
                )?;
            }
        }
    }
    for sparkline in sheet.sparklines() {
        if !selected_ranges.iter().any(|range| {
            range.first_row <= sparkline.location.0
                && sparkline.location.0 <= range.last_row
                && range.first_col <= sparkline.location.1
                && sparkline.location.1 <= range.last_col
        }) {
            continue;
        }
        add_a1_dependency_range(
            sheet,
            &sparkline.range,
            &mut chart_points,
            &mut dependencies,
            limits,
            aggregate_limit,
        )?;
    }
    Ok(dependencies.into_iter().collect())
}

#[allow(clippy::too_many_arguments)]
fn push_drawing_placeholders(
    nodes: &mut Vec<SceneNode>,
    sheet: &Sheet,
    row_slots: &[AxisSlot<u32>],
    col_slots: &[AxisSlot<u16>],
    geometry: Option<SheetGeometryOverride<'_>>,
    cell_viewport: Rect,
    sheet_viewport: Rect,
    scene_width: Fixed,
    scene_height: Fixed,
    right_to_left: bool,
    calc_single_page_layout: bool,
    text_bytes: &mut u64,
    glyphs: &mut u64,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<(), RenderError> {
    let metadata_index = DrawingMetadataIndex::new(sheet);
    let drawing_row_slots = calc_drawing_row_slots(sheet, row_slots);
    // Calc keeps imported OOXML images on its drawing-row axis during print
    // replay. Charts and shapes continue to use the prepared cell-row axis.
    let prepared_image_row_slots =
        geometry.map(|geometry| calc_drawing_row_slots(sheet, geometry.rows));
    let image_geometry = match (geometry, prepared_image_row_slots.as_deref()) {
        (Some(geometry), Some(rows)) => Some(SheetGeometryOverride::new(rows, geometry.columns)),
        _ => None,
    };
    let mut placeholders = Vec::<DrawingPlaceholder>::new();
    let mut ordinal = 0_u64;
    for (index, image) in sheet.images().iter().enumerate() {
        let metadata = metadata_index.get(DrawingObjectKind::Image, index);
        let to = image.to.unwrap_or((
            image.from.0.saturating_add(10),
            image.from.1.saturating_add(4),
        ));
        match drawing_rect(
            &drawing_row_slots,
            col_slots,
            cell_viewport,
            sheet_viewport,
            scene_width,
            DrawingObjectKind::Image,
            image.from,
            to,
            metadata,
            right_to_left,
            image_geometry,
        )? {
            DrawingPlacement::Placed(rect) => placeholders.push(DrawingPlaceholder {
                kind: DrawingPlaceholderKind::Image(index),
                rect,
                z_order: metadata
                    .and_then(|metadata| metadata.z_order)
                    .map_or(ordinal as i64, i64::from),
                ordinal,
                source: CellCoordinate {
                    row: image.from.0,
                    col: image.from.1,
                },
                clip: drawing_clip(
                    DrawingObjectKind::Image,
                    rect,
                    metadata,
                    geometry,
                    cell_viewport,
                    scene_width,
                    scene_height,
                )?,
            }),
            DrawingPlacement::Unavailable => warnings.add(
                WarningCode::DrawingAnchorUnavailable,
                Some(CellCoordinate {
                    row: image.from.0,
                    col: image.from.1,
                }),
            ),
            DrawingPlacement::OutsideViewport => {}
        }
        ordinal = ordinal.saturating_add(1);
    }
    for (index, chart) in sheet.charts().iter().enumerate() {
        let metadata = metadata_index.get(DrawingObjectKind::Chart, index);
        match drawing_rect(
            row_slots,
            col_slots,
            cell_viewport,
            sheet_viewport,
            scene_width,
            DrawingObjectKind::Chart,
            chart.from,
            chart.to,
            metadata,
            right_to_left,
            geometry,
        )? {
            DrawingPlacement::Placed(rect) => placeholders.push(DrawingPlaceholder {
                kind: DrawingPlaceholderKind::Chart(index, chart.kind),
                rect,
                z_order: metadata
                    .and_then(|metadata| metadata.z_order)
                    .map_or(ordinal as i64, i64::from),
                ordinal,
                source: CellCoordinate {
                    row: chart.from.0,
                    col: chart.from.1,
                },
                clip: drawing_clip(
                    DrawingObjectKind::Chart,
                    rect,
                    metadata,
                    geometry,
                    cell_viewport,
                    scene_width,
                    scene_height,
                )?,
            }),
            DrawingPlacement::Unavailable => warnings.add(
                WarningCode::DrawingAnchorUnavailable,
                Some(CellCoordinate {
                    row: chart.from.0,
                    col: chart.from.1,
                }),
            ),
            DrawingPlacement::OutsideViewport => {}
        }
        ordinal = ordinal.saturating_add(1);
    }
    for metadata in sheet.drawing_metadata() {
        if metadata.kind != DrawingObjectKind::Shape {
            continue;
        }
        let Some(from) = metadata.from_cell else {
            warnings.add(WarningCode::ShapeAnchorUnavailable, None);
            ordinal = ordinal.saturating_add(1);
            continue;
        };
        let to = metadata.to_cell.unwrap_or(from);
        match drawing_rect(
            &drawing_row_slots,
            col_slots,
            cell_viewport,
            sheet_viewport,
            scene_width,
            DrawingObjectKind::Shape,
            from,
            to,
            Some(metadata),
            right_to_left,
            geometry,
        )? {
            DrawingPlacement::Placed(rect) => placeholders.push(DrawingPlaceholder {
                kind: DrawingPlaceholderKind::Shape,
                rect,
                z_order: metadata.z_order.map_or(ordinal as i64, i64::from),
                ordinal,
                source: CellCoordinate {
                    row: from.0,
                    col: from.1,
                },
                clip: drawing_clip(
                    DrawingObjectKind::Shape,
                    rect,
                    Some(metadata),
                    geometry,
                    cell_viewport,
                    scene_width,
                    scene_height,
                )?,
            }),
            DrawingPlacement::Unavailable => warnings.add(
                WarningCode::ShapeAnchorUnavailable,
                Some(CellCoordinate {
                    row: from.0,
                    col: from.1,
                }),
            ),
            DrawingPlacement::OutsideViewport => {}
        }
        ordinal = ordinal.saturating_add(1);
    }
    // LibreOffice Calc currently retains imported OOXML sparkline metadata but
    // does not paint those x14 worksheet extensions. Keep authored sparklines
    // fully renderable while deferring the imported paint so parity does not
    // invent an extra in-cell chart that the oracle omits.
    for (index, sparkline) in sheet.sparklines().iter().enumerate() {
        if sheet.style_fidelity() != StyleFidelity::Authored {
            continue;
        }
        let source = CellCoordinate {
            row: sparkline.location.0,
            col: sparkline.location.1,
        };
        match cell_rect(row_slots, col_slots, source, scene_width, right_to_left)? {
            Some(rect) => placeholders.push(DrawingPlaceholder {
                kind: DrawingPlaceholderKind::Sparkline(index, sparkline.kind),
                rect,
                z_order: i64::MAX,
                ordinal,
                source,
                clip: None,
            }),
            None => warnings.add(WarningCode::DrawingAnchorUnavailable, Some(source)),
        }
        ordinal = ordinal.saturating_add(1);
    }
    placeholders.sort_by_key(|placeholder| (placeholder.z_order, placeholder.ordinal));
    let mut decoded_media_bytes = 0_u64;
    let mut chart_points = 0_u64;
    for placeholder in placeholders {
        let mut object_nodes = Vec::new();
        match placeholder.kind {
            DrawingPlaceholderKind::Image(index) => {
                let image = &sheet.images()[index];
                let metadata = metadata_index.get(DrawingObjectKind::Image, index);
                match decode_image(
                    image,
                    metadata.and_then(|metadata| metadata.crop),
                    &options.limits,
                    &mut decoded_media_bytes,
                )? {
                    Some(decoded) => push_node(
                        &mut object_nodes,
                        SceneNode::Image(ImageNode {
                            rect: placeholder.rect,
                            pixel_width: decoded.width,
                            pixel_height: decoded.height,
                            rgba: Arc::from(decoded.rgba),
                            rotation_mdeg: metadata
                                .and_then(|metadata| metadata.rotation_mdeg)
                                .unwrap_or(0)
                                % 360_000,
                            alt_text: metadata.and_then(|metadata| metadata.alt_text.clone()),
                        }),
                        options,
                    )?,
                    None => {
                        push_image_placeholder(&mut object_nodes, placeholder.rect, options)?;
                        warnings.add(WarningCode::ImagePlaceholder, Some(placeholder.source));
                    }
                }
            }
            DrawingPlaceholderKind::Chart(index, kind) => {
                let metadata = metadata_index.get(DrawingObjectKind::Chart, index);
                if !try_push_chart_with_layout(
                    &mut object_nodes,
                    placeholder.rect,
                    &sheet.charts()[index],
                    metadata,
                    calc_single_page_layout,
                    sheet,
                    &mut chart_points,
                    text_bytes,
                    glyphs,
                    typography_stats,
                    options,
                    warnings,
                    placeholder.source,
                )? {
                    push_chart_placeholder(&mut object_nodes, placeholder.rect, kind, options)?;
                    warnings.add(WarningCode::ChartPlaceholder, Some(placeholder.source));
                }
            }
            DrawingPlaceholderKind::Sparkline(index, kind) => {
                if !try_push_sparkline(
                    &mut object_nodes,
                    placeholder.rect,
                    &sheet.sparklines()[index],
                    sheet,
                    &mut chart_points,
                    options,
                )? {
                    push_sparkline_placeholder(&mut object_nodes, placeholder.rect, kind, options)?;
                    warnings.add(WarningCode::SparklinePlaceholder, Some(placeholder.source));
                }
            }
            DrawingPlaceholderKind::Shape => {
                push_shape_placeholder(&mut object_nodes, placeholder.rect, options)?;
                warnings.add(WarningCode::ShapePlaceholder, Some(placeholder.source));
            }
        }
        append_drawing_nodes(nodes, placeholder.clip, object_nodes, options)?;
    }
    Ok(())
}

fn append_drawing_nodes(
    output: &mut Vec<SceneNode>,
    clip: Option<Rect>,
    children: Vec<SceneNode>,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    if children.is_empty() {
        return Ok(());
    }
    let child_count = scene_node_count(&children)?;
    let added = child_count
        .checked_add(u64::from(clip.is_some()))
        .ok_or(RenderError::CoordinateOverflow)?;
    let actual = scene_node_count(output)?
        .checked_add(added)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::SceneNodes,
        options.limits.max_scene_nodes,
        actual,
    )?;
    if let Some(clip) = clip {
        output.push(SceneNode::ClipGroup(ClipGroupNode {
            clip,
            nodes: children,
        }));
    } else {
        output.extend(children);
    }
    Ok(())
}

fn scene_node_count(nodes: &[SceneNode]) -> Result<u64, RenderError> {
    nodes.iter().try_fold(0_u64, |count, node| {
        let descendants = match node {
            SceneNode::ClipGroup(group) => scene_node_count(&group.nodes)?,
            _ => 0,
        };
        count
            .checked_add(1)
            .and_then(|count| count.checked_add(descendants))
            .ok_or(RenderError::CoordinateOverflow)
    })
}

struct DrawingMetadataIndex<'a> {
    images: Vec<Option<&'a DrawingMetadata>>,
    charts: Vec<Option<&'a DrawingMetadata>>,
}

impl<'a> DrawingMetadataIndex<'a> {
    fn new(sheet: &'a Sheet) -> Self {
        let mut index = Self {
            images: vec![None; sheet.images().len()],
            charts: vec![None; sheet.charts().len()],
        };
        for metadata in sheet.drawing_metadata() {
            let slot = match metadata.kind {
                DrawingObjectKind::Image => index.images.get_mut(metadata.object_index),
                DrawingObjectKind::Chart => index.charts.get_mut(metadata.object_index),
                _ => None,
            };
            if let Some(slot) = slot.filter(|slot| slot.is_none()) {
                *slot = Some(metadata);
            }
        }
        index
    }

    fn get(&self, kind: DrawingObjectKind, object_index: usize) -> Option<&'a DrawingMetadata> {
        match kind {
            DrawingObjectKind::Image => self.images.get(object_index).copied().flatten(),
            DrawingObjectKind::Chart => self.charts.get(object_index).copied().flatten(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DrawingLayoutViewport {
    /// Selected sheet-space viewport. Its origin is global sheet geometry and
    /// its width/height are the local scene dimensions before the 1px clamp.
    sheet: Rect,
    /// Cell-grid rectangle in local scene coordinates.
    cell: Rect,
}

fn is_sheet_absolute_metadata(metadata: Option<&DrawingMetadata>) -> bool {
    metadata.is_some_and(|metadata| {
        metadata.behavior == DrawingAnchorBehavior::Absolute && metadata.from_cell.is_none()
    })
}

fn absolute_drawing_positive_extent(sheet: &Sheet) -> Result<Option<(Fixed, Fixed)>, RenderError> {
    let metadata_index = DrawingMetadataIndex::new(sheet);
    let mut rightmost = Fixed::ZERO;
    let mut bottommost = Fixed::ZERO;
    let mut visible = false;
    for (kind, object_count) in [
        (DrawingObjectKind::Image, sheet.images().len()),
        (DrawingObjectKind::Chart, sheet.charts().len()),
    ] {
        for object_index in 0..object_count {
            let Some(rect) =
                absolute_drawing_paint_bounds(kind, metadata_index.get(kind, object_index))?
            else {
                continue;
            };
            let right = rect
                .x
                .checked_add(rect.width)
                .ok_or(RenderError::CoordinateOverflow)?;
            let bottom = rect
                .y
                .checked_add(rect.height)
                .ok_or(RenderError::CoordinateOverflow)?;
            if right <= Fixed::ZERO || bottom <= Fixed::ZERO {
                continue;
            }
            visible = true;
            rightmost = rightmost.max(right);
            bottommost = bottommost.max(bottom);
        }
    }
    Ok(visible.then_some((rightmost, bottommost)))
}

fn absolute_drawing_bounds(
    metadata: Option<&DrawingMetadata>,
) -> Result<Option<Rect>, RenderError> {
    let Some(metadata) = metadata.filter(|metadata| is_sheet_absolute_metadata(Some(metadata)))
    else {
        return Ok(None);
    };
    let (Some((x, y)), Some((width, height))) =
        (metadata.from_offset_emu, metadata.absolute_size_emu)
    else {
        return Ok(None);
    };
    if width == 0 || height == 0 {
        return Ok(None);
    }
    let left = emu_to_fixed(x)?;
    let top = emu_to_fixed(y)?;
    let width = emu_size_to_fixed(width)?;
    let height = emu_size_to_fixed(height)?;
    left.checked_add(width)
        .ok_or(RenderError::CoordinateOverflow)?;
    top.checked_add(height)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(Some(Rect {
        x: left,
        y: top,
        width,
        height,
    }))
}

fn absolute_drawing_paint_bounds(
    kind: DrawingObjectKind,
    metadata: Option<&DrawingMetadata>,
) -> Result<Option<Rect>, RenderError> {
    let Some(rect) = absolute_drawing_bounds(metadata)? else {
        return Ok(None);
    };
    if kind != DrawingObjectKind::Image {
        return Ok(Some(rect));
    }
    let rotation_mdeg = metadata
        .and_then(|metadata| metadata.rotation_mdeg)
        .unwrap_or(0);
    rotated_rect_bounds(rect, rotation_mdeg).map(Some)
}

fn rotated_rect_bounds(rect: Rect, rotation_mdeg: i32) -> Result<Rect, RenderError> {
    let rotation_mdeg = rotation_mdeg.rem_euclid(360_000);
    if rotation_mdeg == 0 || rotation_mdeg == 180_000 {
        return Ok(rect);
    }
    if rotation_mdeg == 90_000 || rotation_mdeg == 270_000 {
        return centered_rect_bounds(rect, rect.height.raw(), rect.width.raw());
    }

    let radians = f64::from(rotation_mdeg) * std::f64::consts::PI / 180_000.0;
    let cosine = radians.cos().abs();
    let sine = radians.sin().abs();
    let width = rect.width.raw() as f64;
    let height = rect.height.raw() as f64;
    let rotated_width = width * cosine + height * sine;
    let rotated_height = width * sine + height * cosine;
    let center_x = rect.x.raw() as f64 + width / 2.0;
    let center_y = rect.y.raw() as f64 + height / 2.0;
    // Expand by a scale-aware floating-point margin before rounding outward.
    // This prevents a backend-painted edge from being clipped when libm lands
    // immediately to the other side of an integer fixed-point boundary.
    let x_margin = ((center_x.abs() + rotated_width + 1.0) * f64::EPSILON * 8.0).max(1.0);
    let y_margin = ((center_y.abs() + rotated_height + 1.0) * f64::EPSILON * 8.0).max(1.0);
    let left = f64_floor_to_i64(center_x - rotated_width / 2.0 - x_margin)?;
    let right = f64_ceil_to_i64(center_x + rotated_width / 2.0 + x_margin)?;
    let top = f64_floor_to_i64(center_y - rotated_height / 2.0 - y_margin)?;
    let bottom = f64_ceil_to_i64(center_y + rotated_height / 2.0 + y_margin)?;
    Ok(Rect {
        x: Fixed::from_raw(left),
        y: Fixed::from_raw(top),
        width: Fixed::from_raw(
            right
                .checked_sub(left)
                .ok_or(RenderError::CoordinateOverflow)?,
        ),
        height: Fixed::from_raw(
            bottom
                .checked_sub(top)
                .ok_or(RenderError::CoordinateOverflow)?,
        ),
    })
}

fn centered_rect_bounds(
    rect: Rect,
    rotated_width: i64,
    rotated_height: i64,
) -> Result<Rect, RenderError> {
    let center_x_twice = i128::from(rect.x.raw())
        .checked_mul(2)
        .and_then(|value| value.checked_add(i128::from(rect.width.raw())))
        .ok_or(RenderError::CoordinateOverflow)?;
    let center_y_twice = i128::from(rect.y.raw())
        .checked_mul(2)
        .and_then(|value| value.checked_add(i128::from(rect.height.raw())))
        .ok_or(RenderError::CoordinateOverflow)?;
    let left = floor_half(center_x_twice - i128::from(rotated_width))?;
    let right = ceil_half(center_x_twice + i128::from(rotated_width))?;
    let top = floor_half(center_y_twice - i128::from(rotated_height))?;
    let bottom = ceil_half(center_y_twice + i128::from(rotated_height))?;
    Ok(Rect {
        x: Fixed::from_raw(left),
        y: Fixed::from_raw(top),
        width: Fixed::from_raw(
            right
                .checked_sub(left)
                .ok_or(RenderError::CoordinateOverflow)?,
        ),
        height: Fixed::from_raw(
            bottom
                .checked_sub(top)
                .ok_or(RenderError::CoordinateOverflow)?,
        ),
    })
}

fn floor_half(value: i128) -> Result<i64, RenderError> {
    let quotient = value.div_euclid(2);
    i64::try_from(quotient).map_err(|_| RenderError::CoordinateOverflow)
}

fn ceil_half(value: i128) -> Result<i64, RenderError> {
    let quotient = value
        .checked_add(1)
        .ok_or(RenderError::CoordinateOverflow)?
        .div_euclid(2);
    i64::try_from(quotient).map_err(|_| RenderError::CoordinateOverflow)
}

fn f64_floor_to_i64(value: f64) -> Result<i64, RenderError> {
    let value = value.floor();
    if !value.is_finite() || value < i64::MIN as f64 || value >= 9_223_372_036_854_775_808.0 {
        return Err(RenderError::CoordinateOverflow);
    }
    Ok(value as i64)
}

fn f64_ceil_to_i64(value: f64) -> Result<i64, RenderError> {
    let value = value.ceil();
    if !value.is_finite() || value < i64::MIN as f64 || value >= 9_223_372_036_854_775_808.0 {
        return Err(RenderError::CoordinateOverflow);
    }
    Ok(value as i64)
}

fn rect_intersects_positive_sheet(rect: Rect) -> bool {
    let right = i128::from(rect.x.raw()) + i128::from(rect.width.raw());
    let bottom = i128::from(rect.y.raw()) + i128::from(rect.height.raw());
    right > 0 && bottom > 0
}

#[allow(clippy::too_many_arguments)]
fn drawing_layout_viewport(
    sheet: &Sheet,
    range: RenderRange,
    row_slots: &[AxisSlot<u32>],
    grid_width: Fixed,
    grid_height: Fixed,
    maximum_digit_width: Fixed,
    used_selection: bool,
    options: &RenderOptions,
    geometry: Option<SheetGeometryOverride<'_>>,
    endpoint_policy: AxisEndpointPolicy,
    warnings: &mut Warnings,
) -> Result<DrawingLayoutViewport, RenderError> {
    let absolute_extent = absolute_drawing_positive_extent(sheet)?;
    let Some((absolute_right, absolute_bottom)) = absolute_extent else {
        return Ok(DrawingLayoutViewport {
            sheet: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: grid_width,
                height: grid_height,
            },
            cell: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: grid_width,
                height: grid_height,
            },
        });
    };
    let (grid_x, grid_y) = prepared_sheet_grid_origin(
        sheet,
        range,
        row_slots,
        maximum_digit_width,
        options,
        geometry,
        endpoint_policy,
        warnings,
    )?;
    if used_selection {
        let grid_right = grid_x
            .checked_add(grid_width)
            .ok_or(RenderError::CoordinateOverflow)?;
        let grid_bottom = grid_y
            .checked_add(grid_height)
            .ok_or(RenderError::CoordinateOverflow)?;
        Ok(DrawingLayoutViewport {
            sheet: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: grid_right.max(absolute_right),
                height: grid_bottom.max(absolute_bottom),
            },
            cell: Rect {
                x: grid_x,
                y: grid_y,
                width: grid_width,
                height: grid_height,
            },
        })
    } else {
        Ok(DrawingLayoutViewport {
            sheet: Rect {
                x: grid_x,
                y: grid_y,
                width: grid_width,
                height: grid_height,
            },
            cell: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: grid_width,
                height: grid_height,
            },
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn prepared_sheet_grid_origin(
    sheet: &Sheet,
    range: RenderRange,
    row_slots: &[AxisSlot<u32>],
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    geometry: Option<SheetGeometryOverride<'_>>,
    endpoint_policy: AxisEndpointPolicy,
    warnings: &mut Warnings,
) -> Result<(Fixed, Fixed), RenderError> {
    let (x, persisted_y) = sheet_grid_origin_with_policy(
        sheet,
        range,
        maximum_digit_width,
        options,
        endpoint_policy,
        warnings,
    )?;
    let Some(geometry) = geometry else {
        return Ok((x, persisted_y));
    };
    let Some(first_prepared_row) = geometry.rows.first().map(|slot| slot.index) else {
        return Ok((x, persisted_y));
    };
    let (_, prepared_base_y) = sheet_grid_origin_with_policy(
        sheet,
        RenderRange::new(
            first_prepared_row,
            range.first_col,
            first_prepared_row,
            range.first_col,
        ),
        maximum_digit_width,
        options,
        endpoint_policy,
        warnings,
    )?;
    let prepared_y = prepared_base_y
        .checked_add(prepared_row_offset(geometry.rows, row_slots)?)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok((x, prepared_y))
}

fn sheet_grid_origin(
    sheet: &Sheet,
    range: RenderRange,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<(Fixed, Fixed), RenderError> {
    sheet_grid_origin_with_policy(
        sheet,
        range,
        maximum_digit_width,
        options,
        AxisEndpointPolicy::PerTrackFixed,
        warnings,
    )
}

fn sheet_grid_origin_with_policy(
    sheet: &Sheet,
    range: RenderRange,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    endpoint_policy: AxisEndpointPolicy,
    warnings: &mut Warnings,
) -> Result<(Fixed, Fixed), RenderError> {
    if endpoint_policy == AxisEndpointPolicy::SourceNative {
        let x = calc_twips_position_to_fixed(source_native_column_prefix(
            sheet,
            range.first_col,
            maximum_digit_width,
            options,
            warnings,
        )?)
        .ok_or(RenderError::CoordinateOverflow)?;
        let y = calc_twips_position_to_fixed(source_native_row_prefix(
            sheet,
            range.first_row,
            maximum_digit_width,
            options,
            warnings,
        )?)
        .ok_or(RenderError::CoordinateOverflow)?;
        return Ok((x, y));
    }

    let mut x = Fixed::ZERO;
    for column in 0..range.first_col {
        if !options.include_hidden && sheet.hidden_columns().contains(&column) {
            continue;
        }
        x = x
            .checked_add(column_width(
                sheet,
                column,
                maximum_digit_width,
                options,
                warnings,
            ))
            .ok_or(RenderError::CoordinateOverflow)?;
    }

    // A sheet-absolute object does not move with renderer-derived automatic
    // text height. Its sheet-space row boundary therefore follows persisted
    // default/explicit row geometry. Compute that prefix sparsely instead of
    // scanning up to Excel's million-row ceiling.
    let base_row_height = match sheet.default_row_height().and_then(points_to_fixed) {
        Some(height) => height,
        None => fallback_row_height(sheet, options),
    };
    let visible_rows = if options.include_hidden {
        u64::from(range.first_row)
    } else if let Some(exceptions) = sheet.default_hidden_row_exceptions() {
        exceptions
            .range(..range.first_row)
            .filter(|&&row| !sheet.hidden_rows().contains(&row))
            .count() as u64
    } else {
        u64::from(range.first_row)
            .saturating_sub(sheet.hidden_rows().range(..range.first_row).count() as u64)
    };
    let mut y_raw = i128::from(base_row_height.raw())
        .checked_mul(i128::from(visible_rows))
        .ok_or(RenderError::CoordinateOverflow)?;
    for (&row, _) in sheet.row_heights().range(..range.first_row) {
        if !options.include_hidden && row_is_hidden(sheet, row) {
            continue;
        }
        let height = row_height(sheet, row, options, warnings);
        y_raw = y_raw
            .checked_add(i128::from(height.raw()) - i128::from(base_row_height.raw()))
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    let y = Fixed::from_raw(i64::try_from(y_raw).map_err(|_| RenderError::CoordinateOverflow)?);
    Ok((x, y))
}

fn offset_axis_slots<I>(
    slots: &mut [MeasuredAxisSlot<I>],
    offset: Fixed,
) -> Result<(), RenderError> {
    if offset == Fixed::ZERO {
        return Ok(());
    }
    for slot in slots {
        slot.offset = slot
            .offset
            .checked_add(offset)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(())
}

/// Test exact prepared sheet-space paint geometry for cell-anchored drawings
/// against one print tile. Rotation is evaluated after the full unrotated
/// destination rectangle is established, so a continuation beyond the final
/// anchor cell remains eligible for sparse-page retention.
pub(crate) fn cell_drawings_intersect_prepared_range(
    sheet: &Sheet,
    range: RenderRange,
    geometry: SheetGeometryOverride<'_>,
) -> Result<bool, RenderError> {
    let range = range.validate()?;
    let row_slots = geometry
        .rows
        .iter()
        .copied()
        .filter(|slot| slot.index >= range.first_row && slot.index <= range.last_row)
        .collect::<Vec<_>>();
    let image_geometry_rows = calc_drawing_row_slots(sheet, geometry.rows);
    let image_row_slots = image_geometry_rows
        .iter()
        .copied()
        .filter(|slot| slot.index >= range.first_row && slot.index <= range.last_row)
        .collect::<Vec<_>>();
    let image_geometry = SheetGeometryOverride::new(&image_geometry_rows, geometry.columns);
    let col_slots = geometry
        .columns
        .iter()
        .copied()
        .filter(|slot| slot.index >= range.first_col && slot.index <= range.last_col)
        .collect::<Vec<_>>();
    if row_slots.is_empty() || col_slots.is_empty() {
        return Ok(false);
    }
    let viewport = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: sum_fixed(col_slots.iter().map(|slot| slot.size))?,
        height: sum_fixed(row_slots.iter().map(|slot| slot.size))?,
    };
    let metadata_index = DrawingMetadataIndex::new(sheet);
    let right_to_left = sheet.sheet_view().right_to_left;

    for (index, image) in sheet.images().iter().enumerate() {
        let metadata = metadata_index.get(DrawingObjectKind::Image, index);
        if is_sheet_absolute_metadata(metadata) {
            continue;
        }
        let to = image.to.unwrap_or((
            image.from.0.saturating_add(10),
            image.from.1.saturating_add(4),
        ));
        if matches!(
            drawing_rect(
                &image_row_slots,
                &col_slots,
                viewport,
                viewport,
                viewport.width,
                DrawingObjectKind::Image,
                image.from,
                to,
                metadata,
                right_to_left,
                Some(image_geometry),
            )?,
            DrawingPlacement::Placed(_)
        ) {
            return Ok(true);
        }
    }
    for (index, chart) in sheet.charts().iter().enumerate() {
        let metadata = metadata_index.get(DrawingObjectKind::Chart, index);
        if is_sheet_absolute_metadata(metadata) {
            continue;
        }
        if matches!(
            drawing_rect(
                &row_slots,
                &col_slots,
                viewport,
                viewport,
                viewport.width,
                DrawingObjectKind::Chart,
                chart.from,
                chart.to,
                metadata,
                right_to_left,
                Some(geometry),
            )?,
            DrawingPlacement::Placed(_)
        ) {
            return Ok(true);
        }
    }
    for metadata in sheet
        .drawing_metadata()
        .iter()
        .filter(|metadata| metadata.kind == DrawingObjectKind::Shape)
    {
        let Some(from) = metadata.from_cell else {
            continue;
        };
        let to = metadata.to_cell.unwrap_or(from);
        if matches!(
            drawing_rect(
                &row_slots,
                &col_slots,
                viewport,
                viewport,
                viewport.width,
                DrawingObjectKind::Shape,
                from,
                to,
                Some(metadata),
                right_to_left,
                Some(geometry),
            )?,
            DrawingPlacement::Placed(_)
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn absolute_drawings_intersect_range(
    sheet: &Sheet,
    range: RenderRange,
    width: Fixed,
    height: Fixed,
    options: &RenderOptions,
    geometry: SheetGeometryOverride<'_>,
) -> Result<bool, RenderError> {
    if width <= Fixed::ZERO || height <= Fixed::ZERO {
        return Ok(false);
    }
    let range = range.validate()?;
    let row_slots = geometry
        .rows
        .iter()
        .copied()
        .filter(|slot| slot.index >= range.first_row && slot.index <= range.last_row)
        .collect::<Vec<_>>();
    if row_slots.is_empty() {
        return Ok(false);
    }
    let mut warnings = Warnings::default();
    let mut typography = TypographyStats::default();
    let style_snapshot = RenderStyleSnapshot::new(sheet);
    let maximum_digit_width =
        maximum_digit_width(&style_snapshot, options, &mut warnings, &mut typography)?;
    let (x, y) = prepared_sheet_grid_origin(
        sheet,
        range,
        &row_slots,
        maximum_digit_width,
        options,
        Some(geometry),
        AxisEndpointPolicy::PerTrackFixed,
        &mut warnings,
    )?;
    let viewport = Rect {
        x,
        y,
        width,
        height,
    };
    let metadata_index = DrawingMetadataIndex::new(sheet);
    for (kind, object_count) in [
        (DrawingObjectKind::Image, sheet.images().len()),
        (DrawingObjectKind::Chart, sheet.charts().len()),
    ] {
        for object_index in 0..object_count {
            if absolute_drawing_paint_bounds(kind, metadata_index.get(kind, object_index))?
                .is_some_and(|rect| rectangles_intersect(rect, viewport))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn rectangles_intersect(left: Rect, right: Rect) -> bool {
    let left_right = i128::from(left.x.raw()) + i128::from(left.width.raw());
    let left_bottom = i128::from(left.y.raw()) + i128::from(left.height.raw());
    let right_right = i128::from(right.x.raw()) + i128::from(right.width.raw());
    let right_bottom = i128::from(right.y.raw()) + i128::from(right.height.raw());
    i128::from(left.x.raw()) < right_right
        && left_right > i128::from(right.x.raw())
        && i128::from(left.y.raw()) < right_bottom
        && left_bottom > i128::from(right.y.raw())
}

fn fixed_as_pixels(value: Fixed) -> f64 {
    value.raw() as f64 / FIXED_UNITS_PER_PIXEL as f64
}

fn rounded_scaled_raw(value: i64, scale: f64) -> Result<i128, RenderError> {
    if !scale.is_finite() {
        return Err(RenderError::CoordinateOverflow);
    }
    if value == 0 || scale == 0.0 {
        return Ok(0);
    }

    // Decode the binary float so boundary checks use the exact represented
    // scale. Casting a large i64 to f64 first can round away a one-unit
    // overflow at either signed boundary.
    let bits = scale.to_bits();
    let exponent_bits = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1_u64 << 52) - 1);
    let (significand, binary_exponent) = if exponent_bits == 0 {
        (fraction, -1_074)
    } else {
        ((1_u64 << 52) | fraction, exponent_bits - 1_023 - 52)
    };
    if significand == 0 {
        return Ok(0);
    }

    let numerator = u128::from(value.unsigned_abs())
        .checked_mul(u128::from(significand))
        .ok_or(RenderError::CoordinateOverflow)?;
    let magnitude = if binary_exponent >= 0 {
        let factor = 1_u128
            .checked_shl(binary_exponent as u32)
            .ok_or(RenderError::CoordinateOverflow)?;
        numerator
            .checked_mul(factor)
            .ok_or(RenderError::CoordinateOverflow)?
    } else {
        let shift = binary_exponent.unsigned_abs();
        if shift >= u128::BITS {
            0
        } else {
            let truncated = numerator >> shift;
            let remainder_mask = (1_u128 << shift) - 1;
            let halfway = 1_u128 << (shift - 1);
            truncated
                .checked_add(u128::from(numerator & remainder_mask >= halfway))
                .ok_or(RenderError::CoordinateOverflow)?
        }
    };

    // Two signed i64 endpoints differ by at most u64::MAX. A larger rounded
    // delta cannot be brought back into range by any valid start coordinate.
    if magnitude > u128::from(u64::MAX) {
        return Err(RenderError::CoordinateOverflow);
    }
    let magnitude = i128::try_from(magnitude).map_err(|_| RenderError::CoordinateOverflow)?;
    Ok(if (value < 0) ^ scale.is_sign_negative() {
        -magnitude
    } else {
        magnitude
    })
}

fn pixels_as_fixed(value: f64) -> Result<Fixed, RenderError> {
    let raw = rounded_scaled_raw(FIXED_UNITS_PER_PIXEL, value)?;
    Ok(Fixed::from_raw(
        i64::try_from(raw).map_err(|_| RenderError::CoordinateOverflow)?,
    ))
}

fn drawing_clip(
    kind: DrawingObjectKind,
    rect: Rect,
    metadata: Option<&DrawingMetadata>,
    geometry: Option<SheetGeometryOverride<'_>>,
    cell_viewport: Rect,
    scene_width: Fixed,
    scene_height: Fixed,
) -> Result<Option<Rect>, RenderError> {
    if is_sheet_absolute_metadata(metadata) {
        return Ok(Some(Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: scene_width,
            height: scene_height,
        }));
    }
    if geometry.is_none() {
        return Ok(None);
    }
    let paint_bounds = if kind == DrawingObjectKind::Image {
        rotated_rect_bounds(
            rect,
            metadata
                .and_then(|metadata| metadata.rotation_mdeg)
                .unwrap_or(0),
        )?
    } else {
        rect
    };
    Ok((!rect_contains(cell_viewport, paint_bounds)).then_some(cell_viewport))
}

fn rect_contains(outer: Rect, inner: Rect) -> bool {
    let outer_right = i128::from(outer.x.raw()) + i128::from(outer.width.raw());
    let outer_bottom = i128::from(outer.y.raw()) + i128::from(outer.height.raw());
    let inner_right = i128::from(inner.x.raw()) + i128::from(inner.width.raw());
    let inner_bottom = i128::from(inner.y.raw()) + i128::from(inner.height.raw());
    i128::from(inner.x.raw()) >= i128::from(outer.x.raw())
        && i128::from(inner.y.raw()) >= i128::from(outer.y.raw())
        && inner_right <= outer_right
        && inner_bottom <= outer_bottom
}

#[allow(clippy::too_many_arguments)]
fn drawing_rect(
    row_slots: &[AxisSlot<u32>],
    col_slots: &[AxisSlot<u16>],
    cell_viewport: Rect,
    sheet_viewport: Rect,
    scene_width: Fixed,
    kind: DrawingObjectKind,
    from: (u32, u16),
    to: (u32, u16),
    metadata: Option<&DrawingMetadata>,
    right_to_left: bool,
    geometry: Option<SheetGeometryOverride<'_>>,
) -> Result<DrawingPlacement, RenderError> {
    if is_sheet_absolute_metadata(metadata) {
        let Some(mut rect) = absolute_drawing_bounds(metadata)? else {
            return Ok(DrawingPlacement::Unavailable);
        };
        let Some(paint_bounds) = absolute_drawing_paint_bounds(kind, metadata)? else {
            return Ok(DrawingPlacement::Unavailable);
        };
        if !rectangles_intersect(paint_bounds, sheet_viewport) {
            return Ok(DrawingPlacement::OutsideViewport);
        }
        rect.x = rect
            .x
            .checked_sub(sheet_viewport.x)
            .ok_or(RenderError::CoordinateOverflow)?;
        rect.y = rect
            .y
            .checked_sub(sheet_viewport.y)
            .ok_or(RenderError::CoordinateOverflow)?;
        return Ok(DrawingPlacement::Placed(if right_to_left {
            reflect_rect_horizontally(rect, scene_width)?
        } else {
            rect
        }));
    }
    if let Some(geometry) = geometry {
        return prepared_cell_drawing_rect(
            row_slots,
            col_slots,
            cell_viewport,
            kind,
            from,
            to,
            metadata,
            right_to_left,
            geometry,
        );
    }
    if row_slots.is_empty() || col_slots.is_empty() {
        return Ok(DrawingPlacement::Unavailable);
    }
    let first_row = row_slots.first().map_or(0, |slot| slot.index);
    let last_row = row_slots.last().map_or(0, |slot| slot.index);
    let first_col = col_slots.first().map_or(0, |slot| slot.index);
    let last_col = col_slots.last().map_or(0, |slot| slot.index);
    // A drawing can begin on an earlier print tile and remain visible on this
    // tile. Treat anchors as an intersecting interval rather than requiring the
    // top-left marker to be selected. This is also what lets paginated output
    // retain images/charts across a row or column break.
    if from.0 > last_row || from.1 > last_col || to.0 < first_row || to.1 < first_col {
        return Ok(DrawingPlacement::OutsideViewport);
    }
    let clipped_from_row = from.0 < first_row;
    let clipped_from_col = from.1 < first_col;
    let cell_right = cell_viewport
        .x
        .checked_add(cell_viewport.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let cell_bottom = cell_viewport
        .y
        .checked_add(cell_viewport.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let mut left = row_or_column_boundary_col(col_slots, from.1, cell_right);
    let mut top = row_or_column_boundary_row(row_slots, from.0, cell_bottom);
    if let Some((x, y)) = metadata.and_then(|metadata| metadata.from_offset_emu) {
        // An offset belonging to a marker before the selected range has
        // already been consumed by the clipped-away portion of the drawing.
        if !clipped_from_col {
            left = left
                .checked_add(emu_to_fixed(x)?)
                .ok_or(RenderError::CoordinateOverflow)?;
        }
        if !clipped_from_row {
            top = top
                .checked_add(emu_to_fixed(y)?)
                .ok_or(RenderError::CoordinateOverflow)?;
        }
    }

    let mut anchored_right = row_or_column_boundary_col(col_slots, to.1, cell_right);
    let mut anchored_bottom = row_or_column_boundary_row(row_slots, to.0, cell_bottom);
    if let Some((x, y)) = metadata.and_then(|metadata| metadata.to_offset_emu) {
        if to.1 <= last_col {
            anchored_right = anchored_right
                .checked_add(emu_to_fixed(x)?)
                .ok_or(RenderError::CoordinateOverflow)?;
        }
        if to.0 <= last_row {
            anchored_bottom = anchored_bottom
                .checked_add(emu_to_fixed(y)?)
                .ok_or(RenderError::CoordinateOverflow)?;
        }
    }
    let (right, bottom) =
        if let Some((width, height)) = metadata.and_then(|metadata| metadata.absolute_size_emu) {
            // When the start marker was clipped away, its absolute origin is no
            // longer available in the selected sparse axis. The retained end
            // marker is the exact bounded continuation edge for that dimension.
            let right = if clipped_from_col {
                anchored_right
            } else {
                left.checked_add(emu_size_to_fixed(width)?)
                    .ok_or(RenderError::CoordinateOverflow)?
            };
            let bottom = if clipped_from_row {
                anchored_bottom
            } else {
                top.checked_add(emu_size_to_fixed(height)?)
                    .ok_or(RenderError::CoordinateOverflow)?
            };
            (right, bottom)
        } else {
            (anchored_right, anchored_bottom)
        };
    let Some(rect) = clip_to_rect(left, top, right, bottom, cell_viewport)? else {
        return Ok(DrawingPlacement::OutsideViewport);
    };
    Ok(DrawingPlacement::Placed(if right_to_left {
        reflect_rect_horizontally(rect, scene_width)?
    } else {
        rect
    }))
}

#[allow(clippy::too_many_arguments)]
fn prepared_cell_drawing_rect(
    row_slots: &[AxisSlot<u32>],
    col_slots: &[AxisSlot<u16>],
    cell_viewport: Rect,
    kind: DrawingObjectKind,
    from: (u32, u16),
    to: (u32, u16),
    metadata: Option<&DrawingMetadata>,
    right_to_left: bool,
    geometry: SheetGeometryOverride<'_>,
) -> Result<DrawingPlacement, RenderError> {
    if row_slots.is_empty()
        || col_slots.is_empty()
        || geometry.rows.is_empty()
        || geometry.columns.is_empty()
    {
        return Ok(DrawingPlacement::Unavailable);
    }
    let full_width = axis_slots_end(geometry.columns)?;
    let full_height = axis_slots_end(geometry.rows)?;
    let full_first_row = geometry.rows.first().map_or(0, |slot| slot.index);
    let full_last_row = geometry.rows.last().map_or(0, |slot| slot.index);
    let full_first_col = geometry.columns.first().map_or(0, |slot| slot.index);
    let full_last_col = geometry.columns.last().map_or(0, |slot| slot.index);
    let clipped_from_row = from.0 < full_first_row;
    let clipped_from_col = from.1 < full_first_col;
    let mut left = row_or_column_boundary_col(geometry.columns, from.1, full_width);
    let mut top = row_or_column_boundary_row(geometry.rows, from.0, full_height);
    if let Some((x, y)) = metadata.and_then(|metadata| metadata.from_offset_emu) {
        if !clipped_from_col {
            left = left
                .checked_add(emu_to_fixed(x)?)
                .ok_or(RenderError::CoordinateOverflow)?;
        }
        if !clipped_from_row {
            top = top
                .checked_add(emu_to_fixed(y)?)
                .ok_or(RenderError::CoordinateOverflow)?;
        }
    }

    let mut anchored_right = row_or_column_boundary_col(geometry.columns, to.1, full_width);
    let mut anchored_bottom = row_or_column_boundary_row(geometry.rows, to.0, full_height);
    if let Some((x, y)) = metadata.and_then(|metadata| metadata.to_offset_emu) {
        if to.1 <= full_last_col {
            anchored_right = anchored_right
                .checked_add(emu_to_fixed(x)?)
                .ok_or(RenderError::CoordinateOverflow)?;
        }
        if to.0 <= full_last_row {
            anchored_bottom = anchored_bottom
                .checked_add(emu_to_fixed(y)?)
                .ok_or(RenderError::CoordinateOverflow)?;
        }
    }
    let (right, bottom) =
        if let Some((width, height)) = metadata.and_then(|metadata| metadata.absolute_size_emu) {
            let right = if clipped_from_col {
                anchored_right
            } else {
                left.checked_add(emu_size_to_fixed(width)?)
                    .ok_or(RenderError::CoordinateOverflow)?
            };
            let bottom = if clipped_from_row {
                anchored_bottom
            } else {
                top.checked_add(emu_size_to_fixed(height)?)
                    .ok_or(RenderError::CoordinateOverflow)?
            };
            (right, bottom)
        } else {
            (anchored_right, anchored_bottom)
        };
    if right <= left || bottom <= top {
        return Ok(DrawingPlacement::OutsideViewport);
    }
    let mut global_rect = Rect {
        x: left,
        y: top,
        width: right
            .checked_sub(left)
            .ok_or(RenderError::CoordinateOverflow)?,
        height: bottom
            .checked_sub(top)
            .ok_or(RenderError::CoordinateOverflow)?,
    };
    if right_to_left {
        global_rect = reflect_rect_horizontally(global_rect, full_width)?;
    }

    let tile_y = prepared_row_offset(geometry.rows, row_slots)?;
    let tile_x = prepared_column_offset(geometry.columns, col_slots, full_width, right_to_left)?;
    let tile = Rect {
        x: tile_x,
        y: tile_y,
        width: cell_viewport.width,
        height: cell_viewport.height,
    };
    let paint_bounds = if kind == DrawingObjectKind::Image {
        rotated_rect_bounds(
            global_rect,
            metadata
                .and_then(|metadata| metadata.rotation_mdeg)
                .unwrap_or(0),
        )?
    } else {
        global_rect
    };
    if !rectangles_intersect(paint_bounds, tile) {
        return Ok(DrawingPlacement::OutsideViewport);
    }

    global_rect.x = global_rect
        .x
        .checked_sub(tile_x)
        .and_then(|value| value.checked_add(cell_viewport.x))
        .ok_or(RenderError::CoordinateOverflow)?;
    global_rect.y = global_rect
        .y
        .checked_sub(tile_y)
        .and_then(|value| value.checked_add(cell_viewport.y))
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(DrawingPlacement::Placed(global_rect))
}

fn prepared_row_offset(
    geometry: &[AxisSlot<u32>],
    local: &[AxisSlot<u32>],
) -> Result<Fixed, RenderError> {
    let index = local
        .first()
        .map(|slot| slot.index)
        .ok_or(RenderError::CoordinateOverflow)?;
    geometry
        .binary_search_by_key(&index, |slot| slot.index)
        .ok()
        .and_then(|position| geometry.get(position))
        .map(|slot| slot.offset)
        .ok_or(RenderError::Backend {
            reason: "prepared_print_geometry_missing_axis_slot",
        })
}

fn prepared_column_offset(
    geometry: &[AxisSlot<u16>],
    local: &[AxisSlot<u16>],
    full_width: Fixed,
    right_to_left: bool,
) -> Result<Fixed, RenderError> {
    local
        .iter()
        .map(|local| {
            let slot = geometry
                .binary_search_by_key(&local.index, |slot| slot.index)
                .ok()
                .and_then(|position| geometry.get(position))
                .ok_or(RenderError::Backend {
                    reason: "prepared_print_geometry_missing_axis_slot",
                })?;
            if right_to_left {
                reflected_x(slot.offset, slot.size, full_width)
            } else {
                Ok(slot.offset)
            }
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .min()
        .ok_or(RenderError::CoordinateOverflow)
}

fn row_or_column_boundary_row(slots: &[AxisSlot<u32>], index: u32, total: Fixed) -> Fixed {
    slots
        .iter()
        .find(|slot| slot.index >= index)
        .map_or(total, |slot| slot.offset)
}

fn calc_drawing_row_slots(sheet: &Sheet, slots: &[AxisSlot<u32>]) -> Vec<AxisSlot<u32>> {
    if sheet.implicit_ooxml_row_height_source()
        != Some(OoxmlImplicitRowHeight::XlsxApplicationDefault)
        || sheet.default_row_height().is_some()
    {
        return slots.to_vec();
    }
    let mut offset = slots.first().map_or(Fixed::ZERO, |slot| slot.offset);
    slots
        .iter()
        .map(|slot| {
            let size = if sheet.row_heights().contains_key(&slot.index) {
                slot.size
            } else {
                CALC_OOXML_DRAWING_DEFAULT_ROW_HEIGHT
            };
            let current = MeasuredAxisSlot {
                index: slot.index,
                offset,
                size,
            };
            offset = Fixed::from_raw(offset.raw().saturating_add(size.raw()));
            current
        })
        .collect()
}

fn row_or_column_boundary_col(slots: &[AxisSlot<u16>], index: u16, total: Fixed) -> Fixed {
    slots
        .iter()
        .find(|slot| slot.index >= index)
        .map_or(total, |slot| slot.offset)
}

fn cell_rect(
    row_slots: &[AxisSlot<u32>],
    col_slots: &[AxisSlot<u16>],
    coordinate: CellCoordinate,
    canvas_width: Fixed,
    right_to_left: bool,
) -> Result<Option<Rect>, RenderError> {
    let Some(row) = row_slots.iter().find(|slot| slot.index == coordinate.row) else {
        return Ok(None);
    };
    let Some(col) = col_slots.iter().find(|slot| slot.index == coordinate.col) else {
        return Ok(None);
    };
    let rect = Rect {
        x: col.offset,
        y: row.offset,
        width: col.size,
        height: row.size,
    };
    Ok(Some(if right_to_left {
        reflect_rect_horizontally(rect, canvas_width)?
    } else {
        rect
    }))
}

fn emu_to_fixed(emu: i64) -> Result<Fixed, RenderError> {
    let scaled = i128::from(emu)
        .checked_mul(i128::from(FIXED_UNITS_PER_PIXEL))
        .ok_or(RenderError::CoordinateOverflow)?;
    let rounded = if scaled >= 0 {
        scaled + 4_762
    } else {
        scaled - 4_762
    } / 9_525;
    Ok(Fixed::from_raw(
        i64::try_from(rounded).map_err(|_| RenderError::CoordinateOverflow)?,
    ))
}

fn emu_size_to_fixed(emu: u64) -> Result<Fixed, RenderError> {
    let emu = i64::try_from(emu).map_err(|_| RenderError::CoordinateOverflow)?;
    emu_to_fixed(emu).map(|value| value.max(Fixed::from_raw(1)))
}

fn clip_to_rect(
    left: Fixed,
    top: Fixed,
    right: Fixed,
    bottom: Fixed,
    bounds: Rect,
) -> Result<Option<Rect>, RenderError> {
    let bounds_right = bounds
        .x
        .checked_add(bounds.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bounds_bottom = bounds
        .y
        .checked_add(bounds.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let left = Fixed::from_raw(left.raw().clamp(bounds.x.raw(), bounds_right.raw()));
    let top = Fixed::from_raw(top.raw().clamp(bounds.y.raw(), bounds_bottom.raw()));
    let right = Fixed::from_raw(right.raw().clamp(bounds.x.raw(), bounds_right.raw()));
    let bottom = Fixed::from_raw(bottom.raw().clamp(bounds.y.raw(), bounds_bottom.raw()));
    if right <= left || bottom <= top {
        return Ok(None);
    }
    Ok(Some(Rect {
        x: left,
        y: top,
        width: right
            .checked_sub(left)
            .ok_or(RenderError::CoordinateOverflow)?,
        height: bottom
            .checked_sub(top)
            .ok_or(RenderError::CoordinateOverflow)?,
    }))
}

fn push_image_placeholder(
    nodes: &mut Vec<SceneNode>,
    rect: Rect,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    push_placeholder_frame(nodes, rect, Rgb::new(242, 242, 242), options)?;
    let inset = placeholder_inset(rect);
    let right = rect
        .x
        .checked_add(rect.width)
        .and_then(|value| value.checked_sub(inset))
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .and_then(|value| value.checked_sub(inset))
        .ok_or(RenderError::CoordinateOverflow)?;
    let left = rect
        .x
        .checked_add(inset)
        .ok_or(RenderError::CoordinateOverflow)?;
    let top = rect
        .y
        .checked_add(inset)
        .ok_or(RenderError::CoordinateOverflow)?;
    for (x1, y1, x2, y2) in [(left, top, right, bottom), (left, bottom, right, top)] {
        push_placeholder_line(nodes, x1, y1, x2, y2, Rgb::new(127, 127, 127), options)?;
    }
    Ok(())
}

fn push_shape_placeholder(
    nodes: &mut Vec<SceneNode>,
    rect: Rect,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    push_placeholder_frame(nodes, rect, Rgb::new(221, 235, 247), options)?;
    let left = rect
        .x
        .checked_add(placeholder_inset(rect))
        .ok_or(RenderError::CoordinateOverflow)?;
    let right = rect
        .x
        .checked_add(rect.width)
        .and_then(|value| value.checked_sub(placeholder_inset(rect)))
        .ok_or(RenderError::CoordinateOverflow)?;
    let top = rect
        .y
        .checked_add(placeholder_inset(rect))
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .and_then(|value| value.checked_sub(placeholder_inset(rect)))
        .ok_or(RenderError::CoordinateOverflow)?;
    let center_x = Fixed::from_raw(left.raw() + (right.raw() - left.raw()) / 2);
    let center_y = Fixed::from_raw(top.raw() + (bottom.raw() - top.raw()) / 2);
    for (x1, y1, x2, y2) in [
        (center_x, top, right, center_y),
        (right, center_y, center_x, bottom),
        (center_x, bottom, left, center_y),
        (left, center_y, center_x, top),
    ] {
        push_placeholder_line(nodes, x1, y1, x2, y2, Rgb::new(68, 114, 196), options)?;
    }
    Ok(())
}

fn push_chart_placeholder(
    nodes: &mut Vec<SceneNode>,
    rect: Rect,
    kind: ChartKind,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    push_placeholder_frame(nodes, rect, Rgb::WHITE, options)?;
    let left = fraction_coordinate(rect.x, rect.width, 1, 5)?;
    let right = fraction_coordinate(rect.x, rect.width, 9, 10)?;
    let top = fraction_coordinate(rect.y, rect.height, 1, 5)?;
    let bottom = fraction_coordinate(rect.y, rect.height, 4, 5)?;
    push_placeholder_line(
        nodes,
        left,
        top,
        left,
        bottom,
        Rgb::new(89, 89, 89),
        options,
    )?;
    push_placeholder_line(
        nodes,
        left,
        bottom,
        right,
        bottom,
        Rgb::new(89, 89, 89),
        options,
    )?;
    match kind {
        ChartKind::Line | ChartKind::Scatter | ChartKind::Radar => {
            let p1x =
                fraction_coordinate(left, right.checked_sub(left).unwrap_or(Fixed::ZERO), 1, 6)?;
            let p2x =
                fraction_coordinate(left, right.checked_sub(left).unwrap_or(Fixed::ZERO), 1, 2)?;
            let p3x =
                fraction_coordinate(left, right.checked_sub(left).unwrap_or(Fixed::ZERO), 5, 6)?;
            let p1y =
                fraction_coordinate(top, bottom.checked_sub(top).unwrap_or(Fixed::ZERO), 2, 3)?;
            let p2y =
                fraction_coordinate(top, bottom.checked_sub(top).unwrap_or(Fixed::ZERO), 1, 4)?;
            let p3y =
                fraction_coordinate(top, bottom.checked_sub(top).unwrap_or(Fixed::ZERO), 1, 2)?;
            push_placeholder_line(nodes, p1x, p1y, p2x, p2y, Rgb::new(68, 114, 196), options)?;
            push_placeholder_line(nodes, p2x, p2y, p3x, p3y, Rgb::new(68, 114, 196), options)?;
        }
        ChartKind::Pie | ChartKind::Doughnut => {
            let center_x =
                fraction_coordinate(left, right.checked_sub(left).unwrap_or(Fixed::ZERO), 1, 2)?;
            let center_y =
                fraction_coordinate(top, bottom.checked_sub(top).unwrap_or(Fixed::ZERO), 1, 2)?;
            push_placeholder_line(
                nodes,
                center_x,
                top,
                right,
                center_y,
                Rgb::new(68, 114, 196),
                options,
            )?;
            push_placeholder_line(
                nodes,
                right,
                center_y,
                center_x,
                bottom,
                Rgb::new(68, 114, 196),
                options,
            )?;
            push_placeholder_line(
                nodes,
                center_x,
                bottom,
                left,
                center_y,
                Rgb::new(68, 114, 196),
                options,
            )?;
            push_placeholder_line(
                nodes,
                left,
                center_y,
                center_x,
                top,
                Rgb::new(68, 114, 196),
                options,
            )?;
        }
        ChartKind::Bar | ChartKind::Area | ChartKind::Bubble => {
            let plot_width = right
                .checked_sub(left)
                .ok_or(RenderError::CoordinateOverflow)?;
            let plot_height = bottom
                .checked_sub(top)
                .ok_or(RenderError::CoordinateOverflow)?;
            for (index, numerator) in [1_i64, 3, 2].iter().enumerate() {
                let bar_left = fraction_coordinate(left, plot_width, (index * 2 + 1) as i64, 7)?;
                let bar_right = fraction_coordinate(left, plot_width, (index * 2 + 2) as i64, 7)?;
                let bar_top = fraction_coordinate(top, plot_height, *numerator, 4)?;
                push_node(
                    nodes,
                    SceneNode::Rect(RectNode {
                        rect: Rect {
                            x: bar_left,
                            y: bar_top,
                            width: bar_right
                                .checked_sub(bar_left)
                                .ok_or(RenderError::CoordinateOverflow)?,
                            height: bottom
                                .checked_sub(bar_top)
                                .ok_or(RenderError::CoordinateOverflow)?,
                        },
                        fill: Some(Rgb::new(68, 114, 196)),
                        stroke: None,
                        stroke_width: Fixed::ZERO,
                    }),
                    options,
                )?;
            }
        }
    }
    Ok(())
}

fn try_push_sparkline(
    nodes: &mut Vec<SceneNode>,
    rect: Rect,
    sparkline: &Sparkline,
    sheet: &Sheet,
    chart_points: &mut u64,
    options: &RenderOptions,
) -> Result<bool, RenderError> {
    let Some(values) =
        resolve_numeric_a1_range(sheet, &sparkline.range, chart_points, options, true)?
    else {
        return Ok(false);
    };
    if values.is_empty() {
        return Ok(false);
    }
    let left = fraction_coordinate(rect.x, rect.width, 1, 10)?;
    let right = fraction_coordinate(rect.x, rect.width, 9, 10)?;
    let top = fraction_coordinate(rect.y, rect.height, 1, 5)?;
    let bottom = fraction_coordinate(rect.y, rect.height, 4, 5)?;
    let width = right
        .checked_sub(left)
        .ok_or(RenderError::CoordinateOverflow)?;
    let height = bottom
        .checked_sub(top)
        .ok_or(RenderError::CoordinateOverflow)?;
    let color = Rgb::new(68, 114, 196);
    match sparkline.kind {
        SparklineKind::Line => {
            let (minimum, maximum) = numeric_bounds(&values).expect("non-empty values");
            let mut previous = None;
            for (index, value) in values.iter().enumerate() {
                let ratio_x = if values.len() == 1 {
                    0.5
                } else {
                    index as f64 / (values.len() - 1) as f64
                };
                let ratio_y = if maximum <= minimum {
                    0.5
                } else {
                    (*value - minimum) / (maximum - minimum)
                };
                let x = interpolate_fixed(left, width, ratio_x)?;
                let y = interpolate_fixed(bottom, Fixed::from_raw(-height.raw()), ratio_y)?;
                if let Some((previous_x, previous_y)) = previous {
                    push_placeholder_line(nodes, previous_x, previous_y, x, y, color, options)?;
                }
                previous = Some((x, y));
            }
            if values.len() == 1 {
                let marker = Fixed::from_pixels(2)
                    .min(width)
                    .min(height)
                    .max(Fixed::from_raw(1));
                let center_x = interpolate_fixed(left, width, 0.5)?;
                let center_y = interpolate_fixed(top, height, 0.5)?;
                push_node(
                    nodes,
                    SceneNode::Rect(RectNode {
                        rect: Rect {
                            x: Fixed::from_raw(center_x.raw() - marker.raw() / 2),
                            y: Fixed::from_raw(center_y.raw() - marker.raw() / 2),
                            width: marker,
                            height: marker,
                        },
                        fill: Some(color),
                        stroke: None,
                        stroke_width: Fixed::ZERO,
                    }),
                    options,
                )?;
            }
        }
        SparklineKind::Column => {
            let minimum = values.iter().copied().fold(0.0_f64, f64::min);
            let maximum = values.iter().copied().fold(0.0_f64, f64::max);
            let span = maximum - minimum;
            let baseline_ratio = if span <= 0.0 { 0.5 } else { -minimum / span };
            let baseline =
                interpolate_fixed(bottom, Fixed::from_raw(-height.raw()), baseline_ratio)?;
            push_sparkline_bars(
                nodes, &values, left, width, top, bottom, baseline, minimum, maximum, color,
                options,
            )?;
        }
        SparklineKind::WinLoss => {
            let baseline = interpolate_fixed(top, height, 0.5)?;
            push_placeholder_line(
                nodes,
                left,
                baseline,
                right,
                baseline,
                Rgb::new(127, 127, 127),
                options,
            )?;
            let count = values.len() as i64;
            for (index, value) in values.iter().enumerate() {
                if *value == 0.0 {
                    continue;
                }
                let bar_left = fraction_coordinate(left, width, index as i64 * 2, count * 2)?;
                let bar_right = fraction_coordinate(left, width, index as i64 * 2 + 1, count * 2)?;
                let y = if *value > 0.0 { top } else { baseline };
                let bar_bottom = if *value > 0.0 { baseline } else { bottom };
                push_solid_rect(nodes, bar_left, y, bar_right, bar_bottom, color, options)?;
            }
        }
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn push_sparkline_bars(
    nodes: &mut Vec<SceneNode>,
    values: &[f64],
    left: Fixed,
    width: Fixed,
    top: Fixed,
    bottom: Fixed,
    baseline: Fixed,
    minimum: f64,
    maximum: f64,
    color: Rgb,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let count = values.len() as i64;
    let height = bottom
        .checked_sub(top)
        .ok_or(RenderError::CoordinateOverflow)?;
    let span = maximum - minimum;
    for (index, value) in values.iter().enumerate() {
        let bar_left = fraction_coordinate(left, width, index as i64 * 2, count * 2)?;
        let bar_right = fraction_coordinate(left, width, index as i64 * 2 + 1, count * 2)?;
        let value_y = if span <= 0.0 {
            interpolate_fixed(top, height, 0.5)?
        } else {
            interpolate_fixed(
                bottom,
                Fixed::from_raw(-height.raw()),
                (*value - minimum) / span,
            )?
        };
        push_solid_rect(
            nodes,
            bar_left,
            value_y.min(baseline),
            bar_right,
            value_y.max(baseline),
            color,
            options,
        )?;
    }
    Ok(())
}

fn push_solid_rect(
    nodes: &mut Vec<SceneNode>,
    left: Fixed,
    top: Fixed,
    right: Fixed,
    bottom: Fixed,
    color: Rgb,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    if right <= left || bottom <= top {
        return Ok(());
    }
    push_node(
        nodes,
        SceneNode::Rect(RectNode {
            rect: Rect {
                x: left,
                y: top,
                width: right
                    .checked_sub(left)
                    .ok_or(RenderError::CoordinateOverflow)?,
                height: bottom
                    .checked_sub(top)
                    .ok_or(RenderError::CoordinateOverflow)?,
            },
            fill: Some(color),
            stroke: None,
            stroke_width: Fixed::ZERO,
        }),
        options,
    )
}

fn interpolate_fixed(start: Fixed, extent: Fixed, ratio: f64) -> Result<Fixed, RenderError> {
    let delta = rounded_scaled_raw(extent.raw(), ratio)?;
    let raw = i128::from(start.raw())
        .checked_add(delta)
        .and_then(|raw| i64::try_from(raw).ok())
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(Fixed::from_raw(raw))
}

fn push_sparkline_placeholder(
    nodes: &mut Vec<SceneNode>,
    rect: Rect,
    kind: SparklineKind,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let left = fraction_coordinate(rect.x, rect.width, 1, 10)?;
    let right = fraction_coordinate(rect.x, rect.width, 9, 10)?;
    let top = fraction_coordinate(rect.y, rect.height, 1, 5)?;
    let middle = fraction_coordinate(rect.y, rect.height, 1, 2)?;
    let bottom = fraction_coordinate(rect.y, rect.height, 4, 5)?;
    match kind {
        SparklineKind::Line => {
            let width = right
                .checked_sub(left)
                .ok_or(RenderError::CoordinateOverflow)?;
            let x2 = fraction_coordinate(left, width, 1, 3)?;
            let x3 = fraction_coordinate(left, width, 2, 3)?;
            push_placeholder_line(
                nodes,
                left,
                bottom,
                x2,
                top,
                Rgb::new(68, 114, 196),
                options,
            )?;
            push_placeholder_line(nodes, x2, top, x3, middle, Rgb::new(68, 114, 196), options)?;
            push_placeholder_line(
                nodes,
                x3,
                middle,
                right,
                top,
                Rgb::new(68, 114, 196),
                options,
            )?;
        }
        SparklineKind::Column | SparklineKind::WinLoss => {
            let width = right
                .checked_sub(left)
                .ok_or(RenderError::CoordinateOverflow)?;
            if matches!(kind, SparklineKind::WinLoss) {
                push_placeholder_line(
                    nodes,
                    left,
                    middle,
                    right,
                    middle,
                    Rgb::new(127, 127, 127),
                    options,
                )?;
            }
            for (index, numerator) in [2_i64, 1, 3].iter().enumerate() {
                let bar_left = fraction_coordinate(left, width, (index * 2) as i64, 6)?;
                let bar_right = fraction_coordinate(left, width, (index * 2 + 1) as i64, 6)?;
                let bar_top = if matches!(kind, SparklineKind::WinLoss) && index == 1 {
                    middle
                } else {
                    fraction_coordinate(
                        top,
                        bottom.checked_sub(top).unwrap_or(Fixed::ZERO),
                        *numerator,
                        4,
                    )?
                };
                let bar_bottom = bottom;
                push_node(
                    nodes,
                    SceneNode::Rect(RectNode {
                        rect: Rect {
                            x: bar_left,
                            y: bar_top,
                            width: bar_right
                                .checked_sub(bar_left)
                                .ok_or(RenderError::CoordinateOverflow)?,
                            height: bar_bottom
                                .checked_sub(bar_top)
                                .ok_or(RenderError::CoordinateOverflow)?,
                        },
                        fill: Some(Rgb::new(68, 114, 196)),
                        stroke: None,
                        stroke_width: Fixed::ZERO,
                    }),
                    options,
                )?;
            }
        }
    }
    Ok(())
}

fn push_placeholder_frame(
    nodes: &mut Vec<SceneNode>,
    rect: Rect,
    fill: Rgb,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    push_node(
        nodes,
        SceneNode::Rect(RectNode {
            rect,
            fill: Some(fill),
            stroke: Some(Rgb::new(127, 127, 127)),
            stroke_width: Fixed::from_pixels(1),
        }),
        options,
    )
}

fn push_chart_frame(
    nodes: &mut Vec<SceneNode>,
    rect: Rect,
    fill: Option<Rgb>,
    stroke: Option<Rgb>,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    push_node(
        nodes,
        SceneNode::Rect(RectNode {
            rect,
            fill,
            stroke: stroke.or(Some(Rgb::new(127, 127, 127))),
            stroke_width: Fixed::from_pixels(1),
        }),
        options,
    )
}

#[allow(clippy::too_many_arguments)]
fn push_chart_series_line(
    nodes: &mut Vec<SceneNode>,
    x1: Fixed,
    y1: Fixed,
    x2: Fixed,
    y2: Fixed,
    color: Rgb,
    width: Fixed,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    push_node(
        nodes,
        SceneNode::Line(LineNode {
            x1,
            y1,
            x2,
            y2,
            color,
            width,
        }),
        options,
    )
}

fn push_placeholder_line(
    nodes: &mut Vec<SceneNode>,
    x1: Fixed,
    y1: Fixed,
    x2: Fixed,
    y2: Fixed,
    color: Rgb,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    push_chart_series_line(nodes, x1, y1, x2, y2, color, Fixed::from_pixels(1), options)
}

fn placeholder_inset(rect: Rect) -> Fixed {
    Fixed::from_raw(
        Fixed::from_pixels(2)
            .raw()
            .min(rect.width.raw().max(1) / 4)
            .min(rect.height.raw().max(1) / 4)
            .max(1),
    )
}

fn fraction_coordinate(
    start: Fixed,
    extent: Fixed,
    numerator: i64,
    denominator: i64,
) -> Result<Fixed, RenderError> {
    let offset = i128::from(extent.raw())
        .checked_mul(i128::from(numerator))
        .and_then(|value| value.checked_div(i128::from(denominator)))
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(RenderError::CoordinateOverflow)?;
    start
        .checked_add(Fixed::from_raw(offset))
        .ok_or(RenderError::CoordinateOverflow)
}

fn resolve_fill(
    style: Option<&CellStyle>,
    coordinate: CellCoordinate,
    warnings: &mut Warnings,
) -> Option<Rgb> {
    let style = style?;
    if let Some(fill) = style.pattern_fill {
        match fill.pattern {
            FormatPattern::None => style.fill.map(rgb),
            FormatPattern::Solid => fill.foreground.or(fill.background).or(style.fill).map(rgb),
            _ => {
                warnings.add(WarningCode::PatternFillSimplified, Some(coordinate));
                fill.foreground.or(fill.background).or(style.fill).map(rgb)
            }
        }
    } else {
        style.fill.map(rgb)
    }
}

fn collect_style_warnings(
    style: Option<&CellStyle>,
    coordinate: CellCoordinate,
    typography_is_approximate: bool,
    warnings: &mut Warnings,
) {
    let Some(style) = style else {
        return;
    };
    if !typography_is_approximate {
        return;
    }
    if let Some(alignment) = style.align.as_ref() {
        if alignment.wrap {
            warnings.add(WarningCode::TextWrappingSimplified, Some(coordinate));
        }
        if alignment.shrink_to_fit {
            warnings.add(WarningCode::ShrinkToFitIgnored, Some(coordinate));
        }
    }
    if style
        .font
        .as_ref()
        .is_some_and(|font| font.script != rxls::FormatScript::None)
    {
        warnings.add(WarningCode::FontScriptIgnored, Some(coordinate));
    }
}

fn cached_cell_value(mut cell: &Cell) -> &Cell {
    while let Cell::Formula { cached, .. } = cell {
        cell = cached;
    }
    cell
}

fn cell_is_date_or_time(cell: &Cell, style: Option<&CellStyle>) -> bool {
    match cached_cell_value(cell) {
        Cell::Date(_) => true,
        Cell::Number(value) => style
            .and_then(|style| style.num_fmt.as_deref())
            .is_some_and(|format| rxls::number_format_displays_datetime(*value, format)),
        Cell::Text(_) | Cell::Bool(_) | Cell::Error(_) | Cell::Formula { .. } => false,
    }
}

fn cell_defaults_to_right_alignment(cell: &Cell) -> bool {
    match cached_cell_value(cell) {
        Cell::Number(_) | Cell::Date(_) => true,
        Cell::Text(_) | Cell::Bool(_) | Cell::Error(_) | Cell::Formula { .. } => false,
    }
}

fn cell_allows_horizontal_overflow(cell: &Cell) -> bool {
    match cached_cell_value(cell) {
        Cell::Text(_) => true,
        Cell::Number(_) | Cell::Date(_) | Cell::Bool(_) | Cell::Error(_) | Cell::Formula { .. } => {
            false
        }
    }
}

fn regions_by_visual_row(regions: &[Region]) -> Result<BTreeMap<i64, Vec<usize>>, RenderError> {
    let mut rows = BTreeMap::<i64, Vec<usize>>::new();
    for (index, region) in regions.iter().enumerate() {
        rows.entry(region.rect.y.raw()).or_default().push(index);
    }
    for row in rows.values_mut() {
        row.sort_by_key(|index| regions[*index].rect.x.raw());
        for pair in row.windows(2) {
            let left = regions[pair[0]].rect;
            let left_end = left
                .x
                .checked_add(left.width)
                .ok_or(RenderError::CoordinateOverflow)?;
            if left_end > regions[pair[1]].rect.x {
                return Err(RenderError::Typography {
                    reason: "overlapping_visual_regions",
                });
            }
        }
    }
    Ok(rows)
}

fn text_clip_bounds(
    region_index: usize,
    regions: &[Region],
    rows: &BTreeMap<i64, Vec<usize>>,
    text_style: &TextStyle,
    scene_bounds: Rect,
) -> Result<Rect, RenderError> {
    let mut clip = horizontal_text_clip_bounds(region_index, regions, rows, text_style)?;
    if regions[region_index].print_vertical_overflow {
        // Calc's printer path suppresses vertical cell clipping for automatic
        // rows. The enclosing page still clips paint at the printable scene.
        clip.y = scene_bounds.y;
        clip.height = scene_bounds.height;
    }
    Ok(clip)
}

fn horizontal_text_clip_bounds(
    region_index: usize,
    regions: &[Region],
    rows: &BTreeMap<i64, Vec<usize>>,
    text_style: &TextStyle,
) -> Result<Rect, RenderError> {
    let region = &regions[region_index];
    let alignment = region.style.as_ref().and_then(|style| style.align.as_ref());
    if !region.text_can_overflow
        || region.is_merged
        || alignment.is_some_and(|alignment| {
            alignment.wrap || alignment.shrink_to_fit || alignment.rotation != 0
        })
    {
        return Ok(region.rect);
    }
    let row = rows
        .get(&region.rect.y.raw())
        .ok_or(RenderError::Typography {
            reason: "missing_visual_row",
        })?;
    let position =
        row.iter()
            .position(|index| *index == region_index)
            .ok_or(RenderError::Typography {
                reason: "missing_visual_region",
            })?;
    let expand_left = matches!(text_style.anchor, TextAnchor::End | TextAnchor::Middle);
    let expand_right = matches!(text_style.anchor, TextAnchor::Start | TextAnchor::Middle);
    let mut left = region.rect.x;
    let mut right = region
        .rect
        .x
        .checked_add(region.rect.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    if expand_left {
        let mut cursor = left;
        for index in row[..position].iter().rev() {
            let candidate = &regions[*index];
            let candidate_right = candidate
                .rect
                .x
                .checked_add(candidate.rect.width)
                .ok_or(RenderError::CoordinateOverflow)?;
            if candidate_right != cursor || overflow_blocked_by(candidate) {
                break;
            }
            left = candidate.rect.x;
            cursor = left;
        }
    }
    if expand_right {
        let mut cursor = right;
        for index in &row[position + 1..] {
            let candidate = &regions[*index];
            if candidate.rect.x != cursor || overflow_blocked_by(candidate) {
                break;
            }
            right = candidate
                .rect
                .x
                .checked_add(candidate.rect.width)
                .ok_or(RenderError::CoordinateOverflow)?;
            cursor = right;
        }
    }
    Ok(Rect {
        x: left,
        y: region.rect.y,
        width: right
            .checked_sub(left)
            .ok_or(RenderError::CoordinateOverflow)?,
        height: region.rect.height,
    })
}

fn overflow_blocked_by(region: &Region) -> bool {
    region.is_merged || !region.text.is_empty()
}

fn rgb(color: Color) -> Rgb {
    let [red, green, blue] = color.as_rgb();
    Rgb::new(red, green, blue)
}

fn sanitize_xml_text(text: &str) -> (String, u64) {
    let mut replaced = 0_u64;
    let mut sanitized = String::with_capacity(text.len());
    for ch in text.chars() {
        if is_valid_xml_char(ch) {
            sanitized.push(ch);
        } else {
            sanitized.push('\u{fffd}');
            replaced += 1;
        }
    }
    (sanitized, replaced)
}

fn sanitize_rich_text(runs: &[rxls::TextRun]) -> Vec<rxls::TextRun> {
    runs.iter()
        .map(|run| rxls::TextRun {
            text: sanitize_xml_text(&run.text).0,
            font: run.font.clone(),
        })
        .collect()
}

fn is_valid_xml_char(ch: char) -> bool {
    matches!(ch, '\u{9}' | '\u{A}' | '\u{D}')
        || ('\u{20}'..='\u{D7FF}').contains(&ch)
        || ('\u{E000}'..='\u{FFFD}').contains(&ch)
        || ('\u{10000}'..='\u{10FFFF}').contains(&ch)
}

pub(crate) fn push_json_escaped(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch < '\u{20}' => {
                out.push_str("\\u00");
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let value = ch as u8;
                out.push(HEX[(value >> 4) as usize] as char);
                out.push(HEX[(value & 0x0f) as usize] as char);
            }
            ch => out.push(ch),
        }
    }
}

#[cfg(test)]
mod tests;
