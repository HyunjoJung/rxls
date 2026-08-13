//! Bounded worksheet-to-scene layout.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::sync::Arc;

use rxls::{
    Border, BorderStyle, Cell, CellStyle, CfRule, Chart, ChartBarDirection, ChartCachedPoint,
    ChartFrameFill, ChartKind, ChartMarkerSymbol, ChartSeriesStyle, ChartTextStyle, Color,
    DisplayCell, DrawingAnchorBehavior, DrawingMetadata, DrawingObjectKind, DvOp, Font,
    FormatPattern, FormatScript, HAlign, ImportedAxisMeasure, OoxmlImplicitRowHeight, Sheet,
    Sparkline, SparklineKind, StyleFidelity, StyleLossKind, VAlign, Workbook,
    XlsbDefaultColumnWidth,
};

use crate::error::{LimitKind, RenderError};
use crate::font::{
    helvetica_text_advance_units, BaseDirection, FontId, FontOutlineCommand, FontPack,
    FontPackError, FontRequest, ShapeOptions, ShapedText, StyledFontRequest, FONT_OUTLINE_UNITS,
};
use crate::media::decode_image;
use crate::scene::{
    ClipGroupNode, Fixed, GlyphCluster, GlyphClusterMetrics, GlyphPaint, GlyphRunNode,
    GlyphSemanticGroup, ImageNode, LineNode, PathCommand, PathNode, Rect, RectNode, Rgb, Scene,
    SceneFontFace, SceneNode, ShapedGlyph, TextAnchor, TextBaseline, TextNode, TextStyle,
    FIXED_UNITS_PER_PIXEL,
};
use crate::typography::{wrap_text_lines, CellLineLayoutPolicy};
use unicode_bidi::{bidi_class, BidiClass, BidiInfo};
use unicode_script::{Script, UnicodeScript};

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
const CALC_OOXML_DRAWING_DEFAULT_ROW_HEIGHT: Fixed = Fixed::from_raw(17_476);
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
    maximum_digit_width: Fixed,
    typography: TypographyStats,
    conditional_evaluations: u64,
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

// Calc's SinglePageSheets path serializes cell-grid offsets in a Map10thMM-like
// coordinate space while the page and cell content remain in normal PDF
// points. Relative to scene CSS pixels this is an exact 127/36 expansion:
// (127/48 tenth-mm per CSS pixel) / (3/4 point per CSS pixel).
const CALC_SINGLE_PAGE_GRIDLINE_SCALE_NUMERATOR: i64 = 127;
const CALC_SINGLE_PAGE_GRIDLINE_SCALE_DENOMINATOR: i64 = 36;

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
    if source_native_columns {
        let prefix = source_native_column_prefix(
            sheet,
            range.first_col,
            maximum_digit_width,
            options,
            warnings,
        )?;
        apply_source_native_axis_endpoints(
            &mut columns,
            prefix,
            maximum_digit_width,
            options,
            |column| imported_column_axis_measure(sheet, column, options),
        )?;
        for slot in &columns {
            column_widths.insert(slot.index, slot.size);
        }
    }

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
        let contributions = row_sizes
            .iter()
            .map(|(&row, &fallback)| {
                source_axis_contribution_twips(
                    imported_row_axis_measure(sheet, row, options),
                    fallback,
                    maximum_digit_width,
                )
                .map(|twips| (row, twips))
                .ok_or(RenderError::CoordinateOverflow)
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let mut native_rows = row_sizes
            .iter()
            .map(|(&row, &size)| MeasuredAxisSlot {
                index: row,
                offset: Fixed::ZERO,
                size,
            })
            .collect::<Vec<_>>();
        apply_source_native_axis_endpoints(
            &mut native_rows,
            prefix,
            maximum_digit_width,
            options,
            |row| imported_row_axis_measure(sheet, row, options),
        )?;
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
    if let Some((prefix, native_contributions, native_baselines)) = source_native_rows {
        // Automatic-height measurement operates in the renderer's Fixed
        // domain. Calc persists the resulting track as integer twips before
        // SinglePageSheets converts cumulative endpoints to Map100thMM, so
        // preserve untouched source measures and requantize only grown rows.
        let mut cursor = SourceAxisCursor::new(prefix)?;
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
            let (offset, size, boundary) = cursor.advance(contribution)?;
            rows.push(MeasuredAxisSlot {
                index: row,
                offset,
                size,
            });
            enforce_dimension(boundary, options)?;
        }
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
    Ok(AxisMeasurement {
        rows,
        columns,
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
        calc_wrap_space: None,
        style: None,
        conditional: ConditionalPaint::default(),
        text,
        rich_text: None,
        hyperlink: None,
        numeric_default: false,
        text_can_overflow: false,
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
        &style,
        false,
        kerning,
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

#[derive(Debug, Clone)]
struct Region {
    source: CellCoordinate,
    rect: Rect,
    is_merged: bool,
    line_layout_policy: CellLineLayoutPolicy,
    calc_wrap_space: Option<CalcWrapSpace>,
    style: Option<CellStyle>,
    conditional: ConditionalPaint,
    text: String,
    rich_text: Option<Vec<rxls::TextRun>>,
    hyperlink: Option<String>,
    numeric_default: bool,
    text_can_overflow: bool,
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
    if endpoint_policy == AxisEndpointPolicy::SourceNative {
        x = calc_inclusive_rectangle_extent(x).ok_or(RenderError::CoordinateOverflow)?;
        y = calc_inclusive_rectangle_extent(y).ok_or(RenderError::CoordinateOverflow)?;
    }
    let viewport = drawing_layout_viewport(
        sheet,
        range,
        &row_slots,
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
    let canvas_width = viewport.sheet.width.max(Fixed::from_pixels(1));
    let canvas_height = viewport.sheet.height.max(Fixed::from_pixels(1));
    enforce_dimension(canvas_width, options)?;
    enforce_dimension(canvas_height, options)?;
    let sheet_right_to_left = sheet.sheet_view().right_to_left;
    let reflected_col_slots = visual_column_slots(&col_slots, canvas_width, sheet_right_to_left)?;
    let visual_col_slots = reflected_col_slots.as_deref().unwrap_or(&col_slots);

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
            let line_layout_policy = cell_line_layout_policy(
                sheet,
                source,
                style.as_ref(),
                rich_text.as_deref(),
                CalcLineLayoutEvidence {
                    is_plain_text: display_cell
                        .is_some_and(|cell| matches!(cell.value, Cell::Text(_))),
                    has_adjustable_row,
                    wrap_space_available: calc_line_layout_available && calc_wrap_space.is_some(),
                },
                options,
            );
            regions.push(Region {
                source,
                rect,
                is_merged,
                line_layout_policy,
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
                ods_fixed_height_row: ods_native_sheet && !has_adjustable_row,
                print_vertical_overflow: print_vertical_overflow_available && has_adjustable_row,
                vertical_margin,
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
        options,
    )?;
    let scene_bounds = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: canvas_width,
        height: canvas_height,
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
        let style = text_style(region, options, sheet_right_to_left);
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
                &style,
                sheet_right_to_left,
                true,
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
        &row_slots,
        &col_slots,
        geometry,
        viewport.cell,
        viewport.sheet,
        canvas_width,
        canvas_height,
        sheet_right_to_left,
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

fn round_positive_mul_div(value: i128, multiplier: i128, divisor: i128) -> Option<i128> {
    if value < 0 || multiplier <= 0 || divisor <= 0 {
        return None;
    }
    value
        .checked_mul(multiplier)?
        .checked_add(divisor.checked_div(2)?)?
        .checked_div(divisor)
}

/// Convert a cumulative Calc position from integer twips to 1/100 mm.
///
/// Calc's SinglePageSheets path sums raw track widths/heights before converting
/// each rectangle endpoint with ordinary half-up `twip -> mm100` conversion.
fn calc_twips_position_to_hmm(twips: i128) -> Option<i128> {
    round_positive_mul_div(twips, 127, 72)
}

fn calc_hmm_to_fixed_raw(hmm: i128) -> Option<i128> {
    round_positive_mul_div(
        hmm,
        24_i128.checked_mul(i128::from(FIXED_UNITS_PER_PIXEL))?,
        635,
    )
}

fn calc_hmm_to_fixed(hmm: i128) -> Option<Fixed> {
    let raw = calc_hmm_to_fixed_raw(hmm)?;
    i64::try_from(raw).ok().map(Fixed::from_raw)
}

fn calc_twips_position_to_fixed_raw(twips: i128) -> Option<i128> {
    calc_hmm_to_fixed_raw(calc_twips_position_to_hmm(twips)?)
}

fn calc_twips_position_to_fixed(twips: i128) -> Option<Fixed> {
    let raw = calc_twips_position_to_fixed_raw(twips)?;
    i64::try_from(raw).ok().map(Fixed::from_raw)
}

/// Calc's `tools::Rectangle` dimensions are inclusive, so SinglePageSheets
/// contributes one additional 1/100-mm unit after converting both endpoints.
fn calc_inclusive_rectangle_extent(value: Fixed) -> Option<Fixed> {
    let hmm = round_positive_mul_div(
        i128::from(value.raw()),
        635,
        24_i128.checked_mul(i128::from(FIXED_UNITS_PER_PIXEL))?,
    )?;
    calc_hmm_to_fixed(hmm.checked_add(1)?)
}

/// Cumulative source-space axis cursor seeded at the global visible prefix.
///
/// Calc retains imported worksheet axes as integer twips, sums tracks, then
/// projects cumulative endpoints to 1/100 mm. Boundaries are translated back to
/// local scene coordinates, preserving the global phase for nonzero selections
/// without exposing a potentially large prefix to the local dimension limit.
struct SourceAxisCursor {
    twips: i128,
    origin_raw: i128,
    previous_raw: i128,
}

impl SourceAxisCursor {
    fn new(prefix_twips: i128) -> Result<Self, RenderError> {
        let origin_raw = calc_twips_position_to_fixed_raw(prefix_twips)
            .ok_or(RenderError::CoordinateOverflow)?;
        Ok(Self {
            twips: prefix_twips,
            origin_raw,
            previous_raw: origin_raw,
        })
    }

    fn advance(&mut self, contribution_twips: i128) -> Result<(Fixed, Fixed, Fixed), RenderError> {
        let offset_raw = self
            .previous_raw
            .checked_sub(self.origin_raw)
            .ok_or(RenderError::CoordinateOverflow)?;
        self.twips = self
            .twips
            .checked_add(contribution_twips)
            .ok_or(RenderError::CoordinateOverflow)?;
        let boundary_raw =
            calc_twips_position_to_fixed_raw(self.twips).ok_or(RenderError::CoordinateOverflow)?;
        let size_raw = boundary_raw
            .checked_sub(self.previous_raw)
            .filter(|size| *size > 0)
            .ok_or(RenderError::CoordinateOverflow)?;
        let local_boundary_raw = boundary_raw
            .checked_sub(self.origin_raw)
            .ok_or(RenderError::CoordinateOverflow)?;
        self.previous_raw = boundary_raw;
        Ok((
            Fixed::from_raw(
                i64::try_from(offset_raw).map_err(|_| RenderError::CoordinateOverflow)?,
            ),
            Fixed::from_raw(i64::try_from(size_raw).map_err(|_| RenderError::CoordinateOverflow)?),
            Fixed::from_raw(
                i64::try_from(local_boundary_raw).map_err(|_| RenderError::CoordinateOverflow)?,
            ),
        ))
    }
}

fn imported_column_axis_measure(
    sheet: &Sheet,
    column: u16,
    options: &RenderOptions,
) -> Option<ImportedAxisMeasure> {
    let has_explicit_width = sheet.column_widths().contains_key(&column)
        || sheet.physical_column_widths().contains_key(&column)
        || sheet.xlsb_column_widths_256().contains_key(&column);
    if has_explicit_width {
        sheet.imported_column_axis_measures().get(&column).copied()
    } else {
        sheet.imported_default_column_axis_measure().or_else(|| {
            (options.font_pack.is_some() && sheet.implicit_ooxml_column_width() == Some(None)).then(
                || {
                    if sheet.ooxml_uses_defaulted_base_column_width() {
                        ImportedAxisMeasure::DigitBaseWidth256(8 * XLSB_DIGIT_WIDTH_SCALE)
                    } else {
                        ImportedAxisMeasure::DigitWidth256(
                            OOXML_APPLICATION_DEFAULT_COLUMN_WIDTH_256,
                        )
                    }
                },
            )
        })
    }
}

fn imported_default_row_axis_measure(
    sheet: &Sheet,
    options: &RenderOptions,
) -> Option<ImportedAxisMeasure> {
    sheet.imported_default_row_axis_measure().or_else(|| {
        sheet.has_implicit_ooxml_row_height().then(|| {
            verified_ooxml_normal_font_size(sheet, options)
                .and_then(|(points, _)| calc_ooxml_row_height_twips_from_points(points))
                .map(ImportedAxisMeasure::Twips)
                .unwrap_or(ImportedAxisMeasure::MillimeterHundredths(500))
        })
    })
}

fn imported_row_axis_measure(
    sheet: &Sheet,
    row: u32,
    options: &RenderOptions,
) -> Option<ImportedAxisMeasure> {
    if sheet.row_heights().contains_key(&row) {
        sheet.imported_row_axis_measures().get(&row).copied()
    } else {
        imported_default_row_axis_measure(sheet, options)
    }
}

fn maximum_digit_width_twips(maximum_digit_width: Fixed) -> Option<i128> {
    i128::from(maximum_digit_width.raw())
        .checked_mul(TWIPS_PER_CSS_PIXEL)?
        .checked_div(i128::from(FIXED_UNITS_PER_PIXEL))
        .filter(|twips| *twips > 0)
}

/// Calc's BIFF importer truncates `width * digit_twips - 0.5`.
fn biff_character_width_256_to_twips(width_256: u32, maximum_digit_width: Fixed) -> Option<i128> {
    let digit_twips = maximum_digit_width_twips(maximum_digit_width)?;
    i128::from(width_256)
        .checked_mul(digit_twips)?
        .checked_sub(i128::from(XLSB_DIGIT_WIDTH_SCALE / 2))?
        .checked_div(i128::from(XLSB_DIGIT_WIDTH_SCALE))
        .filter(|twips| *twips > 0)
}

/// Calc's OOXML importer rounds source character widths in the default font's
/// integer-twip maximum-digit domain. Base widths additionally carry five
/// 96-DPI screen pixels.
fn ooxml_character_width_ratio_to_twips(
    numerator: u64,
    denominator: u64,
    maximum_digit_width: Fixed,
    extra_screen_pixels: u16,
) -> Option<i128> {
    if numerator == 0 || denominator == 0 {
        return None;
    }
    let digit_twips = maximum_digit_width_twips(maximum_digit_width)?;
    round_positive_mul_div(i128::from(numerator), digit_twips, i128::from(denominator))?
        .checked_add(i128::from(extra_screen_pixels).checked_mul(TWIPS_PER_CSS_PIXEL)?)
        .filter(|twips| *twips > 0)
}

#[cfg(test)]
fn character_width_ratio_to_pixels(
    numerator: u64,
    denominator: u64,
    maximum_digit_width: Fixed,
    screen_padding_pixels: u16,
) -> Option<i128> {
    if numerator == 0 || denominator == 0 || maximum_digit_width.raw() <= 0 {
        return None;
    }
    let digit_pixels = i128::from(maximum_digit_width.raw())
        .checked_div(i128::from(FIXED_UNITS_PER_PIXEL))?
        .max(1);
    let bias = 128_i128.checked_div(digit_pixels)?;
    let numerator = i128::from(numerator);
    let denominator = i128::from(denominator);
    numerator
        .checked_mul(i128::from(XLSB_DIGIT_WIDTH_SCALE))?
        .checked_add(bias.checked_mul(denominator)?)?
        .checked_mul(digit_pixels)?
        .checked_div(denominator.checked_mul(i128::from(XLSB_DIGIT_WIDTH_SCALE))?)?
        .checked_add(i128::from(screen_padding_pixels))
        .filter(|pixels| *pixels > 0)
}

fn imported_axis_measure_twips(
    measure: ImportedAxisMeasure,
    maximum_digit_width: Fixed,
) -> Option<i128> {
    match measure {
        ImportedAxisMeasure::Twips(twips) => Some(i128::from(twips)),
        ImportedAxisMeasure::MillimeterHundredths(mm100) => {
            round_positive_mul_div(i128::from(mm100), 72, 127)
        }
        ImportedAxisMeasure::PointRatio(numerator, denominator) => round_positive_mul_div(
            i128::from(numerator),
            TWIPS_PER_POINT,
            i128::from(denominator),
        ),
        ImportedAxisMeasure::CharacterWidth256(width) => {
            biff_character_width_256_to_twips(width, maximum_digit_width)
        }
        ImportedAxisMeasure::CharacterWidthRatio(numerator, denominator) => {
            ooxml_character_width_ratio_to_twips(numerator, denominator, maximum_digit_width, 0)
        }
        ImportedAxisMeasure::CharacterBaseWidth256(width) => ooxml_character_width_ratio_to_twips(
            u64::from(width),
            u64::from(XLSB_DIGIT_WIDTH_SCALE),
            maximum_digit_width,
            OOXML_BASE_COLUMN_EXTRA_PADDING_PIXELS,
        ),
        ImportedAxisMeasure::DigitWidth256(width) => {
            xlsb_digits_to_twips(width, maximum_digit_width, 0)
        }
        ImportedAxisMeasure::DigitBaseWidth256(width) => {
            xlsb_digits_to_twips(width, maximum_digit_width, XLSB_BASE_COLUMN_SCREEN_PIXELS)
        }
    }
}

fn source_axis_contribution_twips(
    measure: Option<ImportedAxisMeasure>,
    fallback: Fixed,
    maximum_digit_width: Fixed,
) -> Option<i128> {
    measure
        .and_then(|measure| imported_axis_measure_twips(measure, maximum_digit_width))
        .filter(|twips| *twips > 0)
        .or_else(|| {
            round_positive_mul_div(
                i128::from(fallback.raw()),
                TWIPS_PER_CSS_PIXEL,
                i128::from(FIXED_UNITS_PER_PIXEL),
            )
            .filter(|twips| *twips > 0)
        })
}

fn source_native_column_prefix(
    sheet: &Sheet,
    first_column: u16,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<i128, RenderError> {
    // Column iteration is bounded by the 16,384-column worksheet schema.
    let mut prefix = 0_i128;
    for column in 0..first_column {
        if !options.include_hidden && sheet.hidden_columns().contains(&column) {
            continue;
        }
        let fallback = column_width(sheet, column, maximum_digit_width, options, warnings);
        let contribution = source_axis_contribution_twips(
            imported_column_axis_measure(sheet, column, options),
            fallback,
            maximum_digit_width,
        )
        .ok_or(RenderError::CoordinateOverflow)?;
        prefix = prefix
            .checked_add(contribution)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(prefix)
}

fn persisted_default_row_height(sheet: &Sheet, options: &RenderOptions) -> Fixed {
    sheet
        .default_row_height()
        .and_then(points_to_fixed)
        .unwrap_or_else(|| fallback_row_height(sheet, options))
}

fn visible_row_prefix_count(sheet: &Sheet, first_row: u32, options: &RenderOptions) -> u64 {
    if options.include_hidden {
        u64::from(first_row)
    } else if let Some(exceptions) = sheet.default_hidden_row_exceptions() {
        exceptions
            .range(..first_row)
            .filter(|&&row| !sheet.hidden_rows().contains(&row))
            .count() as u64
    } else {
        u64::from(first_row).saturating_sub(sheet.hidden_rows().range(..first_row).count() as u64)
    }
}

fn source_native_row_prefix(
    sheet: &Sheet,
    first_row: u32,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<i128, RenderError> {
    let visible_rows = visible_row_prefix_count(sheet, first_row, options);
    if visible_rows == 0 {
        return Ok(0);
    }
    let default_contribution = source_axis_contribution_twips(
        imported_default_row_axis_measure(sheet, options),
        persisted_default_row_height(sheet, options),
        maximum_digit_width,
    )
    .ok_or(RenderError::CoordinateOverflow)?;
    // Rows can reach the million-row schema ceiling. Multiply the default
    // contribution by the visible count and visit only sparse explicit rows.
    let mut explicit_rows = 0_u64;
    let mut explicit_total = 0_i128;
    for (&row, _) in sheet.row_heights().range(..first_row) {
        if !options.include_hidden && row_is_hidden(sheet, row) {
            continue;
        }
        explicit_rows = explicit_rows
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        let contribution = source_axis_contribution_twips(
            imported_row_axis_measure(sheet, row, options),
            row_height(sheet, row, options, warnings),
            maximum_digit_width,
        )
        .ok_or(RenderError::CoordinateOverflow)?;
        explicit_total = explicit_total
            .checked_add(contribution)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    let default_rows = visible_rows
        .checked_sub(explicit_rows)
        .ok_or(RenderError::CoordinateOverflow)?;
    default_contribution
        .checked_mul(i128::from(default_rows))
        .and_then(|defaults| defaults.checked_add(explicit_total))
        .ok_or(RenderError::CoordinateOverflow)
}

fn apply_source_native_axis_endpoints<I: Copy>(
    slots: &mut [MeasuredAxisSlot<I>],
    prefix_twips: i128,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    mut measure: impl FnMut(I) -> Option<ImportedAxisMeasure>,
) -> Result<(), RenderError> {
    let mut cursor = SourceAxisCursor::new(prefix_twips)?;
    for slot in slots {
        let contribution_twips =
            source_axis_contribution_twips(measure(slot.index), slot.size, maximum_digit_width)
                .ok_or(RenderError::CoordinateOverflow)?;
        let (offset, size, boundary) = cursor.advance(contribution_twips)?;
        slot.offset = offset;
        slot.size = size;
        enforce_dimension(boundary, options)?;
    }
    Ok(())
}

fn apply_axis_geometry<I: Copy + Ord>(
    slots: &mut [MeasuredAxisSlot<I>],
    geometry: &[MeasuredAxisSlot<I>],
) -> Result<(), RenderError> {
    let mut offset = Fixed::ZERO;
    for slot in slots {
        let replacement = geometry
            .binary_search_by(|candidate| candidate.index.cmp(&slot.index))
            .ok()
            .and_then(|index| geometry.get(index))
            .ok_or(RenderError::Backend {
                reason: "prepared_print_geometry_missing_axis_slot",
            })?;
        slot.offset = offset;
        slot.size = replacement.size;
        offset = offset
            .checked_add(slot.size)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(())
}

fn visual_column_slots<I: Copy>(
    logical_slots: &[MeasuredAxisSlot<I>],
    canvas_width: Fixed,
    right_to_left: bool,
) -> Result<Option<Vec<MeasuredAxisSlot<I>>>, RenderError> {
    if !right_to_left {
        return Ok(None);
    }
    let slots = logical_slots
        .iter()
        .map(|slot| {
            Ok(MeasuredAxisSlot {
                index: slot.index,
                offset: reflected_x(slot.offset, slot.size, canvas_width)?,
                size: slot.size,
            })
        })
        .collect::<Result<Vec<_>, RenderError>>()?;
    Ok(Some(slots))
}

fn reflected_x(x: Fixed, width: Fixed, canvas_width: Fixed) -> Result<Fixed, RenderError> {
    canvas_width
        .checked_sub(
            x.checked_add(width)
                .ok_or(RenderError::CoordinateOverflow)?,
        )
        .ok_or(RenderError::CoordinateOverflow)
}

fn reflect_rect_horizontally(mut rect: Rect, canvas_width: Fixed) -> Result<Rect, RenderError> {
    rect.x = reflected_x(rect.x, rect.width, canvas_width)?;
    Ok(rect)
}

fn maximum_digit_width(
    style_snapshot: &RenderStyleSnapshot,
    options: &RenderOptions,
    warnings: &mut Warnings,
    statistics: &mut TypographyStats,
) -> Result<Fixed, RenderError> {
    let Some(pack) = options.font_pack.as_ref() else {
        return Ok(Fixed::from_pixels(7));
    };
    let font = style_snapshot
        .default_style()
        .and_then(|style| style.font.as_ref());
    let family = font
        .and_then(|font| font.name.as_deref())
        .unwrap_or(&options.default_font_family);
    let size = font
        .and_then(|font| font.size_pt)
        .and_then(|points| points_to_fixed(points as f32))
        .unwrap_or(options.default_font_size);
    let request = FontRequest {
        family,
        weight: if font.is_some_and(|font| font.bold) {
            700
        } else {
            400
        },
        italic: font.is_some_and(|font| font.italic),
    };
    let resolution = pack.resolve(request);
    if !resolution.exact_family {
        warnings.add(WarningCode::FontFamilySubstituted, None);
    }
    let (font_id, width) = pack.max_digit_width(request).map_err(map_font_error)?;
    statistics.record_face(pack, font_id, !resolution.exact_family)?;
    let metrics = pack.metrics(font_id).map_err(map_font_error)?;
    scale_font_units(i64::from(width), size, metrics.units_per_em, 1)
}

fn column_width(
    sheet: &Sheet,
    col: u16,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Fixed {
    if let Some(points) = sheet.physical_column_widths().get(&col).copied() {
        if let Some(width) = points_to_fixed(points) {
            return width;
        }
        warnings.add(
            WarningCode::InvalidGeometryFallback,
            Some(CellCoordinate { row: 0, col }),
        );
    }
    if let Some(width_256) = sheet.xlsb_column_widths_256().get(&col).copied() {
        return resolve_column_width(
            xlsb_digits_to_fixed(width_256, maximum_digit_width, 0),
            true,
            col,
            options,
            warnings,
        );
    }
    if let Some(chars) = sheet.column_widths().get(&col).copied() {
        return resolve_column_width(
            column_chars_to_fixed(chars, maximum_digit_width, IMPORTED_COLUMN_PADDING_PIXELS),
            true,
            col,
            options,
            warnings,
        );
    }
    if let Some(provenance) = sheet.xlsb_default_column_width() {
        let (width_256, screen_pixels) = match provenance {
            XlsbDefaultColumnWidth::ApplicationDefault => {
                (XLSB_DIGIT_WIDTH_SCALE * 8 + XLSB_DIGIT_WIDTH_SCALE / 2, 0)
            }
            XlsbDefaultColumnWidth::Digits256(width_256) => (width_256, 0),
            XlsbDefaultColumnWidth::BaseCharacters(characters) => (
                u32::from(characters) * XLSB_DIGIT_WIDTH_SCALE,
                XLSB_BASE_COLUMN_SCREEN_PIXELS,
            ),
        };
        return resolve_column_width(
            xlsb_digits_to_fixed(width_256, maximum_digit_width, screen_pixels),
            true,
            col,
            options,
            warnings,
        );
    }
    if sheet.biff_uses_application_default_column_width() {
        return BIFF_APPLICATION_DEFAULT_COLUMN_WIDTH;
    }
    let (measured, invalid_source_geometry) = match sheet.default_column_width() {
        Some(chars) => (
            column_chars_to_fixed(chars, maximum_digit_width, IMPORTED_COLUMN_PADDING_PIXELS),
            true,
        ),
        None => match sheet.implicit_ooxml_column_width() {
            Some(Some(base_characters)) => (
                column_chars_to_fixed(
                    base_characters,
                    maximum_digit_width,
                    IMPORTED_COLUMN_PADDING_PIXELS + OOXML_BASE_COLUMN_EXTRA_PADDING_PIXELS,
                ),
                true,
            ),
            // Without verified font metrics the caller's physical fallback is
            // safer than projecting Calc's font-dependent application default.
            Some(None)
                if options.font_pack.is_some()
                    && sheet.ooxml_uses_defaulted_base_column_width() =>
            {
                (
                    xlsb_digits_to_fixed(
                        8 * XLSB_DIGIT_WIDTH_SCALE,
                        maximum_digit_width,
                        XLSB_BASE_COLUMN_SCREEN_PIXELS,
                    ),
                    true,
                )
            }
            Some(None) if options.font_pack.is_some() => (
                xlsb_digits_to_fixed(
                    OOXML_APPLICATION_DEFAULT_COLUMN_WIDTH_256,
                    maximum_digit_width,
                    0,
                ),
                true,
            ),
            Some(None) | None if options.font_pack.is_some() => (
                column_chars_to_fixed(
                    DEFAULT_COLUMN_CHARACTERS,
                    maximum_digit_width,
                    DEFAULT_COLUMN_PADDING_PIXELS,
                ),
                false,
            ),
            Some(None) | None => (None, false),
        },
    };
    resolve_column_width(measured, invalid_source_geometry, col, options, warnings)
}

fn resolve_column_width(
    measured: Option<Fixed>,
    invalid_source_geometry: bool,
    col: u16,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Fixed {
    match measured {
        Some(width) => width,
        None => {
            if invalid_source_geometry {
                warnings.add(
                    WarningCode::InvalidGeometryFallback,
                    Some(CellCoordinate { row: 0, col }),
                );
            }
            options.default_column_width.max(Fixed::from_raw(1))
        }
    }
}

fn empty_used_column_width(
    sheet: &Sheet,
    style_snapshot: &RenderStyleSnapshot,
    options: &RenderOptions,
    warnings: &mut Warnings,
    statistics: &mut TypographyStats,
) -> Result<Fixed, RenderError> {
    if sheet.column_widths().len() == 256 {
        return Ok(Fixed::from_pixels(1));
    }
    if let Some(XlsbDefaultColumnWidth::Digits256(width_256)) = sheet.xlsb_default_column_width() {
        let maximum_digit_width =
            maximum_digit_width(style_snapshot, options, warnings, statistics)?;
        return match xlsb_digits_to_fixed(width_256, maximum_digit_width, 0) {
            Some(width) => Ok(width),
            None => {
                warnings.add(
                    WarningCode::InvalidGeometryFallback,
                    Some(CellCoordinate { row: 0, col: 0 }),
                );
                Ok(Fixed::from_pixels(1))
            }
        };
    }
    if sheet.biff_uses_application_default_column_width() {
        return Ok(BIFF_APPLICATION_DEFAULT_COLUMN_WIDTH);
    }
    let Some(chars) = sheet.default_column_width() else {
        return Ok(Fixed::from_pixels(1));
    };
    if !chars.is_finite() || chars <= 0.0 {
        warnings.add(
            WarningCode::InvalidGeometryFallback,
            Some(CellCoordinate { row: 0, col: 0 }),
        );
        return Ok(Fixed::from_pixels(1));
    }
    let maximum_digit_width = maximum_digit_width(style_snapshot, options, warnings, statistics)?;
    match column_chars_to_fixed(chars, maximum_digit_width, IMPORTED_COLUMN_PADDING_PIXELS) {
        Some(width) => Ok(width),
        None => {
            warnings.add(
                WarningCode::InvalidGeometryFallback,
                Some(CellCoordinate { row: 0, col: 0 }),
            );
            Ok(Fixed::from_pixels(1))
        }
    }
}

fn row_height(sheet: &Sheet, row: u32, options: &RenderOptions, warnings: &mut Warnings) -> Fixed {
    let points = sheet
        .row_heights()
        .get(&row)
        .copied()
        .or_else(|| sheet.default_row_height());
    match points.and_then(points_to_fixed) {
        Some(height) => height,
        None => {
            if points.is_some() {
                warnings.add(
                    WarningCode::InvalidGeometryFallback,
                    Some(CellCoordinate { row, col: 0 }),
                );
            }
            fallback_row_height(sheet, options)
        }
    }
}

fn fallback_row_height(sheet: &Sheet, options: &RenderOptions) -> Fixed {
    if sheet.biff_uses_application_default_row_height() {
        BIFF_APPLICATION_DEFAULT_ROW_HEIGHT
    } else {
        match sheet.implicit_ooxml_row_height_source() {
            Some(OoxmlImplicitRowHeight::XlsxApplicationDefault) => {
                calc_ooxml_implicit_row_height(sheet, options)
                    .unwrap_or(OOXML_APPLICATION_DEFAULT_ROW_HEIGHT)
            }
            Some(OoxmlImplicitRowHeight::XlsbApplicationDefault) => {
                calc_ooxml_implicit_row_height(sheet, options)
                    .unwrap_or(OOXML_APPLICATION_DEFAULT_ROW_HEIGHT)
            }
            Some(OoxmlImplicitRowHeight::None) | None => imported_no_information_row_height(sheet)
                .unwrap_or(options.default_row_height)
                .max(Fixed::from_raw(1)),
        }
    }
}

/// An imported sheet's own native "no information" default row height,
/// consumed only when the sheet carries no points-based default row height
/// at all (`Sheet::default_row_height` is `None`).
///
/// XLS, XLSX, and XLSB importers always populate
/// `imported_default_row_axis_measure` together with `default_row_height`
/// from the very same source record or attribute -- BIFF's
/// `DEFAULTROWHEIGHT`, XLSX's `sheetFormatPr defaultRowHeight`, XLSB's
/// `BrtWsFmtInfo` -- so this function can only ever return `Some` for an
/// importer, currently only ODS's, that records a native default-row measure
/// while leaving `default_row_height` unset. The `default_row_height().is_some()`
/// guard below is a second, independent proof of that: even if a future
/// importer bug broke the "populated together" invariant, this function
/// still could not change BIFF/XLSX/XLSB behavior, because those formats
/// only ever reach `fallback_row_height`'s final branch (this one) with a
/// per-row or sheet-wide invalid height while `default_row_height` is
/// `Some`.
///
/// `calc_hmm_to_fixed` is the same hundredths-of-millimetre-to-`Fixed`
/// conversion the SinglePageSheets path already applies to print geometry
/// (e.g. `calc_twips_position_to_fixed_raw`), so this keeps the physical
/// quantity to one exact spelling instead of re-deriving a second,
/// differently-rounded one: `calc_hmm_to_fixed(500)` equals
/// `OOXML_APPLICATION_DEFAULT_ROW_HEIGHT` exactly.
fn imported_no_information_row_height(sheet: &Sheet) -> Option<Fixed> {
    if sheet.default_row_height().is_some() {
        return None;
    }
    match sheet.imported_default_row_axis_measure()? {
        ImportedAxisMeasure::MillimeterHundredths(mm100) => calc_hmm_to_fixed(i128::from(mm100)),
        _ => None,
    }
}

fn verified_ooxml_normal_font_size(sheet: &Sheet, options: &RenderOptions) -> Option<(u16, Fixed)> {
    // The model retains these source sizes only for structurally complete XLSX
    // or XLSB style tables whose first cell XF and Normal style agree exactly.
    // Fractional, invalid, ambiguous, authored, BIFF, and ODS sources stay on
    // the existing physical fallback.
    let source_points = match (
        sheet.verified_xlsx_normal_font_size_pt(),
        sheet.verified_xlsb_normal_font_size_pt(),
    ) {
        (Some(points), None) | (None, Some(points)) => points,
        (Some(_), Some(_)) | (None, None) => return None,
    };
    let pack = options.font_pack.as_ref()?;
    let font = sheet.default_cell_style()?.font.as_ref()?;
    let family = font.name.as_deref()?;
    let points = font.size_pt.filter(|points| *points == source_points)?;
    let resolution = pack.resolve(FontRequest {
        family,
        weight: if font.bold { 700 } else { 400 },
        italic: font.italic,
    });
    if !(resolution.exact_family || resolution.declared_alias) || !resolution.exact_style {
        return None;
    }
    Some((points, points_to_fixed(f32::from(points))?))
}

fn verified_ooxml_cell_font_size_pt(sheet: &Sheet, row: u32, col: u16) -> Option<u16> {
    match sheet.implicit_ooxml_row_height_source()? {
        OoxmlImplicitRowHeight::XlsxApplicationDefault => {
            sheet.verified_xlsx_cell_font_size_pt(row, col)
        }
        OoxmlImplicitRowHeight::XlsbApplicationDefault => {
            sheet.verified_xlsb_cell_font_size_pt(row, col)
        }
        OoxmlImplicitRowHeight::None => None,
    }
}

fn verified_calc_cell_font_size_pt(
    sheet: &Sheet,
    source: CellCoordinate,
    style: &CellStyle,
    options: &RenderOptions,
) -> Option<u16> {
    let points = verified_ooxml_cell_font_size_pt(sheet, source.row, source.col)?;
    let font = style.font.as_ref()?;
    if font.size_pt != Some(points) || font.script != FormatScript::None {
        return None;
    }
    let resolution = options.font_pack.as_ref()?.resolve(FontRequest {
        family: font.name.as_deref()?,
        weight: if font.bold { 700 } else { 400 },
        italic: font.italic,
    });
    ((resolution.exact_family || resolution.declared_alias) && resolution.exact_style)
        .then_some(points)
}

fn verified_implicit_ooxml(sheet: &Sheet, options: &RenderOptions) -> bool {
    sheet.has_implicit_ooxml_row_height()
        && verified_ooxml_normal_font_size(sheet, options).is_some()
}

fn has_conditional_text_layout_overlay(sheet: &Sheet) -> bool {
    sheet.conditional_format_metadata().iter().any(|metadata| {
        let has_unsafe_losses = metadata
            .style_losses
            .iter()
            .any(|loss| loss.kind != StyleLossKind::UnresolvedColor);
        let unresolved_color_without_retained_font = metadata
            .style_losses
            .iter()
            .any(|loss| loss.kind == StyleLossKind::UnresolvedColor)
            && metadata
                .differential_style
                .as_ref()
                .is_none_or(|style| style.font.is_none());
        metadata.differential_style.as_ref().map_or(
            has_unsafe_losses || unresolved_color_without_retained_font,
            |style| {
                unresolved_color_without_retained_font
                    || conditional_style_affects_text_layout(style, has_unsafe_losses)
            },
        )
    })
}

fn calc_line_layout_available(sheet: &Sheet, options: &RenderOptions) -> bool {
    verified_implicit_ooxml(sheet, options) && !has_conditional_text_layout_overlay(sheet)
}

fn conditional_style_is_geometry_safe_color_only(
    style: &CellStyle,
    has_unsafe_losses: bool,
) -> bool {
    if has_unsafe_losses
        || style.align.is_some()
        || style.num_fmt.is_some()
        || style.protection.is_some()
    {
        return false;
    }
    let Some(font) = style.font.as_ref() else {
        return false;
    };
    let mut font_without_color = font.clone();
    if font_without_color.color.take().is_none() || font_without_color != Font::default() {
        return false;
    }
    true
}

fn conditional_style_affects_text_layout(style: &CellStyle, has_unsafe_losses: bool) -> bool {
    has_unsafe_losses
        || style.align.is_some()
        || style.num_fmt.is_some()
        || (style.font.is_some()
            && !conditional_style_is_geometry_safe_color_only(style, has_unsafe_losses))
}

fn conditional_metadata_requires_text_measurement(
    metadata: Option<&rxls::ConditionalFormatMetadata>,
) -> bool {
    let Some(metadata) = metadata else {
        return false;
    };
    let has_unsafe_losses = metadata
        .style_losses
        .iter()
        .any(|loss| loss.kind != StyleLossKind::UnresolvedColor);
    let unresolved_color_without_retained_font = metadata
        .style_losses
        .iter()
        .any(|loss| loss.kind == StyleLossKind::UnresolvedColor)
        && metadata
            .differential_style
            .as_ref()
            .is_none_or(|style| style.font.is_none());
    has_unsafe_losses
        || unresolved_color_without_retained_font
        || metadata.differential_style.as_ref().is_some_and(|style| {
            style.font.is_some() || style.align.is_some() || style.num_fmt.is_some()
        })
}

fn conditional_metadata_text_measurement_is_unresolved(
    metadata: Option<&rxls::ConditionalFormatMetadata>,
) -> bool {
    let Some(metadata) = metadata else {
        return false;
    };
    metadata
        .style_losses
        .iter()
        .any(|loss| loss.kind != StyleLossKind::UnresolvedColor)
        || (metadata
            .style_losses
            .iter()
            .any(|loss| loss.kind == StyleLossKind::UnresolvedColor)
            && metadata
                .differential_style
                .as_ref()
                .is_none_or(|style| style.font.is_none()))
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum ColorOnlyConditionalActivation {
    Known(BTreeSet<CellCoordinate>),
    Unknown,
}

#[cfg(test)]
impl ColorOnlyConditionalActivation {
    fn requires_individual_metrics(&self, source: CellCoordinate) -> bool {
        match self {
            Self::Known(active) => active.contains(&source),
            Self::Unknown => true,
        }
    }
}

#[cfg(test)]
fn active_color_only_conditional_cells(
    sheet: &Sheet,
    candidates: &[DisplayCell<'_>],
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<ColorOnlyConditionalActivation, RenderError> {
    let mut relevant_rule = None;
    for (index, metadata) in sheet.conditional_format_metadata().iter().enumerate() {
        if conditional_metadata_text_measurement_is_unresolved(Some(metadata)) {
            return Ok(ColorOnlyConditionalActivation::Unknown);
        }
        let Some(style) = metadata.differential_style.as_ref() else {
            continue;
        };
        let has_unsafe_losses = metadata
            .style_losses
            .iter()
            .any(|loss| loss.kind != StyleLossKind::UnresolvedColor);
        if !conditional_style_is_geometry_safe_color_only(style, has_unsafe_losses) {
            continue;
        }
        if relevant_rule.replace(index).is_some() {
            return Ok(ColorOnlyConditionalActivation::Unknown);
        }
    }
    let Some(rule_index) = relevant_rule else {
        return Ok(ColorOnlyConditionalActivation::Known(BTreeSet::new()));
    };
    let Some(conditional) = sheet.conditional_formats().get(rule_index) else {
        return Ok(ColorOnlyConditionalActivation::Unknown);
    };
    let CfRule::CellIs {
        op,
        formula1,
        formula2,
        ..
    } = &conditional.rule
    else {
        return Ok(ColorOnlyConditionalActivation::Unknown);
    };
    let (first_row, first_col, last_row, last_col) = conditional.sqref;
    if first_row > last_row || first_col > last_col {
        return Ok(ColorOnlyConditionalActivation::Unknown);
    }
    let Some(first) = parse_conditional_operand(formula1) else {
        warnings.add(WarningCode::ConditionalFormattingDeferred, None);
        return Ok(ColorOnlyConditionalActivation::Unknown);
    };
    let second = match op {
        DvOp::Between | DvOp::NotBetween => {
            let Some(second) = formula2.as_deref().and_then(parse_conditional_operand) else {
                warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                return Ok(ColorOnlyConditionalActivation::Unknown);
            };
            Some(second)
        }
        _ => None,
    };
    let mut evaluations = 0_u64;
    let mut active = BTreeSet::new();
    for &cell in candidates {
        bump_conditional_evaluations(&mut evaluations, options)?;
        let source = CellCoordinate {
            row: cell.row,
            col: cell.col,
        };
        if !coordinate_in_range(source, conditional.sqref) {
            continue;
        }
        let Some(value) = numeric_cell_value(cell.value) else {
            continue;
        };
        let Some(first) = resolve_conditional_operand(&first, sheet, source, conditional.sqref)
        else {
            warnings.add(WarningCode::ConditionalFormattingDeferred, None);
            return Ok(ColorOnlyConditionalActivation::Unknown);
        };
        let second = match second.as_ref() {
            Some(second) => {
                let Some(value) =
                    resolve_conditional_operand(second, sheet, source, conditional.sqref)
                else {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    return Ok(ColorOnlyConditionalActivation::Unknown);
                };
                Some(value)
            }
            None => None,
        };
        if compare_conditional(value, *op, first, second) {
            active.insert(source);
        }
    }
    Ok(ColorOnlyConditionalActivation::Known(active))
}

#[derive(Debug, Clone)]
struct ConditionalLayoutCell {
    effective_style: Option<CellStyle>,
    active_style: Option<CellStyle>,
}

fn resolve_conditional_layout_cells(
    sheet: &Sheet,
    candidates: &[DisplayCell<'_>],
    style_snapshot: &RenderStyleSnapshot,
    options: &RenderOptions,
    evaluations: &mut u64,
) -> Result<BTreeMap<CellCoordinate, ConditionalLayoutCell>, RenderError> {
    if sheet.conditional_formats().is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut display_cells = BTreeMap::new();
    for &cell in candidates {
        if cell.row > MAX_WORKSHEET_ROW || cell.col > MAX_WORKSHEET_COLUMN {
            continue;
        }
        display_cells.insert(
            CellCoordinate {
                row: cell.row,
                col: cell.col,
            },
            cell,
        );
    }
    let mut regions = display_cells
        .keys()
        .copied()
        .map(|source| Region {
            source,
            rect: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_raw(1),
                height: Fixed::from_raw(1),
            },
            is_merged: false,
            line_layout_policy: CellLineLayoutPolicy::Native,
            calc_wrap_space: None,
            style: style_snapshot
                .owned_style(source)
                .or_else(|| sheet.resolved_cell_style(source.row, source.col)),
            conditional: ConditionalPaint::default(),
            text: String::new(),
            rich_text: None,
            hyperlink: None,
            numeric_default: false,
            text_can_overflow: false,
            ods_fixed_height_row: false,
            print_vertical_overflow: false,
            vertical_margin: calc_cell_vertical_margin(sheet),
        })
        .collect::<Vec<_>>();
    let mut measurement_warnings = Warnings::default();
    let deferred = resolve_conditional_paints(
        sheet,
        &display_cells,
        &mut regions,
        options,
        &mut measurement_warnings,
        evaluations,
        true,
    )?;
    if deferred {
        return Err(RenderError::Typography {
            reason: "conditional_text_layout_unresolved",
        });
    }
    let mut resolved = BTreeMap::new();
    for region in regions {
        if region
            .conditional
            .style
            .as_ref()
            .is_some_and(|style| style.num_fmt.is_some())
        {
            return Err(RenderError::Typography {
                reason: "conditional_number_format_layout_unresolved",
            });
        }
        resolved.insert(
            region.source,
            ConditionalLayoutCell {
                effective_style: region.style,
                active_style: region.conditional.style,
            },
        );
    }
    Ok(resolved)
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
    } else {
        CellLineLayoutPolicy::Native
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CalcScriptClass {
    Western,
    Asian,
    Complex,
}

fn calc_uax_script_class(script: Script) -> Option<CalcScriptClass> {
    match script {
        Script::Common | Script::Inherited | Script::Unknown => None,
        Script::Bopomofo
        | Script::Han
        | Script::Hangul
        | Script::Hiragana
        | Script::Katakana
        | Script::Khitan_Small_Script
        | Script::Tangut
        | Script::Yi => Some(CalcScriptClass::Asian),
        Script::Armenian
        | Script::Braille
        | Script::Canadian_Aboriginal
        | Script::Cherokee
        | Script::Coptic
        | Script::Cypriot
        | Script::Cyrillic
        | Script::Georgian
        | Script::Glagolitic
        | Script::Gothic
        | Script::Greek
        | Script::Latin
        | Script::Ogham
        | Script::Old_Hungarian
        | Script::Old_Italic
        | Script::Osmanya
        | Script::Runic
        | Script::Shavian => Some(CalcScriptClass::Western),
        _ => Some(CalcScriptClass::Complex),
    }
}

// Mirrors LibreOffice `i18nutil::GetScriptClass`: compatibility code-point
// overrides and Unicode block classifications precede the UAX #24 fallback.
fn calc_script_class(character: char) -> Option<CalcScriptClass> {
    let codepoint = character as u32;
    if matches!(
        codepoint,
        0x0001
            | 0x0002
            | 0x0020
            | 0x00a0
            | 0x00b2
            | 0x00b3
            | 0x00b9
            | 0x02c7
            | 0x02ca
            | 0x02cb
            | 0x02d9
    ) {
        return None;
    }
    if (0x2c80..=0x2ce3).contains(&codepoint) {
        return Some(CalcScriptClass::Western);
    }
    match codepoint {
        // Basic Latin through Spacing Modifier Letters.
        0x0000..=0x02ff
        // Greek, Cyrillic, and Armenian compatibility blocks.
        | 0x0370..=0x03ff
        | 0x0400..=0x04ff
        | 0x0530..=0x058f
        // Georgian.
        | 0x10a0..=0x10ff
        // Cherokee through Runic.
        | 0x13a0..=0x16ff
        // Latin Extended Additional and Greek Extended.
        | 0x1e00..=0x1fff
        // Latin Extended-C and Latin Extended-D.
        | 0x2c60..=0x2c7f
        | 0xa720..=0xa7ff => Some(CalcScriptClass::Western),

        // Hebrew through Myanmar, retaining the original ICU block set.
        0x0590..=0x05ff
        | 0x0600..=0x06ff
        | 0x0700..=0x074f
        | 0x0780..=0x07bf
        | 0x0900..=0x097f
        | 0x0980..=0x09ff
        | 0x0a00..=0x0a7f
        | 0x0a80..=0x0aff
        | 0x0b00..=0x0b7f
        | 0x0b80..=0x0bff
        | 0x0c00..=0x0c7f
        | 0x0c80..=0x0cff
        | 0x0d00..=0x0d7f
        | 0x0d80..=0x0dff
        | 0x0e00..=0x0e7f
        | 0x0e80..=0x0eff
        | 0x0f00..=0x0fff
        | 0x1000..=0x109f
        // Ethiopic, Khmer, Mongolian, and Arabic presentation forms.
        | 0x1200..=0x137f
        | 0x1780..=0x17ff
        | 0x1800..=0x18af
        | 0xfb50..=0xfdff
        | 0xfe70..=0xfeff => Some(CalcScriptClass::Complex),

        // Hangul Jamo.
        0x1100..=0x11ff
        // CJK Radicals Supplement through Hangul Syllables, retaining the
        // original ICU block set rather than treating every intervening scalar
        // value as Asian.
        | 0x2e80..=0x2eff
        | 0x2f00..=0x2fdf
        | 0x2ff0..=0x2fff
        | 0x3000..=0x303f
        | 0x3040..=0x309f
        | 0x30a0..=0x30ff
        | 0x3100..=0x312f
        | 0x3130..=0x318f
        | 0x3190..=0x319f
        | 0x31a0..=0x31bf
        | 0x3200..=0x32ff
        | 0x3300..=0x33ff
        | 0x3400..=0x4dbf
        | 0x4e00..=0x9fff
        | 0xa000..=0xa48f
        | 0xa490..=0xa4cf
        | 0xac00..=0xd7af
        // Later compatibility blocks named explicitly by Calc.
        | 0xf900..=0xfaff
        | 0xfe30..=0xfe4f
        | 0xff00..=0xffef
        | 0x20000..=0x2a6df
        | 0x2f800..=0x2fa1f
        | 0x31c0..=0x31ef => Some(CalcScriptClass::Asian),

        // Number Forms is explicitly weak in the compatibility table.
        0x2150..=0x218f => None,
        _ => calc_uax_script_class(character.script()),
    }
}

/// Whether `text` mixes Calc script classes inside one cell.
///
/// Calc keeps an internally-uniform cell on its pattern font height; only a
/// cell that itself spans script classes selects a taller face for part of the
/// run and grows the automatic row.
fn has_mixed_calc_script_classes(text: &str) -> bool {
    let mut resolved = None;
    for script in text.chars().filter_map(calc_script_class) {
        if resolved.is_some_and(|resolved| resolved != script) {
            return true;
        }
        resolved = Some(script);
    }
    false
}

/// Partition an ambiguous Calc cell the way EditEngine retains script runs.
///
/// Weak characters inherit the preceding strong script, matching Calc's
/// script-change scanner. Leading weak characters inherit the first strong
/// script. A uniform cell stays on the faster DrawStrings path and therefore
/// carries no semantic groups.
fn calc_edit_engine_semantic_groups(
    text: &str,
    lines: &[PreparedLine],
) -> Result<Vec<GlyphSemanticGroup>, RenderError> {
    let Some(first_script) = text.chars().find_map(calc_script_class) else {
        return Ok(Vec::new());
    };
    let mut current_script = first_script;
    let mut previous_script = first_script;
    let mut boundaries = BTreeSet::from([0_usize, text.len()]);
    for (byte_start, character) in text.char_indices() {
        let script = calc_script_class(character).unwrap_or(previous_script);
        if script != current_script {
            boundaries.insert(byte_start);
            current_script = script;
        }
        previous_script = script;
    }
    if boundaries.len() == 2 {
        return Ok(Vec::new());
    }
    for line in lines {
        boundaries.insert(line.source.start);
        boundaries.insert(line.source.end);
    }
    boundaries
        .into_iter()
        .collect::<Vec<_>>()
        .windows(2)
        .map(|pair| {
            Ok(GlyphSemanticGroup {
                source_start: u64::try_from(pair[0])
                    .map_err(|_| RenderError::CoordinateOverflow)?,
                source_end: u64::try_from(pair[1]).map_err(|_| RenderError::CoordinateOverflow)?,
            })
        })
        .collect()
}

/// Preserve the logical paragraph that Calc exports for clipped ODS wrapping.
///
/// A fixed-height, non-rotated wrapped ODS cell is painted as a clipped
/// EditEngine paragraph. Calc's PDF keeps the complete paragraph in its text
/// semantics when any laid-out line remains visible, even when later lines
/// fall below the row clip. One bounded full-cell group reproduces that
/// contract without changing outline clipping; ordinary and rotated cells
/// retain their script-run grouping.
fn glyph_semantic_groups(
    region: &Region,
    lines: &[PreparedLine],
    block_height: Fixed,
) -> Result<Vec<GlyphSemanticGroup>, RenderError> {
    let retains_complete_ods_paragraph = region.ods_fixed_height_row
        && lines.len() > 1
        && block_height > region.rect.height
        && region
            .style
            .as_ref()
            .and_then(|style| style.align.as_ref())
            .is_some_and(|alignment| alignment.wrap && alignment.rotation == 0);
    if retains_complete_ods_paragraph {
        return Ok(vec![GlyphSemanticGroup {
            source_start: 0,
            source_end: u64::try_from(region.text.len())
                .map_err(|_| RenderError::CoordinateOverflow)?,
        }]);
    }
    calc_edit_engine_semantic_groups(&region.text, lines)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CalcScriptClassSummary {
    first: Option<CalcScriptClass>,
    mixed: bool,
    has_asian: bool,
    has_complex: bool,
}

impl CalcScriptClassSummary {
    fn record(&mut self, script: CalcScriptClass) {
        self.has_asian |= script == CalcScriptClass::Asian;
        self.has_complex |= script == CalcScriptClass::Complex;
        if self.first.is_some_and(|first| first != script) {
            self.mixed = true;
        } else if self.first.is_none() {
            self.first = Some(script);
        }
    }

    fn merge(&mut self, other: Self) {
        self.has_asian |= other.has_asian;
        self.has_complex |= other.has_complex;
        self.mixed |= other.mixed;
        if let Some(script) = other.first {
            self.record(script);
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CalcCellScriptAnalysis {
    edit_engine_uses_only_complex_role: bool,
}

fn account_automatic_text_bytes(
    text: &str,
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<(), RenderError> {
    stats.text_bytes = stats
        .text_bytes
        .checked_add(text.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::TextBytes,
        options.limits.max_text_bytes,
        stats.text_bytes,
    )
}

fn calc_script_class_summary_bounded(
    text: &str,
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<CalcScriptClassSummary, RenderError> {
    let base_work = stats.text_work;
    let mut scanned = 0_u64;
    let mut summary = CalcScriptClassSummary::default();
    for character in text.chars() {
        scanned = scanned
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        let actual = base_work
            .checked_add(scanned)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(LimitKind::TextRuns, options.limits.max_text_runs, actual)?;
        if let Some(script) = calc_script_class(character) {
            summary.record(script);
        }
    }
    stats.text_work = base_work
        .checked_add(scanned)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(summary)
}

fn is_explicit_bidi_control(class: BidiClass) -> bool {
    matches!(
        class,
        BidiClass::LRE
            | BidiClass::RLE
            | BidiClass::LRO
            | BidiClass::RLO
            | BidiClass::PDF
            | BidiClass::LRI
            | BidiClass::RLI
            | BidiClass::FSI
            | BidiClass::PDI
    )
}

/// Whether Calc's `MakeScriptChangeScanner` assigns every scalar in one
/// ambiguous plain-text cell to the COMPLEX role.
///
/// `unicode-bidi` supplies the same UAX #9 logical embedding levels consumed
/// by LibreOffice's `MakeDirectionChangeScanner`. Within an RTL level, or an
/// embedded LTR level that has no strong LTR scalar, EditEngine promotes every
/// non-Asian script class (including ASCII punctuation and digits) to COMPLEX.
/// Explicit embeddings, overrides, and isolates fail closed because faithfully
/// replaying their directional-status stack is outside this source-specific
/// automatic-row contract.
fn calc_edit_engine_uses_only_complex_role(
    text: &str,
    summary: CalcScriptClassSummary,
    options: &RenderOptions,
) -> Result<bool, RenderError> {
    if !summary.mixed || !summary.has_complex || text.is_empty() {
        return Ok(false);
    }
    enforce(
        LimitKind::TextBytes,
        options.limits.max_text_bytes,
        text.len() as u64,
    )?;
    if text.chars().map(bidi_class).any(is_explicit_bidi_control) {
        return Ok(false);
    }

    let bidi = BidiInfo::new(text, None);
    let Some(paragraph) = bidi.paragraphs.first() else {
        return Ok(false);
    };
    if bidi.paragraphs.len() != 1 || paragraph.range != (0..text.len()) {
        return Ok(false);
    }

    let mut previous = text
        .chars()
        .find_map(calc_script_class)
        .unwrap_or(CalcScriptClass::Western);
    let mut run_start = 0_usize;
    while run_start < text.len() {
        let Some(level) = bidi.levels.get(run_start).copied() else {
            return Ok(false);
        };
        let mut run_end = text.len();
        for (relative, _) in text[run_start..].char_indices().skip(1) {
            let index = run_start
                .checked_add(relative)
                .ok_or(RenderError::CoordinateOverflow)?;
            if bidi.levels.get(index).copied() != Some(level) {
                run_end = index;
                break;
            }
        }
        if run_end <= run_start || !text.is_char_boundary(run_end) {
            return Ok(false);
        }
        let run = &text[run_start..run_end];
        let embedded_ltr_has_strong = level.is_ltr()
            && level.number() > 1
            && run
                .chars()
                .map(bidi_class)
                .any(|class| class == BidiClass::L);
        for character in run.chars() {
            let mut script = calc_script_class(character);
            if (level.is_rtl() || (level.number() > 0 && !embedded_ltr_has_strong))
                && script != Some(CalcScriptClass::Asian)
            {
                script = Some(CalcScriptClass::Complex);
            } else if script.is_none() {
                script = Some(previous);
            }
            let Some(script) = script else {
                return Ok(false);
            };
            if script != CalcScriptClass::Complex {
                return Ok(false);
            }
            previous = script;
        }
        run_start = run_end;
    }
    Ok(true)
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
        account_automatic_text_bytes(cell.formatted, options, typography)?;
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

fn column_chars_to_fixed(
    chars: f32,
    maximum_digit_width: Fixed,
    padding_pixels: u16,
) -> Option<Fixed> {
    if !chars.is_finite() || chars <= 0.0 {
        return None;
    }
    let digit_pixels = (maximum_digit_width.raw() as f64 / FIXED_UNITS_PER_PIXEL as f64)
        .floor()
        .max(1.0);
    // ECMA-376 18.3.1.13 uses the maximum digit width of the workbook's
    // default font. The caller selects the source-specific device-pixel
    // allowance: Calc-compatible import geometry or the ECMA fallback.
    let pixels = (((f64::from(chars) * 256.0 + (128.0 / digit_pixels).floor()) / 256.0)
        * digit_pixels)
        .floor()
        + f64::from(padding_pixels);
    float_pixels_to_fixed(pixels)
}

fn xlsb_digits_to_fixed(
    width_256: u32,
    maximum_digit_width: Fixed,
    extra_screen_pixels: u16,
) -> Option<Fixed> {
    let width_twips = xlsb_digits_to_twips(width_256, maximum_digit_width, extra_screen_pixels)?;
    let raw = width_twips
        .checked_mul(i128::from(FIXED_UNITS_PER_PIXEL))?
        .checked_add(TWIPS_PER_CSS_PIXEL / 2)?
        .checked_div(TWIPS_PER_CSS_PIXEL)?;
    if raw <= 0 {
        return None;
    }
    i64::try_from(raw).ok().map(Fixed::from_raw)
}

fn xlsb_digits_to_twips(
    width_256: u32,
    maximum_digit_width: Fixed,
    extra_screen_pixels: u16,
) -> Option<i128> {
    if width_256 == 0 || maximum_digit_width.raw() <= 0 {
        return None;
    }
    let digit_twips = i128::from(maximum_digit_width.raw())
        .checked_mul(TWIPS_PER_CSS_PIXEL)?
        .checked_div(i128::from(FIXED_UNITS_PER_PIXEL))?;
    if digit_twips <= 0 {
        return None;
    }
    let width_twips = i128::from(width_256)
        .checked_mul(digit_twips)?
        .checked_add(i128::from(XLSB_DIGIT_WIDTH_SCALE / 2))?
        .checked_div(i128::from(XLSB_DIGIT_WIDTH_SCALE))?
        .checked_add(i128::from(extra_screen_pixels) * TWIPS_PER_CSS_PIXEL)?;
    (width_twips > 0).then_some(width_twips)
}

fn points_to_fixed(points: f32) -> Option<Fixed> {
    if !points.is_finite() || points <= 0.0 {
        return None;
    }
    float_pixels_to_fixed(f64::from(points) * 4.0 / 3.0)
}

fn float_pixels_to_fixed(pixels: f64) -> Option<Fixed> {
    let raw = (pixels * FIXED_UNITS_PER_PIXEL as f64).round();
    if !raw.is_finite() || raw <= 0.0 || raw > i64::MAX as f64 {
        None
    } else {
        Some(Fixed::from_raw(raw as i64))
    }
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
        let style = text_style(region, options, sheet_right_to_left);
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
            let padding = outlined_horizontal_padding(pack, request, font_size, region, options)?;
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
    let shaped = shape_text(pack, text, request, direction, options)?;
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

fn resolve_conditional_paints(
    sheet: &Sheet,
    display_cells: &BTreeMap<CellCoordinate, DisplayCell<'_>>,
    regions: &mut [Region],
    options: &RenderOptions,
    warnings: &mut Warnings,
    evaluations: &mut u64,
    retain_layout_fields: bool,
) -> Result<bool, RenderError> {
    let mut paints = BTreeMap::<CellCoordinate, ConditionalPaint>::new();
    let mut stopped = BTreeSet::<CellCoordinate>::new();
    let mut deferred_text_layout = false;
    let metadata = sheet.conditional_format_metadata();
    let mut rule_order = (0..sheet.conditional_formats().len()).collect::<Vec<_>>();
    rule_order.sort_by_key(|&index| {
        let authored_priority = u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1);
        (
            metadata
                .get(index)
                .and_then(|metadata| metadata.priority)
                .unwrap_or(authored_priority),
            index,
        )
    });
    for rule_index in rule_order {
        let conditional = &sheet.conditional_formats()[rule_index];
        let rule_metadata = metadata.get(rule_index);
        let text_measurement_relevant =
            conditional_metadata_requires_text_measurement(rule_metadata);
        let text_measurement_unresolved =
            conditional_metadata_text_measurement_is_unresolved(rule_metadata);
        let stop_if_true = rule_metadata.is_some_and(|metadata| metadata.stop_if_true);
        if let Some(metadata) = rule_metadata {
            for loss in &metadata.style_losses {
                warnings.add_count(
                    WarningCode::ConditionalFormattingDeferred,
                    u64::from(loss.occurrences),
                    None,
                );
            }
        }
        let differential_style = rule_metadata
            .and_then(|metadata| metadata.differential_style.as_ref())
            .cloned()
            .map(|mut style| {
                if style.num_fmt.is_some() {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    if !retain_layout_fields {
                        style.num_fmt = None;
                    }
                }
                if style.protection.is_some() {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    if !retain_layout_fields {
                        style.protection = None;
                    }
                }
                style
            });
        let has_imported_differential =
            rule_metadata.is_some_and(|metadata| metadata.differential_style.is_some());
        let range = conditional.sqref;
        let measurement_range_intersects = text_measurement_relevant
            && regions
                .iter()
                .any(|region| coordinate_in_range(region.source, range));
        if range.0 > range.2 || range.1 > range.3 {
            warnings.add(WarningCode::ConditionalFormattingDeferred, None);
            deferred_text_layout |= measurement_range_intersects;
            continue;
        }
        match &conditional.rule {
            CfRule::CellIs {
                op,
                formula1,
                formula2,
                fill,
            } => {
                let Some(first) = parse_conditional_operand(formula1) else {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    deferred_text_layout |= measurement_range_intersects;
                    continue;
                };
                let second = match op {
                    DvOp::Between | DvOp::NotBetween => {
                        let Some(second) = formula2.as_deref().and_then(parse_conditional_operand)
                        else {
                            warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                            deferred_text_layout |= measurement_range_intersects;
                            continue;
                        };
                        Some(second)
                    }
                    _ => None,
                };
                let mut matches = Vec::new();
                let mut deferred = false;
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(value) = display_cells
                        .get(&region.source)
                        .and_then(|cell| numeric_cell_value(cell.value))
                    else {
                        continue;
                    };
                    let Some(first) =
                        resolve_conditional_operand(&first, sheet, region.source, range)
                    else {
                        deferred = true;
                        break;
                    };
                    let second = match second.as_ref() {
                        Some(second) => {
                            let Some(value) =
                                resolve_conditional_operand(second, sheet, region.source, range)
                            else {
                                deferred = true;
                                break;
                            };
                            Some(value)
                        }
                        None => None,
                    };
                    if compare_conditional(value, *op, first, second) {
                        matches.push(region.source);
                    }
                }
                if deferred {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    deferred_text_layout |= measurement_range_intersects;
                    continue;
                }
                for coordinate in matches {
                    apply_conditional_paint(
                        &mut paints,
                        &mut stopped,
                        coordinate,
                        ConditionalOutcome {
                            style: Some(conditional_fill_overlay(
                                rgb(*fill),
                                differential_style.as_ref(),
                                has_imported_differential,
                            )),
                            data_bar: None,
                            stop_if_true,
                            text_measurement_unresolved,
                        },
                        &mut deferred_text_layout,
                    );
                }
            }
            CfRule::ColorScale2 { min, max } => {
                let values = conditional_numeric_values(sheet, range, evaluations, options)?;
                let Some((minimum, maximum)) = numeric_bounds(&values) else {
                    continue;
                };
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(value) = display_cells
                        .get(&region.source)
                        .and_then(|cell| numeric_cell_value(cell.value))
                    else {
                        continue;
                    };
                    let ratio = normalized_ppm(value, minimum, maximum);
                    apply_conditional_paint(
                        &mut paints,
                        &mut stopped,
                        region.source,
                        ConditionalOutcome {
                            style: Some(conditional_fill_overlay(
                                interpolate_rgb(rgb(*min), rgb(*max), ratio),
                                differential_style.as_ref(),
                                has_imported_differential,
                            )),
                            data_bar: None,
                            stop_if_true,
                            text_measurement_unresolved,
                        },
                        &mut deferred_text_layout,
                    );
                }
            }
            CfRule::ColorScale3 { min, mid, max } => {
                let mut values = conditional_numeric_values(sheet, range, evaluations, options)?;
                if values.is_empty() {
                    continue;
                }
                values.sort_by(f64::total_cmp);
                let minimum = values[0];
                let maximum = values[values.len() - 1];
                let midpoint = percentile_50(&values);
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(value) = display_cells
                        .get(&region.source)
                        .and_then(|cell| numeric_cell_value(cell.value))
                    else {
                        continue;
                    };
                    let color = if value <= midpoint {
                        interpolate_rgb(
                            rgb(*min),
                            rgb(*mid),
                            normalized_ppm(value, minimum, midpoint),
                        )
                    } else {
                        interpolate_rgb(
                            rgb(*mid),
                            rgb(*max),
                            normalized_ppm(value, midpoint, maximum),
                        )
                    };
                    apply_conditional_paint(
                        &mut paints,
                        &mut stopped,
                        region.source,
                        ConditionalOutcome {
                            style: Some(conditional_fill_overlay(
                                color,
                                differential_style.as_ref(),
                                has_imported_differential,
                            )),
                            data_bar: None,
                            stop_if_true,
                            text_measurement_unresolved,
                        },
                        &mut deferred_text_layout,
                    );
                }
            }
            CfRule::DataBar { color } => {
                let values = conditional_numeric_values(sheet, range, evaluations, options)?;
                let Some((minimum, maximum)) = numeric_bounds(&values) else {
                    continue;
                };
                warnings.add(WarningCode::ConditionalDataBarSimplified, None);
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(value) = display_cells
                        .get(&region.source)
                        .and_then(|cell| numeric_cell_value(cell.value))
                    else {
                        continue;
                    };
                    apply_conditional_paint(
                        &mut paints,
                        &mut stopped,
                        region.source,
                        ConditionalOutcome {
                            style: differential_style.clone(),
                            data_bar: Some(DataBarPaint {
                                color: rgb(*color),
                                width_ppm: normalized_ppm(value, minimum, maximum),
                            }),
                            stop_if_true,
                            text_measurement_unresolved,
                        },
                        &mut deferred_text_layout,
                    );
                }
            }
            CfRule::TopBottom {
                rank,
                bottom,
                percent,
                fill,
            } => {
                let mut values = conditional_numeric_values(sheet, range, evaluations, options)?;
                if values.is_empty() || *rank == 0 {
                    continue;
                }
                values.sort_by(f64::total_cmp);
                let selected = if *percent {
                    let percentage = u64::from((*rank).min(100));
                    ((values.len() as u64)
                        .checked_mul(percentage)
                        .ok_or(RenderError::CoordinateOverflow)?
                        .saturating_add(99)
                        / 100) as usize
                } else {
                    usize::try_from(*rank).unwrap_or(usize::MAX)
                }
                .max(1)
                .min(values.len());
                let threshold = if *bottom {
                    values[selected - 1]
                } else {
                    values[values.len() - selected]
                };
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(value) = display_cells
                        .get(&region.source)
                        .and_then(|cell| numeric_cell_value(cell.value))
                    else {
                        continue;
                    };
                    if (*bottom && value <= threshold) || (!*bottom && value >= threshold) {
                        apply_conditional_paint(
                            &mut paints,
                            &mut stopped,
                            region.source,
                            ConditionalOutcome {
                                style: Some(conditional_fill_overlay(
                                    rgb(*fill),
                                    differential_style.as_ref(),
                                    has_imported_differential,
                                )),
                                data_bar: None,
                                stop_if_true,
                                text_measurement_unresolved,
                            },
                            &mut deferred_text_layout,
                        );
                    }
                }
            }
            CfRule::AboveAverage { below, fill } => {
                let values = conditional_numeric_values(sheet, range, evaluations, options)?;
                if values.is_empty() {
                    continue;
                }
                let sum = values.iter().try_fold(0.0_f64, |sum, value| {
                    let next = sum + value;
                    next.is_finite().then_some(next)
                });
                let Some(sum) = sum else {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    deferred_text_layout |= measurement_range_intersects;
                    continue;
                };
                let average = sum / values.len() as f64;
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(value) = display_cells
                        .get(&region.source)
                        .and_then(|cell| numeric_cell_value(cell.value))
                    else {
                        continue;
                    };
                    if (*below && value < average) || (!*below && value > average) {
                        apply_conditional_paint(
                            &mut paints,
                            &mut stopped,
                            region.source,
                            ConditionalOutcome {
                                style: Some(conditional_fill_overlay(
                                    rgb(*fill),
                                    differential_style.as_ref(),
                                    has_imported_differential,
                                )),
                                data_bar: None,
                                stop_if_true,
                                text_measurement_unresolved,
                            },
                            &mut deferred_text_layout,
                        );
                    }
                }
            }
            CfRule::DuplicateValues { unique, fill } => {
                let Some(keys) = conditional_value_keys(sheet, range, evaluations, options)? else {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    deferred_text_layout |= measurement_range_intersects;
                    continue;
                };
                let mut counts = BTreeMap::<ConditionalValueKey, u64>::new();
                for key in keys.values() {
                    let count = counts.entry(key.clone()).or_default();
                    *count = count
                        .checked_add(1)
                        .ok_or(RenderError::CoordinateOverflow)?;
                }
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(key) = keys.get(&region.source) else {
                        continue;
                    };
                    let count = counts.get(key).copied().unwrap_or(0);
                    if (*unique && count == 1) || (!*unique && count > 1) {
                        apply_conditional_paint(
                            &mut paints,
                            &mut stopped,
                            region.source,
                            ConditionalOutcome {
                                style: Some(conditional_fill_overlay(
                                    rgb(*fill),
                                    differential_style.as_ref(),
                                    has_imported_differential,
                                )),
                                data_bar: None,
                                stop_if_true,
                                text_measurement_unresolved,
                            },
                            &mut deferred_text_layout,
                        );
                    }
                }
            }
            CfRule::Expression { formula, fill } => {
                let Some(expression) = parse_conditional_expression(formula) else {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    deferred_text_layout |= measurement_range_intersects;
                    continue;
                };
                let mut matches = Vec::new();
                let mut deferred = false;
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(left) =
                        resolve_conditional_operand(&expression.left, sheet, region.source, range)
                    else {
                        deferred = true;
                        break;
                    };
                    let Some(right) =
                        resolve_conditional_operand(&expression.right, sheet, region.source, range)
                    else {
                        deferred = true;
                        break;
                    };
                    if expression.op.compare(left, right) {
                        matches.push(region.source);
                    }
                }
                if deferred {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    deferred_text_layout |= measurement_range_intersects;
                    continue;
                }
                for coordinate in matches {
                    apply_conditional_paint(
                        &mut paints,
                        &mut stopped,
                        coordinate,
                        ConditionalOutcome {
                            style: Some(conditional_fill_overlay(
                                rgb(*fill),
                                differential_style.as_ref(),
                                has_imported_differential,
                            )),
                            data_bar: None,
                            stop_if_true,
                            text_measurement_unresolved,
                        },
                        &mut deferred_text_layout,
                    );
                }
            }
        }
    }
    for region in regions {
        let Some(paint) = paints.remove(&region.source) else {
            continue;
        };
        if let Some(overlay) = paint.style.as_ref() {
            region.style = Some(match region.style.take() {
                Some(base) => base.merge(overlay),
                None => overlay.clone(),
            });
        }
        region.conditional = paint;
    }
    Ok(deferred_text_layout)
}

fn bump_conditional_evaluations(
    evaluations: &mut u64,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    *evaluations = evaluations
        .checked_add(1)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::ConditionalEvaluations,
        options.limits.max_conditional_evaluations,
        *evaluations,
    )
}

fn conditional_numeric_values(
    sheet: &Sheet,
    range: (u32, u16, u32, u16),
    evaluations: &mut u64,
    options: &RenderOptions,
) -> Result<Vec<f64>, RenderError> {
    let mut values = Vec::new();
    // Aggregate conditional rules are defined over their authored sqref, not
    // the clipped scene. The sparse sheet iterator visits only retained cells,
    // while each visit still consumes the shared conditional-evaluation budget.
    for cell in SparseDisplayCellIndex::new(sheet).range(range) {
        bump_conditional_evaluations(evaluations, options)?;
        if let Some(value) = numeric_cell_value(cell.value) {
            values.push(value);
        }
    }
    Ok(values)
}

fn coordinate_in_range(
    coordinate: CellCoordinate,
    (first_row, first_col, last_row, last_col): (u32, u16, u32, u16),
) -> bool {
    (first_row..=last_row).contains(&coordinate.row)
        && (first_col..=last_col).contains(&coordinate.col)
}

fn numeric_cell_value(cell: &Cell) -> Option<f64> {
    let mut cell = cell;
    for _ in 0..=64 {
        match cell {
            Cell::Number(value) | Cell::Date(value) if value.is_finite() => return Some(*value),
            Cell::Formula { cached, .. } => cell = cached,
            Cell::Number(_) | Cell::Date(_) | Cell::Text(_) | Cell::Bool(_) | Cell::Error(_) => {
                return None;
            }
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq)]
enum ConditionalOperand {
    Literal(f64),
    Reference(A1Reference),
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

fn parse_conditional_operand(formula: &str) -> Option<ConditionalOperand> {
    let formula = formula.trim().strip_prefix('=').unwrap_or(formula.trim());
    if let Ok(value) = formula.parse::<f64>() {
        return value
            .is_finite()
            .then_some(ConditionalOperand::Literal(value));
    }
    parse_a1_reference(formula).map(ConditionalOperand::Reference)
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

fn a1_range_points(range: &A1RangeReference) -> Option<u64> {
    let rows = u64::from(range.last_row) - u64::from(range.first_row) + 1;
    let columns = u64::from(range.last_col) - u64::from(range.first_col) + 1;
    rows.checked_mul(columns)
}

fn reserve_chart_points(
    total: &mut u64,
    additional: u64,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let actual = total
        .checked_add(additional)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::ChartPoints,
        options.limits.max_chart_points,
        actual,
    )?;
    *total = actual;
    Ok(())
}

fn resolve_numeric_a1_range(
    sheet: &Sheet,
    source: &str,
    points: &mut u64,
    options: &RenderOptions,
    require_one_dimension: bool,
) -> Result<Option<Vec<f64>>, RenderError> {
    let Some(range) = parse_a1_range(source) else {
        return Ok(None);
    };
    if !range_belongs_to_sheet(&range, sheet)
        || (require_one_dimension
            && range.first_row != range.last_row
            && range.first_col != range.last_col)
    {
        return Ok(None);
    }
    let Some(count) = a1_range_points(&range) else {
        return Err(RenderError::CoordinateOverflow);
    };
    reserve_chart_points(points, count, options)?;
    let capacity = usize::try_from(count).map_err(|_| RenderError::CoordinateOverflow)?;
    let mut values = Vec::with_capacity(capacity);
    for row in range.first_row..=range.last_row {
        for col in range.first_col..=range.last_col {
            let Some(value) = sheet.cell(row, col).and_then(numeric_cell_value) else {
                return Ok(None);
            };
            values.push(value);
        }
    }
    Ok(Some(values))
}

fn resolve_label_a1_range(
    sheet: &Sheet,
    source: &str,
    points: &mut u64,
    options: &RenderOptions,
) -> Result<Option<Vec<String>>, RenderError> {
    let Some(range) = parse_a1_range(source) else {
        return Ok(None);
    };
    if !range_belongs_to_sheet(&range, sheet)
        || (range.first_row != range.last_row && range.first_col != range.last_col)
    {
        return Ok(None);
    }
    let Some(count) = a1_range_points(&range) else {
        return Err(RenderError::CoordinateOverflow);
    };
    reserve_chart_points(points, count, options)?;
    let capacity = usize::try_from(count).map_err(|_| RenderError::CoordinateOverflow)?;
    let mut labels = Vec::with_capacity(capacity);
    for row in range.first_row..=range.last_row {
        for col in range.first_col..=range.last_col {
            labels.push(sheet.formatted(row, col).unwrap_or("").to_string());
        }
    }
    Ok(Some(labels))
}

fn contiguous_cached_values(points: &[ChartCachedPoint]) -> Option<Vec<&str>> {
    if points.is_empty() {
        return None;
    }
    points
        .iter()
        .enumerate()
        .map(|(expected, point)| {
            (usize::try_from(point.index).ok()? == expected).then_some(point.value.as_str())
        })
        .collect()
}

fn resolve_numeric_chart_source(
    sheet: &Sheet,
    source: &str,
    cached: &[ChartCachedPoint],
    points: &mut u64,
    options: &RenderOptions,
) -> Result<Option<Vec<f64>>, RenderError> {
    let initial_points = *points;
    if let Some(values) = resolve_numeric_a1_range(sheet, source, points, options, true)? {
        return Ok(Some(values));
    }
    *points = initial_points;
    let Some(cached) = contiguous_cached_values(cached) else {
        return Ok(None);
    };
    reserve_chart_points(points, cached.len() as u64, options)?;
    let values = cached
        .into_iter()
        .map(|value| {
            value
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite())
        })
        .collect::<Option<Vec<_>>>();
    if values.is_none() {
        *points = initial_points;
    }
    Ok(values)
}

fn resolve_label_chart_source(
    sheet: &Sheet,
    source: &str,
    cached: &[ChartCachedPoint],
    points: &mut u64,
    options: &RenderOptions,
) -> Result<Option<Vec<String>>, RenderError> {
    let initial_points = *points;
    if let Some(labels) = resolve_label_a1_range(sheet, source, points, options)? {
        return Ok(Some(labels));
    }
    *points = initial_points;
    let Some(cached) = contiguous_cached_values(cached) else {
        return Ok(None);
    };
    reserve_chart_points(points, cached.len() as u64, options)?;
    Ok(Some(cached.into_iter().map(str::to_string).collect()))
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

fn resolve_conditional_operand(
    operand: &ConditionalOperand,
    sheet: &Sheet,
    target: CellCoordinate,
    origin: (u32, u16, u32, u16),
) -> Option<f64> {
    match operand {
        ConditionalOperand::Literal(value) => Some(*value),
        ConditionalOperand::Reference(reference) => {
            conditional_reference_coordinate(reference, sheet, target, origin)
                .and_then(|coordinate| sheet.cell(coordinate.row, coordinate.col))
                .and_then(numeric_cell_value)
        }
    }
}

fn conditional_reference_coordinate(
    reference: &A1Reference,
    sheet: &Sheet,
    target: CellCoordinate,
    origin: (u32, u16, u32, u16),
) -> Option<CellCoordinate> {
    if reference.sheet.as_deref().is_some_and(|name| {
        name != sheet.name
            && !(name.is_ascii() && sheet.name.is_ascii() && name.eq_ignore_ascii_case(&sheet.name))
    }) {
        return None;
    }
    let row = if reference.row_absolute {
        reference.row
    } else {
        offset_a1_axis(
            u64::from(target.row),
            u64::from(reference.row),
            u64::from(origin.0),
            u64::from(MAX_WORKSHEET_ROW),
        )? as u32
    };
    let col = if reference.col_absolute {
        reference.col
    } else {
        offset_a1_axis(
            u64::from(target.col),
            u64::from(reference.col),
            u64::from(origin.1),
            u64::from(MAX_WORKSHEET_COLUMN),
        )? as u16
    };
    Some(CellCoordinate { row, col })
}

fn offset_a1_axis(target: u64, reference: u64, origin: u64, maximum: u64) -> Option<u64> {
    let value = i128::from(target)
        .checked_add(i128::from(reference))?
        .checked_sub(i128::from(origin))?;
    (0..=i128::from(maximum))
        .contains(&value)
        .then_some(value as u64)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionalComparison {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

impl ConditionalComparison {
    fn compare(self, left: f64, right: f64) -> bool {
        match self {
            Self::Equal => left == right,
            Self::NotEqual => left != right,
            Self::Less => left < right,
            Self::LessOrEqual => left <= right,
            Self::Greater => left > right,
            Self::GreaterOrEqual => left >= right,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ConditionalExpression {
    left: ConditionalOperand,
    op: ConditionalComparison,
    right: ConditionalOperand,
}

fn parse_conditional_expression(formula: &str) -> Option<ConditionalExpression> {
    let formula = formula.trim().strip_prefix('=').unwrap_or(formula.trim());
    let bytes = formula.as_bytes();
    let mut quoted = false;
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\'' {
            if quoted && bytes.get(cursor + 1) == Some(&b'\'') {
                cursor += 2;
                continue;
            }
            quoted = !quoted;
            cursor += 1;
            continue;
        }
        if quoted {
            cursor += 1;
            continue;
        }
        let (op, width) = match (bytes[cursor], bytes.get(cursor + 1).copied()) {
            (b'<', Some(b'>')) => (ConditionalComparison::NotEqual, 2),
            (b'<', Some(b'=')) => (ConditionalComparison::LessOrEqual, 2),
            (b'>', Some(b'=')) => (ConditionalComparison::GreaterOrEqual, 2),
            (b'=', _) => (ConditionalComparison::Equal, 1),
            (b'<', _) => (ConditionalComparison::Less, 1),
            (b'>', _) => (ConditionalComparison::Greater, 1),
            _ => {
                cursor += 1;
                continue;
            }
        };
        let left = parse_conditional_operand(&formula[..cursor])?;
        let right = parse_conditional_operand(&formula[cursor + width..])?;
        return Some(ConditionalExpression { left, op, right });
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ConditionalValueKey {
    Number(u64),
    Text(String),
    Bool(bool),
}

fn conditional_value_key(cell: &Cell) -> Option<ConditionalValueKey> {
    let mut cell = cell;
    for _ in 0..=64 {
        match cell {
            Cell::Number(value) | Cell::Date(value) if value.is_finite() => {
                let value = if *value == 0.0 { 0.0 } else { *value };
                return Some(ConditionalValueKey::Number(value.to_bits()));
            }
            Cell::Text(value)
                if value.is_ascii() && !value.contains('*') && !value.contains('?') =>
            {
                return Some(ConditionalValueKey::Text(value.to_ascii_lowercase()));
            }
            Cell::Bool(value) => return Some(ConditionalValueKey::Bool(*value)),
            Cell::Formula { cached, .. } => cell = cached,
            Cell::Number(_) | Cell::Date(_) | Cell::Text(_) | Cell::Error(_) => return None,
        }
    }
    None
}

fn conditional_value_keys(
    sheet: &Sheet,
    range: (u32, u16, u32, u16),
    evaluations: &mut u64,
    options: &RenderOptions,
) -> Result<Option<BTreeMap<CellCoordinate, ConditionalValueKey>>, RenderError> {
    if range.2 > MAX_WORKSHEET_ROW || range.3 > MAX_WORKSHEET_COLUMN {
        return Ok(None);
    }
    let rows = u64::from(range.2) - u64::from(range.0) + 1;
    let columns = u64::from(range.3) - u64::from(range.1) + 1;
    let cells = rows
        .checked_mul(columns)
        .ok_or(RenderError::CoordinateOverflow)?;
    let actual = evaluations
        .checked_add(cells)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::ConditionalEvaluations,
        options.limits.max_conditional_evaluations,
        actual,
    )?;
    *evaluations = actual;

    // Duplicate/unique classification likewise uses the complete authored
    // range. This rule intentionally remains exact-only: unsupported or blank
    // members still defer the whole result instead of guessing.
    let mut keys = BTreeMap::new();
    for row in range.0..=range.2 {
        for col in range.1..=range.3 {
            let coordinate = CellCoordinate { row, col };
            let Some(key) = sheet.cell(row, col).and_then(conditional_value_key) else {
                return Ok(None);
            };
            keys.insert(coordinate, key);
        }
    }
    Ok(Some(keys))
}

fn compare_conditional(value: f64, op: DvOp, first: f64, second: Option<f64>) -> bool {
    match op {
        DvOp::Between => second.is_some_and(|second| first <= value && value <= second),
        DvOp::NotBetween => second.is_some_and(|second| value < first || value > second),
        DvOp::Equal => value == first,
        DvOp::NotEqual => value != first,
        DvOp::GreaterThan => value > first,
        DvOp::LessThan => value < first,
        DvOp::GreaterThanOrEqual => value >= first,
        DvOp::LessThanOrEqual => value <= first,
    }
}

fn numeric_bounds(values: &[f64]) -> Option<(f64, f64)> {
    let mut values = values.iter().copied();
    let first = values.next()?;
    Some(values.fold((first, first), |(minimum, maximum), value| {
        (minimum.min(value), maximum.max(value))
    }))
}

fn percentile_50(sorted: &[f64]) -> f64 {
    let upper = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        sorted[upper - 1] / 2.0 + sorted[upper] / 2.0
    } else {
        sorted[upper]
    }
}

fn normalized_ppm(value: f64, minimum: f64, maximum: f64) -> u32 {
    if maximum <= minimum {
        return 1_000_000;
    }
    (((value - minimum) / (maximum - minimum)).clamp(0.0, 1.0) * 1_000_000.0).round() as u32
}

fn interpolate_rgb(start: Rgb, end: Rgb, ratio_ppm: u32) -> Rgb {
    let channel = |start: u8, end: u8| {
        let delta = i64::from(end) - i64::from(start);
        let scaled = i64::from(start) * 1_000_000 + delta * i64::from(ratio_ppm);
        u8::try_from(((scaled + 500_000) / 1_000_000).clamp(0, 255)).unwrap_or(start)
    };
    Rgb::new(
        channel(start.red, end.red),
        channel(start.green, end.green),
        channel(start.blue, end.blue),
    )
}

fn conditional_fill_overlay(
    color: Rgb,
    differential_style: Option<&CellStyle>,
    has_imported_differential: bool,
) -> CellStyle {
    if has_imported_differential {
        return differential_style.cloned().unwrap_or_default();
    }
    CellStyle::new().fill(Color::rgb(color.red, color.green, color.blue))
}

fn apply_conditional_paint(
    paints: &mut BTreeMap<CellCoordinate, ConditionalPaint>,
    stopped: &mut BTreeSet<CellCoordinate>,
    coordinate: CellCoordinate,
    outcome: ConditionalOutcome,
    deferred_text_layout: &mut bool,
) {
    let ConditionalOutcome {
        style,
        data_bar,
        stop_if_true,
        text_measurement_unresolved,
    } = outcome;
    if stopped.contains(&coordinate) {
        return;
    }
    *deferred_text_layout |= text_measurement_unresolved;
    let paint = paints.entry(coordinate).or_default();
    if let Some(lower_priority) = style {
        paint.style = Some(match paint.style.take() {
            // The existing overlay came from a higher-priority rule. Merge it
            // last so each of its explicitly represented properties wins,
            // while the lower-priority rule may still supply missing ones.
            Some(higher_priority) => lower_priority.merge(&higher_priority),
            None => lower_priority,
        });
    }
    if paint.data_bar.is_none() {
        paint.data_bar = data_bar;
    }
    if stop_if_true {
        stopped.insert(coordinate);
    }
}

fn push_data_bar(
    nodes: &mut Vec<SceneNode>,
    rect: Rect,
    paint: DataBarPaint,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    if paint.width_ppm == 0 || rect.width.raw() <= 0 || rect.height.raw() <= 0 {
        return Ok(());
    }
    let horizontal_inset = Fixed::from_pixels(1).max(Fixed::from_raw(1));
    let vertical_inset = Fixed::from_raw((rect.height.raw() / 5).max(1));
    let inner_width = rect
        .width
        .checked_sub(multiply_fixed(horizontal_inset, 2)?)
        .ok_or(RenderError::CoordinateOverflow)?
        .max(Fixed::from_raw(1));
    let inner_height = rect
        .height
        .checked_sub(multiply_fixed(vertical_inset, 2)?)
        .ok_or(RenderError::CoordinateOverflow)?
        .max(Fixed::from_raw(1));
    let width_raw = i128::from(inner_width.raw())
        .checked_mul(i128::from(paint.width_ppm))
        .and_then(|value| value.checked_div(1_000_000))
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(RenderError::CoordinateOverflow)?;
    if width_raw <= 0 {
        return Ok(());
    }
    push_node(
        nodes,
        SceneNode::Rect(RectNode {
            rect: Rect {
                x: rect
                    .x
                    .checked_add(horizontal_inset)
                    .ok_or(RenderError::CoordinateOverflow)?,
                y: rect
                    .y
                    .checked_add(vertical_inset)
                    .ok_or(RenderError::CoordinateOverflow)?,
                width: Fixed::from_raw(width_raw),
                height: inner_height,
            },
            fill: Some(paint.color),
            stroke: None,
            stroke_width: Fixed::ZERO,
        }),
        options,
    )
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
    text_bytes: &mut u64,
    glyphs: &mut u64,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<(), RenderError> {
    let metadata_index = DrawingMetadataIndex::new(sheet);
    let drawing_row_slots = calc_drawing_row_slots(sheet, row_slots);
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
            geometry,
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
            &drawing_row_slots,
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
                if !try_push_chart(
                    &mut object_nodes,
                    placeholder.rect,
                    &sheet.charts()[index],
                    metadata,
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
                &row_slots,
                &col_slots,
                viewport,
                viewport,
                viewport.width,
                DrawingObjectKind::Image,
                image.from,
                to,
                metadata,
                right_to_left,
                Some(geometry),
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

#[allow(clippy::too_many_arguments)]
fn try_push_chart(
    nodes: &mut Vec<SceneNode>,
    rect: Rect,
    chart: &Chart,
    metadata: Option<&DrawingMetadata>,
    sheet: &Sheet,
    chart_points: &mut u64,
    text_bytes: &mut u64,
    glyphs: &mut u64,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
    warnings: &mut Warnings,
    warning_cell: CellCoordinate,
) -> Result<bool, RenderError> {
    if chart.series.is_empty()
        || metadata.is_some_and(|metadata| !metadata.chart_unsupported_reasons.is_empty())
        || rect.width < Fixed::from_pixels(120)
        || rect.height < Fixed::from_pixels(80)
    {
        return Ok(false);
    }
    let initial_points = *chart_points;
    let style_loss_count = metadata.map_or(0_u64, |metadata| {
        metadata.chart_frame_style_losses.len() as u64
            + metadata
                .chart_series_styles
                .iter()
                .map(|style| style.losses.len() as u64)
                .sum::<u64>()
    });
    warnings.add_count(
        WarningCode::ChartMetadataSimplified,
        style_loss_count,
        Some(warning_cell),
    );
    let mut series = Vec::with_capacity(chart.series.len());
    for (index, source) in chart.series.iter().enumerate() {
        let cache = metadata.and_then(|metadata| metadata.chart_series_caches.get(index));
        let value_cache = cache.map_or(&[][..], |cache| cache.values.as_slice());
        let Some(values) = resolve_numeric_chart_source(
            sheet,
            &source.values,
            value_cache,
            chart_points,
            options,
        )?
        else {
            *chart_points = initial_points;
            return Ok(false);
        };
        if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
            *chart_points = initial_points;
            return Ok(false);
        }
        let category_cache = cache.map_or(&[][..], |cache| cache.categories.as_slice());
        let (x_values, labels) = if matches!(chart.kind, ChartKind::Scatter | ChartKind::Bubble) {
            let Some(x_values) = resolve_numeric_chart_source(
                sheet,
                source.categories.as_deref().unwrap_or(""),
                category_cache,
                chart_points,
                options,
            )?
            else {
                *chart_points = initial_points;
                return Ok(false);
            };
            if x_values.len() != values.len() {
                *chart_points = initial_points;
                return Ok(false);
            }
            if x_values.iter().any(|value| !value.is_finite()) {
                *chart_points = initial_points;
                return Ok(false);
            }
            (Some(x_values), Vec::new())
        } else {
            let labels = match source.categories.as_deref() {
                Some(categories) => {
                    let Some(labels) = resolve_label_chart_source(
                        sheet,
                        categories,
                        category_cache,
                        chart_points,
                        options,
                    )?
                    else {
                        *chart_points = initial_points;
                        return Ok(false);
                    };
                    if labels.len() != values.len() {
                        *chart_points = initial_points;
                        return Ok(false);
                    }
                    labels
                }
                None if !category_cache.is_empty() => {
                    let Some(labels) = resolve_label_chart_source(
                        sheet,
                        "",
                        category_cache,
                        chart_points,
                        options,
                    )?
                    else {
                        *chart_points = initial_points;
                        return Ok(false);
                    };
                    if labels.len() != values.len() {
                        *chart_points = initial_points;
                        return Ok(false);
                    }
                    labels
                }
                None => (1..=values.len()).map(|value| value.to_string()).collect(),
            };
            (None, labels)
        };
        let point_count = values.len();
        let bubble_sizes = if chart.kind == ChartKind::Bubble {
            let bubble_cache = cache.map_or(&[][..], |cache| cache.bubble_sizes.as_slice());
            let values = match source.bubble_sizes.as_deref() {
                Some(source) => resolve_numeric_chart_source(
                    sheet,
                    source,
                    bubble_cache,
                    chart_points,
                    options,
                )?,
                None if !bubble_cache.is_empty() => {
                    resolve_numeric_chart_source(sheet, "", bubble_cache, chart_points, options)?
                }
                None => Some(vec![1.0; point_count]),
            };
            let Some(values) = values else {
                *chart_points = initial_points;
                return Ok(false);
            };
            if values.len() != point_count || values.iter().any(|value| *value <= 0.0) {
                *chart_points = initial_points;
                return Ok(false);
            }
            Some(values)
        } else {
            None
        };
        let cached_name = cache
            .and_then(|cache| match cache.name.as_slice() {
                [point] if point.index == 0 && !point.value.trim().is_empty() => Some(point),
                _ => None,
            })
            .map(|point| point.value.trim().to_string());
        series.push(ResolvedChartSeries {
            name: cached_name
                .or_else(|| source.name.clone())
                .unwrap_or_else(|| format!("Series {}", index + 1)),
            values,
            x_values,
            labels,
            bubble_sizes,
            style: metadata
                .and_then(|metadata| metadata.chart_series_styles.get(index))
                .cloned()
                .unwrap_or_default(),
        });
    }
    if matches!(chart.kind, ChartKind::Pie | ChartKind::Doughnut)
        && ((chart.kind == ChartKind::Pie && series.len() != 1)
            || series.iter().any(|series| {
                let total = series.values.iter().sum::<f64>();
                series.values.iter().any(|value| *value < 0.0) || !total.is_finite() || total <= 0.0
            }))
    {
        *chart_points = initial_points;
        return Ok(false);
    }
    if chart.kind == ChartKind::Radar && series.iter().any(|series| series.values.len() < 3) {
        *chart_points = initial_points;
        return Ok(false);
    }
    let data_label_count = series
        .iter()
        .map(|series| series.values.len())
        .sum::<usize>();
    let legend_count = if matches!(chart.kind, ChartKind::Pie | ChartKind::Doughnut) {
        series[0].labels.len()
    } else {
        series.len()
    };
    if (chart.data_labels && data_label_count > 256) || (chart.legend && legend_count > 16) {
        *chart_points = initial_points;
        return Ok(false);
    }

    let chart_title = chart.title.as_deref().filter(|text| !text.is_empty());
    let raw_x_axis_title = chart
        .x_axis_title
        .as_deref()
        .filter(|text| !text.is_empty());
    let raw_y_axis_title = chart
        .y_axis_title
        .as_deref()
        .filter(|text| !text.is_empty());
    let chart_font_family = metadata
        .and_then(|metadata| metadata.chart_default_latin_font_family.as_deref())
        .unwrap_or(&options.default_font_family);
    let typography_before_chart_text = typography_stats.clone();
    let warnings_before_chart_text = warnings.clone();

    let palette = metadata.map_or(&[][..], |metadata| metadata.chart_palette.as_slice());
    let cartesian = matches!(
        chart.kind,
        ChartKind::Bar | ChartKind::Line | ChartKind::Scatter | ChartKind::Area | ChartKind::Bubble
    );
    let horizontal_bar = chart.kind == ChartKind::Bar
        && metadata
            .is_some_and(|metadata| metadata.chart_bar_direction == ChartBarDirection::Horizontal);
    let category_axis_visible = metadata
        .and_then(|metadata| metadata.chart_category_axis_visible)
        .unwrap_or(true);
    let category_axis_shifted = metadata
        .and_then(|metadata| metadata.chart_category_axis_shifted)
        .unwrap_or(false);
    let value_axis_visible = metadata
        .and_then(|metadata| metadata.chart_value_axis_visible)
        .unwrap_or(true);
    let (horizontal_axis_visible, vertical_axis_visible) = if horizontal_bar {
        (value_axis_visible, category_axis_visible)
    } else {
        (category_axis_visible, value_axis_visible)
    };
    let x_axis_title = raw_x_axis_title.filter(|_| horizontal_axis_visible);
    let y_axis_title = raw_y_axis_title.filter(|_| vertical_axis_visible);
    let Some(text_styles) = ResolvedChartTextStyles::resolve(metadata, chart_font_family) else {
        *chart_points = initial_points;
        return Ok(false);
    };
    let (x_axis_title_style, y_axis_title_style) = text_styles.physical_axis_titles(horizontal_bar);
    let (horizontal_axis_style, vertical_axis_style) = if horizontal_bar {
        (
            &text_styles.value_axis_labels,
            &text_styles.category_axis_labels,
        )
    } else {
        (
            &text_styles.category_axis_labels,
            &text_styles.value_axis_labels,
        )
    };
    let cartesian_axis = if cartesian {
        let Some(axis) = chart_nice_value_axis(
            &series,
            matches!(chart.kind, ChartKind::Bar | ChartKind::Area)
                || (metadata.is_some() && chart.kind == ChartKind::Line),
        ) else {
            *chart_points = initial_points;
            return Ok(false);
        };
        Some(axis)
    } else {
        None
    };
    let x_value_axis = if matches!(chart.kind, ChartKind::Scatter | ChartKind::Bubble) {
        let Some(axis) = chart_nice_x_axis(&series) else {
            *chart_points = initial_points;
            return Ok(false);
        };
        Some(axis)
    } else {
        None
    };
    let x_data_bounds = if matches!(chart.kind, ChartKind::Scatter | ChartKind::Bubble) {
        let Some(bounds) = chart_x_data_bounds(&series) else {
            *chart_points = initial_points;
            return Ok(false);
        };
        Some(bounds)
    } else {
        None
    };
    let radar_axis = if chart.kind == ChartKind::Radar {
        let Some(axis) = chart_nice_value_axis(&series, true) else {
            *chart_points = initial_points;
            return Ok(false);
        };
        Some(axis)
    } else {
        None
    };
    let legend_entries = if chart.legend {
        if matches!(chart.kind, ChartKind::Pie | ChartKind::Doughnut) {
            series[0]
                .labels
                .iter()
                .cloned()
                .enumerate()
                .collect::<Vec<_>>()
        } else {
            series
                .iter()
                .map(|series| series.name.clone())
                .enumerate()
                .collect::<Vec<_>>()
        }
    } else {
        Vec::new()
    };
    let has_chart_text = chart_title.is_some()
        || x_axis_title.is_some()
        || y_axis_title.is_some()
        || legend_entries.iter().any(|(_, text)| !text.is_empty())
        || chart.data_labels
        || ((cartesian || chart.kind == ChartKind::Radar)
            && (category_axis_visible || value_axis_visible));
    if options.font_pack.is_none() && has_chart_text {
        // Fontless chart text is still deterministic because measurement and
        // backend emission share the same Helvetica byte/advance mapping. It
        // cannot, however, reproduce source kerning or selected-font metrics,
        // so retain explicit provenance instead of replacing the whole chart.
        warnings.add(WarningCode::ApproximateTextMetrics, Some(warning_cell));
    }

    let value_axis_text = if value_axis_visible {
        cartesian_axis.as_ref().or(radar_axis.as_ref()).map(|axis| {
            axis.ticks
                .iter()
                .map(|value| chart_axis_number(*value, axis.major))
                .collect::<Vec<_>>()
        })
    } else {
        None
    }
    .unwrap_or_default();
    let value_axis_metrics = max_chart_text_metrics_with_style(
        value_axis_text.iter().map(String::as_str),
        &text_styles.value_axis_labels,
        options,
        typography_stats,
        warnings,
        warning_cell,
    )?;
    let category_axis_metrics = if !category_axis_visible {
        ChartTextMetrics::default()
    } else if matches!(chart.kind, ChartKind::Scatter | ChartKind::Bubble) {
        let x_axis = x_value_axis
            .as_ref()
            .expect("scatter and bubble charts have a retained x-value axis");
        let x_data_bounds = x_data_bounds.expect("scatter and bubble charts have x-data bounds");
        let labels = x_axis
            .ticks
            .iter()
            .filter(|value| **value >= x_data_bounds.0 && **value <= x_data_bounds.1)
            .map(|value| chart_axis_number(*value, x_axis.major))
            .collect::<Vec<_>>();
        max_chart_text_metrics_with_style(
            labels.iter().map(String::as_str),
            &text_styles.category_axis_labels,
            options,
            typography_stats,
            warnings,
            warning_cell,
        )?
    } else if cartesian || chart.kind == ChartKind::Radar {
        let categories = series
            .first()
            .map(|series| series.labels.as_slice())
            .unwrap_or_default();
        let stride = chart_category_label_stride(categories.len());
        max_chart_text_metrics_with_style(
            categories
                .iter()
                .enumerate()
                .filter_map(|(index, category)| {
                    chart_category_label_is_retained(index, categories.len(), stride)
                        .then_some(category.as_str())
                }),
            &text_styles.category_axis_labels,
            options,
            typography_stats,
            warnings,
            warning_cell,
        )?
    } else {
        ChartTextMetrics::default()
    };
    let (horizontal_axis_metrics, vertical_axis_metrics) = if horizontal_bar {
        (value_axis_metrics, category_axis_metrics)
    } else {
        (category_axis_metrics, value_axis_metrics)
    };
    let title_metrics = chart_title
        .map(|title| {
            measure_chart_text_with_style(
                title,
                &text_styles.chart_title,
                options,
                Some((typography_stats, warnings, warning_cell)),
            )
        })
        .transpose()?;
    let x_axis_title_metrics = x_axis_title
        .map(|title| {
            measure_chart_text_with_style(
                title,
                x_axis_title_style,
                options,
                Some((typography_stats, warnings, warning_cell)),
            )
        })
        .transpose()?;
    let y_axis_title_metrics = y_axis_title
        .map(|title| {
            measure_chart_text_with_style(
                title,
                y_axis_title_style,
                options,
                Some((typography_stats, warnings, warning_cell)),
            )
        })
        .transpose()?;
    let legend_metrics = max_chart_text_metrics_with_style(
        legend_entries.iter().map(|(_, name)| name.as_str()),
        &text_styles.legend,
        options,
        typography_stats,
        warnings,
        warning_cell,
    )?;
    let horizontal_axis_extents =
        horizontal_axis_metrics.rotated(horizontal_axis_style.rotation_degrees(0))?;
    let vertical_axis_extents =
        vertical_axis_metrics.rotated(vertical_axis_style.rotation_degrees(0))?;
    let title_extents = title_metrics
        .map(|metrics| metrics.rotated(text_styles.chart_title.rotation_degrees(0)))
        .transpose()?;
    let x_axis_title_extents = x_axis_title_metrics
        .map(|metrics| metrics.rotated(x_axis_title_style.rotation_degrees(0)))
        .transpose()?;
    let y_axis_title_extents = y_axis_title_metrics
        .map(|metrics| metrics.rotated(y_axis_title_style.rotation_degrees(-90)))
        .transpose()?;
    let legend_extents = legend_metrics.rotated(text_styles.legend.rotation_degrees(0))?;

    let frame_padding = Fixed::from_pixels(8);
    let text_gap = Fixed::from_pixels(4);
    let horizontal_overhang = Fixed::from_raw(horizontal_axis_extents.width.raw() / 2);
    let vertical_axis_space = if vertical_axis_extents.width > Fixed::ZERO {
        vertical_axis_extents
            .width
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)?
    } else {
        Fixed::ZERO
    };
    let left_axis_space = vertical_axis_space.max(horizontal_overhang);
    let y_axis_title_space = y_axis_title_extents.map_or(Ok(Fixed::ZERO), |extents| {
        extents
            .width
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)
    })?;
    let left_gutter = sum_fixed([frame_padding, y_axis_title_space, left_axis_space])?;
    let vertical_axis_overhang = Fixed::from_raw(
        vertical_axis_extents
            .height
            .raw()
            .max(if chart.kind == ChartKind::Radar {
                horizontal_axis_extents.height.raw()
            } else {
                0
            })
            / 2,
    );
    let title_band = match title_extents {
        Some(extents) => extents
            .height
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)?,
        None => frame_padding,
    };
    // A shifted category axis keeps its first and last bands inside the
    // diagram, so Calc does not reserve the endpoint half-label overhang in
    // the vertical extent either.  Keep the title band, but avoid shrinking
    // the plot by the value-label half-height used by endpoint axes.
    let top_axis_overhang = if category_axis_shifted && !horizontal_bar {
        Fixed::ZERO
    } else {
        vertical_axis_overhang
    };
    let top_gutter = title_band
        .checked_add(top_axis_overhang)
        .ok_or(RenderError::CoordinateOverflow)?;
    let horizontal_axis_space = if horizontal_axis_extents.height > Fixed::ZERO {
        horizontal_axis_extents
            .height
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)?
    } else {
        Fixed::ZERO
    };
    let x_axis_title_space = x_axis_title_extents.map_or(Ok(Fixed::ZERO), |extents| {
        extents
            .height
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)
    })?;
    let bottom_text_space = sum_fixed([horizontal_axis_space, x_axis_title_space])?;
    let bottom_gutter = frame_padding
        .checked_add(bottom_text_space.max(vertical_axis_overhang))
        .ok_or(RenderError::CoordinateOverflow)?;

    let legend_layout = if legend_entries.is_empty() {
        None
    } else {
        let row_height = legend_extents
            .height
            .max(Fixed::from_pixels(10))
            .checked_add(Fixed::from_pixels(2))
            .ok_or(RenderError::CoordinateOverflow)?;
        let available_height = rect
            .height
            .checked_sub(top_gutter)
            .and_then(|value| value.checked_sub(bottom_gutter))
            .ok_or(RenderError::CoordinateOverflow)?;
        let rows_per_column = if available_height > Fixed::ZERO {
            usize::try_from(available_height.raw() / row_height.raw())
                .map_err(|_| RenderError::CoordinateOverflow)?
                .min(legend_entries.len())
        } else {
            0
        };
        if rows_per_column == 0 {
            *typography_stats = typography_before_chart_text;
            *warnings = warnings_before_chart_text;
            *chart_points = initial_points;
            return Ok(false);
        }
        let columns = legend_entries.len().div_ceil(rows_per_column);
        let column_width = sum_fixed([
            Fixed::from_pixels(10),
            Fixed::from_pixels(2),
            legend_extents.width,
        ])?;
        let column_step = column_width
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)?;
        let total_width = multiply_fixed(
            column_width,
            i64::try_from(columns).map_err(|_| RenderError::CoordinateOverflow)?,
        )?
        .checked_add(multiply_fixed(
            text_gap,
            i64::try_from(columns.saturating_sub(1))
                .map_err(|_| RenderError::CoordinateOverflow)?,
        )?)
        .ok_or(RenderError::CoordinateOverflow)?;
        let plot_offset = horizontal_overhang
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)?;
        Some(ChartLegendLayout {
            row_height,
            rows_per_column,
            column_step,
            plot_offset,
            total_width,
        })
    };
    let right_gutter = match legend_layout {
        Some(layout) => sum_fixed([frame_padding, layout.plot_offset, layout.total_width])?,
        None => {
            // Calc's `crossBetween=between` category axis places every
            // category inside the plot bands.  The final category therefore
            // does not need the endpoint overhang reserved by a legacy
            // endpoint axis; retaining it shrinks the imported plot and
            // moves the last marker left of Calc's position.  Authored and
            // endpoint charts keep the historical overhang.
            let endpoint_overhang = if category_axis_shifted && !horizontal_bar {
                Fixed::ZERO
            } else {
                horizontal_overhang
            };
            sum_fixed([frame_padding, endpoint_overhang])?
        }
    };

    let left = rect
        .x
        .checked_add(left_gutter)
        .ok_or(RenderError::CoordinateOverflow)?;
    let top = rect
        .y
        .checked_add(top_gutter)
        .ok_or(RenderError::CoordinateOverflow)?;
    let right = rect
        .x
        .checked_add(rect.width)
        .and_then(|value| value.checked_sub(right_gutter))
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .and_then(|value| value.checked_sub(bottom_gutter))
        .ok_or(RenderError::CoordinateOverflow)?;
    if right <= left || bottom <= top {
        *typography_stats = typography_before_chart_text;
        *warnings = warnings_before_chart_text;
        *chart_points = initial_points;
        return Ok(false);
    }
    let plot = Rect {
        x: left,
        y: top,
        width: right
            .checked_sub(left)
            .ok_or(RenderError::CoordinateOverflow)?,
        height: bottom
            .checked_sub(top)
            .ok_or(RenderError::CoordinateOverflow)?,
    };
    let imported_chart_frame =
        metadata.is_some_and(|metadata| metadata.chart_default_latin_font_family.is_some());
    let frame_fill = match metadata.map_or(ChartFrameFill::Automatic, |metadata| {
        metadata.chart_frame_fill
    }) {
        ChartFrameFill::Automatic => {
            // Imported OOXML chart spaces with no c:spPr paint are
            // transparent in Calc. Authored charts retain the historical
            // white compatibility default; an explicit unsupported paint
            // also keeps that fallback so it is not mistaken for noFill.
            let imported_implicit_no_fill = metadata.is_some_and(|metadata| {
                metadata.chart_default_latin_font_family.is_some()
                    && metadata.chart_frame_style_losses.is_empty()
            });
            (!imported_implicit_no_fill).then_some(Rgb::WHITE)
        }
        ChartFrameFill::NoFill => None,
        ChartFrameFill::Solid(color) => {
            let [red, green, blue] = color.as_rgb();
            Some(Rgb::new(red, green, blue))
        }
        _ => Some(Rgb::WHITE),
    };
    // Calc's imported OOXML chart-space frame uses the light gray DrawingML
    // default outline even when its fill is omitted or explicitly `noFill`.
    // Authored charts retain the historical neutral outline for compatibility.
    let frame_stroke = imported_chart_frame.then_some(Rgb::new(217, 217, 217));
    push_chart_frame(nodes, rect, frame_fill, frame_stroke, options)?;
    let category_data_plot = chart_category_data_plot(plot, category_axis_shifted, horizontal_bar)?;
    let mut labels = Vec::<ChartLabel>::new();
    let mut category_labels = Vec::<ChartLabel>::new();
    let mut value_labels = Vec::<ChartLabel>::new();
    let category_major_gridlines = metadata
        .and_then(|metadata| metadata.chart_category_major_gridlines)
        .unwrap_or(false)
        && category_axis_visible;
    let value_major_gridlines = metadata
        .and_then(|metadata| metadata.chart_value_major_gridlines)
        .unwrap_or(true)
        && value_axis_visible;
    if let Some(axis) = cartesian_axis.as_ref() {
        push_cartesian_chart_axes(
            nodes,
            rect,
            plot,
            chart.kind,
            horizontal_bar,
            axis,
            x_value_axis.as_ref(),
            x_data_bounds,
            &series,
            category_axis_visible,
            category_axis_shifted,
            value_axis_visible,
            category_major_gridlines,
            value_major_gridlines,
            &text_styles.category_axis_labels,
            &text_styles.value_axis_labels,
            text_bytes,
            glyphs,
            typography_stats,
            options,
            warnings,
            warning_cell,
        )?;
    }
    match chart.kind {
        ChartKind::Pie => {
            push_pie_chart(
                nodes,
                plot,
                &series[0],
                palette,
                chart.data_labels,
                &mut labels,
                typography_stats,
                options,
            )?;
        }
        ChartKind::Doughnut => {
            push_doughnut_chart(
                nodes,
                plot,
                &series,
                palette,
                chart.data_labels,
                &mut labels,
                typography_stats,
                options,
            )?;
        }
        ChartKind::Radar => {
            let axis = radar_axis
                .as_ref()
                .expect("radar charts have a retained value axis");
            push_radar_chart(
                nodes,
                plot,
                &series,
                axis,
                palette,
                category_axis_visible,
                value_axis_visible,
                category_major_gridlines,
                value_major_gridlines,
                &mut category_labels,
                &mut value_labels,
                chart.data_labels,
                &mut labels,
                typography_stats,
                options,
                warnings,
                warning_cell,
            )?;
        }
        _ => {
            let axis = cartesian_axis
                .as_ref()
                .expect("all cartesian chart kinds have a value axis");
            let bounds = (axis.minimum, axis.maximum);
            match chart.kind {
                ChartKind::Line => push_line_chart(
                    nodes,
                    category_data_plot,
                    &series,
                    bounds,
                    category_axis_shifted,
                    palette,
                    chart.data_labels,
                    &mut labels,
                    typography_stats,
                    options,
                )?,
                ChartKind::Scatter => push_scatter_chart(
                    nodes,
                    plot,
                    &series,
                    bounds,
                    x_value_axis
                        .as_ref()
                        .expect("scatter charts have a retained x-value axis"),
                    palette,
                    chart.data_labels,
                    &mut labels,
                    typography_stats,
                    options,
                )?,
                ChartKind::Bar => {
                    if horizontal_bar {
                        push_horizontal_bar_chart(
                            nodes,
                            plot,
                            &series,
                            bounds,
                            palette,
                            chart.data_labels,
                            &mut labels,
                            options,
                        )?;
                    } else {
                        push_column_chart(
                            nodes,
                            category_data_plot,
                            &series,
                            bounds,
                            palette,
                            chart.data_labels,
                            &mut labels,
                            options,
                        )?;
                    }
                }
                ChartKind::Area => push_area_chart(
                    nodes,
                    category_data_plot,
                    &series,
                    bounds,
                    category_axis_shifted,
                    palette,
                    chart.data_labels,
                    &mut labels,
                    typography_stats,
                    options,
                )?,
                ChartKind::Bubble => push_bubble_chart(
                    nodes,
                    plot,
                    &series,
                    bounds,
                    x_value_axis
                        .as_ref()
                        .expect("bubble charts have a retained x-value axis"),
                    palette,
                    chart.data_labels,
                    &mut labels,
                    typography_stats,
                    options,
                )?,
                ChartKind::Pie | ChartKind::Doughnut | ChartKind::Radar => {
                    unreachable!("non-cartesian chart handled above")
                }
            }
        }
    }

    if let Some(title) = chart_title {
        let extents = title_extents.expect("chart title extents were measured");
        let paint_bounds = Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: extents.height,
        };
        push_chart_text_with_style(
            nodes,
            title.to_string(),
            paint_bounds,
            rect,
            TextAnchor::Middle,
            0,
            &text_styles.chart_title,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    if let Some(title) = x_axis_title {
        let extents = x_axis_title_extents.expect("x-axis title extents were measured");
        let paint_bounds = Rect {
            x: left,
            y: bottom
                .checked_add(horizontal_axis_space)
                .and_then(|value| value.checked_add(text_gap))
                .ok_or(RenderError::CoordinateOverflow)?,
            width: plot.width,
            height: extents.height,
        };
        push_chart_text_with_style(
            nodes,
            title.to_string(),
            paint_bounds,
            rect,
            TextAnchor::Middle,
            0,
            x_axis_title_style,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    if let Some(title) = y_axis_title {
        let extents = y_axis_title_extents.expect("y-axis title extents were measured");
        let paint_bounds = Rect {
            x: rect
                .x
                .checked_add(frame_padding)
                .ok_or(RenderError::CoordinateOverflow)?,
            y: top,
            width: extents.width,
            height: plot.height,
        };
        push_chart_text_with_style(
            nodes,
            title.to_string(),
            paint_bounds,
            rect,
            TextAnchor::Middle,
            -90,
            y_axis_title_style,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    if let Some(layout) = legend_layout {
        for (entry_index, (index, name)) in legend_entries.into_iter().enumerate() {
            let column = entry_index / layout.rows_per_column;
            let row = entry_index % layout.rows_per_column;
            let column_offset = multiply_fixed(
                layout.column_step,
                i64::try_from(column).map_err(|_| RenderError::CoordinateOverflow)?,
            )?;
            let legend_x = right
                .checked_add(layout.plot_offset)
                .and_then(|value| value.checked_add(column_offset))
                .ok_or(RenderError::CoordinateOverflow)?;
            let row_offset = multiply_fixed(
                layout.row_height,
                i64::try_from(row).map_err(|_| RenderError::CoordinateOverflow)?,
            )?;
            let y = top
                .checked_add(row_offset)
                .ok_or(RenderError::CoordinateOverflow)?;
            let metrics = measure_chart_text_with_style(&name, &text_styles.legend, options, None)?;
            let entry_extents = metrics.rotated(text_styles.legend.rotation_degrees(0))?;
            let swatch_offset = Fixed::from_raw(
                layout
                    .row_height
                    .raw()
                    .checked_sub(Fixed::from_pixels(10).raw())
                    .ok_or(RenderError::CoordinateOverflow)?
                    / 2,
            );
            push_solid_rect(
                nodes,
                legend_x,
                y.checked_add(swatch_offset)
                    .ok_or(RenderError::CoordinateOverflow)?,
                legend_x
                    .checked_add(Fixed::from_pixels(10))
                    .ok_or(RenderError::CoordinateOverflow)?,
                y.checked_add(swatch_offset)
                    .and_then(|value| value.checked_add(Fixed::from_pixels(10)))
                    .ok_or(RenderError::CoordinateOverflow)?,
                chart_color(index, palette),
                options,
            )?;
            let paint_bounds = Rect {
                x: legend_x
                    .checked_add(Fixed::from_pixels(12))
                    .ok_or(RenderError::CoordinateOverflow)?,
                y,
                width: entry_extents.width,
                height: layout.row_height,
            };
            let (bounds, anchor) = if text_styles.legend.rotation_degrees(0).rem_euclid(360) == 0 {
                (
                    Rect {
                        x: paint_bounds.x,
                        y,
                        width: metrics.width,
                        height: metrics.height,
                    },
                    TextAnchor::Start,
                )
            } else {
                (
                    centered_chart_text_bounds(paint_bounds, metrics)?,
                    TextAnchor::Middle,
                )
            };
            push_chart_text_with_style(
                nodes,
                name,
                bounds,
                rect,
                anchor,
                0,
                &text_styles.legend,
                text_bytes,
                glyphs,
                typography_stats,
                options,
            )?;
        }
    }
    for label in value_labels {
        let metrics = measure_chart_text_with_style(
            &label.text,
            &text_styles.value_axis_labels,
            options,
            None,
        )?;
        let extents = metrics.rotated(text_styles.value_axis_labels.rotation_degrees(0))?;
        let paint_bounds = Rect {
            x: label
                .x
                .checked_sub(Fixed::from_raw(extents.width.raw() / 2))
                .ok_or(RenderError::CoordinateOverflow)?,
            y: label
                .y
                .checked_sub(Fixed::from_raw(extents.height.raw() / 2))
                .ok_or(RenderError::CoordinateOverflow)?,
            width: extents.width,
            height: extents.height,
        };
        push_chart_text_with_style(
            nodes,
            label.text,
            centered_chart_text_bounds(paint_bounds, metrics)?,
            rect,
            TextAnchor::Middle,
            0,
            &text_styles.value_axis_labels,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    for label in category_labels {
        let metrics = measure_chart_text_with_style(
            &label.text,
            &text_styles.category_axis_labels,
            options,
            None,
        )?;
        let extents = metrics.rotated(text_styles.category_axis_labels.rotation_degrees(0))?;
        let paint_bounds = Rect {
            x: label
                .x
                .checked_sub(Fixed::from_raw(extents.width.raw() / 2))
                .ok_or(RenderError::CoordinateOverflow)?,
            y: label
                .y
                .checked_sub(Fixed::from_raw(extents.height.raw() / 2))
                .ok_or(RenderError::CoordinateOverflow)?,
            width: extents.width,
            height: extents.height,
        };
        push_chart_text_with_style(
            nodes,
            label.text,
            centered_chart_text_bounds(paint_bounds, metrics)?,
            rect,
            TextAnchor::Middle,
            0,
            &text_styles.category_axis_labels,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    for label in labels {
        let metrics = measure_chart_text_with_style(
            &label.text,
            &text_styles.data_labels,
            options,
            Some((typography_stats, warnings, warning_cell)),
        )?;
        let extents = metrics.rotated(text_styles.data_labels.rotation_degrees(0))?;
        let paint_bounds = Rect {
            x: label
                .x
                .checked_sub(Fixed::from_raw(extents.width.raw() / 2))
                .ok_or(RenderError::CoordinateOverflow)?,
            y: label
                .y
                .checked_sub(Fixed::from_raw(extents.height.raw() / 2))
                .ok_or(RenderError::CoordinateOverflow)?,
            width: extents.width,
            height: extents.height,
        };
        push_chart_text_with_style(
            nodes,
            label.text,
            centered_chart_text_bounds(paint_bounds, metrics)?,
            rect,
            TextAnchor::Middle,
            0,
            &text_styles.data_labels,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    Ok(true)
}

struct ResolvedChartSeries {
    name: String,
    values: Vec<f64>,
    x_values: Option<Vec<f64>>,
    labels: Vec<String>,
    bubble_sizes: Option<Vec<f64>>,
    style: ChartSeriesStyle,
}

struct ChartLabel {
    text: String,
    x: Fixed,
    y: Fixed,
}

#[cfg(test)]
const CALC_MISSING_THEME_CHART_LATIN_FAMILY: &str = "Liberation Sans";
const CHART_BODY_TEXT_POINTS: f32 = 10.0;
const CHART_TITLE_TEXT_POINTS: f32 = 18.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChartTextRole {
    ChartTitle,
    AxisTitle,
    AxisLabel,
    Legend,
    DataLabel,
}

impl ChartTextRole {
    fn points(self) -> f32 {
        match self {
            Self::ChartTitle => CHART_TITLE_TEXT_POINTS,
            Self::AxisTitle | Self::AxisLabel | Self::Legend | Self::DataLabel => {
                CHART_BODY_TEXT_POINTS
            }
        }
    }

    fn size(self) -> Fixed {
        points_to_fixed(self.points()).expect("static chart point size is valid")
    }

    fn bold(self) -> bool {
        matches!(self, Self::ChartTitle | Self::AxisTitle)
    }

    #[cfg(test)]
    fn style(self, family: &str, anchor: TextAnchor, rotation_degrees: i16) -> TextStyle {
        ResolvedChartTextStyle::for_role(self, family).text_style(anchor, rotation_degrees)
    }

    #[cfg(test)]
    fn resolved_style(self, family: &str) -> ResolvedRunStyle {
        ResolvedChartTextStyle::for_role(self, family).resolved_run_style()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedChartTextStyle {
    family: String,
    size: Fixed,
    size_hundredths_of_point: u32,
    color: Rgb,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    kerning_minimum_hundredths_of_point: Option<u32>,
    rotation_degrees: Option<i16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedChartTextStyles {
    chart_title: ResolvedChartTextStyle,
    category_axis_title: ResolvedChartTextStyle,
    value_axis_title: ResolvedChartTextStyle,
    legend: ResolvedChartTextStyle,
    category_axis_labels: ResolvedChartTextStyle,
    value_axis_labels: ResolvedChartTextStyle,
    data_labels: ResolvedChartTextStyle,
}

impl ResolvedChartTextStyles {
    fn resolve(metadata: Option<&DrawingMetadata>, fallback_family: &str) -> Option<Self> {
        let imported = metadata.map(|metadata| &metadata.chart_text_styles);
        let resolve = |style: Option<&ChartTextStyle>, role| match style {
            Some(style) => ResolvedChartTextStyle::imported(style),
            None => Some(ResolvedChartTextStyle::for_role(role, fallback_family)),
        };
        Some(Self {
            chart_title: resolve(
                imported.and_then(|styles| styles.chart_title.as_ref()),
                ChartTextRole::ChartTitle,
            )?,
            category_axis_title: resolve(
                imported.and_then(|styles| styles.category_axis_title.as_ref()),
                ChartTextRole::AxisTitle,
            )?,
            value_axis_title: resolve(
                imported.and_then(|styles| styles.value_axis_title.as_ref()),
                ChartTextRole::AxisTitle,
            )?,
            legend: resolve(
                imported.and_then(|styles| styles.legend.as_ref()),
                ChartTextRole::Legend,
            )?,
            category_axis_labels: resolve(
                imported.and_then(|styles| styles.category_axis_labels.as_ref()),
                ChartTextRole::AxisLabel,
            )?,
            value_axis_labels: resolve(
                imported.and_then(|styles| styles.value_axis_labels.as_ref()),
                ChartTextRole::AxisLabel,
            )?,
            data_labels: resolve(
                imported.and_then(|styles| styles.data_labels.as_ref()),
                ChartTextRole::DataLabel,
            )?,
        })
    }

    fn physical_axis_titles(
        &self,
        horizontal_bar: bool,
    ) -> (&ResolvedChartTextStyle, &ResolvedChartTextStyle) {
        if horizontal_bar {
            (&self.value_axis_title, &self.category_axis_title)
        } else {
            (&self.category_axis_title, &self.value_axis_title)
        }
    }
}

impl ResolvedChartTextStyle {
    fn for_role(role: ChartTextRole, family: &str) -> Self {
        let size_hundredths_of_point = match role {
            ChartTextRole::ChartTitle => 1_800,
            ChartTextRole::AxisTitle
            | ChartTextRole::AxisLabel
            | ChartTextRole::Legend
            | ChartTextRole::DataLabel => 1_000,
        };
        Self {
            family: family.to_string(),
            size: role.size(),
            size_hundredths_of_point,
            color: Rgb::BLACK,
            bold: role.bold(),
            italic: false,
            underline: false,
            strikethrough: false,
            kerning_minimum_hundredths_of_point: None,
            rotation_degrees: None,
        }
    }

    fn imported(style: &ChartTextStyle) -> Option<Self> {
        if style.latin_font_family.trim().is_empty()
            || style.latin_font_family.len() > 255
            || !(100..=400_000).contains(&style.size_hundredths_of_point)
            || style
                .kerning_minimum_hundredths_of_point
                .is_some_and(|value| value > 400_000)
        {
            return None;
        }
        Some(Self {
            family: style.latin_font_family.clone(),
            size: chart_text_size_to_fixed(style.size_hundredths_of_point)?,
            size_hundredths_of_point: style.size_hundredths_of_point,
            color: rgb(style.color),
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            strikethrough: style.strikethrough,
            kerning_minimum_hundredths_of_point: style.kerning_minimum_hundredths_of_point,
            rotation_degrees: style.rotation_degrees,
        })
    }

    fn text_style(&self, anchor: TextAnchor, fallback_rotation_degrees: i16) -> TextStyle {
        TextStyle {
            family: self.family.clone(),
            size: self.size,
            color: self.color,
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            strikethrough: self.strikethrough,
            anchor,
            baseline: TextBaseline::Middle,
            rotation_degrees: self.rotation_degrees(fallback_rotation_degrees),
        }
    }

    fn resolved_run_style(&self) -> ResolvedRunStyle {
        ResolvedRunStyle {
            family: self.family.clone(),
            size: self.size,
            color: self.color,
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            strikethrough: self.strikethrough,
            script: FormatScript::None,
        }
    }

    fn kerning(&self) -> bool {
        self.kerning_minimum_hundredths_of_point
            .is_none_or(|minimum| self.size_hundredths_of_point >= minimum)
    }

    fn rotation_degrees(&self, fallback_rotation_degrees: i16) -> i16 {
        normalize_chart_text_rotation(self.rotation_degrees.unwrap_or(fallback_rotation_degrees))
    }
}

fn normalize_chart_text_rotation(rotation_degrees: i16) -> i16 {
    let normalized = rotation_degrees.rem_euclid(360);
    if normalized > 180 {
        normalized - 360
    } else {
        normalized
    }
}

// Q62 sine coefficients for each whole degree in the closed first quadrant.
// Non-cardinal values are rounded toward positive infinity. Combining those
// coefficients with an outward integer division therefore cannot shrink the
// exact axis-aligned bounds of rotated chart text, while avoiding libm and its
// platform-dependent boundary behavior entirely.
const CHART_TEXT_TRIG_SCALE: u128 = 1_u128 << 62;
const CHART_TEXT_SINE_Q62_CEIL: [u64; 91] = [
    0,
    80_485_018_754_732_518,
    160_945_520_993_076_460,
    241_356_997_666_611_640,
    321_694_954_660_579_833,
    401_934_920_255_029_412,
    482_052_452_579_138_307,
    562_023_147_056_444_632,
    641_822_643_838_717_085,
    721_426_635_226_200_705,
    800_810_873_071_977_682,
    879_951_176_168_187_814,
    958_823_437_611_858_663,
    1_037_403_632_148_101_736,
    1_115_667_823_488_437_854,
    1_193_592_171_602_022_506,
    1_271_152_939_977_550_183,
    1_348_326_502_853_625_666,
    1_425_089_352_415_399_811,
    1_501_418_105_955_277_681,
    1_577_289_512_995_517_789,
    1_652_680_462_370_552_856,
    1_727_567_989_266_874_720,
    1_801_929_282_218_338_998,
    1_875_741_690_054_758_654,
    1_948_982_728_801_669_865,
    2_021_630_088_529_168_447,
    2_093_661_640_147_730_629,
    2_165_055_442_148_948_087,
    2_235_789_747_289_123_961,
    2_305_843_009_213_693_952,
    2_375_193_889_020_454_652,
    2_443_821_261_759_599_862,
    2_511_704_222_868_584_951,
    2_578_822_094_539_859_112,
    2_645_154_432_019_525_853,
    2_710_681_029_835_013_084,
    2_775_381_927_949_855_802,
    2_839_237_417_843_716_553,
    2_902_228_048_515_791_645,
    2_964_334_632_409_774_424,
    3_025_538_251_258_570_794,
    3_085_820_261_846_986_643,
    3_145_162_301_690_631_791,
    3_203_546_294_629_310_615,
    3_260_954_456_333_195_554,
    3_317_369_299_720_106_254,
    3_372_773_640_282_244_205,
    3_427_150_601_320_760_279,
    3_480_483_619_086_560_694,
    3_532_756_447_825_785_445,
    3_583_953_164_728_422_300,
    3_634_058_174_778_548_983,
    3_683_056_215_504_726_095,
    3_730_932_361_629_093_763,
    3_777_672_029_613_755_864,
    3_823_260_982_103_066_937,
    3_867_685_332_260_468_641,
    3_910_931_547_998_554_687,
    3_952_986_456_101_075_756,
    3_993_837_246_235_628_776,
    4_033_471_474_855_808_273,
    4_071_877_068_991_631_147,
    4_109_042_329_927_080_280,
    4_144_955_936_763_646_770,
    4_179_606_949_868_785_275,
    4_212_984_814_208_232_070,
    4_245_079_362_561_170_726,
    4_275_880_818_617_266_068,
    4_305_379_799_954_623_004,
    4_333_567_320_897_763_126,
    4_360_434_795_254_748_507,
    4_385_974_038_932_618_941,
    4_410_177_272_430_345_940,
    4_433_037_123_208_544_108,
    4_454_546_627_935_218_059,
    4_474_699_234_606_860_798,
    4_493_488_804_544_257_457,
    4_510_909_614_262_386_454,
    4_526_956_357_213_848_474,
    4_541_624_145_405_292_213,
    4_554_908_510_886_344_501,
    4_566_805_407_110_591_272,
    4_577_311_210_168_194_800,
    4_586_422_719_889_771_738,
    4_594_137_160_821_195_717,
    4_600_452_183_069_027_559,
    4_605_365_863_016_315_580,
    4_608_876_703_908_547_948,
    4_610_983_636_309_578_612,
    4_611_686_018_427_387_904,
];

fn chart_text_size_to_fixed(size_hundredths_of_point: u32) -> Option<Fixed> {
    if !(100..=400_000).contains(&size_hundredths_of_point) {
        return None;
    }
    let raw = u64::from(size_hundredths_of_point)
        .checked_mul(4)?
        .checked_mul(FIXED_UNITS_PER_PIXEL as u64)?
        .checked_add(150)?
        .checked_div(300)?;
    i64::try_from(raw).ok().map(Fixed::from_raw)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ChartTextMetrics {
    width: Fixed,
    height: Fixed,
}

#[derive(Debug, Clone, Copy)]
struct ChartLegendLayout {
    row_height: Fixed,
    rows_per_column: usize,
    column_step: Fixed,
    plot_offset: Fixed,
    total_width: Fixed,
}

impl ChartTextMetrics {
    fn max(self, other: Self) -> Self {
        Self {
            width: self.width.max(other.width),
            height: self.height.max(other.height),
        }
    }

    fn rotated(self, rotation_degrees: i16) -> Result<Self, RenderError> {
        let normalized = rotation_degrees.rem_euclid(360);
        match normalized {
            0 | 180 => Ok(self),
            90 | 270 => Ok(Self {
                width: self.height,
                height: self.width,
            }),
            degrees => {
                let first_half = degrees.min(360 - degrees);
                let acute = first_half.min(180 - first_half) as usize;
                let sine = CHART_TEXT_SINE_Q62_CEIL[acute];
                let cosine = CHART_TEXT_SINE_Q62_CEIL[90 - acute];
                Ok(Self {
                    width: chart_text_rotated_extent(self.width, cosine, self.height, sine)?,
                    height: chart_text_rotated_extent(self.width, sine, self.height, cosine)?,
                })
            }
        }
    }
}

fn chart_text_rotated_extent(
    primary: Fixed,
    primary_coefficient: u64,
    secondary: Fixed,
    secondary_coefficient: u64,
) -> Result<Fixed, RenderError> {
    let primary = u128::try_from(primary.raw()).map_err(|_| RenderError::CoordinateOverflow)?;
    let secondary = u128::try_from(secondary.raw()).map_err(|_| RenderError::CoordinateOverflow)?;
    let numerator = primary
        .checked_mul(u128::from(primary_coefficient))
        .and_then(|value| {
            secondary
                .checked_mul(u128::from(secondary_coefficient))
                .and_then(|secondary| value.checked_add(secondary))
        })
        .ok_or(RenderError::CoordinateOverflow)?;
    let extent = numerator.div_ceil(CHART_TEXT_TRIG_SCALE);
    Ok(Fixed::from_raw(
        i64::try_from(extent).map_err(|_| RenderError::CoordinateOverflow)?,
    ))
}

fn centered_chart_text_bounds(
    container: Rect,
    metrics: ChartTextMetrics,
) -> Result<Rect, RenderError> {
    let x_offset = container
        .width
        .raw()
        .checked_sub(metrics.width.raw())
        .ok_or(RenderError::CoordinateOverflow)?
        / 2;
    let y_offset = container
        .height
        .raw()
        .checked_sub(metrics.height.raw())
        .ok_or(RenderError::CoordinateOverflow)?
        / 2;
    Ok(Rect {
        x: container
            .x
            .checked_add(Fixed::from_raw(x_offset))
            .ok_or(RenderError::CoordinateOverflow)?,
        y: container
            .y
            .checked_add(Fixed::from_raw(y_offset))
            .ok_or(RenderError::CoordinateOverflow)?,
        width: metrics.width,
        height: metrics.height,
    })
}

#[cfg(test)]
fn measure_chart_text(
    text: &str,
    role: ChartTextRole,
    family: &str,
    options: &RenderOptions,
    accounting: Option<(&mut TypographyStats, &mut Warnings, CellCoordinate)>,
) -> Result<ChartTextMetrics, RenderError> {
    let style = ResolvedChartTextStyle::for_role(role, family);
    measure_chart_text_with_style(text, &style, options, accounting)
}

fn measure_chart_text_with_style(
    text: &str,
    chart_style: &ResolvedChartTextStyle,
    options: &RenderOptions,
    mut accounting: Option<(&mut TypographyStats, &mut Warnings, CellCoordinate)>,
) -> Result<ChartTextMetrics, RenderError> {
    if text.is_empty() {
        return Ok(ChartTextMetrics::default());
    }
    if let Some((stats, _, _)) = accounting.as_mut() {
        let scalar_count = text.chars().count() as u64;
        let work = scalar_count
            .checked_mul(2)
            .and_then(|value| value.checked_add(1))
            .ok_or(RenderError::CoordinateOverflow)?;
        stats.text_work = stats
            .text_work
            .checked_add(work)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(
            LimitKind::TextRuns,
            options.limits.max_text_runs,
            stats.text_work,
        )?;
    }
    let style = chart_style.resolved_run_style();
    let (width, height) = if let Some(pack) = options.font_pack.as_ref() {
        let shaped = shape_text_with_kerning(
            pack,
            text,
            style.request(),
            text_base_direction(text, false),
            chart_style.kerning(),
            options,
        )?;
        if let Some((stats, warnings, warning_cell)) = accounting.as_mut() {
            account_shaping(pack, &shaped, options, stats)?;
            warnings.add_count(
                WarningCode::MissingGlyph,
                shaped.missing_glyphs as u64,
                Some(*warning_cell),
            );
            if !shaped.requested_family_matched {
                warnings.add(WarningCode::FontFamilySubstituted, Some(*warning_cell));
            }
        }
        let width = shaped_width(pack, &shaped, style.size)?;
        let metrics = styled_line_metrics(
            pack,
            &shaped,
            std::slice::from_ref(&style),
            CellLineLayoutPolicy::Native,
            1,
            1,
        )?;
        (
            width,
            line_height_from_metrics(metrics, CellLineLayoutPolicy::Native)?,
        )
    } else {
        // Preserve deterministic, backend-neutral geometry for callers that
        // deliberately render without a verified pack. Hosted release paths
        // use the shaped branch above.
        let advance_units =
            helvetica_text_advance_units(text).ok_or(RenderError::CoordinateOverflow)?;
        let width = style
            .size
            .raw()
            .checked_mul(advance_units)
            .and_then(|value| value.checked_div(1_000))
            .map(Fixed::from_raw)
            .ok_or(RenderError::CoordinateOverflow)?;
        (width, scale_ratio(style.size, 6, 5)?)
    };
    if let Some((stats, _, _)) = accounting.as_mut() {
        stats.text_lines = stats
            .text_lines
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(
            LimitKind::TextLines,
            options.limits.max_text_lines,
            stats.text_lines,
        )?;
    }
    let padding = Fixed::from_pixels(4);
    Ok(ChartTextMetrics {
        width: width
            .checked_add(padding)
            .ok_or(RenderError::CoordinateOverflow)?,
        height: height
            .checked_add(padding)
            .ok_or(RenderError::CoordinateOverflow)?,
    })
}

fn max_chart_text_metrics_with_style<'a>(
    texts: impl IntoIterator<Item = &'a str>,
    style: &ResolvedChartTextStyle,
    options: &RenderOptions,
    typography_stats: &mut TypographyStats,
    warnings: &mut Warnings,
    warning_cell: CellCoordinate,
) -> Result<ChartTextMetrics, RenderError> {
    texts
        .into_iter()
        .try_fold(ChartTextMetrics::default(), |metrics, text| {
            Ok(metrics.max(measure_chart_text_with_style(
                text,
                style,
                options,
                Some((typography_stats, warnings, warning_cell)),
            )?))
        })
}

#[derive(Debug, Clone, PartialEq)]
struct NiceChartAxis {
    minimum: f64,
    maximum: f64,
    major: f64,
    ticks: Vec<f64>,
}

const CHART_AXIS_TARGET_INTERVALS: f64 = 8.0;
const MAX_CHART_AXIS_INTERVALS: usize = 12;
const MAX_CHART_CATEGORY_LABELS: usize = 64;

fn nice_chart_step(value: f64) -> Option<f64> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let exponent = value.log10().floor();
    let magnitude = 10_f64.powf(exponent);
    if !magnitude.is_finite() || magnitude <= 0.0 {
        return None;
    }
    let normalized = value / magnitude;
    let factor = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    let step = factor * magnitude;
    (step.is_finite() && step > 0.0).then_some(step)
}

fn chart_nice_axis(values: impl Iterator<Item = f64>, force_zero: bool) -> Option<NiceChartAxis> {
    let mut raw_minimum = f64::INFINITY;
    let mut raw_maximum = f64::NEG_INFINITY;
    for value in values {
        if !value.is_finite() {
            return None;
        }
        raw_minimum = raw_minimum.min(value);
        raw_maximum = raw_maximum.max(value);
    }
    if !raw_minimum.is_finite() || !raw_maximum.is_finite() {
        return None;
    }
    if raw_maximum <= raw_minimum {
        let padding = raw_maximum.abs().max(1.0) * 0.5;
        if !padding.is_finite() || padding <= 0.0 {
            return None;
        }
        raw_minimum -= padding;
        raw_maximum += padding;
        if !raw_minimum.is_finite() || !raw_maximum.is_finite() || raw_maximum <= raw_minimum {
            return None;
        }
    }
    let include_zero = force_zero
        || (raw_minimum >= 0.0 && raw_minimum <= raw_maximum * 0.5)
        || (raw_maximum <= 0.0 && raw_maximum >= raw_minimum * 0.5);
    let data_minimum = if include_zero {
        raw_minimum.min(0.0)
    } else {
        raw_minimum
    };
    let data_maximum = if include_zero {
        raw_maximum.max(0.0)
    } else {
        raw_maximum
    };
    let span = data_maximum - data_minimum;
    if !span.is_finite() || span <= 0.0 {
        return None;
    }
    let step_input = span / CHART_AXIS_TARGET_INTERVALS;
    let major = nice_chart_step(if step_input > 0.0 { step_input } else { span })?;
    let padding = span * 0.05;
    if !padding.is_finite() {
        return None;
    }
    let padded_minimum = if include_zero && data_minimum == 0.0 {
        0.0
    } else {
        data_minimum - padding
    };
    let padded_maximum = if include_zero && data_maximum == 0.0 {
        0.0
    } else {
        data_maximum + padding
    };
    if !padded_minimum.is_finite() || !padded_maximum.is_finite() {
        return None;
    }
    let mut minimum = (padded_minimum / major).floor() * major;
    let mut maximum = (padded_maximum / major).ceil() * major;
    if !minimum.is_finite() || !maximum.is_finite() {
        return None;
    }
    if include_zero {
        minimum = minimum.min(0.0);
        maximum = maximum.max(0.0);
    }
    if maximum <= minimum {
        maximum = minimum + major;
    }
    let axis_span = maximum - minimum;
    if !axis_span.is_finite() || axis_span <= 0.0 {
        return None;
    }
    let interval_count = (axis_span / major).round();
    if !interval_count.is_finite() || interval_count <= 0.0 {
        return None;
    }
    if interval_count > MAX_CHART_AXIS_INTERVALS as f64 {
        return None;
    }
    let intervals = interval_count as usize;
    maximum = minimum + major * intervals as f64;
    let final_span = maximum - minimum;
    if !maximum.is_finite()
        || !final_span.is_finite()
        || final_span <= 0.0
        || minimum > data_minimum
        || maximum < data_maximum
    {
        return None;
    }
    let ticks = (0..=intervals)
        .map(|index| {
            let value = minimum + major * index as f64;
            if !value.is_finite() {
                None
            } else if value.abs() < major.abs() * 1e-10 {
                Some(0.0)
            } else {
                Some(value)
            }
        })
        .collect::<Option<Vec<_>>>()?;
    if ticks.windows(2).any(|pair| pair[1] <= pair[0]) {
        return None;
    }
    Some(NiceChartAxis {
        minimum,
        maximum,
        major,
        ticks,
    })
}

fn chart_nice_value_axis(
    series: &[ResolvedChartSeries],
    force_zero: bool,
) -> Option<NiceChartAxis> {
    chart_nice_axis(
        series
            .iter()
            .flat_map(|series| series.values.iter().copied()),
        force_zero,
    )
}

fn chart_nice_x_axis(series: &[ResolvedChartSeries]) -> Option<NiceChartAxis> {
    chart_nice_axis(
        series
            .iter()
            .filter_map(|series| series.x_values.as_ref())
            .flatten()
            .copied(),
        false,
    )
}

fn chart_x_data_bounds(series: &[ResolvedChartSeries]) -> Option<(f64, f64)> {
    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for value in series
        .iter()
        .filter_map(|series| series.x_values.as_ref())
        .flatten()
    {
        if !value.is_finite() {
            return None;
        }
        minimum = minimum.min(*value);
        maximum = maximum.max(*value);
    }
    if !minimum.is_finite() || !maximum.is_finite() {
        return None;
    }
    if maximum <= minimum {
        let lower = minimum - 0.5;
        let upper = maximum + 0.5;
        (lower.is_finite() && upper.is_finite() && upper > lower).then_some((lower, upper))
    } else {
        Some((minimum, maximum))
    }
}

fn chart_category_label_is_retained(index: usize, count: usize, stride: usize) -> bool {
    index < count && (index % stride.max(1) == 0 || index + 1 == count)
}

fn chart_category_ratio(index: usize, count: usize, shifted: bool) -> f64 {
    if shifted {
        (index as f64 + 0.5) / count.max(1) as f64
    } else if count == 1 {
        0.5
    } else {
        index as f64 / (count - 1) as f64
    }
}

fn chart_category_label_stride(count: usize) -> usize {
    if count <= MAX_CHART_CATEGORY_LABELS {
        1
    } else {
        (count - 1).div_ceil(MAX_CHART_CATEGORY_LABELS - 1)
    }
}

fn chart_axis_number(value: f64, major: f64) -> String {
    let decimal_places = if major.abs() >= 1.0 {
        0
    } else {
        (-major.abs().log10().floor() as i32 + 1).clamp(0, 12) as usize
    };
    let mut output = format!("{value:.decimal_places$}");
    if output.contains('.') {
        while output.ends_with('0') {
            output.pop();
        }
        if output.ends_with('.') {
            output.pop();
        }
    }
    if output == "-0" {
        "0".to_string()
    } else {
        output
    }
}

fn chart_color(index: usize, palette: &[Color]) -> Rgb {
    const COLORS: [Rgb; 8] = [
        Rgb::new(68, 114, 196),
        Rgb::new(237, 125, 49),
        Rgb::new(165, 165, 165),
        Rgb::new(255, 192, 0),
        Rgb::new(91, 155, 213),
        Rgb::new(112, 173, 71),
        Rgb::new(38, 68, 120),
        Rgb::new(158, 72, 14),
    ];
    if let Some(color) = palette.get(index % palette.len().max(1)) {
        let [red, green, blue] = color.as_rgb();
        Rgb::new(red, green, blue)
    } else {
        COLORS[index % COLORS.len()]
    }
}

fn light_chart_color(color: Rgb) -> Rgb {
    let lighten =
        |channel: u8| (u16::from(channel) + (u16::from(255_u8) - u16::from(channel)) * 3 / 5) as u8;
    Rgb::new(
        lighten(color.red),
        lighten(color.green),
        lighten(color.blue),
    )
}

fn chart_series_line_width(style: &ChartSeriesStyle) -> Fixed {
    let Some(width_emu) = style.line_width_emu else {
        return Fixed::from_pixels(1);
    };
    if width_emu == 0 {
        // LibreOffice imports an explicit zero chart-line width as a solid
        // zero-width drawing-layer stroke, whose width-zero convention is a
        // visible, view-dependent hairline. Normalize that convention at the
        // backend-neutral scene boundary so SVG, PDF, and raster output agree.
        return Fixed::from_pixels(1);
    }
    // 914,400 EMUs/in at 96 CSS px/in = 9,525 EMUs/CSS px.
    let raw = (u64::from(width_emu) * FIXED_UNITS_PER_PIXEL as u64 + 9_525 / 2) / 9_525;
    Fixed::from_raw(raw as i64)
}

fn chart_y(plot: Rect, value: f64, bounds: (f64, f64)) -> Result<Fixed, RenderError> {
    interpolate_fixed(
        plot.y
            .checked_add(plot.height)
            .ok_or(RenderError::CoordinateOverflow)?,
        Fixed::from_raw(-plot.height.raw()),
        (value - bounds.0) / (bounds.1 - bounds.0),
    )
}

fn chart_x(plot: Rect, ratio: f64) -> Result<Fixed, RenderError> {
    interpolate_fixed(plot.x, plot.width, ratio)
}

fn chart_category_data_plot(
    plot: Rect,
    category_axis_shifted: bool,
    horizontal_bar: bool,
) -> Result<Rect, RenderError> {
    if !category_axis_shifted || horizontal_bar {
        return Ok(plot);
    }
    // Calc keeps the category axis and value-axis bounds at the diagram edge,
    // but reserves one frame-padding interval on either side for shifted
    // category bands.  Keep that inset on labels and series while leaving
    // value ticks/gridlines on the full plot rectangle.
    let inset = Fixed::from_pixels(8);
    let double_inset = Fixed::from_raw(
        inset
            .raw()
            .checked_mul(2)
            .ok_or(RenderError::CoordinateOverflow)?,
    );
    let width = plot
        .width
        .checked_sub(double_inset)
        .ok_or(RenderError::CoordinateOverflow)?;
    if width <= Fixed::ZERO {
        return Ok(plot);
    }
    Ok(Rect {
        x: plot
            .x
            .checked_add(inset)
            .ok_or(RenderError::CoordinateOverflow)?,
        y: plot.y,
        width,
        height: plot.height,
    })
}

#[allow(clippy::too_many_arguments)]
fn push_cartesian_chart_axes(
    nodes: &mut Vec<SceneNode>,
    chart_clip_bounds: Rect,
    plot: Rect,
    chart_kind: ChartKind,
    horizontal_bar: bool,
    axis: &NiceChartAxis,
    x_value_axis: Option<&NiceChartAxis>,
    x_data_bounds: Option<(f64, f64)>,
    series: &[ResolvedChartSeries],
    category_axis_visible: bool,
    category_axis_shifted: bool,
    value_axis_visible: bool,
    category_major_gridlines: bool,
    value_major_gridlines: bool,
    category_axis_style: &ResolvedChartTextStyle,
    value_axis_style: &ResolvedChartTextStyle,
    text_bytes: &mut u64,
    glyphs: &mut u64,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
    warnings: &mut Warnings,
    warning_cell: CellCoordinate,
) -> Result<(), RenderError> {
    let category_plot = chart_category_data_plot(plot, category_axis_shifted, horizontal_bar)?;
    let plot_right = plot
        .x
        .checked_add(plot.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let plot_bottom = plot
        .y
        .checked_add(plot.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let grid = Rgb::new(217, 217, 217);
    let text_gap = Fixed::from_pixels(4);
    if value_axis_visible {
        for value in &axis.ticks {
            let label = chart_axis_number(*value, axis.major);
            let metrics = measure_chart_text_with_style(&label, value_axis_style, options, None)?;
            let extents = metrics.rotated(value_axis_style.rotation_degrees(0))?;
            if horizontal_bar {
                let x = chart_x(
                    plot,
                    (*value - axis.minimum) / (axis.maximum - axis.minimum),
                )?;
                if value_major_gridlines {
                    push_placeholder_line(nodes, x, plot.y, x, plot_bottom, grid, options)?;
                }
                let paint_bounds = Rect {
                    x: x.checked_sub(Fixed::from_raw(extents.width.raw() / 2))
                        .ok_or(RenderError::CoordinateOverflow)?,
                    y: plot_bottom
                        .checked_add(text_gap)
                        .ok_or(RenderError::CoordinateOverflow)?,
                    width: extents.width,
                    height: extents.height,
                };
                push_chart_text_with_style(
                    nodes,
                    label,
                    centered_chart_text_bounds(paint_bounds, metrics)?,
                    chart_clip_bounds,
                    TextAnchor::Middle,
                    0,
                    value_axis_style,
                    text_bytes,
                    glyphs,
                    typography_stats,
                    options,
                )?;
            } else {
                let y = chart_y(plot, *value, (axis.minimum, axis.maximum))?;
                if value_major_gridlines {
                    push_placeholder_line(nodes, plot.x, y, plot_right, y, grid, options)?;
                }
                let paint_bounds = Rect {
                    x: plot
                        .x
                        .checked_sub(text_gap)
                        .and_then(|value| value.checked_sub(extents.width))
                        .ok_or(RenderError::CoordinateOverflow)?,
                    y: y.checked_sub(Fixed::from_raw(extents.height.raw() / 2))
                        .ok_or(RenderError::CoordinateOverflow)?,
                    width: extents.width,
                    height: extents.height,
                };
                let (bounds, anchor) = if value_axis_style.rotation_degrees(0).rem_euclid(360) == 0
                {
                    (
                        Rect {
                            x: plot
                                .x
                                .checked_sub(text_gap)
                                .and_then(|value| value.checked_sub(metrics.width))
                                .ok_or(RenderError::CoordinateOverflow)?,
                            y: y.checked_sub(Fixed::from_raw(metrics.height.raw() / 2))
                                .ok_or(RenderError::CoordinateOverflow)?,
                            width: metrics.width,
                            height: metrics.height,
                        },
                        TextAnchor::End,
                    )
                } else {
                    (
                        centered_chart_text_bounds(paint_bounds, metrics)?,
                        TextAnchor::Middle,
                    )
                };
                push_chart_text_with_style(
                    nodes,
                    label,
                    bounds,
                    chart_clip_bounds,
                    anchor,
                    0,
                    value_axis_style,
                    text_bytes,
                    glyphs,
                    typography_stats,
                    options,
                )?;
            }
        }
    }
    if matches!(chart_kind, ChartKind::Scatter | ChartKind::Bubble) && category_axis_visible {
        let x_axis = x_value_axis.ok_or(RenderError::Typography {
            reason: "missing_chart_x_value_axis",
        })?;
        let x_data_bounds = x_data_bounds.ok_or(RenderError::Typography {
            reason: "missing_chart_x_data_bounds",
        })?;
        for value in x_axis
            .ticks
            .iter()
            .filter(|value| **value >= x_data_bounds.0 && **value <= x_data_bounds.1)
        {
            let x = chart_x(
                category_plot,
                (*value - x_axis.minimum) / (x_axis.maximum - x_axis.minimum),
            )?;
            if category_major_gridlines {
                push_placeholder_line(nodes, x, plot.y, x, plot_bottom, grid, options)?;
            }
            let label = chart_axis_number(*value, x_axis.major);
            let metrics =
                measure_chart_text_with_style(&label, category_axis_style, options, None)?;
            let extents = metrics.rotated(category_axis_style.rotation_degrees(0))?;
            let paint_bounds = Rect {
                x: x.checked_sub(Fixed::from_raw(extents.width.raw() / 2))
                    .ok_or(RenderError::CoordinateOverflow)?,
                y: plot_bottom
                    .checked_add(text_gap)
                    .ok_or(RenderError::CoordinateOverflow)?,
                width: extents.width,
                height: extents.height,
            };
            push_chart_text_with_style(
                nodes,
                label,
                centered_chart_text_bounds(paint_bounds, metrics)?,
                chart_clip_bounds,
                TextAnchor::Middle,
                0,
                category_axis_style,
                text_bytes,
                glyphs,
                typography_stats,
                options,
            )?;
        }
    }
    let physical_vertical_axis_visible = if horizontal_bar {
        category_axis_visible
    } else {
        value_axis_visible
    };
    let physical_horizontal_axis_visible = if horizontal_bar {
        value_axis_visible
    } else {
        category_axis_visible
    };
    if physical_vertical_axis_visible {
        push_placeholder_line(
            nodes,
            plot.x,
            plot.y,
            plot.x,
            plot_bottom,
            Rgb::BLACK,
            options,
        )?;
    }
    if physical_horizontal_axis_visible {
        push_placeholder_line(
            nodes,
            plot.x,
            plot_bottom,
            plot_right,
            plot_bottom,
            Rgb::BLACK,
            options,
        )?;
    }

    if matches!(chart_kind, ChartKind::Scatter | ChartKind::Bubble) {
        return Ok(());
    }
    if !category_axis_visible {
        return Ok(());
    }
    let Some(categories) = series.first().map(|series| series.labels.as_slice()) else {
        return Ok(());
    };
    if categories.is_empty() {
        return Ok(());
    }
    let stride = chart_category_label_stride(categories.len());
    let retained = categories
        .iter()
        .enumerate()
        .filter(|(index, _)| chart_category_label_is_retained(*index, categories.len(), stride))
        .count();
    warnings.add_count(
        WarningCode::ChartMetadataSimplified,
        categories.len().saturating_sub(retained) as u64,
        Some(warning_cell),
    );
    for (index, category) in categories.iter().enumerate() {
        if !chart_category_label_is_retained(index, categories.len(), stride) {
            continue;
        }
        let metrics = measure_chart_text_with_style(category, category_axis_style, options, None)?;
        let extents = metrics.rotated(category_axis_style.rotation_degrees(0))?;
        let paint_bounds = if horizontal_bar {
            let ratio = (index as f64 + 0.5) / categories.len() as f64;
            let y = interpolate_fixed(plot.y, plot.height, ratio)?;
            if category_major_gridlines {
                push_placeholder_line(nodes, plot.x, y, plot_right, y, grid, options)?;
            }
            Rect {
                x: plot
                    .x
                    .checked_sub(text_gap)
                    .and_then(|value| value.checked_sub(extents.width))
                    .ok_or(RenderError::CoordinateOverflow)?,
                y: y.checked_sub(Fixed::from_raw(extents.height.raw() / 2))
                    .ok_or(RenderError::CoordinateOverflow)?,
                width: extents.width,
                height: extents.height,
            }
        } else {
            let ratio = chart_category_ratio(
                index,
                categories.len(),
                category_axis_shifted || chart_kind == ChartKind::Bar,
            );
            let x = chart_x(category_plot, ratio)?;
            if category_major_gridlines {
                push_placeholder_line(nodes, x, plot.y, x, plot_bottom, grid, options)?;
            }
            Rect {
                x: x.checked_sub(Fixed::from_raw(extents.width.raw() / 2))
                    .ok_or(RenderError::CoordinateOverflow)?,
                y: plot_bottom
                    .checked_add(text_gap)
                    .ok_or(RenderError::CoordinateOverflow)?,
                width: extents.width,
                height: extents.height,
            }
        };
        let (bounds, anchor) =
            if horizontal_bar && category_axis_style.rotation_degrees(0).rem_euclid(360) == 0 {
                let ratio = (index as f64 + 0.5) / categories.len() as f64;
                let y = interpolate_fixed(plot.y, plot.height, ratio)?;
                (
                    Rect {
                        x: plot
                            .x
                            .checked_sub(text_gap)
                            .and_then(|value| value.checked_sub(metrics.width))
                            .ok_or(RenderError::CoordinateOverflow)?,
                        y: y.checked_sub(Fixed::from_raw(metrics.height.raw() / 2))
                            .ok_or(RenderError::CoordinateOverflow)?,
                        width: metrics.width,
                        height: metrics.height,
                    },
                    TextAnchor::End,
                )
            } else {
                (
                    centered_chart_text_bounds(paint_bounds, metrics)?,
                    TextAnchor::Middle,
                )
            };
        push_chart_text_with_style(
            nodes,
            category.clone(),
            bounds,
            chart_clip_bounds,
            anchor,
            0,
            category_axis_style,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_line_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    bounds: (f64, f64),
    category_axis_shifted: bool,
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    for (series_index, series) in series.iter().enumerate() {
        let palette_color = chart_color(series_index, palette);
        let line_color = series.style.line_color.map_or(palette_color, |color| {
            let [red, green, blue] = color.as_rgb();
            Rgb::new(red, green, blue)
        });
        let mut previous = None;
        for (index, value) in series.values.iter().enumerate() {
            let ratio = chart_category_ratio(index, series.values.len(), category_axis_shifted);
            let x = chart_x(plot, ratio)?;
            let y = chart_y(plot, *value, bounds)?;
            if series.style.line_visible {
                if let Some((previous_x, previous_y)) = previous {
                    push_chart_series_line(
                        nodes,
                        previous_x,
                        previous_y,
                        x,
                        y,
                        line_color,
                        chart_series_line_width(&series.style),
                        options,
                    )?;
                }
            }
            previous = Some((x, y));
            push_chart_marker(
                nodes,
                x,
                y,
                line_color,
                &series.style,
                typography_stats,
                options,
            )?;
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*value),
                    x,
                    y: Fixed::from_raw(y.raw() - Fixed::from_pixels(8).raw()),
                });
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_scatter_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    y_bounds: (f64, f64),
    x_axis: &NiceChartAxis,
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    for (series_index, series) in series.iter().enumerate() {
        let x_values = series.x_values.as_ref().expect("scatter x values");
        let palette_color = chart_color(series_index, palette);
        let color = series.style.line_color.map_or(palette_color, |color| {
            let [red, green, blue] = color.as_rgb();
            Rgb::new(red, green, blue)
        });
        let draw_retained_line = series.style.line_visible
            && (series.style.line_width_emu.is_some() || series.style.line_color.is_some());
        let mut previous = None;
        for (x_value, y_value) in x_values.iter().zip(&series.values) {
            let x = chart_x(
                plot,
                (*x_value - x_axis.minimum) / (x_axis.maximum - x_axis.minimum),
            )?;
            let y = chart_y(plot, *y_value, y_bounds)?;
            if draw_retained_line {
                if let Some((previous_x, previous_y)) = previous {
                    push_chart_series_line(
                        nodes,
                        previous_x,
                        previous_y,
                        x,
                        y,
                        color,
                        chart_series_line_width(&series.style),
                        options,
                    )?;
                }
            }
            previous = Some((x, y));
            push_chart_marker(nodes, x, y, color, &series.style, typography_stats, options)?;
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*y_value),
                    x,
                    y: Fixed::from_raw(y.raw() - Fixed::from_pixels(8).raw()),
                });
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_column_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    bounds: (f64, f64),
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let categories = series
        .iter()
        .map(|series| series.values.len())
        .max()
        .unwrap_or(1);
    let series_count = series.len();
    let baseline = chart_y(plot, 0.0, bounds)?;
    for (series_index, series_item) in series.iter().enumerate() {
        for (index, value) in series_item.values.iter().enumerate() {
            let group_start = index as f64 / categories as f64;
            let group_end = (index + 1) as f64 / categories as f64;
            let group_span = group_end - group_start;
            let left_ratio =
                group_start + group_span * (0.1 + 0.8 * series_index as f64 / series_count as f64);
            let right_ratio = group_start
                + group_span * (0.1 + 0.8 * (series_index + 1) as f64 / series_count as f64);
            let left = chart_x(plot, left_ratio)?;
            let right = chart_x(plot, right_ratio)?;
            let value_y = chart_y(plot, *value, bounds)?;
            push_solid_rect(
                nodes,
                left,
                value_y.min(baseline),
                right,
                value_y.max(baseline),
                chart_color(series_index, palette),
                options,
            )?;
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*value),
                    x: Fixed::from_raw(left.raw() + (right.raw() - left.raw()) / 2),
                    y: Fixed::from_raw(value_y.min(baseline).raw() - Fixed::from_pixels(8).raw()),
                });
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_horizontal_bar_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    bounds: (f64, f64),
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let categories = series
        .iter()
        .map(|series| series.values.len())
        .max()
        .unwrap_or(1);
    let series_count = series.len();
    let value_x = |value| chart_x(plot, (value - bounds.0) / (bounds.1 - bounds.0));
    let baseline = value_x(0.0)?;
    let plot_bottom = plot
        .y
        .checked_add(plot.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    push_placeholder_line(
        nodes,
        baseline,
        plot.y,
        baseline,
        plot_bottom,
        Rgb::BLACK,
        options,
    )?;
    for (series_index, series_item) in series.iter().enumerate() {
        for (index, value) in series_item.values.iter().enumerate() {
            let group_start = index as f64 / categories as f64;
            let group_end = (index + 1) as f64 / categories as f64;
            let group_span = group_end - group_start;
            let top_ratio =
                group_start + group_span * (0.1 + 0.8 * series_index as f64 / series_count as f64);
            let bottom_ratio = group_start
                + group_span * (0.1 + 0.8 * (series_index + 1) as f64 / series_count as f64);
            let top = interpolate_fixed(plot.y, plot.height, top_ratio)?;
            let bottom = interpolate_fixed(plot.y, plot.height, bottom_ratio)?;
            let end = value_x(*value)?;
            push_solid_rect(
                nodes,
                end.min(baseline),
                top,
                end.max(baseline),
                bottom,
                chart_color(series_index, palette),
                options,
            )?;
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*value),
                    x: end,
                    y: Fixed::from_raw(top.raw() + (bottom.raw() - top.raw()) / 2),
                });
            }
        }
    }
    Ok(())
}

fn push_chart_path(
    nodes: &mut Vec<SceneNode>,
    commands: Vec<PathCommand>,
    fill: Option<Rgb>,
    stroke: Option<Rgb>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    push_chart_path_with_width(
        nodes,
        commands,
        fill,
        stroke,
        Fixed::from_pixels(1),
        typography_stats,
        options,
    )
}

fn preflight_chart_path_commands(
    typography_stats: &TypographyStats,
    additional: usize,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let actual = typography_stats
        .path_commands
        .checked_add(u64::try_from(additional).map_err(|_| RenderError::CoordinateOverflow)?)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::PathCommands,
        options.limits.max_path_commands,
        actual,
    )
}

#[allow(clippy::too_many_arguments)]
fn push_chart_path_with_width(
    nodes: &mut Vec<SceneNode>,
    commands: Vec<PathCommand>,
    fill: Option<Rgb>,
    stroke: Option<Rgb>,
    stroke_width: Fixed,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    typography_stats.path_commands = typography_stats
        .path_commands
        .checked_add(commands.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::PathCommands,
        options.limits.max_path_commands,
        typography_stats.path_commands,
    )?;
    push_node(
        nodes,
        SceneNode::Path(PathNode {
            commands,
            fill,
            stroke,
            stroke_width,
        }),
        options,
    )
}

#[allow(clippy::too_many_arguments)]
fn push_area_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    bounds: (f64, f64),
    category_axis_shifted: bool,
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let baseline = chart_y(plot, 0.0, bounds)?;
    // Draw later series first so the first (primary) series remains visible,
    // matching the foreground ordering used by common office renderers.
    for (series_index, series) in series.iter().enumerate().rev() {
        let first_ratio = chart_category_ratio(0, series.values.len(), category_axis_shifted);
        let last_ratio = chart_category_ratio(
            series.values.len().saturating_sub(1),
            series.values.len(),
            category_axis_shifted,
        );
        let first_x = chart_x(plot, first_ratio)?;
        let last_x = chart_x(plot, last_ratio)?;
        let mut commands = Vec::with_capacity(series.values.len().saturating_add(3));
        commands.push(PathCommand::MoveTo {
            x: first_x,
            y: baseline,
        });
        for (index, value) in series.values.iter().enumerate() {
            let ratio = chart_category_ratio(index, series.values.len(), category_axis_shifted);
            let x = chart_x(plot, ratio)?;
            let y = chart_y(plot, *value, bounds)?;
            commands.push(PathCommand::LineTo { x, y });
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*value),
                    x,
                    y: Fixed::from_raw(y.raw() - Fixed::from_pixels(8).raw()),
                });
            }
        }
        commands.push(PathCommand::LineTo {
            x: last_x,
            y: baseline,
        });
        commands.push(PathCommand::Close);
        let color = chart_color(series_index, palette);
        let line_color = series.style.line_color.map_or(color, |color| {
            let [red, green, blue] = color.as_rgb();
            Rgb::new(red, green, blue)
        });
        push_chart_path_with_width(
            nodes,
            commands,
            Some(light_chart_color(color)),
            series.style.line_visible.then_some(line_color),
            chart_series_line_width(&series.style),
            typography_stats,
            options,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_doughnut_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let center_x = Fixed::from_raw(plot.x.raw() + plot.width.raw() / 2);
    let center_y = Fixed::from_raw(plot.y.raw() + plot.height.raw() / 2);
    let outer_radius = fixed_as_pixels(plot.width.min(plot.height)) * 0.42;
    let hole_radius = outer_radius * 0.5;
    let ring_width = (outer_radius - hole_radius) / series.len() as f64;
    for (series_index, series) in series.iter().enumerate() {
        let outer = outer_radius - ring_width * series_index as f64;
        let inner = (outer - ring_width).max(hole_radius);
        let total = series.values.iter().sum::<f64>();
        let mut start = -std::f64::consts::FRAC_PI_2;
        for (index, value) in series.values.iter().enumerate() {
            if *value == 0.0 {
                continue;
            }
            let end = start + std::f64::consts::TAU * (*value / total);
            let segments = ((end - start).abs() / std::f64::consts::TAU * 64.0)
                .ceil()
                .max(1.0) as usize;
            let mut commands = Vec::with_capacity(segments.saturating_mul(2).saturating_add(4));
            commands.push(PathCommand::MoveTo {
                x: pixels_as_fixed(fixed_as_pixels(center_x) + outer * start.cos())?,
                y: pixels_as_fixed(fixed_as_pixels(center_y) + outer * start.sin())?,
            });
            for segment in 1..=segments {
                let angle = start + (end - start) * segment as f64 / segments as f64;
                commands.push(PathCommand::LineTo {
                    x: pixels_as_fixed(fixed_as_pixels(center_x) + outer * angle.cos())?,
                    y: pixels_as_fixed(fixed_as_pixels(center_y) + outer * angle.sin())?,
                });
            }
            for segment in (0..=segments).rev() {
                let angle = start + (end - start) * segment as f64 / segments as f64;
                commands.push(PathCommand::LineTo {
                    x: pixels_as_fixed(fixed_as_pixels(center_x) + inner * angle.cos())?,
                    y: pixels_as_fixed(fixed_as_pixels(center_y) + inner * angle.sin())?,
                });
            }
            commands.push(PathCommand::Close);
            push_chart_path(
                nodes,
                commands,
                Some(chart_color(index, palette)),
                Some(Rgb::WHITE),
                typography_stats,
                options,
            )?;
            if data_labels {
                let angle = (start + end) / 2.0;
                let radius = (inner + outer) / 2.0;
                labels.push(ChartLabel {
                    text: chart_number(*value),
                    x: pixels_as_fixed(fixed_as_pixels(center_x) + radius * angle.cos())?,
                    y: pixels_as_fixed(fixed_as_pixels(center_y) + radius * angle.sin())?,
                });
            }
            start = end;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_radar_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    axis: &NiceChartAxis,
    palette: &[Color],
    category_axis_visible: bool,
    value_axis_visible: bool,
    category_major_gridlines: bool,
    value_major_gridlines: bool,
    category_labels: &mut Vec<ChartLabel>,
    value_labels: &mut Vec<ChartLabel>,
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
    warnings: &mut Warnings,
    warning_cell: CellCoordinate,
) -> Result<(), RenderError> {
    let center_x = Fixed::from_raw(plot.x.raw() + plot.width.raw() / 2);
    let center_y = Fixed::from_raw(plot.y.raw() + plot.height.raw() / 2);
    let radius = fixed_as_pixels(plot.width.min(plot.height)) * 0.42;
    let categories = series
        .iter()
        .map(|series| series.values.len())
        .max()
        .unwrap_or(3);
    let polar = |index: usize, scale: f64| -> Result<(Fixed, Fixed), RenderError> {
        let angle =
            -std::f64::consts::FRAC_PI_2 + std::f64::consts::TAU * index as f64 / categories as f64;
        Ok((
            pixels_as_fixed(fixed_as_pixels(center_x) + radius * scale * angle.cos())?,
            pixels_as_fixed(fixed_as_pixels(center_y) + radius * scale * angle.sin())?,
        ))
    };
    let scale =
        |value: f64| ((value - axis.minimum) / (axis.maximum - axis.minimum)).clamp(0.0, 1.0);
    if value_axis_visible && value_major_gridlines {
        for value in &axis.ticks {
            let ring_scale = scale(*value);
            if ring_scale <= 0.0 {
                continue;
            }
            preflight_chart_path_commands(typography_stats, categories.saturating_add(1), options)?;
            let mut commands = Vec::with_capacity(categories.saturating_add(1));
            for index in 0..categories {
                let (x, y) = polar(index, ring_scale)?;
                commands.push(if index == 0 {
                    PathCommand::MoveTo { x, y }
                } else {
                    PathCommand::LineTo { x, y }
                });
            }
            commands.push(PathCommand::Close);
            push_chart_path(
                nodes,
                commands,
                None,
                Some(Rgb::new(205, 205, 205)),
                typography_stats,
                options,
            )?;
        }
    }
    if category_axis_visible && category_major_gridlines {
        for index in 0..categories {
            let (x, y) = polar(index, 1.0)?;
            push_placeholder_line(
                nodes,
                center_x,
                center_y,
                x,
                y,
                Rgb::new(205, 205, 205),
                options,
            )?;
        }
    }
    if category_axis_visible {
        if let Some(labels) = series.first().map(|series| series.labels.as_slice()) {
            let label_count = labels.len().min(categories);
            let stride = chart_category_label_stride(label_count);
            let retained = (0..label_count)
                .filter(|index| chart_category_label_is_retained(*index, label_count, stride))
                .count();
            warnings.add_count(
                WarningCode::ChartMetadataSimplified,
                label_count.saturating_sub(retained) as u64,
                Some(warning_cell),
            );
            for (index, text) in labels.iter().take(label_count).enumerate() {
                if !chart_category_label_is_retained(index, label_count, stride) {
                    continue;
                }
                let (x, y) = polar(index, 1.12)?;
                category_labels.push(ChartLabel {
                    text: text.clone(),
                    x,
                    y,
                });
            }
        }
    }
    if value_axis_visible {
        for value in &axis.ticks {
            let (x, y) = polar(0, scale(*value))?;
            value_labels.push(ChartLabel {
                text: chart_axis_number(*value, axis.major),
                x,
                y,
            });
        }
    }
    for (series_index, series) in series.iter().enumerate() {
        preflight_chart_path_commands(
            typography_stats,
            series.values.len().saturating_add(1),
            options,
        )?;
        let mut commands = Vec::with_capacity(series.values.len().saturating_add(2));
        for (index, value) in series.values.iter().enumerate() {
            let (x, y) = polar(index, scale(*value))?;
            commands.push(if index == 0 {
                PathCommand::MoveTo { x, y }
            } else {
                PathCommand::LineTo { x, y }
            });
            push_chart_marker(
                nodes,
                x,
                y,
                chart_color(series_index, palette),
                &series.style,
                typography_stats,
                options,
            )?;
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*value),
                    x,
                    y: Fixed::from_raw(y.raw() - Fixed::from_pixels(8).raw()),
                });
            }
        }
        commands.push(PathCommand::Close);
        push_chart_path(
            nodes,
            commands,
            None,
            Some(chart_color(series_index, palette)),
            typography_stats,
            options,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_bubble_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    y_bounds: (f64, f64),
    x_axis: &NiceChartAxis,
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let max_size = series
        .iter()
        .filter_map(|series| series.bubble_sizes.as_ref())
        .flatten()
        .copied()
        .fold(0.0_f64, f64::max);
    let maximum_radius = fixed_as_pixels(plot.width.min(plot.height)) * 0.08;
    for (series_index, series) in series.iter().enumerate() {
        let x_values = series.x_values.as_ref().expect("bubble x values");
        let sizes = series.bubble_sizes.as_ref().expect("bubble sizes");
        for ((x_value, y_value), size) in x_values.iter().zip(&series.values).zip(sizes) {
            let x = chart_x(
                plot,
                (*x_value - x_axis.minimum) / (x_axis.maximum - x_axis.minimum),
            )?;
            let y = chart_y(plot, *y_value, y_bounds)?;
            let radius = (maximum_radius * (*size / max_size).sqrt()).max(2.0);
            let segments = 24_usize;
            let mut commands = Vec::with_capacity(segments + 2);
            for segment in 0..segments {
                let angle = std::f64::consts::TAU * segment as f64 / segments as f64;
                let point_x = pixels_as_fixed(fixed_as_pixels(x) + radius * angle.cos())?;
                let point_y = pixels_as_fixed(fixed_as_pixels(y) + radius * angle.sin())?;
                commands.push(if segment == 0 {
                    PathCommand::MoveTo {
                        x: point_x,
                        y: point_y,
                    }
                } else {
                    PathCommand::LineTo {
                        x: point_x,
                        y: point_y,
                    }
                });
            }
            commands.push(PathCommand::Close);
            let color = chart_color(series_index, palette);
            push_chart_path(
                nodes,
                commands,
                Some(light_chart_color(color)),
                Some(color),
                typography_stats,
                options,
            )?;
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*y_value),
                    x,
                    y: Fixed::from_raw(y.raw() - pixels_as_fixed(radius)?.raw()),
                });
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_pie_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &ResolvedChartSeries,
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let center_x = Fixed::from_raw(plot.x.raw() + plot.width.raw() / 2);
    let center_y = Fixed::from_raw(plot.y.raw() + plot.height.raw() / 2);
    let radius = fixed_as_pixels(plot.width.min(plot.height)) * 0.42;
    let total = series.values.iter().sum::<f64>();
    let mut start = -std::f64::consts::FRAC_PI_2;
    for (index, value) in series.values.iter().enumerate() {
        if *value == 0.0 {
            continue;
        }
        let end = start + std::f64::consts::TAU * (*value / total);
        let segments = ((end - start).abs() / std::f64::consts::TAU * 64.0)
            .ceil()
            .max(1.0) as usize;
        let mut commands = Vec::with_capacity(segments + 3);
        commands.push(PathCommand::MoveTo {
            x: center_x,
            y: center_y,
        });
        for segment in 0..=segments {
            let angle = start + (end - start) * segment as f64 / segments as f64;
            commands.push(PathCommand::LineTo {
                x: pixels_as_fixed(fixed_as_pixels(center_x) + radius * angle.cos())?,
                y: pixels_as_fixed(fixed_as_pixels(center_y) + radius * angle.sin())?,
            });
        }
        commands.push(PathCommand::Close);
        typography_stats.path_commands = typography_stats
            .path_commands
            .checked_add(commands.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(
            LimitKind::PathCommands,
            options.limits.max_path_commands,
            typography_stats.path_commands,
        )?;
        push_node(
            nodes,
            SceneNode::Path(PathNode {
                commands,
                fill: Some(chart_color(index, palette)),
                stroke: Some(Rgb::WHITE),
                stroke_width: Fixed::from_pixels(1),
            }),
            options,
        )?;
        if data_labels {
            let angle = (start + end) / 2.0;
            labels.push(ChartLabel {
                text: chart_number(*value),
                x: pixels_as_fixed(fixed_as_pixels(center_x) + radius * 0.62 * angle.cos())?,
                y: pixels_as_fixed(fixed_as_pixels(center_y) + radius * 0.62 * angle.sin())?,
            });
        }
        start = end;
    }
    Ok(())
}

fn push_chart_marker(
    nodes: &mut Vec<SceneNode>,
    x: Fixed,
    y: Fixed,
    color: Rgb,
    style: &ChartSeriesStyle,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let marker = match style.marker {
        ChartMarkerSymbol::Automatic => ChartMarkerSymbol::Square,
        ChartMarkerSymbol::None => ChartMarkerSymbol::None,
        ChartMarkerSymbol::Circle => ChartMarkerSymbol::Circle,
        ChartMarkerSymbol::Square => ChartMarkerSymbol::Square,
        ChartMarkerSymbol::Diamond => ChartMarkerSymbol::Diamond,
        ChartMarkerSymbol::Triangle => ChartMarkerSymbol::Triangle,
        _ => ChartMarkerSymbol::Square,
    };
    if marker == ChartMarkerSymbol::None {
        return Ok(());
    }
    let diameter = style
        .marker_size
        .map_or(3.0, |points| f64::from(points) * 96.0 / 72.0);
    let radius = diameter / 2.0;
    let center_x = fixed_as_pixels(x);
    let center_y = fixed_as_pixels(y);
    match marker {
        ChartMarkerSymbol::Automatic | ChartMarkerSymbol::None => unreachable!("normalized above"),
        ChartMarkerSymbol::Square => {
            let half = pixels_as_fixed(radius)?;
            push_solid_rect(
                nodes,
                x.checked_sub(half).ok_or(RenderError::CoordinateOverflow)?,
                y.checked_sub(half).ok_or(RenderError::CoordinateOverflow)?,
                x.checked_add(half).ok_or(RenderError::CoordinateOverflow)?,
                y.checked_add(half).ok_or(RenderError::CoordinateOverflow)?,
                color,
                options,
            )
        }
        ChartMarkerSymbol::Circle => {
            let control = radius * 0.552_284_749_830_793_6;
            push_chart_path(
                nodes,
                vec![
                    PathCommand::MoveTo {
                        x: pixels_as_fixed(center_x + radius)?,
                        y,
                    },
                    PathCommand::CubicTo {
                        control1_x: pixels_as_fixed(center_x + radius)?,
                        control1_y: pixels_as_fixed(center_y + control)?,
                        control2_x: pixels_as_fixed(center_x + control)?,
                        control2_y: pixels_as_fixed(center_y + radius)?,
                        x,
                        y: pixels_as_fixed(center_y + radius)?,
                    },
                    PathCommand::CubicTo {
                        control1_x: pixels_as_fixed(center_x - control)?,
                        control1_y: pixels_as_fixed(center_y + radius)?,
                        control2_x: pixels_as_fixed(center_x - radius)?,
                        control2_y: pixels_as_fixed(center_y + control)?,
                        x: pixels_as_fixed(center_x - radius)?,
                        y,
                    },
                    PathCommand::CubicTo {
                        control1_x: pixels_as_fixed(center_x - radius)?,
                        control1_y: pixels_as_fixed(center_y - control)?,
                        control2_x: pixels_as_fixed(center_x - control)?,
                        control2_y: pixels_as_fixed(center_y - radius)?,
                        x,
                        y: pixels_as_fixed(center_y - radius)?,
                    },
                    PathCommand::CubicTo {
                        control1_x: pixels_as_fixed(center_x + control)?,
                        control1_y: pixels_as_fixed(center_y - radius)?,
                        control2_x: pixels_as_fixed(center_x + radius)?,
                        control2_y: pixels_as_fixed(center_y - control)?,
                        x: pixels_as_fixed(center_x + radius)?,
                        y,
                    },
                    PathCommand::Close,
                ],
                Some(color),
                Some(color),
                typography_stats,
                options,
            )
        }
        ChartMarkerSymbol::Diamond => push_chart_path(
            nodes,
            vec![
                PathCommand::MoveTo {
                    x,
                    y: pixels_as_fixed(center_y - radius)?,
                },
                PathCommand::LineTo {
                    x: pixels_as_fixed(center_x + radius)?,
                    y,
                },
                PathCommand::LineTo {
                    x,
                    y: pixels_as_fixed(center_y + radius)?,
                },
                PathCommand::LineTo {
                    x: pixels_as_fixed(center_x - radius)?,
                    y,
                },
                PathCommand::Close,
            ],
            Some(color),
            Some(color),
            typography_stats,
            options,
        ),
        ChartMarkerSymbol::Triangle => push_chart_path(
            nodes,
            vec![
                PathCommand::MoveTo {
                    x,
                    y: pixels_as_fixed(center_y - radius)?,
                },
                PathCommand::LineTo {
                    x: pixels_as_fixed(center_x + radius)?,
                    y: pixels_as_fixed(center_y + radius)?,
                },
                PathCommand::LineTo {
                    x: pixels_as_fixed(center_x - radius)?,
                    y: pixels_as_fixed(center_y + radius)?,
                },
                PathCommand::Close,
            ],
            Some(color),
            Some(color),
            typography_stats,
            options,
        ),
        _ => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn push_chart_text(
    nodes: &mut Vec<SceneNode>,
    text: String,
    bounds: Rect,
    anchor: TextAnchor,
    rotation_degrees: i16,
    role: ChartTextRole,
    family: &str,
    text_bytes: &mut u64,
    glyphs: &mut u64,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let style = ResolvedChartTextStyle::for_role(role, family);
    push_chart_text_with_style(
        nodes,
        text,
        bounds,
        bounds,
        anchor,
        rotation_degrees,
        &style,
        text_bytes,
        glyphs,
        typography_stats,
        options,
    )
}

#[allow(clippy::too_many_arguments)]
fn push_chart_text_with_style(
    nodes: &mut Vec<SceneNode>,
    text: String,
    bounds: Rect,
    clip_bounds: Rect,
    anchor: TextAnchor,
    fallback_rotation_degrees: i16,
    chart_style: &ResolvedChartTextStyle,
    text_bytes: &mut u64,
    glyphs: &mut u64,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    if text.is_empty() {
        return Ok(());
    }
    *text_bytes = text_bytes
        .checked_add(text.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::TextBytes,
        options.limits.max_text_bytes,
        *text_bytes,
    )?;
    *glyphs = glyphs
        .checked_add(text.chars().count() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(LimitKind::Glyphs, options.limits.max_glyphs, *glyphs)?;
    let node = build_auxiliary_text_node_with_clip_and_kerning(
        text,
        bounds,
        clip_bounds,
        Fixed::from_pixels(2),
        chart_style.text_style(anchor, fallback_rotation_degrees),
        chart_style.kerning(),
        options,
    )?;
    if let SceneNode::GlyphRun(run) = &node {
        typography_stats.path_commands = typography_stats
            .path_commands
            .checked_add(run.commands.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(
            LimitKind::PathCommands,
            options.limits.max_path_commands,
            typography_stats.path_commands,
        )?;
    }
    push_node(nodes, node, options)
}

fn chart_number(value: f64) -> String {
    if value == 0.0 {
        "0".to_string()
    } else {
        value.to_string()
    }
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

fn text_style(region: &Region, options: &RenderOptions, sheet_right_to_left: bool) -> TextStyle {
    let style = region.style.as_ref();
    let font = style.and_then(|style| style.font.as_ref());
    let alignment = style.and_then(|style| style.align.as_ref());
    let anchor = match alignment.and_then(|alignment| alignment.horizontal) {
        Some(HAlign::Left) => TextAnchor::Start,
        Some(HAlign::Center) => TextAnchor::Middle,
        Some(HAlign::Right) => TextAnchor::End,
        None if region.numeric_default => TextAnchor::End,
        None if sheet_right_to_left
            && text_base_direction(&region.text, true) == BaseDirection::RightToLeft =>
        {
            TextAnchor::End
        }
        None => TextAnchor::Start,
    };
    let baseline = match alignment.and_then(|alignment| alignment.vertical) {
        Some(VAlign::Top) => TextBaseline::Top,
        Some(VAlign::Middle) => TextBaseline::Middle,
        // ECMA-376 Part 1 section 18.8.1 gives `vertical` a default of
        // `bottom`, which is what Excel and Calc both render for a cell that
        // declares no vertical alignment.
        Some(VAlign::Bottom) | None => TextBaseline::Bottom,
    };
    let size = font
        .and_then(|font| font.size_pt)
        .and_then(|points| points_to_fixed(points as f32))
        .unwrap_or(options.default_font_size);
    TextStyle {
        family: font
            .and_then(|font| font.name.clone())
            .unwrap_or_else(|| options.default_font_family.clone()),
        size,
        color: font
            .and_then(|font| font.color)
            .map(rgb)
            .unwrap_or(Rgb::BLACK),
        bold: font.is_some_and(|font| font.bold),
        italic: font.is_some_and(|font| font.italic),
        underline: font.is_some_and(|font| font.underline),
        strikethrough: font.is_some_and(|font| font.strikethrough),
        anchor,
        baseline,
        rotation_degrees: alignment.map_or(0, |alignment| alignment.rotation),
    }
}

fn text_base_direction(text: &str, sheet_right_to_left: bool) -> BaseDirection {
    for character in text.chars() {
        match bidi_class(character) {
            BidiClass::L => return BaseDirection::LeftToRight,
            BidiClass::R | BidiClass::AL => return BaseDirection::RightToLeft,
            _ => {}
        }
    }
    if sheet_right_to_left {
        BaseDirection::RightToLeft
    } else {
        BaseDirection::Auto
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedRunStyle {
    family: String,
    size: Fixed,
    color: Rgb,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    script: FormatScript,
}

impl ResolvedRunStyle {
    fn request(&self) -> FontRequest<'_> {
        FontRequest {
            family: &self.family,
            weight: if self.bold { 700 } else { 400 },
            italic: self.italic,
        }
    }
}

#[derive(Debug, Clone)]
struct StyledSourceSpan {
    source: Range<usize>,
    style_index: usize,
}

struct PreparedLine {
    source: Range<usize>,
    advance_end: usize,
    shaped: ShapedText,
    width: Fixed,
    metrics: CombinedLineMetrics,
}

struct PreparedText {
    styles: Vec<ResolvedRunStyle>,
    lines: Vec<PreparedLine>,
    line_layout_policy: CellLineLayoutPolicy,
    horizontal_padding: Fixed,
    available_width: Fixed,
    max_width: Fixed,
    missing_glyphs: u64,
    family_substituted: bool,
}

fn measure_automatic_cell_height(
    pack: &FontPack,
    region: &Region,
    sheet_right_to_left: bool,
    options: &RenderOptions,
    stats: &mut TypographyStats,
    calc_metric_source: Option<CalcAutomaticMetricSource>,
    calc_pattern_points: Option<u16>,
) -> Result<Fixed, RenderError> {
    let style = text_style(region, options, sheet_right_to_left);
    let prepared = prepare_styled_text(
        pack,
        region,
        &style,
        sheet_right_to_left,
        true,
        options,
        stats,
    )?;
    if let Some(source) = calc_metric_source {
        if let Some(height) = calc_verified_automatic_cell_height(
            pack,
            &prepared,
            &region.text,
            source,
            calc_pattern_points,
            options,
            stats,
        )? {
            return Ok(height);
        }
    }
    match prepared.line_layout_policy {
        CellLineLayoutPolicy::Native => sum_fixed(
            prepared
                .lines
                .iter()
                .map(|line| line_height_from_metrics(line.metrics, prepared.line_layout_policy))
                .collect::<Result<Vec<_>, _>>()?,
        )?
        .checked_add(Fixed::from_pixels(AUTO_ROW_VERTICAL_PADDING_PIXELS))
        .ok_or(RenderError::CoordinateOverflow),
        CellLineLayoutPolicy::CalcEditEngine => calc_automatic_cell_height(&prepared.lines),
    }
}

fn calc_verified_automatic_cell_height(
    pack: &FontPack,
    prepared: &PreparedText,
    text: &str,
    source: CalcAutomaticMetricSource,
    pattern_points: Option<u16>,
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<Option<Fixed>, RenderError> {
    let style = prepared.styles.first().ok_or(RenderError::Typography {
        reason: "missing_text_style",
    })?;
    let resolution = pack.resolve(style.request());
    if !(resolution.exact_family || resolution.declared_alias) || !resolution.exact_style {
        return Ok(None);
    }
    // A single line whose cell stays on the pattern font resolves through the
    // same formula the implicit row height already uses, so an automatic row
    // and an untouched one agree by construction. Multi-line runs keep the
    // per-line metric accumulation below, because Calc stacks measured lines.
    if prepared.lines.len() == 1 {
        if let Some(height) = pattern_points.and_then(calc_ooxml_row_height_from_points) {
            return Ok(Some(height));
        }
    }
    let font_id = match source {
        CalcAutomaticMetricSource::RequestedFont => resolution.id,
        CalcAutomaticMetricSource::PreparedAsianOrRequested => {
            match prepared_asian_face(prepared, text, options, stats)? {
                PreparedAsianFace::None => resolution.id,
                PreparedAsianFace::Verified(font_id)
                    if pack.weight(font_id).map_err(map_font_error)?
                        == if style.bold { 700 } else { 400 }
                        && pack.is_italic(font_id).map_err(map_font_error)? == style.italic =>
                {
                    font_id
                }
                PreparedAsianFace::Verified(_) | PreparedAsianFace::Unverified => return Ok(None),
            }
        }
        CalcAutomaticMetricSource::CalcComplexRole => {
            let Some(font_id) = calc_verified_complex_role_face(pack, style, resolution.id)? else {
                return Ok(None);
            };
            font_id
        }
    };
    let metrics = single_face_line_metrics(
        pack,
        font_id,
        style,
        CellLineLayoutPolicy::CalcEditEngine,
        1,
        1,
    )?;
    let line_height = calc_line_height_mm100(metrics)?;
    let total = line_height
        .checked_mul(prepared.lines.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    calc_automatic_height_from_engine_mm100(total).map(Some)
}

fn calc_face_has_complex_role_coverage(
    pack: &FontPack,
    font_id: FontId,
) -> Result<bool, RenderError> {
    for probe in CALC_CTL_FONT_PROBES {
        if pack
            .face_supports_text(font_id, probe)
            .map_err(map_font_error)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn calc_verified_complex_role_face(
    pack: &FontPack,
    style: &ResolvedRunStyle,
    requested_font_id: FontId,
) -> Result<Option<FontId>, RenderError> {
    // OOXML import keeps a requested family in the CTL slot whenever the
    // resolved face passes Calc's complex-script classification. The logical
    // document default is consulted only when that slot was left untouched.
    if calc_face_has_complex_role_coverage(pack, requested_font_id)? {
        return Ok(Some(requested_font_id));
    }
    let resolution = pack.resolve(FontRequest {
        family: CALC_CTL_LOGICAL_FAMILY,
        weight: if style.bold { 700 } else { 400 },
        italic: style.italic,
    });
    if !(resolution.exact_family || resolution.declared_alias) || !resolution.exact_style {
        return Ok(None);
    }
    if !calc_face_has_complex_role_coverage(pack, resolution.id)? {
        return Ok(None);
    }
    Ok(Some(resolution.id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedAsianFace {
    None,
    Verified(FontId),
    Unverified,
}

fn prepared_asian_face(
    prepared: &PreparedText,
    text: &str,
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<PreparedAsianFace, RenderError> {
    let mut selected = None;
    for line in &prepared.lines {
        for run in &line.shaped.runs {
            let Some(start) = line.source.start.checked_add(run.source.start) else {
                return Ok(PreparedAsianFace::Unverified);
            };
            let Some(end) = line.source.start.checked_add(run.source.end) else {
                return Ok(PreparedAsianFace::Unverified);
            };
            if start > end || end > line.advance_end {
                return Ok(PreparedAsianFace::Unverified);
            }
            let Some(source) = text.get(start..end) else {
                return Ok(PreparedAsianFace::Unverified);
            };
            let summary = calc_script_class_summary_bounded(source, options, stats)?;
            if !summary.has_asian {
                continue;
            }
            if run.style_index != 0 || run.glyphs.iter().any(|glyph| glyph.glyph_id == 0) {
                return Ok(PreparedAsianFace::Unverified);
            }
            if selected.is_some_and(|font_id| font_id != run.font_id) {
                return Ok(PreparedAsianFace::Unverified);
            }
            selected = Some(run.font_id);
        }
    }
    Ok(selected.map_or(PreparedAsianFace::None, PreparedAsianFace::Verified))
}

fn account_shaping(
    pack: &FontPack,
    shaped: &ShapedText,
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<(), RenderError> {
    for selected in &shaped.selected_faces {
        stats.record_face(pack, selected.font_id, selected.substituted)?;
    }
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
    )
}

fn line_height_from_metrics(
    metrics: CombinedLineMetrics,
    policy: CellLineLayoutPolicy,
) -> Result<Fixed, RenderError> {
    let physical = metrics
        .ascent
        .checked_sub(metrics.descent)
        .ok_or(RenderError::CoordinateOverflow)?;
    let height = match policy {
        CellLineLayoutPolicy::Native => physical
            .checked_add(metrics.line_gap)
            .ok_or(RenderError::CoordinateOverflow)?,
        CellLineLayoutPolicy::CalcEditEngine => physical,
    };
    Ok(height.max(Fixed::from_raw(1)))
}

fn calc_line_height_mm100(metrics: CombinedLineMetrics) -> Result<u64, RenderError> {
    let ascent =
        u64::try_from(metrics.calc_ascent_pixels).map_err(|_| RenderError::CoordinateOverflow)?;
    let descent = metrics.calc_descent_pixels.unsigned_abs();
    let formatter = round_unsigned_ratio(ascent, MM100_PER_INCH, CALC_DEVICE_DPI)
        .and_then(|value| {
            round_unsigned_ratio(descent, MM100_PER_INCH, CALC_DEVICE_DPI)
                .and_then(|descent| value.checked_add(descent))
        })
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(metrics.calc_portion_height_mm100.max(formatter).max(1))
}

fn calc_automatic_cell_height(lines: &[PreparedLine]) -> Result<Fixed, RenderError> {
    let total_mm100 = lines.iter().try_fold(0_u64, |total, line| {
        total
            .checked_add(calc_line_height_mm100(line.metrics)?)
            .ok_or(RenderError::CoordinateOverflow)
    })?;
    calc_automatic_height_from_engine_mm100(total_mm100)
}

fn calc_automatic_height_from_engine_mm100(total_mm100: u64) -> Result<Fixed, RenderError> {
    let text_pixels = round_unsigned_ratio(total_mm100, CALC_DEVICE_DPI, MM100_PER_INCH)
        .ok_or(RenderError::CoordinateOverflow)?;
    let row_pixels = text_pixels
        .checked_add(
            u64::try_from(AUTO_ROW_VERTICAL_PADDING_PIXELS)
                .map_err(|_| RenderError::CoordinateOverflow)?,
        )
        .ok_or(RenderError::CoordinateOverflow)?;
    let row_twips = row_pixels
        .checked_mul(CALC_OPTIMAL_HEIGHT_SAMPLE_TWIPS)
        .and_then(|value| value.checked_div(CALC_OPTIMAL_HEIGHT_SAMPLE_PIXELS))
        .ok_or(RenderError::CoordinateOverflow)?;
    let raw = round_unsigned_ratio(
        row_twips,
        FIXED_UNITS_PER_PIXEL as u64,
        u64::try_from(TWIPS_PER_CSS_PIXEL).map_err(|_| RenderError::CoordinateOverflow)?,
    )
    .and_then(|value| i64::try_from(value).ok())
    .ok_or(RenderError::CoordinateOverflow)?;
    Ok(Fixed::from_raw(raw.max(1)))
}

#[allow(clippy::too_many_arguments)]
fn build_glyph_run(
    pack: &FontPack,
    region: &Region,
    layout_bounds: Rect,
    clip_bounds: Rect,
    style: &TextStyle,
    sheet_right_to_left: bool,
    kerning: bool,
    options: &RenderOptions,
    stats: &mut TypographyStats,
    warnings: &mut Warnings,
) -> Result<GlyphRunNode, RenderError> {
    let alignment = region.style.as_ref().and_then(|style| style.align.as_ref());
    let mut prepared = prepare_styled_text(
        pack,
        region,
        style,
        sheet_right_to_left,
        kerning,
        options,
        stats,
    )?;
    warnings.add_count(
        WarningCode::MissingGlyph,
        prepared.missing_glyphs,
        Some(region.source),
    );
    if prepared.family_substituted {
        warnings.add(WarningCode::FontFamilySubstituted, Some(region.source));
    }

    let mut scale_numerator = 1_i64;
    let mut scale_denominator = 1_i64;
    if alignment.is_some_and(|alignment| alignment.shrink_to_fit)
        && !alignment.is_some_and(|alignment| alignment.wrap)
        && prepared.max_width > prepared.available_width
        && prepared.max_width.raw() > 0
    {
        scale_numerator = prepared.available_width.raw().max(1);
        scale_denominator = prepared.max_width.raw();
        let floor = options.min_shrink_font_size.max(Fixed::from_raw(1));
        if scale_ratio(prepared.styles[0].size, scale_numerator, scale_denominator)? < floor {
            scale_numerator = floor.raw();
            scale_denominator = prepared.styles[0].size.raw().max(1);
        }
        for line in &mut prepared.lines {
            line.width = styled_shaped_width(
                pack,
                &line.shaped,
                &prepared.styles,
                scale_numerator,
                scale_denominator,
            )?;
            line.metrics = styled_line_metrics(
                pack,
                &line.shaped,
                &prepared.styles,
                prepared.line_layout_policy,
                scale_numerator,
                scale_denominator,
            )?;
        }
    }

    let line_heights = prepared
        .lines
        .iter()
        .map(|line| line_height_from_metrics(line.metrics, prepared.line_layout_policy))
        .collect::<Result<Vec<_>, _>>()?;
    let block_height = sum_fixed(line_heights.iter().copied())?;
    let top = if region.ods_fixed_height_row
        && block_height > region.rect.height
        && alignment.is_some_and(|alignment| {
            alignment.wrap && alignment.vertical.is_none() && alignment.rotation == 0
        }) {
        // Calc preserves the beginning of implicitly aligned wrapped ODS text
        // in a fixed-height row. Its generic bottom default would otherwise
        // translate the beginning above the clip and retain only the tail.
        let top_bounds =
            calc_cell_text_layout_bounds(region.rect, TextBaseline::Top, region.vertical_margin)?;
        vertical_block_top(top_bounds, block_height, TextBaseline::Top)?
    } else {
        vertical_block_top(layout_bounds, block_height, style.baseline)?
    };

    let mut commands = Vec::new();
    let mut clusters = Vec::new();
    let mut cluster_metrics = Vec::new();
    let mut paints = Vec::new();
    let mut decorations = Vec::new();
    let mut glyphs = Vec::new();
    let mut font_faces = Vec::new();
    let mut line_top = top;
    for (line, line_height) in prepared.lines.iter().zip(line_heights) {
        let baseline = line_top
            .checked_add(line.metrics.ascent)
            .ok_or(RenderError::CoordinateOverflow)?;
        let line_x = horizontal_line_start(
            layout_bounds,
            prepared.horizontal_padding,
            line.width,
            style.anchor,
        )?;
        append_styled_shaped_outlines(
            pack,
            &region.text,
            line.source.start,
            &line.shaped,
            line_x,
            baseline,
            &prepared.styles,
            scale_numerator,
            scale_denominator,
            options,
            stats,
            &mut commands,
            &mut clusters,
            &mut cluster_metrics,
            &mut paints,
            &mut decorations,
            &mut glyphs,
            &mut font_faces,
        )?;
        if line.advance_end < line.source.end {
            if region.text.get(line.advance_end..line.source.end) != Some(" ") {
                return Err(RenderError::Typography {
                    reason: "invalid_suppressed_line_suffix",
                });
            }
            let command_index =
                u64::try_from(commands.len()).map_err(|_| RenderError::CoordinateOverflow)?;
            let source_start =
                u64::try_from(line.advance_end).map_err(|_| RenderError::CoordinateOverflow)?;
            let source_end =
                u64::try_from(line.source.end).map_err(|_| RenderError::CoordinateOverflow)?;
            clusters.push(GlyphCluster {
                source_start,
                source_end,
                command_start: command_index,
                command_end: command_index,
            });
            let origin_x = if line.shaped.base_direction == BaseDirection::RightToLeft {
                line_x
            } else {
                line_x
                    .checked_add(line.width)
                    .ok_or(RenderError::CoordinateOverflow)?
            };
            cluster_metrics.push(GlyphClusterMetrics {
                origin_x,
                advance_x: Fixed::ZERO,
                baseline_y: baseline,
                ascent: line.metrics.ascent.max(Fixed::from_raw(1)),
                descent: line.metrics.descent.min(Fixed::ZERO),
            });
        }
        line_top = line_top
            .checked_add(line_height)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    let (pivot_x, pivot_y) = rotation_pivot(layout_bounds, prepared.horizontal_padding, style)?;
    let node = GlyphRunNode {
        glyphs,
        font_faces,
        text: region.text.clone(),
        clip_bounds,
        commands,
        clusters,
        cluster_metrics,
        semantic_groups: glyph_semantic_groups(region, &prepared.lines, block_height)?,
        paints,
        decorations,
        color: style.color,
        rotation_degrees: style.rotation_degrees,
        pivot_x,
        pivot_y,
        hyperlink: region.hyperlink.clone(),
    };
    if !node.metadata_is_valid() {
        return Err(RenderError::Typography {
            reason: "invalid_glyph_metadata",
        });
    }
    Ok(node)
}

fn prepare_styled_text(
    pack: &FontPack,
    region: &Region,
    base: &TextStyle,
    sheet_right_to_left: bool,
    kerning: bool,
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<PreparedText, RenderError> {
    let (styles, spans) = resolve_rich_styles(region, base)?;
    let direction = text_base_direction(&region.text, sheet_right_to_left);
    let primary_size = styled_font_size(&styles[0], 1, 1)?;
    let horizontal_padding =
        outlined_horizontal_padding(pack, styles[0].request(), primary_size, region, options)?;
    let available_width = inner_width(region.rect.width, horizontal_padding)?;
    let calc_wrap_space = match region.line_layout_policy {
        CellLineLayoutPolicy::Native => None,
        CellLineLayoutPolicy::CalcEditEngine => {
            Some(region.calc_wrap_space.ok_or(RenderError::Typography {
                reason: "missing_calc_wrap_space",
            })?)
        }
    };
    let line_available_width = match calc_wrap_space {
        Some(space) => space.line_width()?,
        None => available_width,
    };
    let scalar_count = region.text.chars().count() as u64;
    let work = scalar_count
        .checked_mul(2)
        .and_then(|value| value.checked_add(1))
        .ok_or(RenderError::CoordinateOverflow)?;
    stats.text_work = stats
        .text_work
        .checked_add(work)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::TextRuns,
        options.limits.max_text_runs,
        stats.text_work,
    )?;
    let remaining_lines = options
        .limits
        .max_text_lines
        .saturating_sub(stats.text_lines);
    let wrap = region
        .style
        .as_ref()
        .and_then(|style| style.align.as_ref())
        .is_some_and(|alignment| alignment.wrap);
    let wrapped_lines = wrap_text_lines(
        &region.text,
        wrap,
        region.line_layout_policy,
        line_available_width,
        remaining_lines,
        work,
        |range| {
            let shaped = shape_styled_range(
                pack,
                &region.text,
                range,
                &spans,
                &styles,
                direction,
                kerning,
                options,
            )?;
            let width = styled_shaped_width(pack, &shaped, &styles, 1, 1)?;
            match calc_wrap_space {
                Some(_) => CalcWrapSpace::measure_physical_width(width, primary_size),
                None => Ok(width),
            }
        },
    )?;

    let mut lines = Vec::with_capacity(wrapped_lines.len());
    let mut max_width = Fixed::ZERO;
    let mut missing_glyphs = 0_u64;
    let mut family_substituted = false;
    for line in wrapped_lines {
        let source = line.source;
        let shaped = shape_styled_range(
            pack,
            &region.text,
            source.start..line.advance_end,
            &spans,
            &styles,
            direction,
            kerning,
            options,
        )?;
        account_shaping(pack, &shaped, options, stats)?;
        let width = styled_shaped_width(pack, &shaped, &styles, 1, 1)?;
        let metrics = styled_line_metrics(pack, &shaped, &styles, region.line_layout_policy, 1, 1)?;
        max_width = max_width.max(width);
        missing_glyphs = missing_glyphs.saturating_add(shaped.missing_glyphs as u64);
        family_substituted |= !shaped.requested_family_matched;
        lines.push(PreparedLine {
            source,
            advance_end: line.advance_end,
            shaped,
            width,
            metrics,
        });
    }
    stats.text_lines = stats
        .text_lines
        .checked_add(lines.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::TextLines,
        options.limits.max_text_lines,
        stats.text_lines,
    )?;
    Ok(PreparedText {
        styles,
        lines,
        line_layout_policy: region.line_layout_policy,
        horizontal_padding,
        available_width,
        max_width,
        missing_glyphs,
        family_substituted,
    })
}

fn resolve_rich_styles(
    region: &Region,
    base: &TextStyle,
) -> Result<(Vec<ResolvedRunStyle>, Vec<StyledSourceSpan>), RenderError> {
    let cell_script = region
        .style
        .as_ref()
        .and_then(|style| style.font.as_ref())
        .map_or(FormatScript::None, |font| font.script);
    let base_style = ResolvedRunStyle {
        family: base.family.clone(),
        size: base.size,
        color: base.color,
        bold: base.bold,
        italic: base.italic,
        underline: base.underline,
        strikethrough: base.strikethrough,
        script: cell_script,
    };
    let mut styles = vec![base_style.clone()];
    let Some(runs) = region.rich_text.as_deref() else {
        return Ok((
            styles,
            vec![StyledSourceSpan {
                source: 0..region.text.len(),
                style_index: 0,
            }],
        ));
    };
    let mut spans: Vec<StyledSourceSpan> = Vec::new();
    let mut cursor = 0_usize;
    for run in runs {
        let end = cursor
            .checked_add(run.text.len())
            .ok_or(RenderError::CoordinateOverflow)?;
        if end > region.text.len()
            || !region.text.is_char_boundary(cursor)
            || !region.text.is_char_boundary(end)
        {
            return Err(RenderError::Typography {
                reason: "invalid_rich_text_range",
            });
        }
        let candidate = ResolvedRunStyle {
            family: run
                .font
                .name
                .clone()
                .unwrap_or_else(|| base_style.family.clone()),
            size: run
                .font
                .size_pt
                .and_then(|points| points_to_fixed(points as f32))
                .unwrap_or(base_style.size),
            color: run.font.color.map(rgb).unwrap_or(base_style.color),
            bold: base_style.bold || run.font.bold,
            italic: base_style.italic || run.font.italic,
            underline: base_style.underline || run.font.underline,
            strikethrough: base_style.strikethrough || run.font.strikethrough,
            script: if run.font.script == FormatScript::None {
                base_style.script
            } else {
                run.font.script
            },
        };
        let style_index = styles
            .iter()
            .position(|style| style == &candidate)
            .unwrap_or_else(|| {
                styles.push(candidate);
                styles.len() - 1
            });
        if cursor != end {
            if let Some(last) = spans.last_mut() {
                if last.style_index == style_index && last.source.end == cursor {
                    last.source.end = end;
                } else {
                    spans.push(StyledSourceSpan {
                        source: cursor..end,
                        style_index,
                    });
                }
            } else {
                spans.push(StyledSourceSpan {
                    source: cursor..end,
                    style_index,
                });
            }
        }
        cursor = end;
    }
    if cursor != region.text.len() || (!region.text.is_empty() && spans.is_empty()) {
        return Err(RenderError::Typography {
            reason: "invalid_rich_text_range",
        });
    }
    Ok((styles, spans))
}

#[allow(clippy::too_many_arguments)]
fn shape_styled_range(
    pack: &FontPack,
    text: &str,
    source: Range<usize>,
    spans: &[StyledSourceSpan],
    styles: &[ResolvedRunStyle],
    direction: BaseDirection,
    kerning: bool,
    options: &RenderOptions,
) -> Result<ShapedText, RenderError> {
    let value = text.get(source.clone()).ok_or(RenderError::Typography {
        reason: "invalid_rich_text_range",
    })?;
    let requests = spans
        .iter()
        .filter_map(|span| {
            let start = span.source.start.max(source.start);
            let end = span.source.end.min(source.end);
            (start < end).then_some((start, end, span.style_index))
        })
        .map(|(start, end, style_index)| {
            let style = styles.get(style_index).ok_or(RenderError::Typography {
                reason: "invalid_rich_style_index",
            })?;
            Ok(StyledFontRequest {
                source: start - source.start..end - source.start,
                request: style.request(),
                style_index,
            })
        })
        .collect::<Result<Vec<_>, RenderError>>()?;
    let glyph_limit = usize::try_from(options.limits.max_glyphs).unwrap_or(usize::MAX);
    let run_limit = usize::try_from(options.limits.max_text_runs).unwrap_or(usize::MAX);
    pack.shape_styled(
        value,
        &requests,
        ShapeOptions {
            direction,
            max_glyphs: glyph_limit,
            max_runs: run_limit,
            kerning,
        },
    )
    .map_err(map_font_error)
}

fn styled_shaped_width(
    pack: &FontPack,
    shaped: &ShapedText,
    styles: &[ResolvedRunStyle],
    scale_numerator: i64,
    scale_denominator: i64,
) -> Result<Fixed, RenderError> {
    let mut width = Fixed::ZERO;
    for run in &shaped.runs {
        let style = styles.get(run.style_index).ok_or(RenderError::Typography {
            reason: "invalid_rich_style_index",
        })?;
        let metrics = pack.metrics(run.font_id).map_err(map_font_error)?;
        let advance = run.glyphs.iter().try_fold(0_i64, |sum, glyph| {
            sum.checked_add(i64::from(glyph.x_advance))
                .ok_or(RenderError::CoordinateOverflow)
        })?;
        let advance =
            i64::try_from(advance.unsigned_abs()).map_err(|_| RenderError::CoordinateOverflow)?;
        width = width
            .checked_add(scale_font_units(
                advance,
                styled_font_size(style, scale_numerator, scale_denominator)?,
                metrics.units_per_em,
                1,
            )?)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(width)
}

fn styled_font_size(
    style: &ResolvedRunStyle,
    scale_numerator: i64,
    scale_denominator: i64,
) -> Result<Fixed, RenderError> {
    let size = scale_ratio(style.size, scale_numerator, scale_denominator)?;
    match style.script {
        FormatScript::None => Ok(size),
        FormatScript::Superscript | FormatScript::Subscript => scale_ratio(size, 13, 20),
    }
}

fn styled_script_shift(
    style: &ResolvedRunStyle,
    scale_numerator: i64,
    scale_denominator: i64,
) -> Result<Fixed, RenderError> {
    let size = scale_ratio(style.size, scale_numerator, scale_denominator)?;
    match style.script {
        FormatScript::None => Ok(Fixed::ZERO),
        FormatScript::Superscript => negate_fixed(scale_ratio(size, 7, 20)?),
        FormatScript::Subscript => scale_ratio(size, 1, 5),
    }
}

fn shape_text(
    pack: &FontPack,
    text: &str,
    request: FontRequest<'_>,
    direction: BaseDirection,
    options: &RenderOptions,
) -> Result<ShapedText, RenderError> {
    shape_text_with_kerning(pack, text, request, direction, true, options)
}

fn shape_text_with_kerning(
    pack: &FontPack,
    text: &str,
    request: FontRequest<'_>,
    direction: BaseDirection,
    kerning: bool,
    options: &RenderOptions,
) -> Result<ShapedText, RenderError> {
    let glyph_limit = usize::try_from(options.limits.max_glyphs).unwrap_or(usize::MAX);
    let run_limit = usize::try_from(options.limits.max_text_runs).unwrap_or(usize::MAX);
    pack.shape(
        text,
        request,
        ShapeOptions {
            direction,
            max_glyphs: glyph_limit,
            max_runs: run_limit,
            kerning,
        },
    )
    .map_err(map_font_error)
}

fn shaped_width(
    pack: &FontPack,
    shaped: &ShapedText,
    font_size: Fixed,
) -> Result<Fixed, RenderError> {
    let mut width = Fixed::ZERO;
    for run in &shaped.runs {
        let metrics = pack.metrics(run.font_id).map_err(map_font_error)?;
        let advance = run
            .glyphs
            .iter()
            .try_fold(0_i64, |sum, glyph| {
                sum.checked_add(i64::from(glyph.x_advance))
                    .ok_or(RenderError::CoordinateOverflow)
            })?
            .unsigned_abs();
        let advance = i64::try_from(advance).map_err(|_| RenderError::CoordinateOverflow)?;
        width = width
            .checked_add(scale_font_units(
                advance,
                font_size,
                metrics.units_per_em,
                1,
            )?)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(width)
}

fn outlined_horizontal_padding(
    pack: &FontPack,
    request: FontRequest<'_>,
    font_size: Fixed,
    region: &Region,
    options: &RenderOptions,
) -> Result<Fixed, RenderError> {
    let base = options.horizontal_padding.max(Fixed::ZERO);
    let indent = region
        .style
        .as_ref()
        .and_then(|style| style.align.as_ref())
        .map_or(0_u8, |alignment| alignment.indent);
    if indent == 0 {
        return Ok(base);
    }
    let (font_id, digit_width) = pack.max_digit_width(request).map_err(map_font_error)?;
    let metrics = pack.metrics(font_id).map_err(map_font_error)?;
    let indent_width =
        scale_font_units(i64::from(digit_width), font_size, metrics.units_per_em, 1)?;
    base.checked_add(multiply_fixed(indent_width, i64::from(indent))?)
        .ok_or(RenderError::CoordinateOverflow)
}

fn inner_width(width: Fixed, padding: Fixed) -> Result<Fixed, RenderError> {
    let inset = multiply_fixed(padding, 2)?;
    Ok(width
        .checked_sub(inset)
        .ok_or(RenderError::CoordinateOverflow)?
        .max(Fixed::from_raw(1)))
}

#[derive(Debug, Clone, Copy)]
struct CombinedLineMetrics {
    ascent: Fixed,
    descent: Fixed,
    line_gap: Fixed,
    calc_ascent_pixels: i64,
    calc_descent_pixels: i64,
    calc_portion_height_mm100: u64,
}

fn single_face_line_metrics(
    pack: &FontPack,
    font_id: FontId,
    style: &ResolvedRunStyle,
    policy: CellLineLayoutPolicy,
    scale_numerator: i64,
    scale_denominator: i64,
) -> Result<CombinedLineMetrics, RenderError> {
    let metrics = pack.metrics(font_id).map_err(map_font_error)?;
    let font_size = styled_font_size(style, scale_numerator, scale_denominator)?;
    let shift = styled_script_shift(style, scale_numerator, scale_denominator)?;
    let ascent = scale_font_units(
        i64::from(metrics.ascent),
        font_size,
        metrics.units_per_em,
        1,
    )?
    .checked_sub(shift)
    .ok_or(RenderError::CoordinateOverflow)?;
    let descent = scale_font_units(
        i64::from(metrics.descent),
        font_size,
        metrics.units_per_em,
        1,
    )?
    .checked_sub(shift)
    .ok_or(RenderError::CoordinateOverflow)?;
    let line_gap = scale_font_units(
        i64::from(metrics.line_gap.max(0)),
        font_size,
        metrics.units_per_em,
        1,
    )?;
    let (calc_ascent_pixels, calc_descent_pixels, calc_portion_height_mm100) =
        if policy == CellLineLayoutPolicy::CalcEditEngine {
            let device_em_pixels = round_unsigned_ratio(
                u64::try_from(font_size.raw()).map_err(|_| RenderError::CoordinateOverflow)?,
                1,
                FIXED_UNITS_PER_PIXEL as u64,
            )
            .ok_or(RenderError::CoordinateOverflow)?;
            let calc_ascent_pixels = round_signed_ratio(
                i128::from(metrics.ascent),
                i128::from(device_em_pixels),
                i128::from(metrics.units_per_em),
            )?;
            let calc_descent_pixels = round_signed_ratio(
                i128::from(metrics.descent),
                i128::from(device_em_pixels),
                i128::from(metrics.units_per_em),
            )?;
            let calc_portion_pixels = calc_ascent_pixels
                .checked_sub(calc_descent_pixels)
                .and_then(|value| u64::try_from(value).ok())
                .ok_or(RenderError::CoordinateOverflow)?;
            let calc_portion_height_mm100 =
                round_unsigned_ratio(calc_portion_pixels, MM100_PER_INCH, CALC_DEVICE_DPI)
                    .ok_or(RenderError::CoordinateOverflow)?;
            (
                calc_ascent_pixels,
                calc_descent_pixels,
                calc_portion_height_mm100,
            )
        } else {
            (0, 0, 0)
        };
    Ok(CombinedLineMetrics {
        ascent,
        descent,
        line_gap,
        calc_ascent_pixels,
        calc_descent_pixels,
        calc_portion_height_mm100,
    })
}

fn styled_line_metrics(
    pack: &FontPack,
    shaped: &ShapedText,
    styles: &[ResolvedRunStyle],
    policy: CellLineLayoutPolicy,
    scale_numerator: i64,
    scale_denominator: i64,
) -> Result<CombinedLineMetrics, RenderError> {
    let mut combined = CombinedLineMetrics {
        ascent: Fixed::ZERO,
        descent: Fixed::ZERO,
        line_gap: Fixed::ZERO,
        calc_ascent_pixels: 0,
        calc_descent_pixels: 0,
        calc_portion_height_mm100: 0,
    };
    let mut initialized = false;
    let mut include = |font_id: FontId, style: &ResolvedRunStyle| -> Result<(), RenderError> {
        let metrics = single_face_line_metrics(
            pack,
            font_id,
            style,
            policy,
            scale_numerator,
            scale_denominator,
        )?;
        if !initialized {
            combined = metrics;
            initialized = true;
        } else {
            combined.ascent = combined.ascent.max(metrics.ascent);
            combined.descent = combined.descent.min(metrics.descent);
            combined.line_gap = combined.line_gap.max(metrics.line_gap);
            combined.calc_ascent_pixels =
                combined.calc_ascent_pixels.max(metrics.calc_ascent_pixels);
            combined.calc_descent_pixels = combined
                .calc_descent_pixels
                .min(metrics.calc_descent_pixels);
            combined.calc_portion_height_mm100 = combined
                .calc_portion_height_mm100
                .max(metrics.calc_portion_height_mm100);
        }
        Ok(())
    };
    if shaped.runs.is_empty() {
        let primary = styles.first().ok_or(RenderError::Typography {
            reason: "missing_text_style",
        })?;
        include(pack.resolve(primary.request()).id, primary)?;
    } else {
        for run in &shaped.runs {
            let style = styles.get(run.style_index).ok_or(RenderError::Typography {
                reason: "invalid_rich_style_index",
            })?;
            include(run.font_id, style)?;
        }
    }
    Ok(combined)
}

fn vertical_block_top(
    rect: Rect,
    block_height: Fixed,
    baseline: TextBaseline,
) -> Result<Fixed, RenderError> {
    let remaining = rect
        .height
        .checked_sub(block_height)
        .ok_or(RenderError::CoordinateOverflow)?;
    match baseline {
        TextBaseline::Top => Ok(rect.y),
        TextBaseline::Middle => rect
            .y
            .checked_add(Fixed::from_raw(remaining.raw() / 2))
            .ok_or(RenderError::CoordinateOverflow),
        TextBaseline::Bottom => rect
            .y
            .checked_add(remaining)
            .ok_or(RenderError::CoordinateOverflow),
    }
}

fn calc_cell_text_layout_bounds(
    rect: Rect,
    baseline: TextBaseline,
    vertical_margin: Fixed,
) -> Result<Rect, RenderError> {
    // Calc applies the corresponding ATTR_MARGIN edge as a positional offset;
    // it does not resize the cell clip. Translating the backend-neutral layout
    // bounds preserves the original clip and keeps the source-derived inset
    // even when a deliberately short row cannot contain the margin. Equal top
    // and bottom defaults cancel for middle alignment, so its bounds stay byte-
    // for-byte unchanged.
    let y = match baseline {
        TextBaseline::Top => rect
            .y
            .checked_add(vertical_margin)
            .ok_or(RenderError::CoordinateOverflow)?,
        TextBaseline::Middle => rect.y,
        TextBaseline::Bottom => rect
            .y
            .checked_sub(vertical_margin)
            .ok_or(RenderError::CoordinateOverflow)?,
    };
    Ok(Rect { y, ..rect })
}

fn horizontal_line_start(
    rect: Rect,
    padding: Fixed,
    line_width: Fixed,
    anchor: TextAnchor,
) -> Result<Fixed, RenderError> {
    let right = rect
        .x
        .checked_add(rect.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    match anchor {
        TextAnchor::Start => rect
            .x
            .checked_add(padding)
            .ok_or(RenderError::CoordinateOverflow),
        TextAnchor::Middle => rect
            .x
            .checked_add(Fixed::from_raw(
                rect.width
                    .raw()
                    .checked_sub(line_width.raw())
                    .ok_or(RenderError::CoordinateOverflow)?
                    / 2,
            ))
            .ok_or(RenderError::CoordinateOverflow),
        TextAnchor::End => right
            .checked_sub(padding)
            .and_then(|value| value.checked_sub(line_width))
            .ok_or(RenderError::CoordinateOverflow),
    }
}

#[allow(clippy::too_many_arguments)]
fn append_styled_shaped_outlines(
    pack: &FontPack,
    text: &str,
    line_source_start: usize,
    shaped: &ShapedText,
    line_x: Fixed,
    baseline: Fixed,
    styles: &[ResolvedRunStyle],
    scale_numerator: i64,
    scale_denominator: i64,
    options: &RenderOptions,
    stats: &mut TypographyStats,
    output: &mut Vec<PathCommand>,
    clusters: &mut Vec<GlyphCluster>,
    cluster_metrics: &mut Vec<GlyphClusterMetrics>,
    paints: &mut Vec<GlyphPaint>,
    decorations: &mut Vec<LineNode>,
    glyphs: &mut Vec<ShapedGlyph>,
    font_faces: &mut Vec<SceneFontFace>,
) -> Result<(), RenderError> {
    let mut visual_cursor = line_x;
    for run in &shaped.runs {
        let style = styles.get(run.style_index).ok_or(RenderError::Typography {
            reason: "invalid_rich_style_index",
        })?;
        let metrics = pack.metrics(run.font_id).map_err(map_font_error)?;
        let font_size = styled_font_size(style, scale_numerator, scale_denominator)?;
        let nominal_ascent = scale_font_units(
            i64::from(metrics.ascent),
            font_size,
            metrics.units_per_em,
            1,
        )?;
        let nominal_descent = scale_font_units(
            i64::from(metrics.descent),
            font_size,
            metrics.units_per_em,
            1,
        )?;
        if nominal_ascent <= Fixed::ZERO
            || nominal_descent > Fixed::ZERO
            || nominal_ascent <= nominal_descent
        {
            return Err(RenderError::Typography {
                reason: "invalid_font_metrics",
            });
        }
        let run_baseline = baseline
            .checked_add(styled_script_shift(
                style,
                scale_numerator,
                scale_denominator,
            )?)
            .ok_or(RenderError::CoordinateOverflow)?;
        let signed_advance = run.glyphs.iter().try_fold(0_i64, |sum, glyph| {
            sum.checked_add(i64::from(glyph.x_advance))
                .ok_or(RenderError::CoordinateOverflow)
        })?;
        let run_width = scale_font_units(
            i64::try_from(signed_advance.unsigned_abs())
                .map_err(|_| RenderError::CoordinateOverflow)?,
            font_size,
            metrics.units_per_em,
            1,
        )?;
        let mut pen = if signed_advance < 0 {
            visual_cursor
                .checked_add(run_width)
                .ok_or(RenderError::CoordinateOverflow)?
        } else {
            visual_cursor
        };
        let synthetic_italic =
            style.italic && !pack.is_italic(run.font_id).map_err(map_font_error)?;
        let synthetic_bold = style.bold && pack.weight(run.font_id).map_err(map_font_error)? < 600;
        let synthetic_style = synthetic_italic || synthetic_bold;
        let face_index = {
            let identity = pack
                .selected_face_identity(run.font_id)
                .map_err(map_font_error)?;
            let existing = font_faces
                .iter()
                .position(|face| face.face_sha256 == identity.face_sha256);
            match existing {
                Some(index) => index,
                None => {
                    font_faces.push(SceneFontFace {
                        family: identity.family.to_string(),
                        weight: identity.weight,
                        italic: identity.italic,
                        units_per_em: metrics.units_per_em,
                        face_sha256: identity.face_sha256.to_string(),
                    });
                    font_faces.len() - 1
                }
            }
        };
        let face_index = u32::try_from(face_index).map_err(|_| RenderError::CoordinateOverflow)?;
        let run_command_start = output.len() as u64;
        let mut logical_cluster_starts = run
            .glyphs
            .iter()
            .map(|glyph| {
                usize::try_from(glyph.cluster).map_err(|_| RenderError::Typography {
                    reason: "invalid_glyph_cluster",
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        logical_cluster_starts.sort_unstable();
        logical_cluster_starts.dedup();
        let mut logical_cluster_ends = HashMap::with_capacity(logical_cluster_starts.len());
        for pair in logical_cluster_starts.windows(2) {
            logical_cluster_ends.insert(pair[0], pair[1]);
        }
        if let Some(&last) = logical_cluster_starts.last() {
            logical_cluster_ends.insert(last, run.source.end);
        }
        let mut glyph_index = 0_usize;
        while glyph_index < run.glyphs.len() {
            let shaped_glyph_start = glyphs.len();
            let cluster_start = usize::try_from(run.glyphs[glyph_index].cluster).map_err(|_| {
                RenderError::Typography {
                    reason: "invalid_glyph_cluster",
                }
            })?;
            let mut group_end = glyph_index + 1;
            while group_end < run.glyphs.len()
                && run.glyphs[group_end].cluster == run.glyphs[glyph_index].cluster
            {
                group_end += 1;
            }
            let cluster_origin_x = pen;
            let command_start = output.len() as u64;
            for glyph in &run.glyphs[glyph_index..group_end] {
                let x_offset = scale_font_units(
                    i64::from(glyph.x_offset),
                    font_size,
                    metrics.units_per_em,
                    1,
                )?;
                let y_offset = scale_font_units(
                    i64::from(glyph.y_offset),
                    font_size,
                    metrics.units_per_em,
                    1,
                )?;
                let origin_x = pen
                    .checked_add(x_offset)
                    .ok_or(RenderError::CoordinateOverflow)?;
                let origin_y = run_baseline
                    .checked_sub(y_offset)
                    .ok_or(RenderError::CoordinateOverflow)?;
                glyphs.push(ShapedGlyph {
                    face: face_index,
                    // Filled with the actual merged-or-new cluster index once
                    // the group's command range and source range are known.
                    cluster: u32::MAX,
                    glyph_id: glyph.glyph_id,
                    origin_x,
                    origin_y,
                    size: font_size,
                    synthetic: synthetic_style,
                });
                let remaining = options
                    .limits
                    .max_path_commands
                    .saturating_sub(stats.path_commands);
                let outline = match pack.outline(run.font_id, glyph.glyph_id, remaining) {
                    Ok(outline) => outline,
                    Err(FontPackError::LimitExceeded { limit, actual, .. }) => {
                        if limit == remaining {
                            return Err(RenderError::LimitExceeded {
                                kind: LimitKind::PathCommands,
                                limit: options.limits.max_path_commands,
                                actual: stats.path_commands.saturating_add(actual),
                            });
                        }
                        return Err(RenderError::Typography {
                            reason: "glyph_outline_complexity",
                        });
                    }
                    Err(error) => return Err(map_font_error(error)),
                };
                let outline_multiplier = if synthetic_bold { 2_u64 } else { 1_u64 };
                let outline_commands = (outline.len() as u64)
                    .checked_mul(outline_multiplier)
                    .ok_or(RenderError::CoordinateOverflow)?;
                stats.path_commands = stats
                    .path_commands
                    .checked_add(outline_commands)
                    .ok_or(RenderError::CoordinateOverflow)?;
                enforce(
                    LimitKind::PathCommands,
                    options.limits.max_path_commands,
                    stats.path_commands,
                )?;
                let bold_offset = scale_ratio(font_size, 1, 32)?.max(Fixed::from_raw(1));
                for copy in 0..outline_multiplier {
                    let copy_origin_x = if copy == 0 {
                        origin_x
                    } else {
                        origin_x
                            .checked_add(bold_offset)
                            .ok_or(RenderError::CoordinateOverflow)?
                    };
                    for command in &outline {
                        output.push(transform_outline_command(
                            *command,
                            copy_origin_x,
                            origin_y,
                            font_size,
                            metrics.units_per_em,
                            synthetic_italic,
                        )?);
                    }
                }
                let advance = scale_font_units(
                    i64::from(glyph.x_advance),
                    font_size,
                    metrics.units_per_em,
                    1,
                )?;
                pen = pen
                    .checked_add(advance)
                    .ok_or(RenderError::CoordinateOverflow)?;
            }
            let metrics = GlyphClusterMetrics {
                origin_x: cluster_origin_x,
                advance_x: pen
                    .checked_sub(cluster_origin_x)
                    .ok_or(RenderError::CoordinateOverflow)?,
                baseline_y: run_baseline,
                ascent: nominal_ascent,
                descent: nominal_descent,
            };
            let cluster_end = logical_cluster_ends.get(&cluster_start).copied().ok_or(
                RenderError::Typography {
                    reason: "invalid_glyph_cluster",
                },
            )?;
            let source_start = line_source_start
                .checked_add(cluster_start)
                .ok_or(RenderError::CoordinateOverflow)?;
            let source_end = line_source_start
                .checked_add(cluster_end)
                .ok_or(RenderError::CoordinateOverflow)?;
            if cluster_start < run.source.start
                || cluster_end > run.source.end
                || cluster_start >= cluster_end
                || source_end > text.len()
                || !text.is_char_boundary(source_start)
                || !text.is_char_boundary(source_end)
            {
                return Err(RenderError::Typography {
                    reason: "invalid_glyph_cluster",
                });
            }
            let command_end = output.len() as u64;
            let cluster_index = if let Some(previous) = clusters.last_mut() {
                if previous.source_start == source_start as u64
                    && previous.source_end == source_end as u64
                    && previous.command_end == command_start
                {
                    previous.command_end = command_end;
                    let previous_metrics =
                        cluster_metrics.last_mut().ok_or(RenderError::Typography {
                            reason: "invalid_glyph_metadata",
                        })?;
                    merge_cluster_metrics(previous_metrics, metrics)?;
                    clusters.len() - 1
                } else {
                    let index = clusters.len();
                    clusters.push(GlyphCluster {
                        source_start: source_start as u64,
                        source_end: source_end as u64,
                        command_start,
                        command_end,
                    });
                    cluster_metrics.push(metrics);
                    index
                }
            } else {
                clusters.push(GlyphCluster {
                    source_start: source_start as u64,
                    source_end: source_end as u64,
                    command_start,
                    command_end,
                });
                cluster_metrics.push(metrics);
                0
            };
            let cluster_index =
                u32::try_from(cluster_index).map_err(|_| RenderError::CoordinateOverflow)?;
            for glyph in &mut glyphs[shaped_glyph_start..] {
                glyph.cluster = cluster_index;
            }
            glyph_index = group_end;
        }
        let run_command_end = output.len() as u64;
        if run_command_start != run_command_end {
            if let Some(previous) = paints.last_mut() {
                if previous.color == style.color && previous.command_end == run_command_start {
                    previous.command_end = run_command_end;
                } else {
                    paints.push(GlyphPaint {
                        command_start: run_command_start,
                        command_end: run_command_end,
                        color: style.color,
                    });
                }
            } else {
                paints.push(GlyphPaint {
                    command_start: run_command_start,
                    command_end: run_command_end,
                    color: style.color,
                });
            }
        }
        append_decorations(
            pack,
            run.font_id,
            visual_cursor,
            run_baseline,
            run_width,
            font_size,
            style.color,
            style.underline,
            style.strikethrough,
            decorations,
        )?;
        visual_cursor = visual_cursor
            .checked_add(run_width)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(())
}

fn merge_cluster_metrics(
    previous: &mut GlyphClusterMetrics,
    current: GlyphClusterMetrics,
) -> Result<(), RenderError> {
    let previous_end = previous
        .origin_x
        .checked_add(previous.advance_x)
        .ok_or(RenderError::CoordinateOverflow)?;
    let current_end = current
        .origin_x
        .checked_add(current.advance_x)
        .ok_or(RenderError::CoordinateOverflow)?;
    let left = Fixed::from_raw(
        previous
            .origin_x
            .raw()
            .min(previous_end.raw())
            .min(current.origin_x.raw())
            .min(current_end.raw()),
    );
    let right = Fixed::from_raw(
        previous
            .origin_x
            .raw()
            .max(previous_end.raw())
            .max(current.origin_x.raw())
            .max(current_end.raw()),
    );
    let previous_top = previous
        .baseline_y
        .checked_sub(previous.ascent)
        .ok_or(RenderError::CoordinateOverflow)?;
    let current_top = current
        .baseline_y
        .checked_sub(current.ascent)
        .ok_or(RenderError::CoordinateOverflow)?;
    let previous_bottom = previous
        .baseline_y
        .checked_sub(previous.descent)
        .ok_or(RenderError::CoordinateOverflow)?;
    let current_bottom = current
        .baseline_y
        .checked_sub(current.descent)
        .ok_or(RenderError::CoordinateOverflow)?;
    let top = Fixed::from_raw(previous_top.raw().min(current_top.raw()));
    let bottom = Fixed::from_raw(previous_bottom.raw().max(current_bottom.raw()));
    previous.origin_x = left;
    previous.advance_x = right
        .checked_sub(left)
        .ok_or(RenderError::CoordinateOverflow)?;
    previous.ascent = previous
        .baseline_y
        .checked_sub(top)
        .ok_or(RenderError::CoordinateOverflow)?;
    previous.descent = previous
        .baseline_y
        .checked_sub(bottom)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(())
}

fn transform_outline_command(
    command: FontOutlineCommand,
    origin_x: Fixed,
    origin_y: Fixed,
    font_size: Fixed,
    units_per_em: u16,
    synthetic_italic: bool,
) -> Result<PathCommand, RenderError> {
    let point = |x: i32, y: i32| {
        outline_point(
            x,
            y,
            origin_x,
            origin_y,
            font_size,
            units_per_em,
            synthetic_italic,
        )
    };
    Ok(match command {
        FontOutlineCommand::MoveTo(x, y) => {
            let (x, y) = point(x, y)?;
            PathCommand::MoveTo { x, y }
        }
        FontOutlineCommand::LineTo(x, y) => {
            let (x, y) = point(x, y)?;
            PathCommand::LineTo { x, y }
        }
        FontOutlineCommand::QuadraticTo(x1, y1, x, y) => {
            let (control_x, control_y) = point(x1, y1)?;
            let (x, y) = point(x, y)?;
            PathCommand::QuadraticTo {
                control_x,
                control_y,
                x,
                y,
            }
        }
        FontOutlineCommand::CubicTo(x1, y1, x2, y2, x, y) => {
            let (control1_x, control1_y) = point(x1, y1)?;
            let (control2_x, control2_y) = point(x2, y2)?;
            let (x, y) = point(x, y)?;
            PathCommand::CubicTo {
                control1_x,
                control1_y,
                control2_x,
                control2_y,
                x,
                y,
            }
        }
        FontOutlineCommand::Close => PathCommand::Close,
    })
}

#[allow(clippy::too_many_arguments)]
fn outline_point(
    x: i32,
    y: i32,
    origin_x: Fixed,
    origin_y: Fixed,
    font_size: Fixed,
    units_per_em: u16,
    synthetic_italic: bool,
) -> Result<(Fixed, Fixed), RenderError> {
    let mut x = i64::from(x);
    let y = i64::from(y);
    if synthetic_italic {
        x = x
            .checked_add(y / 5)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    let x = origin_x
        .checked_add(scale_font_units(
            x,
            font_size,
            units_per_em,
            FONT_OUTLINE_UNITS,
        )?)
        .ok_or(RenderError::CoordinateOverflow)?;
    let y = origin_y
        .checked_sub(scale_font_units(
            y,
            font_size,
            units_per_em,
            FONT_OUTLINE_UNITS,
        )?)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok((x, y))
}

#[allow(clippy::too_many_arguments)]
fn append_decorations(
    pack: &FontPack,
    font_id: crate::font::FontId,
    x: Fixed,
    baseline: Fixed,
    width: Fixed,
    font_size: Fixed,
    color: Rgb,
    underline: bool,
    strikethrough: bool,
    output: &mut Vec<LineNode>,
) -> Result<(), RenderError> {
    if width.raw() <= 0 || (!underline && !strikethrough) {
        return Ok(());
    }
    let metrics = pack.metrics(font_id).map_err(map_font_error)?;
    let x2 = x
        .checked_add(width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let mut push_metric = |position: i16, thickness: i16| -> Result<(), RenderError> {
        let y = baseline
            .checked_sub(scale_font_units(
                i64::from(position),
                font_size,
                metrics.units_per_em,
                1,
            )?)
            .ok_or(RenderError::CoordinateOverflow)?;
        let width = scale_font_units(
            i64::from(thickness).unsigned_abs() as i64,
            font_size,
            metrics.units_per_em,
            1,
        )?
        .max(Fixed::from_raw(1));
        output.push(LineNode {
            x1: x,
            y1: y,
            x2,
            y2: y,
            color,
            width,
        });
        Ok(())
    };
    if underline {
        push_metric(metrics.underline_position, metrics.underline_thickness)?;
    }
    if strikethrough {
        push_metric(metrics.strikeout_position, metrics.strikeout_thickness)?;
    }
    Ok(())
}

fn rotation_pivot(
    rect: Rect,
    padding: Fixed,
    style: &TextStyle,
) -> Result<(Fixed, Fixed), RenderError> {
    let right = rect
        .x
        .checked_add(rect.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let x = match style.anchor {
        TextAnchor::Start => rect.x.checked_add(padding),
        TextAnchor::Middle => rect.x.checked_add(Fixed::from_raw(rect.width.raw() / 2)),
        TextAnchor::End => right.checked_sub(padding),
    }
    .ok_or(RenderError::CoordinateOverflow)?;
    let y = match style.baseline {
        TextBaseline::Top => rect.y,
        TextBaseline::Middle => rect
            .y
            .checked_add(Fixed::from_raw(rect.height.raw() / 2))
            .ok_or(RenderError::CoordinateOverflow)?,
        TextBaseline::Bottom => bottom,
    };
    Ok((x, y))
}

fn scale_font_units(
    value: i64,
    font_size: Fixed,
    units_per_em: u16,
    coordinate_scale: i64,
) -> Result<Fixed, RenderError> {
    let denominator = i64::from(units_per_em)
        .checked_mul(coordinate_scale)
        .ok_or(RenderError::CoordinateOverflow)?;
    scale_ratio(Fixed::from_raw(value), font_size.raw(), denominator)
}

fn scale_ratio(value: Fixed, numerator: i64, denominator: i64) -> Result<Fixed, RenderError> {
    if denominator <= 0 {
        return Err(RenderError::Typography {
            reason: "invalid_scale_denominator",
        });
    }
    let product = i128::from(value.raw())
        .checked_mul(i128::from(numerator))
        .ok_or(RenderError::CoordinateOverflow)?;
    let divisor = i128::from(denominator);
    let rounded = if product >= 0 {
        product
            .checked_add(divisor / 2)
            .ok_or(RenderError::CoordinateOverflow)?
            / divisor
    } else {
        product
            .checked_sub(divisor / 2)
            .ok_or(RenderError::CoordinateOverflow)?
            / divisor
    };
    let raw = i64::try_from(rounded).map_err(|_| RenderError::CoordinateOverflow)?;
    Ok(Fixed::from_raw(raw))
}

fn multiply_fixed(value: Fixed, multiplier: i64) -> Result<Fixed, RenderError> {
    value
        .raw()
        .checked_mul(multiplier)
        .map(Fixed::from_raw)
        .ok_or(RenderError::CoordinateOverflow)
}

fn negate_fixed(value: Fixed) -> Result<Fixed, RenderError> {
    value
        .raw()
        .checked_neg()
        .map(Fixed::from_raw)
        .ok_or(RenderError::CoordinateOverflow)
}

fn map_font_error(error: FontPackError) -> RenderError {
    match error {
        FontPackError::LimitExceeded {
            resource,
            limit,
            actual,
        } => {
            let kind = match resource {
                "shape_glyphs" => LimitKind::Glyphs,
                "shape_runs" => LimitKind::TextRuns,
                "outline_commands" => LimitKind::PathCommands,
                _ => return RenderError::Typography { reason: resource },
            };
            RenderError::LimitExceeded {
                kind,
                limit,
                actual,
            }
        }
        FontPackError::InvalidTextRange => RenderError::Typography {
            reason: "invalid_text_range",
        },
        FontPackError::InvalidFont => RenderError::Typography {
            reason: "invalid_verified_font",
        },
        FontPackError::Io { .. }
        | FontPackError::InvalidManifest { .. }
        | FontPackError::UnsafePath
        | FontPackError::UnexpectedFile
        | FontPackError::MissingMember
        | FontPackError::SizeMismatch
        | FontPackError::DigestMismatch => RenderError::Typography {
            reason: "font_pack_state",
        },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ComposedEdgeOrientation {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ComposedEdgeKey {
    orientation: ComposedEdgeOrientation,
    axis: Fixed,
    start: Fixed,
    end: Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CellEdge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy)]
struct EdgeClaim {
    kind: EdgeClaimKind,
    style: BorderStyle,
    color: Rgb,
    owner: CellCoordinate,
    side: CellEdge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EdgeClaimKind {
    Gridline,
    GridlineSuppression,
    Explicit,
}

fn compose_edges(
    regions: &[Region],
    suppresses_gridlines: &[bool],
    show_gridlines: bool,
    gridline_policy: GridlinePolicy,
    options: &RenderOptions,
) -> Result<Vec<(ComposedEdgeKey, EdgeClaim)>, RenderError> {
    #[derive(Debug, Clone, Copy)]
    struct RawEdgeClaim {
        start: Fixed,
        end: Fixed,
        claim: EdgeClaim,
    }

    #[derive(Default)]
    struct EdgeEvents {
        starts: Vec<(usize, EdgeClaim)>,
        ends: Vec<usize>,
    }

    // Four source-side claims are one bounded compositor work unit. This
    // admits a coalesced 2x2 grid at its exact six-node output limit (16
    // claims = four work units) while stopping large low-output grids from
    // consuming unbounded intermediate memory.
    const CLAIMS_PER_COMPOSITOR_WORK_UNIT: u64 = 4;
    let mut claim_count = 0_u64;
    let mut raw_claims = BTreeMap::<(ComposedEdgeOrientation, Fixed), Vec<RawEdgeClaim>>::new();
    for (region_index, region) in regions.iter().enumerate() {
        let right = region
            .rect
            .x
            .checked_add(region.rect.width)
            .ok_or(RenderError::CoordinateOverflow)?;
        let bottom = region
            .rect
            .y
            .checked_add(region.rect.height)
            .ok_or(RenderError::CoordinateOverflow)?;
        for (side, orientation, axis, start, end) in [
            (
                CellEdge::Left,
                ComposedEdgeOrientation::Vertical,
                region.rect.x,
                region.rect.y,
                bottom,
            ),
            (
                CellEdge::Right,
                ComposedEdgeOrientation::Vertical,
                right,
                region.rect.y,
                bottom,
            ),
            (
                CellEdge::Top,
                ComposedEdgeOrientation::Horizontal,
                region.rect.y,
                region.rect.x,
                right,
            ),
            (
                CellEdge::Bottom,
                ComposedEdgeOrientation::Horizontal,
                bottom,
                region.rect.x,
                right,
            ),
        ] {
            let Some(claim) = region_edge_claim(
                region,
                side,
                suppresses_gridlines
                    .get(region_index)
                    .copied()
                    .unwrap_or(false),
                show_gridlines,
            ) else {
                continue;
            };
            if start >= end {
                continue;
            }
            claim_count = claim_count
                .checked_add(1)
                .ok_or(RenderError::CoordinateOverflow)?;
            let work_units = claim_count
                .checked_add(CLAIMS_PER_COMPOSITOR_WORK_UNIT - 1)
                .ok_or(RenderError::CoordinateOverflow)?
                / CLAIMS_PER_COMPOSITOR_WORK_UNIT;
            enforce(
                LimitKind::SceneNodes,
                options.limits.max_scene_nodes,
                work_units,
            )?;
            raw_claims
                .entry((orientation, axis))
                .or_default()
                .push(RawEdgeClaim { start, end, claim });
        }
    }

    // Resolve only claims sharing the same geometric axis. The event sweep is
    // linear in retained claims and replaces global Cartesian segmentation.
    let mut composed = BTreeMap::<ComposedEdgeKey, EdgeClaim>::new();
    for ((orientation, axis), claims) in raw_claims {
        let mut events = BTreeMap::<Fixed, EdgeEvents>::new();
        for (claim_index, raw) in claims.into_iter().enumerate() {
            events
                .entry(raw.start)
                .or_default()
                .starts
                .push((claim_index, raw.claim));
            events.entry(raw.end).or_default().ends.push(claim_index);
        }
        let mut active = BTreeMap::<usize, EdgeClaim>::new();
        let mut events = events.into_iter().peekable();
        while let Some((start, event)) = events.next() {
            for claim_index in event.ends {
                active.remove(&claim_index);
            }
            for (claim_index, claim) in event.starts {
                active.insert(claim_index, claim);
            }
            let Some((end, _)) = events.peek() else {
                break;
            };
            let end = *end;
            if start >= end {
                continue;
            }
            let Some(winner) = active.values().copied().reduce(|current, candidate| {
                if edge_claim_precedes(candidate, current, gridline_policy) {
                    candidate
                } else {
                    current
                }
            }) else {
                continue;
            };
            composed.insert(
                ComposedEdgeKey {
                    orientation,
                    axis,
                    start,
                    end,
                },
                winner,
            );
        }
    }
    Ok(coalesce_composed_edges(composed))
}

#[allow(clippy::too_many_arguments)]
fn push_composed_edges(
    nodes: &mut Vec<SceneNode>,
    composed: &[(ComposedEdgeKey, EdgeClaim)],
    layer: EdgeClaimKind,
    gridline_policy: GridlinePolicy,
    grid_bounds: Rect,
    scene_bounds: Rect,
    right_to_left: bool,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    debug_assert!(matches!(
        layer,
        EdgeClaimKind::Gridline | EdgeClaimKind::Explicit
    ));
    for &(key, claim) in composed {
        if claim.kind != layer {
            continue;
        }
        let print_gridline =
            layer == EdgeClaimKind::Gridline && gridline_policy != GridlinePolicy::WorksheetView;
        if print_gridline && is_print_gridline_leading_edge(key, grid_bounds, right_to_left)? {
            continue;
        }
        let key = if layer == EdgeClaimKind::Gridline
            && gridline_policy == GridlinePolicy::CalcSinglePagePrint
        {
            let mapped = map_calc_single_page_gridline_key(key, grid_bounds, right_to_left)?;
            let Some(clipped) = clip_composed_edge(mapped, scene_bounds)? else {
                continue;
            };
            clipped
        } else {
            key
        };
        if claim.style == BorderStyle::Double {
            push_double_edge(nodes, key, claim, options)?;
            continue;
        }
        let (color, width) = if print_gridline {
            (Rgb::BLACK, PRINT_GRIDLINE_WIDTH)
        } else {
            let Some(width) = border_width(claim.style) else {
                continue;
            };
            (claim.color, width)
        };
        push_node(
            nodes,
            SceneNode::Line(edge_line(key, Fixed::ZERO, color, width)?),
            options,
        )?;
    }
    Ok(())
}

fn is_print_gridline_leading_edge(
    key: ComposedEdgeKey,
    grid_bounds: Rect,
    right_to_left: bool,
) -> Result<bool, RenderError> {
    let right = grid_bounds
        .x
        .checked_add(grid_bounds.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(match key.orientation {
        ComposedEdgeOrientation::Vertical => {
            key.axis == if right_to_left { right } else { grid_bounds.x }
        }
        ComposedEdgeOrientation::Horizontal => key.axis == grid_bounds.y,
    })
}

fn map_calc_single_page_gridline_key(
    key: ComposedEdgeKey,
    grid_bounds: Rect,
    right_to_left: bool,
) -> Result<ComposedEdgeKey, RenderError> {
    let right = grid_bounds
        .x
        .checked_add(grid_bounds.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let map_x = |value: Fixed| -> Result<Fixed, RenderError> {
        if right_to_left {
            let distance = right
                .checked_sub(value)
                .ok_or(RenderError::CoordinateOverflow)?;
            grid_bounds
                .x
                .checked_sub(scale_ratio(
                    distance,
                    CALC_SINGLE_PAGE_GRIDLINE_SCALE_NUMERATOR,
                    CALC_SINGLE_PAGE_GRIDLINE_SCALE_DENOMINATOR,
                )?)
                .ok_or(RenderError::CoordinateOverflow)
        } else {
            let distance = value
                .checked_sub(grid_bounds.x)
                .ok_or(RenderError::CoordinateOverflow)?;
            grid_bounds
                .x
                .checked_add(scale_ratio(
                    distance,
                    CALC_SINGLE_PAGE_GRIDLINE_SCALE_NUMERATOR,
                    CALC_SINGLE_PAGE_GRIDLINE_SCALE_DENOMINATOR,
                )?)
                .ok_or(RenderError::CoordinateOverflow)
        }
    };
    let map_y = |value: Fixed| -> Result<Fixed, RenderError> {
        let distance = value
            .checked_sub(grid_bounds.y)
            .ok_or(RenderError::CoordinateOverflow)?;
        grid_bounds
            .y
            .checked_add(scale_ratio(
                distance,
                CALC_SINGLE_PAGE_GRIDLINE_SCALE_NUMERATOR,
                CALC_SINGLE_PAGE_GRIDLINE_SCALE_DENOMINATOR,
            )?)
            .ok_or(RenderError::CoordinateOverflow)
    };
    Ok(match key.orientation {
        ComposedEdgeOrientation::Vertical => ComposedEdgeKey {
            orientation: key.orientation,
            axis: map_x(key.axis)?,
            start: map_y(key.start)?,
            end: map_y(key.end)?,
        },
        ComposedEdgeOrientation::Horizontal => ComposedEdgeKey {
            orientation: key.orientation,
            axis: map_y(key.axis)?,
            start: map_x(key.start)?,
            end: map_x(key.end)?,
        },
    })
}

fn clip_composed_edge(
    mut key: ComposedEdgeKey,
    clip: Rect,
) -> Result<Option<ComposedEdgeKey>, RenderError> {
    let right = clip
        .x
        .checked_add(clip.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = clip
        .y
        .checked_add(clip.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    match key.orientation {
        ComposedEdgeOrientation::Vertical => {
            if key.axis < clip.x || key.axis > right {
                return Ok(None);
            }
            key.start = std::cmp::max(key.start, clip.y);
            key.end = std::cmp::min(key.end, bottom);
        }
        ComposedEdgeOrientation::Horizontal => {
            if key.axis < clip.y || key.axis > bottom {
                return Ok(None);
            }
            key.start = std::cmp::max(key.start, clip.x);
            key.end = std::cmp::min(key.end, right);
        }
    }
    Ok((key.start < key.end).then_some(key))
}

fn push_print_gridline_leading_frame(
    nodes: &mut Vec<SceneNode>,
    grid_bounds: Rect,
    scene_bounds: Rect,
    right_to_left: bool,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let grid_right = grid_bounds
        .x
        .checked_add(grid_bounds.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let grid_bottom = grid_bounds
        .y
        .checked_add(grid_bounds.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    // Calc keeps the leading frame in normal page space, but it is inset by
    // its integer Map100thMM stroke rectangle rather than centered on the
    // MediaBox. This makes both frame edges survive PDF clipping at 96 DPI.
    let left = grid_bounds
        .x
        .checked_add(PRINT_GRIDLINE_FRAME_LEFT_INSET)
        .ok_or(RenderError::CoordinateOverflow)?;
    let right = grid_right
        .checked_sub(PRINT_GRIDLINE_FRAME_TRAILING_INSET)
        .ok_or(RenderError::CoordinateOverflow)?;
    let top = grid_bounds
        .y
        .checked_add(PRINT_GRIDLINE_FRAME_TOP_INSET)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = grid_bottom
        .checked_sub(PRINT_GRIDLINE_FRAME_TRAILING_INSET)
        .ok_or(RenderError::CoordinateOverflow)?;
    if left >= right || top >= bottom {
        return Ok(());
    }
    let leading_x = if right_to_left { right } else { left };
    for key in [
        ComposedEdgeKey {
            orientation: ComposedEdgeOrientation::Vertical,
            axis: leading_x,
            start: top,
            end: bottom,
        },
        ComposedEdgeKey {
            orientation: ComposedEdgeOrientation::Horizontal,
            axis: top,
            start: left,
            end: right,
        },
    ] {
        let Some(key) = clip_composed_edge(key, scene_bounds)? else {
            continue;
        };
        push_node(
            nodes,
            SceneNode::Line(edge_line(
                key,
                Fixed::ZERO,
                Rgb::BLACK,
                PRINT_GRIDLINE_WIDTH,
            )?),
            options,
        )?;
    }
    Ok(())
}

fn region_edge_claim(
    region: &Region,
    side: CellEdge,
    suppresses_gridlines: bool,
    show_gridlines: bool,
) -> Option<EdgeClaim> {
    let (style, color) = region
        .style
        .as_ref()
        .and_then(|style| style.border.as_ref())
        .map_or((BorderStyle::None, None), |border| {
            border_edge_style_and_color(border, side)
        });
    if style != BorderStyle::None {
        return Some(EdgeClaim {
            kind: EdgeClaimKind::Explicit,
            style,
            color: color.map(rgb).unwrap_or(Rgb::BLACK),
            owner: region.source,
            side,
        });
    }
    show_gridlines.then_some(EdgeClaim {
        kind: if suppresses_gridlines {
            EdgeClaimKind::GridlineSuppression
        } else {
            EdgeClaimKind::Gridline
        },
        style: BorderStyle::Thin,
        color: Rgb::GRIDLINE,
        owner: region.source,
        side,
    })
}

fn border_edge_style_and_color(border: &Border, side: CellEdge) -> (BorderStyle, Option<Color>) {
    match side {
        CellEdge::Left => (border.left, border.left_color.or(border.color)),
        CellEdge::Right => (border.right, border.right_color.or(border.color)),
        CellEdge::Top => (border.top, border.top_color.or(border.color)),
        CellEdge::Bottom => (border.bottom, border.bottom_color.or(border.color)),
    }
}

fn edge_claim_precedes(
    candidate: EdgeClaim,
    current: EdgeClaim,
    gridline_policy: GridlinePolicy,
) -> bool {
    let candidate_strength = (
        edge_claim_kind_precedence(candidate.kind, gridline_policy),
        border_precedence(candidate.style),
    );
    let current_strength = (
        edge_claim_kind_precedence(current.kind, gridline_policy),
        border_precedence(current.style),
    );
    candidate_strength > current_strength
        || (candidate_strength == current_strength
            && (candidate.owner, candidate.side) < (current.owner, current.side))
}

fn edge_claim_kind_precedence(kind: EdgeClaimKind, gridline_policy: GridlinePolicy) -> u8 {
    match (kind, gridline_policy) {
        (EdgeClaimKind::Explicit, _) => 2,
        (EdgeClaimKind::GridlineSuppression, GridlinePolicy::WorksheetView) => 1,
        (EdgeClaimKind::Gridline, GridlinePolicy::WorksheetView) => 0,
        (EdgeClaimKind::Gridline, _) => 1,
        (EdgeClaimKind::GridlineSuppression, _) => 0,
    }
}

fn border_precedence(style: BorderStyle) -> u8 {
    match style {
        BorderStyle::None => 0,
        BorderStyle::Thin => 1,
        BorderStyle::Medium => 2,
        BorderStyle::Thick => 3,
        BorderStyle::Double => 4,
    }
}

fn coalesce_composed_edges(
    composed: BTreeMap<ComposedEdgeKey, EdgeClaim>,
) -> Vec<(ComposedEdgeKey, EdgeClaim)> {
    let mut coalesced: Vec<(ComposedEdgeKey, EdgeClaim)> = Vec::new();
    for (key, claim) in composed {
        if let Some((previous_key, previous_claim)) = coalesced.last_mut() {
            if previous_key.orientation == key.orientation
                && previous_key.axis == key.axis
                && previous_key.end == key.start
                && claims_are_visually_identical(*previous_claim, claim)
            {
                previous_key.end = key.end;
                continue;
            }
        }
        coalesced.push((key, claim));
    }
    coalesced
}

fn claims_are_visually_identical(left: EdgeClaim, right: EdgeClaim) -> bool {
    left.kind == right.kind && left.style == right.style && left.color == right.color
}

fn push_double_edge(
    nodes: &mut Vec<SceneNode>,
    key: ComposedEdgeKey,
    claim: EdgeClaim,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    // Calc centers a shared double rule on the geometric boundary. Symmetric
    // placement makes equivalent A.right/B.left (and top/bottom) authorship
    // identical, including after RTL reflection.
    for offset in [Fixed::from_pixels(-1), Fixed::from_pixels(1)] {
        push_node(
            nodes,
            SceneNode::Line(edge_line(key, offset, claim.color, Fixed::from_pixels(1))?),
            options,
        )?;
    }
    Ok(())
}

fn edge_line(
    key: ComposedEdgeKey,
    axis_offset: Fixed,
    color: Rgb,
    width: Fixed,
) -> Result<LineNode, RenderError> {
    let axis = key
        .axis
        .checked_add(axis_offset)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(match key.orientation {
        ComposedEdgeOrientation::Vertical => LineNode {
            x1: axis,
            y1: key.start,
            x2: axis,
            y2: key.end,
            color,
            width,
        },
        ComposedEdgeOrientation::Horizontal => LineNode {
            x1: key.start,
            y1: axis,
            x2: key.end,
            y2: axis,
            color,
            width,
        },
    })
}

fn border_width(style: BorderStyle) -> Option<Fixed> {
    match style {
        BorderStyle::None => None,
        BorderStyle::Thin => Some(Fixed::from_pixels(1)),
        BorderStyle::Medium => Some(Fixed::from_pixels(2)),
        BorderStyle::Thick | BorderStyle::Double => Some(Fixed::from_pixels(3)),
    }
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
mod tests {
    use std::collections::BTreeSet;
    use std::io::Write;

    use rxls::{
        Border, BorderStyle, CellStyle, CfRule, Chart, ChartKind, CondFormat, DvOp, Format,
        FormatScript, Image, ImageFmt, PageSetup, Series, Sparkline, SparklineKind, Workbook,
    };
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::font::{synthetic_test_pack, FontId, ShapedGlyph, ShapedRun};
    use crate::{
        build_print_document, render_print_document_pdf, render_print_document_pdf_with_fonts,
        render_print_document_png_pages, render_sheet_svg, PrintOptions,
    };

    fn outlined_options(range: RenderRange) -> RenderOptions {
        let pack = synthetic_test_pack();
        RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: pack.default_family().to_string(),
            font_pack: Some(pack),
            ..RenderOptions::default()
        }
    }

    fn imported_xlsx(styles: &str, worksheet: &str) -> Workbook {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (name, body) in [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="styles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
            ),
            ("xl/styles.xml", styles),
            ("xl/worksheets/sheet1.xml", worksheet),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        Workbook::open(&zip.finish().unwrap().into_inner()).expect("imported OOXML workbook")
    }

    fn imported_biff8_default_row(manual: bool, text: &str) -> Workbook {
        fn record(kind: u16, body: &[u8]) -> Vec<u8> {
            let mut output = kind.to_le_bytes().to_vec();
            output.extend_from_slice(&(body.len() as u16).to_le_bytes());
            output.extend_from_slice(body);
            output
        }

        fn bof(substream: u16) -> Vec<u8> {
            let mut body = Vec::with_capacity(16);
            body.extend_from_slice(&0x0600_u16.to_le_bytes());
            body.extend_from_slice(&substream.to_le_bytes());
            body.extend_from_slice(&[0; 12]);
            body
        }

        let mut boundsheet = vec![0, 0, 0, 0, 0, 0, 8, 0];
        boundsheet.extend_from_slice(b"Geometry");
        let mut label = vec![0, 0, 0, 0, 0, 0];
        label.extend_from_slice(&(text.len() as u16).to_le_bytes());
        label.push(0);
        label.extend_from_slice(text.as_bytes());
        let flags = u16::from(manual);
        let default_row = [flags.to_le_bytes(), 300_u16.to_le_bytes()].concat();

        let mut stream = record(0x0809, &bof(0x0005));
        stream.extend_from_slice(&record(0x0085, &boundsheet));
        stream.extend_from_slice(&record(0x000A, &[]));
        stream.extend_from_slice(&record(0x0809, &bof(0x0010)));
        stream.extend_from_slice(&record(0x0225, &default_row));
        stream.extend_from_slice(&record(0x0204, &label));
        stream.extend_from_slice(&record(0x000A, &[]));

        let mut compound =
            cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).expect("create CFB");
        compound
            .create_stream("/Workbook")
            .expect("create Workbook stream")
            .write_all(&stream)
            .expect("write Workbook stream");
        compound.flush().expect("flush CFB");
        Workbook::open(&compound.into_inner().into_inner()).expect("imported BIFF8 workbook")
    }

    fn xlsb_record(record_type: u32, payload: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        if record_type < 0x80 {
            output.push(record_type as u8);
        } else {
            output.push((record_type & 0x7f) as u8 | 0x80);
            output.push(((record_type >> 7) & 0x7f) as u8);
        }
        let mut size = payload.len();
        loop {
            let mut byte = (size & 0x7f) as u8;
            size >>= 7;
            if size != 0 {
                byte |= 0x80;
            }
            output.push(byte);
            if size == 0 {
                break;
            }
        }
        output.extend_from_slice(payload);
        output
    }

    fn xlsb_wide_string(value: &str) -> Vec<u8> {
        let units = value.encode_utf16().collect::<Vec<_>>();
        let mut output = (units.len() as u32).to_le_bytes().to_vec();
        for unit in units {
            output.extend_from_slice(&unit.to_le_bytes());
        }
        output
    }

    fn imported_width_xlsb(
        sheet_default: Option<(u32, u16)>,
        columns: &[(u16, u16, u32, bool)],
    ) -> Workbook {
        let mut bundle = vec![0_u8; 8];
        bundle.extend_from_slice(&xlsb_wide_string("rId1"));
        bundle.extend_from_slice(&xlsb_wide_string("Widths"));
        let workbook = xlsb_record(156, &bundle);

        let mut sheet = Vec::new();
        if let Some((width_256, base_characters)) = sheet_default {
            let mut format = Vec::new();
            format.extend_from_slice(&width_256.to_le_bytes());
            format.extend_from_slice(&base_characters.to_le_bytes());
            format.extend_from_slice(&300_u16.to_le_bytes());
            format.extend_from_slice(&0_u16.to_le_bytes());
            format.extend_from_slice(&[0, 0]);
            sheet.extend_from_slice(&xlsb_record(0x01E5, &format));
        }
        for &(first, last, width_256, hidden) in columns {
            let mut column = Vec::new();
            column.extend_from_slice(&u32::from(first).to_le_bytes());
            column.extend_from_slice(&u32::from(last).to_le_bytes());
            column.extend_from_slice(&width_256.to_le_bytes());
            column.extend_from_slice(&0_u32.to_le_bytes());
            column.extend_from_slice(&u16::from(hidden).to_le_bytes());
            sheet.extend_from_slice(&xlsb_record(60, &column));
        }

        let relationships = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", workbook.as_slice()),
            ("xl/_rels/workbook.bin.rels", relationships.as_bytes()),
            ("xl/worksheets/sheet1.bin", sheet.as_slice()),
        ] {
            zip.start_file(path, options).unwrap();
            zip.write_all(body).unwrap();
        }
        Workbook::open(&zip.finish().unwrap().into_inner()).expect("imported XLSB workbook")
    }

    fn imported_table_xlsx(styles: &str, worksheet: &str, table: &str) -> Workbook {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (name, body) in [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="styles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
            ),
            ("xl/styles.xml", styles),
            ("xl/worksheets/sheet1.xml", worksheet),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rIdTable" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/></Relationships>"#,
            ),
            ("xl/tables/table1.xml", table),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        Workbook::open(&zip.finish().unwrap().into_inner()).expect("imported OOXML table workbook")
    }

    fn imported_two_cell_drawing(kind: DrawingObjectKind, to_offset: (i64, i64)) -> Workbook {
        imported_two_cell_drawing_with_worksheet(
            kind,
            to_offset,
            r#"<worksheet><sheetData/><drawing r:id="rIdDrawing"/></worksheet>"#,
        )
    }

    fn imported_two_cell_drawing_with_worksheet(
        kind: DrawingObjectKind,
        to_offset: (i64, i64),
        worksheet: &str,
    ) -> Workbook {
        let (drawing_object, object_relationship, object_part) = match kind {
            DrawingObjectKind::Image => (
                r#"<pic><blipFill><blip r:embed="rIdObject"/></blipFill></pic>"#,
                r#"<Relationship Id="rIdObject" Target="../media/image1.png"/>"#,
                ("xl/media/image1.png", b"\x89PNG\r\n\x1a\n".as_slice()),
            ),
            DrawingObjectKind::Chart => (
                r#"<graphicFrame><graphic><graphicData><chart r:id="rIdObject"/></graphicData></graphic></graphicFrame>"#,
                r#"<Relationship Id="rIdObject" Target="../charts/chart1.xml"/>"#,
                (
                    "xl/charts/chart1.xml",
                    br#"<chartSpace><chart><plotArea><lineChart/></plotArea></chart></chartSpace>"#
                        .as_slice(),
                ),
            ),
            _ => panic!("test helper only supports images and charts"),
        };
        let drawing = format!(
            r#"<wsDr><twoCellAnchor><from><col>2</col><colOff>0</colOff><row>3</row><rowOff>0</rowOff></from><to><col>5</col><colOff>{}</colOff><row>7</row><rowOff>{}</rowOff></to>{drawing_object}</twoCellAnchor></wsDr>"#,
            to_offset.0, to_offset.1
        );
        let drawing_relationships = format!("<Relationships>{object_relationship}</Relationships>");
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (name, body) in [
            (
                "xl/workbook.xml",
                br#"<workbook><sheets><sheet name="Drawing" r:id="rId1"/></sheets></workbook>"#
                    .as_slice(),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#
                    .as_slice(),
            ),
            (
                "xl/worksheets/sheet1.xml",
                worksheet.as_bytes(),
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                br#"<Relationships><Relationship Id="rIdDrawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#
                    .as_slice(),
            ),
            ("xl/drawings/drawing1.xml", drawing.as_bytes()),
            (
                "xl/drawings/_rels/drawing1.xml.rels",
                drawing_relationships.as_bytes(),
            ),
            object_part,
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(body).unwrap();
        }
        Workbook::open(&zip.finish().unwrap().into_inner()).expect("two-cell drawing workbook")
    }

    fn imported_single_page_terminal_column_drawing(
        hidden_terminal_column: bool,
        explicit_prefix_widths: bool,
        from_column: u16,
        to_column: u16,
    ) -> Workbook {
        let mut columns = String::new();
        if explicit_prefix_widths {
            columns.push_str(
                r#"<col min="1" max="1" width="18" customWidth="1"/><col min="2" max="5" width="14" customWidth="1"/>"#,
            );
        }
        if hidden_terminal_column {
            columns.push_str(&format!(
                r#"<col min="{to_column}" max="{to_column}" hidden="1"/>"#
            ));
        }
        let columns = if columns.is_empty() {
            String::new()
        } else {
            format!("<cols>{columns}</cols>")
        };
        let worksheet =
            format!(r#"<worksheet>{columns}<sheetData/><drawing r:id="rIdDrawing"/></worksheet>"#);
        let drawing = format!(
            r#"<wsDr><twoCellAnchor><from><col>{from_column}</col><colOff>0</colOff><row>0</row><rowOff>0</rowOff></from><to><col>{to_column}</col><colOff>0</colOff><row>1</row><rowOff>0</rowOff></to><sp><nvSpPr><cNvPr id="1" name="Terminal column"/></nvSpPr></sp></twoCellAnchor></wsDr>"#
        );
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (name, body) in [
            (
                "xl/workbook.xml",
                br#"<workbook><sheets><sheet name="Drawing" r:id="rId1"/></sheets></workbook>"#
                    .as_slice(),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#
                    .as_slice(),
            ),
            ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                br#"<Relationships><Relationship Id="rIdDrawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#
                    .as_slice(),
            ),
            ("xl/drawings/drawing1.xml", drawing.as_bytes()),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(body).unwrap();
        }
        Workbook::open(&zip.finish().unwrap().into_inner())
            .expect("single-page terminal-column drawing workbook")
    }

    fn imported_hidden_two_cell_drawing(
        kind: DrawingObjectKind,
        move_only: bool,
        right_to_left: bool,
    ) -> Workbook {
        let edit_as = if move_only {
            r#" editAs="oneCell""#
        } else {
            ""
        };
        let right_to_left = if right_to_left { "1" } else { "0" };
        let worksheet = format!(
            r#"<worksheet><sheetViews><sheetView rightToLeft="{right_to_left}"/></sheetViews><sheetFormatPr defaultColWidth="8.8571428571" defaultRowHeight="15"/><cols><col min="4" max="6" hidden="1"/></cols><sheetData><row r="6" hidden="1"/><row r="7" hidden="1"/><row r="8" hidden="1"/></sheetData><drawing r:id="rIdDrawing"/></worksheet>"#
        );
        let (drawing_object, object_relationship, object_part) = match kind {
            DrawingObjectKind::Image => (
                r#"<pic><nvPicPr><cNvPr id="1" name="Hidden-axis image"/></nvPicPr><blipFill><blip r:embed="rIdObject"/></blipFill><spPr><xfrm><ext cx="1619250" cy="666750"/></xfrm></spPr></pic>"#,
                Some(r#"<Relationship Id="rIdObject" Target="../media/image1.png"/>"#),
                Some(("xl/media/image1.png", b"\x89PNG\r\n\x1a\n".as_slice())),
            ),
            DrawingObjectKind::Chart => (
                r#"<graphicFrame><nvGraphicFramePr><cNvPr id="1" name="Hidden-axis chart"/></nvGraphicFramePr><xfrm><ext cx="1619250" cy="666750"/></xfrm><graphic><graphicData><chart r:id="rIdObject"/></graphicData></graphic></graphicFrame>"#,
                Some(r#"<Relationship Id="rIdObject" Target="../charts/chart1.xml"/>"#),
                Some((
                    "xl/charts/chart1.xml",
                    br#"<chartSpace><chart><plotArea><lineChart/></plotArea></chart></chartSpace>"#
                        .as_slice(),
                )),
            ),
            DrawingObjectKind::Shape => (
                r#"<sp><nvSpPr><cNvPr id="1" name="Hidden-axis callout"/></nvSpPr><spPr><xfrm><ext cx="1619250" cy="666750"/></xfrm></spPr></sp>"#,
                None,
                None,
            ),
            _ => panic!("unsupported drawing test kind"),
        };
        let drawing = format!(
            r#"<wsDr><twoCellAnchor{edit_as}><from><col>2</col><colOff>0</colOff><row>3</row><rowOff>0</rowOff></from><to><col>5</col><colOff>0</colOff><row>7</row><rowOff>0</rowOff></to>{drawing_object}</twoCellAnchor></wsDr>"#
        );
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (name, body) in [
            (
                "xl/workbook.xml",
                br#"<workbook><sheets><sheet name="Drawing" r:id="rId1"/></sheets></workbook>"#
                    .as_slice(),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#
                    .as_slice(),
            ),
            ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                br#"<Relationships><Relationship Id="rIdDrawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#
                    .as_slice(),
            ),
            ("xl/drawings/drawing1.xml", drawing.as_bytes()),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(body).unwrap();
        }
        if let Some(relationship) = object_relationship {
            zip.start_file("xl/drawings/_rels/drawing1.xml.rels", options)
                .unwrap();
            zip.write_all(format!("<Relationships>{relationship}</Relationships>").as_bytes())
                .unwrap();
        }
        if let Some((name, body)) = object_part {
            zip.start_file(name, options).unwrap();
            zip.write_all(body).unwrap();
        }
        Workbook::open(&zip.finish().unwrap().into_inner())
            .expect("hidden-axis two-cell drawing workbook")
    }

    fn fixed_drawing_outer_rect(nodes: &[SceneNode]) -> Rect {
        for node in nodes {
            match node {
                SceneNode::Rect(RectNode { rect, .. })
                    if rect.width == Fixed::from_pixels(170)
                        && rect.height == Fixed::from_pixels(70) =>
                {
                    return *rect;
                }
                SceneNode::ClipGroup(group) => {
                    if let Some(rect) = group.nodes.iter().find_map(|node| match node {
                        SceneNode::Rect(RectNode { rect, .. })
                            if rect.width == Fixed::from_pixels(170)
                                && rect.height == Fixed::from_pixels(70) =>
                        {
                            Some(*rect)
                        }
                        _ => None,
                    }) {
                        return rect;
                    }
                }
                _ => {}
            }
        }
        panic!("fixed drawing outer frame")
    }

    fn shape_placeholder_rect(nodes: &[SceneNode]) -> Option<Rect> {
        nodes.iter().find_map(|node| match node {
            SceneNode::Rect(RectNode {
                rect,
                fill: Some(color),
                ..
            }) if *color == Rgb::new(221, 235, 247) => Some(*rect),
            SceneNode::ClipGroup(group) => shape_placeholder_rect(&group.nodes),
            _ => None,
        })
    }

    fn glyph_run<'a>(scene: &'a Scene, text: &str) -> &'a GlyphRunNode {
        scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::GlyphRun(run) if run.text == text => Some(run),
                _ => None,
            })
            .expect("outlined text node")
    }

    fn path_x_span(run: &GlyphRunNode) -> i64 {
        let mut minimum = i64::MAX;
        let mut maximum = i64::MIN;
        let mut include = |value: Fixed| {
            minimum = minimum.min(value.raw());
            maximum = maximum.max(value.raw());
        };
        for command in &run.commands {
            match *command {
                PathCommand::MoveTo { x, .. } | PathCommand::LineTo { x, .. } => include(x),
                PathCommand::QuadraticTo { control_x, x, .. } => {
                    include(control_x);
                    include(x);
                }
                PathCommand::CubicTo {
                    control1_x,
                    control2_x,
                    x,
                    ..
                } => {
                    include(control1_x);
                    include(control2_x);
                    include(x);
                }
                PathCommand::Close => {}
            }
        }
        maximum - minimum
    }
    #[test]
    fn a_cell_without_vertical_alignment_sits_on_the_row_bottom() {
        // ECMA-376 Part 1 section 18.8.1 defaults `vertical` to `bottom`, and
        // both Excel and Calc render it that way. Centring instead lifts every
        // unaligned cell off the baseline Calc puts it on. Calc then applies
        // the ordinary 20-twip bottom ATTR_MARGIN before positioning the block.
        let rect = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(100),
            height: Fixed::from_pixels(40),
        };
        let block = Fixed::from_pixels(12);
        assert_eq!(
            CALC_CELL_VERTICAL_MARGIN,
            points_to_fixed(1.0).expect("one point"),
            "20 twips are exactly one point"
        );
        let bottom =
            calc_cell_text_layout_bounds(rect, TextBaseline::Bottom, CALC_CELL_VERTICAL_MARGIN)
                .unwrap();
        let top = calc_cell_text_layout_bounds(rect, TextBaseline::Top, CALC_CELL_VERTICAL_MARGIN)
            .unwrap();
        let middle =
            calc_cell_text_layout_bounds(rect, TextBaseline::Middle, CALC_CELL_VERTICAL_MARGIN)
                .unwrap();
        assert_eq!(
            vertical_block_top(bottom, block, TextBaseline::Bottom).unwrap(),
            Fixed::from_pixels(28)
                .checked_sub(CALC_CELL_VERTICAL_MARGIN)
                .unwrap()
        );
        assert_eq!(
            vertical_block_top(top, block, TextBaseline::Top).unwrap(),
            CALC_CELL_VERTICAL_MARGIN
        );
        assert_eq!(
            vertical_block_top(middle, block, TextBaseline::Middle).unwrap(),
            Fixed::from_pixels(14)
        );
        assert_eq!(
            middle, rect,
            "equal top and bottom margins cancel at center"
        );
    }

    #[test]
    fn calc_vertical_cell_margin_is_shared_by_scene_text_and_outlined_glyphs() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("vertical-margins");
        for row in 0..4 {
            sheet.set_row_height(row, 30.0);
        }
        sheet.write(0, 0, "default");
        sheet.write_styled(1, 0, "bottom", &CellStyle::new().valign(VAlign::Bottom));
        sheet.write_styled(2, 0, "top", &CellStyle::new().valign(VAlign::Top));
        sheet.write_styled(3, 0, "middle", &CellStyle::new().valign(VAlign::Middle));
        let range = RenderRange::new(0, 0, 3, 0);
        let approximate = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(range),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let text_node = |text: &str| {
            approximate
                .scene
                .nodes
                .iter()
                .find_map(|node| match node {
                    SceneNode::Text(node) if node.text == text => Some(node),
                    _ => None,
                })
                .expect("cell text node")
        };
        let default = text_node("default");
        let bottom = text_node("bottom");
        let top = text_node("top");
        let middle = text_node("middle");
        assert_eq!(default.style.baseline, TextBaseline::Bottom);
        assert_eq!(bottom.style.baseline, TextBaseline::Bottom);
        assert_eq!(top.style.baseline, TextBaseline::Top);
        assert_eq!(middle.style.baseline, TextBaseline::Middle);
        for node in [default, bottom] {
            assert_eq!(node.bounds.height, node.clip_bounds.height);
            assert_eq!(
                node.bounds.y.checked_add(CALC_CELL_VERTICAL_MARGIN),
                Some(node.clip_bounds.y),
                "default and explicit bottom alignment share the exact bottom inset"
            );
        }
        assert_eq!(top.bounds.height, top.clip_bounds.height);
        assert_eq!(
            top.bounds.y,
            top.clip_bounds
                .y
                .checked_add(CALC_CELL_VERTICAL_MARGIN)
                .unwrap()
        );
        assert_eq!(middle.bounds, middle.clip_bounds);

        let pack = synthetic_test_pack();
        let outlined = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(range),
                gridlines: false,
                default_font_family: pack.default_family().to_string(),
                font_pack: Some(pack),
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let default = glyph_run(&outlined.scene, "default");
        let bottom = glyph_run(&outlined.scene, "bottom");
        let top = glyph_run(&outlined.scene, "top");
        let relative_baseline =
            |run: &GlyphRunNode| run.cluster_metrics[0].baseline_y.raw() - run.clip_bounds.y.raw();
        assert_eq!(relative_baseline(default), relative_baseline(bottom));
        assert_eq!(
            top.cluster_metrics[0].baseline_y.raw() - top.cluster_metrics[0].ascent.raw(),
            top.clip_bounds.y.raw() + CALC_CELL_VERTICAL_MARGIN.raw(),
            "outlined top text starts exactly one point inside the original clip"
        );
    }

    #[test]
    fn imported_biff_cells_use_their_two_point_default_margin() {
        let biff = imported_biff8_default_row(false, ".");
        assert_eq!(
            calc_cell_vertical_margin(&biff.sheets[0]),
            CALC_BIFF_CELL_VERTICAL_MARGIN
        );

        let mut authored = Workbook::new();
        authored.add_sheet("authored");
        assert_eq!(
            calc_cell_vertical_margin(&authored.sheets[0]),
            CALC_CELL_VERTICAL_MARGIN
        );
    }

    #[test]
    fn calc_print_text_uses_page_vertical_clip_for_automatic_rows() {
        let automatic = imported_biff8_default_row(false, ".");
        let manual = imported_biff8_default_row(true, ".");
        let options = outlined_options(RenderRange::new(0, 0, 1, 0));
        let build = |workbook: &Workbook| {
            build_sheet_scene_for_print(&workbook.sheets[0], 0, &options).unwrap()
        };

        let automatic = build(&automatic);
        let automatic_run = glyph_run(&automatic.scene, ".");
        assert_eq!(automatic_run.clip_bounds.y, Fixed::ZERO);
        assert_eq!(automatic_run.clip_bounds.height, automatic.scene.height);

        let manual = build(&manual);
        let manual_run = glyph_run(&manual.scene, ".");
        assert_eq!(manual_run.clip_bounds.y, Fixed::ZERO);
        assert!(manual_run.clip_bounds.height < manual.scene.height);
    }

    #[test]
    fn calc_vertical_cell_margin_keeps_too_short_row_clips_bounded() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("short-margin");
        sheet.set_row_height(0, 0.5);
        sheet.write_styled(0, 0, "top", &CellStyle::new().valign(VAlign::Top));
        sheet.write_styled(0, 1, "bottom", &CellStyle::new().valign(VAlign::Bottom));
        sheet.write_styled(0, 2, "middle", &CellStyle::new().valign(VAlign::Middle));
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 2)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let text_node = |text: &str| {
            build
                .scene
                .nodes
                .iter()
                .find_map(|node| match node {
                    SceneNode::Text(node) if node.text == text => Some(node),
                    _ => None,
                })
                .expect("short-row text node")
        };
        let top = text_node("top");
        let bottom = text_node("bottom");
        let middle = text_node("middle");
        assert!(top.clip_bounds.height < CALC_CELL_VERTICAL_MARGIN);
        assert_eq!(top.bounds.height, top.clip_bounds.height);
        assert_eq!(bottom.bounds.height, bottom.clip_bounds.height);
        assert_eq!(middle.bounds, middle.clip_bounds);
        assert!(
            top.bounds.y
                > top
                    .clip_bounds
                    .y
                    .checked_add(top.clip_bounds.height)
                    .unwrap(),
            "the exact top inset may fall beyond a sub-point row, while clipping stays on the row"
        );
        assert!(
            bottom
                .bounds
                .y
                .checked_add(bottom.bounds.height)
                .unwrap()
                < bottom.clip_bounds.y,
            "the exact bottom inset may fall above a sub-point row, while clipping stays on the row"
        );
    }

    #[test]
    fn render_options_layer_verified_packs_and_report_every_selected_face_hash() {
        let caller = synthetic_test_pack();
        let fallback = synthetic_test_pack();
        let expected_stack = caller.with_fallback(&fallback).unwrap();
        let expected_source_pack = caller.pack_sha256().to_string();
        let expected_face_sha = caller.face_identities().next().unwrap().sha256.to_string();
        let mut workbook = Workbook::new();
        workbook.add_sheet("fonts").write_styled(
            0,
            0,
            "caller alias",
            &CellStyle::new().font_name("Legacy Sans"),
        );
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            gridlines: false,
            default_font_family: "Wide Sans".to_string(),
            font_pack: Some(caller),
            ..RenderOptions::default()
        }
        .with_fallback_font_pack(&fallback)
        .unwrap();
        let output = render_sheet_svg(&workbook, 0, &options).unwrap();
        assert_eq!(output.report.schema_version, 2);
        assert_eq!(
            output.report.font_pack_sha256.as_deref(),
            Some(expected_stack.pack_sha256())
        );
        assert_eq!(output.report.font_faces.len(), 1);
        let selected = &output.report.font_faces[0];
        assert_eq!(selected.source_pack_sha256, expected_source_pack);
        assert_eq!(selected.face_sha256, expected_face_sha);
        assert_eq!(selected.family, "Wide Sans");
        assert_eq!(selected.weight, 400);
        assert!(!selected.italic);
        assert!(selected.substituted);
        let json = output.report.to_json();
        assert!(json.contains("\"font_pack_sha256\":"));
        assert!(json.contains(&expected_face_sha));
        assert!(json.contains("\"substituted\":true"));
    }

    fn test_rgba_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut output, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(rgba).unwrap();
        }
        output
    }

    #[test]
    fn multilingual_layout_is_outlined_wrapped_shrunk_and_deterministic() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("typography");
        sheet.set_col_width(0, 8.0);
        sheet.set_row_height(3, 54.0);
        let base = CellStyle::new().font_name("Wide Sans").size(11);
        sheet.write_styled(0, 0, "Latin 123", &base);
        sheet.write_styled(1, 0, "한글 日本 中文", &base);
        sheet.write_styled(2, 0, "العربية עברית 123", &base);
        let wrapped = "한글中文日本 wrapped words";
        sheet.write_styled(3, 0, wrapped, &base.clone().wrap());
        let shrunk = "shrink-to-fit-long-text";
        let plain = "plain-unshrunk-long-text";
        sheet.write_styled(4, 0, shrunk, &base.clone().shrink_to_fit());
        sheet.write_styled(5, 0, plain, &base);
        sheet.write_styled(
            6,
            0,
            "decorated",
            &base
                .clone()
                .italic()
                .underline()
                .strikethrough()
                .font_script(FormatScript::Superscript),
        );
        sheet.write_url_with_text_and_format(
            7,
            0,
            "https://example.com/?a=1&b=2",
            "linked",
            &Format::new().font_name("Wide Sans").size(11),
        );

        let options = outlined_options(RenderRange::new(0, 0, 7, 3));
        let first = render_sheet_svg(&workbook, 0, &options).unwrap();
        let second = render_sheet_svg(&workbook, 0, &options).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first
                .scene
                .nodes
                .iter()
                .filter(|node| matches!(node, SceneNode::GlyphRun(_)))
                .count(),
            8
        );
        assert!(!first
            .scene
            .nodes
            .iter()
            .any(|node| matches!(node, SceneNode::Text(_))));
        assert!(first
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::FontFamilySubstituted));
        assert!(first.svg.contains("<g role=\"text\""));
        assert!(first.svg.contains("<path d=\""));
        assert!(!first.svg.contains("<text "));
        assert!(!first.svg.contains("font-family="));
        assert!(first
            .svg
            .contains("href=\"https://example.com/?a=1&amp;b=2\""));

        let wrapped_run = glyph_run(&first.scene, wrapped);
        let baselines = wrapped_run
            .commands
            .iter()
            .filter_map(|command| match command {
                PathCommand::MoveTo { y, .. } => Some(y.raw()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert!(
            baselines.len() >= 2,
            "wrapped text must occupy multiple lines"
        );
        assert!(
            path_x_span(glyph_run(&first.scene, shrunk))
                < path_x_span(glyph_run(&first.scene, plain))
        );
        assert_eq!(glyph_run(&first.scene, "decorated").decorations.len(), 2);
    }

    #[test]
    fn calc_line_height_excludes_gap_while_native_retains_it() {
        let metrics = CombinedLineMetrics {
            ascent: Fixed::from_pixels(8),
            descent: Fixed::from_pixels(-2),
            line_gap: Fixed::from_pixels(3),
            calc_ascent_pixels: 8,
            calc_descent_pixels: -2,
            calc_portion_height_mm100: 265,
        };
        assert_eq!(
            line_height_from_metrics(metrics, CellLineLayoutPolicy::Native).unwrap(),
            Fixed::from_pixels(13)
        );
        assert_eq!(
            line_height_from_metrics(metrics, CellLineLayoutPolicy::CalcEditEngine).unwrap(),
            Fixed::from_pixels(10)
        );
    }

    #[test]
    fn calc_automatic_height_replays_virtual_device_quantization() {
        let locked_metrics = CombinedLineMetrics {
            ascent: Fixed::from_raw(17_422),
            descent: Fixed::from_raw(-4_325),
            line_gap: Fixed::ZERO,
            calc_ascent_pixels: 17,
            calc_descent_pixels: -4,
            calc_portion_height_mm100: 556,
        };
        assert_eq!(calc_line_height_mm100(locked_metrics).unwrap(), 556);
        for (lines, expected_raw) in [(1_u64, 23_415_i64), (21, 451_311), (24, 515_550)] {
            assert_eq!(
                calc_automatic_height_from_engine_mm100(556 * lines).unwrap(),
                Fixed::from_raw(expected_raw),
                "{lines} lines"
            );
        }

        let ctl_metrics = CombinedLineMetrics {
            ascent: Fixed::from_pixels(16),
            descent: Fixed::from_pixels(-4),
            line_gap: Fixed::ZERO,
            calc_ascent_pixels: 16,
            calc_descent_pixels: -4,
            calc_portion_height_mm100: 529,
        };
        assert_eq!(calc_line_height_mm100(ctl_metrics).unwrap(), 529);
        assert_eq!(
            calc_automatic_height_from_engine_mm100(529).unwrap(),
            Fixed::from_raw(22_391),
            "Calc's 20px CTL line plus two padding pixels is 328 twips"
        );
    }

    #[test]
    fn pinned_calc_ctl_base_face_produces_the_verified_mixed_rtl_row_height() {
        let Some(manifest) = std::env::var_os("RXLS_TEST_FONT_PACK_MANIFEST") else {
            return;
        };
        let pack = FontPack::load_manifest(manifest).expect("load pinned render font pack");
        for bold in [false, true] {
            for (requested, selected, ascent, descent, mm100, row_raw) in [
                ("Noto Sans CJK KR", "Noto Sans Hebrew", 16, -4, 529, 22_391),
                ("Noto Sans Arabic", "Noto Sans Arabic", 21, -11, 847, 34_611),
                ("Arial", "Arimo", 14, -3, 450, 19_319),
            ] {
                let style = ResolvedRunStyle {
                    family: requested.to_string(),
                    size: points_to_fixed(11.0).unwrap(),
                    color: Rgb::BLACK,
                    bold,
                    italic: false,
                    underline: false,
                    strikethrough: false,
                    script: FormatScript::None,
                };
                let requested_resolution = pack.resolve(style.request());
                assert!(requested_resolution.exact_family || requested_resolution.declared_alias);
                assert!(requested_resolution.exact_style);
                let font_id =
                    calc_verified_complex_role_face(&pack, &style, requested_resolution.id)
                        .unwrap()
                        .expect("verified pack must retain Calc's CTL metric role");
                let identity = pack.selected_face_identity(font_id).unwrap();
                assert_eq!(identity.family, selected, "{requested}");
                assert_eq!(identity.weight, if bold { 700 } else { 400 });
                assert_eq!(identity.source_pack_sha256, pack.pack_sha256());
                let metrics = single_face_line_metrics(
                    &pack,
                    font_id,
                    &style,
                    CellLineLayoutPolicy::CalcEditEngine,
                    1,
                    1,
                )
                .unwrap();
                assert_eq!(metrics.calc_ascent_pixels, ascent, "{requested}");
                assert_eq!(metrics.calc_descent_pixels, descent, "{requested}");
                assert_eq!(
                    calc_line_height_mm100(metrics).unwrap(),
                    mm100,
                    "{requested}"
                );
                assert_eq!(
                    calc_automatic_height_from_engine_mm100(mm100).unwrap(),
                    Fixed::from_raw(row_raw),
                    "{requested}"
                );
            }
        }

        for (family, expected_raw) in [
            ("Noto Sans CJK KR", 22_391),
            ("Noto Sans Arabic", 34_611),
            ("Arial", 19_319),
        ] {
            let styles = format!(
                r#"<styleSheet><fonts count="2"><font><b/><sz val="11"/><name val="{family}"/></font><font><b/><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
            );
            let workbook = imported_xlsx(
                &styles,
                r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>مرحبا بالعالم 0009</t></is></c></row></sheetData></worksheet>"#,
            );
            let sheet = &workbook.sheets[0];
            let range = RenderRange::new(0, 0, 0, 0);
            let options = RenderOptions {
                selection: RenderSelection::Range(range),
                gridlines: false,
                default_font_family: family.to_string(),
                font_pack: Some(pack.clone()),
                ..RenderOptions::default()
            };
            let mut snapshot = RenderStyleSnapshot::new(sheet);
            snapshot.capture_range(sheet, range, &options).unwrap();
            let measured = measure_sheet_axes_inner(
                sheet,
                range,
                &snapshot,
                &options,
                None,
                &mut Warnings::default(),
            )
            .unwrap();
            assert_eq!(
                measured.rows[0].size,
                Fixed::from_raw(expected_raw),
                "verified XLSX Arabic-plus-digits row for {family}"
            );
        }
    }

    #[test]
    fn calc_wrap_space_replays_per_column_device_truncation() {
        let imported = imported_xlsx(
            "<styleSheet/>",
            r#"<worksheet><cols><col min="1" max="1" width="24" customWidth="1"/></cols><sheetData/></worksheet>"#,
        );
        let sheet = &imported.sheets[0];
        assert_eq!(calc_ooxml_wrap_column_twips(sheet, 0, 122), Some(2_928));
        assert_eq!(calc_ooxml_wrap_column_twips(sheet, 1, 122), Some(1_037));
        assert_eq!(
            calc_ooxml_wrap_space(sheet, [0], Fixed::from_raw(8_329))
                .unwrap()
                .unwrap()
                .paper_width_mm100,
            5_106
        );
        assert_eq!(
            calc_ooxml_wrap_space(sheet, [0], Fixed::ZERO).unwrap(),
            None
        );
        let half_twip = imported_xlsx(
            "<styleSheet/>",
            r#"<worksheet><cols><col min="1" max="1" width="8.25" customWidth="1"/></cols><sheetData/></worksheet>"#,
        );
        assert_eq!(
            calc_ooxml_wrap_column_twips(&half_twip.sheets[0], 0, 122),
            Some(1_007)
        );
        let unsupported_default = imported_xlsx(
            "<styleSheet/>",
            r#"<worksheet><sheetFormatPr defaultColWidth="8.5"/><sheetData/></worksheet>"#,
        );
        assert_eq!(
            calc_ooxml_wrap_column_twips(&unsupported_default.sheets[0], 0, 122),
            None
        );

        let narrow = calc_wrap_space_from_column_twips([1_037]).unwrap().unwrap();
        let merged = calc_wrap_space_from_column_twips([1_037, 1_037])
            .unwrap()
            .unwrap();
        // Calc imports an explicit OOXML width directly in default-font digit
        // units, without the ECMA screen-width projection used for painting.
        let wide = calc_wrap_space_from_column_twips([2_928]).unwrap().unwrap();
        assert_eq!(narrow.paper_width_mm100, 1_746); // 66 device pixels
        assert_eq!(merged.paper_width_mm100, 3_572); // 135 device pixels
        assert_eq!(wide.paper_width_mm100, 5_106); // 193 device pixels
        assert_eq!(
            CalcWrapSpace::physical_width_mm100(Fixed::from_pixels(191))
                .unwrap()
                .raw(),
            5_054
        );

        let separately_truncated = calc_wrap_space_from_column_twips([1_010, 1_010])
            .unwrap()
            .unwrap();
        let trunc_after_sum = calc_wrap_space_from_column_twips([2_020]).unwrap().unwrap();
        assert_eq!(separately_truncated.paper_width_mm100, 3_466); // 131 pixels
        assert_eq!(trunc_after_sum.paper_width_mm100, 3_493); // 132 pixels

        assert_eq!(calc_wrap_space_from_column_twips([0]).unwrap(), None);
    }

    #[test]
    fn calc_wrap_space_locks_narrow_merged_and_wide_endpoints() {
        const TEXT: &str = concat!(
            "한국어 자동 줄바꿈 English 日本語 中文 0123456789 ",
            "한국어 자동 줄바꿈 English 日本語 中文 0123456789 ",
            "한국어 자동 줄바꿈 English 日本語 中文 0123456789"
        );
        let measure_physical = |range: Range<usize>| {
            let value = TEXT.get(range).ok_or(RenderError::Typography {
                reason: "invalid_line_break_range",
            })?;
            let raw = value.chars().try_fold(0_i64, |total, ch| {
                let advance = match ch {
                    '한' | '국' | '자' | '줄' | '바' => 13_817,
                    '어' | '동' | '꿈' => 13_818,
                    'E' => 7_780,
                    'n' | 'g' => 8_500,
                    'l' | 'i' => 3_500,
                    's' => 7_500,
                    'h' => 11_740,
                    '日' | '本' | '語' | '中' | '文' => 15_019,
                    '0' | '1' | '2' | '7' => 8_335,
                    '3' | '4' | '5' | '6' | '8' | '9' => 8_336,
                    ' ' => 3_364,
                    _ => {
                        return Err(RenderError::Typography {
                            reason: "unexpected_locked_probe_character",
                        })
                    }
                };
                total
                    .checked_add(advance)
                    .ok_or(RenderError::CoordinateOverflow)
            })?;
            CalcWrapSpace::measure_physical_width(Fixed::from_raw(raw), Fixed::from_raw(15_019))
        };

        for (columns, expected) in [
            (
                vec![1_037],
                vec![
                    10, 17, 27, 35, 48, 52, 59, 63, 73, 80, 90, 98, 111, 115, 122, 126, 136, 143,
                    153, 161, 174, 178, 185, 188,
                ],
            ),
            (
                vec![1_037, 1_037],
                vec![27, 52, 73, 98, 115, 136, 161, 178, 188],
            ),
            (vec![2_928], vec![38, 63, 101, 126, 164, 188]),
        ] {
            let space = calc_wrap_space_from_column_twips(columns).unwrap().unwrap();
            let lines = wrap_text_lines(
                TEXT,
                true,
                CellLineLayoutPolicy::CalcEditEngine,
                space.line_width().unwrap(),
                100,
                1_000,
                measure_physical,
            )
            .unwrap();
            assert_eq!(
                lines.iter().map(|line| line.source.end).collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn calc_merge_wrap_space_uses_full_source_span_and_rejects_hidden_anchor_ambiguity() {
        let maximum_digit_width = Fixed::from_raw(8_329);
        let options = RenderOptions::default();
        let visible = imported_xlsx(
            "<styleSheet/>",
            r#"<worksheet><sheetData/><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#,
        );
        assert_eq!(
            calc_ooxml_merge_wrap_space(&visible.sheets[0], 0, 1, maximum_digit_width, &options,)
                .unwrap(),
            calc_wrap_space_from_column_twips([1_037, 1_037]).unwrap()
        );

        let hidden_anchor = imported_xlsx(
            "<styleSheet/>",
            r#"<worksheet><cols><col min="1" max="1" hidden="1"/></cols><sheetData/><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#,
        );
        assert_eq!(
            calc_ooxml_merge_wrap_space(
                &hidden_anchor.sheets[0],
                0,
                1,
                maximum_digit_width,
                &options,
            )
            .unwrap(),
            None
        );

        let hidden_tail = imported_xlsx(
            "<styleSheet/>",
            r#"<worksheet><cols><col min="2" max="2" hidden="1"/></cols><sheetData/><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#,
        );
        assert_eq!(
            calc_ooxml_merge_wrap_space(
                &hidden_tail.sheets[0],
                0,
                1,
                maximum_digit_width,
                &options,
            )
            .unwrap(),
            calc_wrap_space_from_column_twips([1_037]).unwrap()
        );
        let include_hidden = RenderOptions {
            include_hidden: true,
            ..RenderOptions::default()
        };
        assert_eq!(
            calc_ooxml_merge_wrap_space(
                &hidden_tail.sheets[0],
                0,
                1,
                maximum_digit_width,
                &include_hidden,
            )
            .unwrap(),
            None
        );

        assert_eq!(calc_wrap_space_from_column_twips([u64::MAX]).unwrap(), None);
    }

    #[test]
    fn calc_suppressed_space_does_not_advance_paint_or_decoration() {
        let pack = synthetic_test_pack();
        let mut options = outlined_options(RenderRange::new(0, 0, 0, 0));
        options.font_pack = Some(pack.clone());
        options.horizontal_padding = Fixed::ZERO;
        let text = "ab cd";
        let style = CellStyle::new()
            .font_name(pack.default_family())
            .size(11)
            .wrap()
            .underline();
        let mut region = Region {
            source: CellCoordinate { row: 0, col: 0 },
            rect: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(100),
                height: Fixed::from_pixels(100),
            },
            is_merged: false,
            line_layout_policy: CellLineLayoutPolicy::CalcEditEngine,
            calc_wrap_space: Some(CalcWrapSpace {
                paper_width_mm100: 1,
            }),
            style: Some(style),
            conditional: ConditionalPaint::default(),
            text: text.to_string(),
            rich_text: None,
            hyperlink: None,
            numeric_default: false,
            text_can_overflow: false,
            ods_fixed_height_row: false,
            print_vertical_overflow: false,
            vertical_margin: CALC_CELL_VERTICAL_MARGIN,
        };
        let base = text_style(&region, &options, false);
        let (styles, spans) = resolve_rich_styles(&region, &base).unwrap();
        let direction = text_base_direction(text, false);
        let full_atom = shape_styled_range(
            &pack,
            text,
            0..3,
            &spans,
            &styles,
            direction,
            true,
            &options,
        )
        .unwrap();
        let prefix = shape_styled_range(
            &pack,
            text,
            0..2,
            &spans,
            &styles,
            direction,
            true,
            &options,
        )
        .unwrap();
        let full_width = styled_shaped_width(&pack, &full_atom, &styles, 1, 1).unwrap();
        let prefix_width = styled_shaped_width(&pack, &prefix, &styles, 1, 1).unwrap();
        assert!(full_width > prefix_width);
        region.rect.width = full_width;
        region.calc_wrap_space = Some(CalcWrapSpace {
            paper_width_mm100: u64::try_from(
                CalcWrapSpace::measure_physical_width(full_width, points_to_fixed(11.0).unwrap())
                    .unwrap()
                    .raw(),
            )
            .unwrap(),
        });

        let mut statistics = TypographyStats::default();
        let prepared = prepare_styled_text(
            &pack,
            &region,
            &base,
            false,
            true,
            &options,
            &mut statistics,
        )
        .unwrap();
        assert_eq!(prepared.lines[0].source, 0..3);
        assert_eq!(prepared.lines[0].width, prefix_width);
        assert!(prepared.lines[0]
            .shaped
            .runs
            .iter()
            .all(|run| run.source.end <= 2));

        let mut statistics = TypographyStats::default();
        let run = build_glyph_run(
            &pack,
            &region,
            region.rect,
            region.rect,
            &base,
            false,
            true,
            &options,
            &mut statistics,
            &mut Warnings::default(),
        )
        .unwrap();
        assert!(run.metadata_is_valid());
        assert!(run.decorations.len() >= 2);
        assert_eq!(
            run.decorations[0].x2.raw() - run.decorations[0].x1.raw(),
            prefix_width.raw()
        );
        let (suppressed_index, suppressed) = run
            .clusters
            .iter()
            .enumerate()
            .find(|(_, cluster)| cluster.source_start == 2 && cluster.source_end == 3)
            .expect("zero-advance trailing-space cluster");
        assert_eq!(suppressed.command_start, suppressed.command_end);
        assert_eq!(run.cluster_metrics[suppressed_index].advance_x, Fixed::ZERO);
        assert!(run
            .clusters
            .iter()
            .filter(|cluster| cluster.source_start < 2)
            .all(|cluster| cluster.source_end <= 2));
    }

    #[test]
    fn rich_text_styles_clusters_backends_and_auto_height_are_exact() {
        let mut workbook = Workbook::new();
        let wrapped_text = "Latin 한글 אב a\u{301}";
        let transformed_text = "shrink 한글 אב";
        {
            let sheet = workbook.add_sheet("rich-typography");
            sheet.set_col_width(0, 7.0);
            sheet.write_rich_styled(
                0,
                0,
                [
                    rxls::TextRun::new("Latin ", rxls::Font::new()),
                    rxls::TextRun::new(
                        "한글 ",
                        rxls::Font::new()
                            .with_size(24)
                            .with_color([200, 10, 20])
                            .bold()
                            .underline(),
                    ),
                    rxls::TextRun::new(
                        "אב ",
                        rxls::Font::new()
                            .with_name("Rtl Sans")
                            .with_color([10, 160, 40])
                            .italic()
                            .strikethrough(),
                    ),
                    rxls::TextRun::new(
                        "a\u{301}",
                        rxls::Font::new()
                            .with_color([20, 30, 220])
                            .with_script(FormatScript::Superscript),
                    ),
                ],
                &CellStyle::new()
                    .font_name("Wide Sans")
                    .size(11)
                    .color([1, 2, 3])
                    .wrap()
                    .valign(VAlign::Top),
            );
            sheet.set_row_height(1, 60.0);
            sheet.write_rich_styled(
                1,
                0,
                [
                    rxls::TextRun::new("shrink ", rxls::Font::new().with_color([1, 2, 3])),
                    rxls::TextRun::new(
                        "한글 ",
                        rxls::Font::new().with_size(18).with_color([200, 10, 20]),
                    ),
                    rxls::TextRun::new(
                        "אב",
                        rxls::Font::new()
                            .with_name("Rtl Sans")
                            .with_color([10, 160, 40]),
                    ),
                ],
                &CellStyle::new()
                    .font_name("Wide Sans")
                    .size(11)
                    .shrink_to_fit()
                    .indent(2)
                    .text_rotation(30)
                    .valign(VAlign::Bottom),
            );
        }

        let range = RenderRange::new(0, 0, 1, 0);
        let options = outlined_options(range);
        let output = render_sheet_svg(&workbook, 0, &options).unwrap();
        assert_eq!(output, render_sheet_svg(&workbook, 0, &options).unwrap());
        assert!(!output
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::RichTextFlattened));
        let run = glyph_run(&output.scene, wrapped_text);
        assert!(run.metadata_is_valid());
        assert!(run.clusters.len() <= wrapped_text.chars().count());
        assert_eq!(run.paints.first().unwrap().command_start, 0);
        assert_eq!(
            run.paints.last().unwrap().command_end,
            run.commands.len() as u64
        );
        for color in [
            Rgb::new(1, 2, 3),
            Rgb::new(200, 10, 20),
            Rgb::new(10, 160, 40),
            Rgb::new(20, 30, 220),
        ] {
            assert!(run.paints.iter().any(|paint| paint.color == color));
        }
        assert!(run
            .clusters
            .windows(2)
            .any(|pair| pair[1].source_start < pair[0].source_start));
        assert!(run.clusters.iter().any(|cluster| {
            &wrapped_text[cluster.source_start as usize..cluster.source_end as usize] == "a\u{301}"
        }));
        assert!(run
            .decorations
            .iter()
            .any(|line| line.color == Rgb::new(200, 10, 20)));
        assert!(run
            .decorations
            .iter()
            .any(|line| line.color == Rgb::new(10, 160, 40)));
        assert!(output.svg.contains("fill=\"#C80A14\""));
        assert!(output.svg.contains("fill=\"#0AA028\""));
        assert!(output.svg.contains("fill=\"#141EDC\""));

        let transformed = glyph_run(&output.scene, transformed_text);
        assert_eq!(transformed.rotation_degrees, 30);
        assert!(transformed.metadata_is_valid());
        assert!(path_x_span(transformed) <= transformed.clip_bounds.width.raw());

        let (rows, _) = measure_sheet_axes(&workbook.sheets[0], range, &options).unwrap();
        assert!(rows[0].size > options.default_row_height);
        assert_eq!(
            output.scene.height,
            sum_fixed(rows.iter().map(|slot| slot.size)).unwrap()
        );

        let document = build_print_document(
            &workbook,
            0,
            &PrintOptions {
                render: options.clone(),
                single_page_sheets: true,
                ..PrintOptions::default()
            },
        )
        .unwrap();
        let pdf = render_print_document_pdf(&document).unwrap();
        assert_eq!(pdf, render_print_document_pdf(&document).unwrap());
        let png = render_print_document_png_pages(&document, 96).unwrap();
        assert_eq!(png, render_print_document_png_pages(&document, 96).unwrap());
        assert_eq!(png.len(), document.pages.len());
        let pdf_source = String::from_utf8_lossy(&pdf);
        assert!(pdf_source.contains("/Subtype /Type3"));
        assert!(!pdf_source.contains("/Helvetica"));
        assert!(pdf_source.contains("0.784314 0.039216 0.078431 rg"));

        // The same document rendered with the pack in hand embeds real font
        // programs instead of outlining every glyph.
        let pack = options
            .font_pack
            .as_ref()
            .expect("test renders with a pack");
        let embedded = render_print_document_pdf_with_fonts(&document, pack).unwrap();
        assert_eq!(
            embedded,
            render_print_document_pdf_with_fonts(&document, pack).unwrap(),
            "embedded output must be byte-deterministic"
        );
        let embedded_source = String::from_utf8_lossy(&embedded);
        assert!(embedded_source.contains("/Subtype /Type0"));
        assert!(embedded_source.contains("/Encoding /Identity-H"));
        assert!(embedded_source.contains("/Subtype /CIDFontType2"));
        assert!(embedded_source.contains("/FontFile2"));
        assert!(
            embedded.len() < pdf.len() + 128,
            "embedding must remain within bounded metadata overhead: {} vs {}",
            embedded.len(),
            pdf.len()
        );

        if std::process::Command::new("pdftotext")
            .arg("-v")
            .output()
            .is_ok()
        {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory =
                std::env::temp_dir().join(format!("rxls-rich-pdf-{}-{nonce}", std::process::id()));
            std::fs::create_dir(&directory).unwrap();
            let pdf_path = directory.join("rich.pdf");
            let text_path = directory.join("rich.txt");
            std::fs::write(&pdf_path, &pdf).unwrap();
            let status = std::process::Command::new("pdftotext")
                .arg(&pdf_path)
                .arg(&text_path)
                .status()
                .unwrap();
            assert!(status.success());
            let extracted = std::fs::read_to_string(text_path).unwrap();
            for fragment in ["Latin", "한글", "a\u{301}"] {
                assert!(extracted.contains(fragment), "{extracted:?}");
            }
            // Poppler preserves the logical RTL source order inside its
            // directional controls without inventing a leading gap.
            assert!(extracted.contains("\u{202b}אב\u{202c}"), "{extracted:?}");
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn ligature_cluster_metadata_spans_all_source_bytes_and_hard_limits() {
        let pack = synthetic_test_pack();
        let styles = vec![ResolvedRunStyle {
            family: "Wide Sans".to_string(),
            size: Fixed::from_pixels(16),
            color: Rgb::BLACK,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            script: FormatScript::None,
        }];
        // One glyph whose HarfBuzz cluster starts at byte zero models a
        // two-source-character ligature without requiring a host test font.
        let shaped = ShapedText {
            runs: vec![ShapedRun {
                font_id: FontId(0),
                direction: BaseDirection::LeftToRight,
                source: 0..2,
                style_index: 0,
                glyphs: vec![ShapedGlyph {
                    glyph_id: 1,
                    cluster: 0,
                    x_advance: 600,
                    y_advance: 0,
                    x_offset: 0,
                    y_offset: 0,
                }],
            }],
            glyph_count: 1,
            missing_glyphs: 0,
            requested_family_matched: true,
            selected_faces: Vec::new(),
            base_direction: BaseDirection::LeftToRight,
        };
        let options = RenderOptions::default();
        let mut stats = TypographyStats::default();
        let mut commands = Vec::new();
        let mut clusters = Vec::new();
        let mut cluster_metrics = Vec::new();
        let mut paints = Vec::new();
        let mut decorations = Vec::new();
        let mut glyphs = Vec::new();
        let mut font_faces = Vec::new();
        append_styled_shaped_outlines(
            &pack,
            "fi",
            0,
            &shaped,
            Fixed::ZERO,
            Fixed::from_pixels(16),
            &styles,
            1,
            1,
            &options,
            &mut stats,
            &mut commands,
            &mut clusters,
            &mut cluster_metrics,
            &mut paints,
            &mut decorations,
            &mut glyphs,
            &mut font_faces,
        )
        .unwrap();
        assert_eq!(font_faces.len(), 1);
        assert_eq!(font_faces[0].units_per_em, 1_000);
        assert_eq!(glyphs.len(), 1, "the ligature shapes to a single glyph");
        assert_eq!(glyphs[0].face, 0);
        assert_eq!(glyphs[0].origin_y, Fixed::from_pixels(16));
        assert_eq!(glyphs[0].size, Fixed::from_pixels(16));
        assert!(!glyphs[0].synthetic);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].source_start, 0);
        assert_eq!(clusters[0].source_end, 2);
        assert_eq!(clusters[0].command_start, 0);
        assert_eq!(clusters[0].command_end, commands.len() as u64);
        assert_eq!(cluster_metrics.len(), 1);
        assert_eq!(cluster_metrics[0].baseline_y, Fixed::from_pixels(16));
        assert_eq!(cluster_metrics[0].ascent, Fixed::from_raw(13_107));
        assert_eq!(cluster_metrics[0].descent, Fixed::from_raw(-3_277));
        assert_eq!(cluster_metrics[0].advance_x, Fixed::from_raw(9_830));

        let mut limited = options;
        limited.limits.max_path_commands = commands.len() as u64 - 1;
        let error = append_styled_shaped_outlines(
            &pack,
            "fi",
            0,
            &shaped,
            Fixed::ZERO,
            Fixed::from_pixels(16),
            &styles,
            1,
            1,
            &limited,
            &mut TypographyStats::default(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            RenderError::LimitExceeded {
                kind: LimitKind::PathCommands,
                ..
            }
        ));
    }

    #[test]
    fn merged_source_clusters_retain_the_actual_shaped_glyph_cluster_index() {
        let pack = synthetic_test_pack();
        let styles = vec![ResolvedRunStyle {
            family: "Wide Sans".to_string(),
            size: Fixed::from_pixels(16),
            color: Rgb::BLACK,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            script: FormatScript::None,
        }];
        let glyph = ShapedGlyph {
            glyph_id: 1,
            cluster: 0,
            x_advance: 600,
            y_advance: 0,
            x_offset: 0,
            y_offset: 0,
        };
        // Layout normally produces disjoint run sources, but the cluster
        // builder deliberately supports adjacent command ranges for one
        // source cluster. Exercise that supported merge directly so retained
        // shaped-glyph identity cannot point at the next/nonexistent cluster.
        let shaped = ShapedText {
            runs: vec![
                ShapedRun {
                    font_id: FontId(0),
                    direction: BaseDirection::LeftToRight,
                    source: 0..1,
                    style_index: 0,
                    glyphs: vec![glyph],
                },
                ShapedRun {
                    font_id: FontId(0),
                    direction: BaseDirection::LeftToRight,
                    source: 0..1,
                    style_index: 0,
                    glyphs: vec![glyph],
                },
            ],
            glyph_count: 2,
            missing_glyphs: 0,
            requested_family_matched: true,
            selected_faces: Vec::new(),
            base_direction: BaseDirection::LeftToRight,
        };
        let mut stats = TypographyStats::default();
        let mut commands = Vec::new();
        let mut clusters = Vec::new();
        let mut cluster_metrics = Vec::new();
        let mut paints = Vec::new();
        let mut decorations = Vec::new();
        let mut glyphs = Vec::new();
        let mut font_faces = Vec::new();
        append_styled_shaped_outlines(
            &pack,
            "A",
            0,
            &shaped,
            Fixed::ZERO,
            Fixed::from_pixels(16),
            &styles,
            1,
            1,
            &RenderOptions::default(),
            &mut stats,
            &mut commands,
            &mut clusters,
            &mut cluster_metrics,
            &mut paints,
            &mut decorations,
            &mut glyphs,
            &mut font_faces,
        )
        .unwrap();

        assert_eq!(clusters.len(), 1);
        assert_eq!(cluster_metrics.len(), 1);
        assert_eq!(glyphs.len(), 2);
        assert!(glyphs.iter().all(|glyph| glyph.cluster == 0));
        assert_eq!(clusters[0].command_end, commands.len() as u64);
    }

    #[test]
    fn automatic_row_heights_are_sparse_exact_and_shared_with_scene_layout() {
        let mut workbook = Workbook::new();
        {
            let sheet = workbook.add_sheet("auto-heights");
            sheet.set_col_width(0, 2.0);
            sheet.write_styled(0, 0, "한글中文", &CellStyle::new().wrap());
            // The selected axis below only contains column A. Row measurement is a
            // worksheet property, so mandatory breaks in B still affect row 2.
            sheet.write(1, 1, "A\nB\n");
            sheet.write_rich(
                2,
                2,
                [rxls::TextRun::new("large", rxls::Font::new().with_size(24))],
            );
            sheet.set_row_height(3, 12.0);
            sheet.write_styled(3, 0, "한글中文", &CellStyle::new().wrap());
            sheet.hide_row(4);
            sheet.write(4, 0, "hidden\nrow");
        }

        let range = RenderRange::new(0, 0, 4, 0);
        let options = outlined_options(range);
        let sheet = &workbook.sheets[0];
        let (first, columns) = measure_sheet_axes(sheet, range, &options).unwrap();
        let (second, _) = measure_sheet_axes(sheet, range, &options).unwrap();
        assert_eq!(first, second);
        assert_eq!(columns.len(), 1);
        assert_eq!(
            first
                .iter()
                .map(|slot| (slot.index, slot.size.raw()))
                .collect::<Vec<_>>(),
            [
                (0, 74_140), // four shaped CJK lines
                (1, 56_117), // two mandatory breaks plus trailing empty line
                (2, 41_370), // retained 24pt rich-run metrics
                (3, 16_384), // explicit 12pt source height is authoritative
            ]
        );

        let first_scene = build_scene(&workbook, 0, &options).unwrap();
        let second_scene = build_scene(&workbook, 0, &options).unwrap();
        assert_eq!(first_scene, second_scene);
        assert_eq!(
            first_scene.scene.height,
            sum_fixed(first.iter().map(|slot| slot.size)).unwrap()
        );

        let included = RenderOptions {
            include_hidden: true,
            ..options
        };
        let (rows, _) = measure_sheet_axes(sheet, range, &included).unwrap();
        assert_eq!(
            rows.last().map(|slot| (slot.index, slot.size.raw())),
            Some((4, 38_094))
        );
    }

    #[test]
    fn prepared_geometry_replays_nonlocal_wrapped_row_height_in_print_tiles() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("prepared-geometry");
        sheet.set_col_width(0, 100.0);
        sheet.set_col_width(1, 2.0);
        sheet.write(0, 0, "plain");
        sheet.write_styled(0, 1, "한글中文한글中文", &CellStyle::new().wrap());

        let full_range = RenderRange::new(0, 0, 0, 1);
        let full_options = outlined_options(full_range);
        let (prepared_rows, prepared_columns) =
            measure_sheet_axes(sheet, full_range, &full_options).unwrap();
        let prepared_height = prepared_rows[0].size;

        let tile_options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            ..full_options
        };
        let tile_local = build_sheet_scene(sheet, 0, &tile_options).unwrap();
        assert_eq!(
            tile_local.scene.height, prepared_height,
            "automatic row height is sheet-row geometry even outside the tile columns"
        );

        let replayed = build_sheet_scene_with_geometry(
            sheet,
            0,
            &tile_options,
            SheetGeometryOverride::new(&prepared_rows, &prepared_columns),
        )
        .unwrap();
        assert_eq!(replayed.scene.height, prepared_height);
    }

    #[test]
    fn merged_auto_height_is_independent_of_selected_columns() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("merged-row-geometry");
        sheet.set_col_width(0, 8.0);
        for col in 1..=3 {
            sheet.set_col_width(col, 2.0);
        }
        sheet.write_styled(
            0,
            1,
            "merged wrapped text must use the complete B:D width",
            &CellStyle::new().wrap(),
        );
        sheet.merge(0, 1, 0, 3);

        let full_options = outlined_options(RenderRange::new(0, 0, 0, 3));
        let tile_options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            ..full_options.clone()
        };
        let full = build_sheet_scene(&workbook.sheets[0], 0, &full_options).unwrap();
        let tile = build_sheet_scene(&workbook.sheets[0], 0, &tile_options).unwrap();
        assert_eq!(
            tile.scene.height, full.scene.height,
            "worksheet-global row height must use the full merged width outside the selected tile"
        );
    }

    #[test]
    fn merged_auto_height_uses_visible_width_without_materializing_covered_cells() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("merged-height");
        sheet.set_col_width(0, 2.0);
        sheet.set_col_width(1, 2.0);
        sheet.hide_column(1);
        sheet.merge(0, 0, 0, 1);
        sheet.write_styled(0, 0, "한글中文", &CellStyle::new().wrap());
        let range = RenderRange::new(0, 0, 0, 0);

        let hidden = outlined_options(range);
        let (rows, _) = measure_sheet_axes(sheet, range, &hidden).unwrap();
        assert_eq!(rows[0].size.raw(), 74_140);

        let visible = RenderOptions {
            include_hidden: true,
            ..hidden
        };
        let (rows, _) = measure_sheet_axes(sheet, range, &visible).unwrap();
        assert_eq!(rows[0].size.raw(), 38_094);
    }

    #[test]
    fn automatic_height_limits_fail_before_unbounded_line_growth() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("height-limit").write(0, 0, "A\nB");
        let range = RenderRange::new(0, 0, 0, 0);
        let mut options = outlined_options(range);
        options.limits.max_text_lines = 1;
        assert_eq!(
            measure_sheet_axes(&workbook.sheets[0], range, &options),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::TextLines,
                limit: 1,
                actual: 2,
            })
        );
    }

    #[test]
    fn horizontal_overflow_respects_empty_cells_blockers_wrap_and_rtl() {
        let mut ltr = Workbook::new();
        let sheet = ltr.add_sheet("ltr");
        sheet.write(0, 0, "spills across empty cells");
        sheet.write(0, 3, "blocker");
        sheet.write_styled(1, 0, "wrapped", &CellStyle::new().wrap());
        let options = outlined_options(RenderRange::new(0, 0, 1, 3));
        let scene = build_scene(&ltr, 0, &options).unwrap().scene;
        assert_eq!(
            glyph_run(&scene, "spills across empty cells").clip_bounds,
            Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(255),
                height: Fixed::from_pixels(20),
            }
        );
        assert_eq!(
            glyph_run(&scene, "wrapped").clip_bounds.width,
            Fixed::from_pixels(85)
        );

        let mut rtl = Workbook::new();
        let sheet = rtl.add_sheet("rtl");
        sheet.set_right_to_left(true);
        sheet.write(0, 2, "עברית");
        sheet.write(0, 3, "blocker");
        let scene = build_scene(&rtl, 0, &outlined_options(RenderRange::new(0, 0, 0, 3)))
            .unwrap()
            .scene;
        assert_eq!(
            glyph_run(&scene, "עברית").clip_bounds,
            Rect {
                x: Fixed::from_pixels(85),
                y: Fixed::ZERO,
                width: Fixed::from_pixels(85),
                height: Fixed::from_pixels(20),
            }
        );
    }

    #[test]
    fn rtl_axis_measurement_remains_logical_ascending_geometry() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("rtl-axis");
        sheet.set_right_to_left(true);
        sheet.set_col_width(1, 5.0);
        sheet.set_col_width(2, 11.0);
        sheet.set_col_width(3, 19.0);
        sheet.set_col_width(4, 8.0);
        sheet.hide_column(2);

        let range = RenderRange::new(0, 1, 0, 4);
        let (_, columns) = measure_sheet_axes(sheet, range, &RenderOptions::default()).unwrap();
        assert_eq!(
            columns.iter().map(|slot| slot.index).collect::<Vec<_>>(),
            [1, 3, 4]
        );
        assert_eq!(columns[0].offset, Fixed::ZERO);
        for pair in columns.windows(2) {
            assert_eq!(
                pair[1].offset,
                pair[0].offset.checked_add(pair[0].size).unwrap()
            );
        }
    }

    #[test]
    fn verified_font_metrics_drive_ecma_column_width_geometry() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("widths");
        sheet.set_default_col_width(10.0);
        sheet.write(0, 0, "A");
        let range = RenderRange::new(0, 0, 0, 0);

        let approximate = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(range),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let outlined = build_scene(&workbook, 0, &outlined_options(range)).unwrap();
        assert_eq!(approximate.scene.width, Fixed::from_pixels(72));
        assert_eq!(outlined.scene.width, Fixed::from_pixels(82));

        let mut empty = Workbook::new();
        empty.add_sheet("empty-widths").set_default_col_width(10.0);
        let mut empty_options = outlined_options(range);
        empty_options.selection = RenderSelection::Used;
        let empty = build_scene(&empty, 0, &empty_options).unwrap();
        assert_eq!(empty.scene.width, Fixed::from_pixels(82));
        assert_eq!(empty.scene.height, Fixed::from_pixels(1));

        let mut no_width_metadata = Workbook::new();
        no_width_metadata.add_sheet("defaults").write(0, 4, "A");
        let five_columns = RenderRange::new(0, 0, 0, 4);
        let approximate = build_scene(
            &no_width_metadata,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(five_columns),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let outlined = build_scene(&no_width_metadata, 0, &outlined_options(five_columns)).unwrap();
        assert_eq!(approximate.scene.width, Fixed::from_pixels(320));
        assert_eq!(outlined.scene.width, Fixed::from_pixels(425));

        let mut imported_widths = Workbook::new();
        let sheet = imported_widths.add_sheet("calc-import");
        sheet.set_col_width(0, 18.0);
        for col in 1..=4 {
            sheet.set_col_width(col, 14.0);
        }
        sheet.write(0, 4, "A");
        let outlined = build_scene(
            &imported_widths,
            0,
            &outlined_options(RenderRange::new(0, 0, 0, 4)),
        )
        .unwrap();
        assert_eq!(outlined.scene.width, Fixed::from_pixels(602));
    }

    #[test]
    fn implicit_biff_columns_use_calcs_fixed_sixty_four_point_default() {
        let candidates = [
            include_bytes!("../../tests/fixtures/xls/reader-basic.xls").as_slice(),
            include_bytes!("../../tests/fixtures/xls/korean-unicode-biff8.xls").as_slice(),
            include_bytes!("../../tests/fixtures/xls/korean-cp949-biff5.xls").as_slice(),
        ];
        let workbook = candidates
            .into_iter()
            .map(|bytes| Workbook::open(bytes).expect("imported BIFF fixture"))
            .find(|workbook| {
                workbook
                    .sheets
                    .first()
                    .is_some_and(|sheet| sheet.biff_uses_application_default_column_width())
            })
            .expect("at least one BIFF fixture without a sheet-wide width record");
        let sheet = &workbook.sheets[0];
        let first_col = (0_u16..=251)
            .find(|first| {
                (*first..=*first + 4).all(|col| !sheet.column_widths().contains_key(&col))
            })
            .expect("five implicit BIFF columns");
        let range = RenderRange::new(0, first_col, 0, first_col + 4);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            include_hidden: true,
            gridlines: false,
            ..RenderOptions::default()
        };

        let (_, columns) = measure_sheet_axes(sheet, range, &options).unwrap();
        assert_eq!(columns.len(), 5);
        assert!(columns
            .iter()
            .all(|column| column.size == BIFF_APPLICATION_DEFAULT_COLUMN_WIDTH));
        assert_eq!(
            columns.iter().map(|column| column.size.raw()).sum::<i64>(),
            436_905
        );
        assert_eq!(
            build_single_page_sheet_scene(sheet, 0, &options)
                .unwrap()
                .scene
                .width,
            Fixed::from_raw(436_950),
            "SinglePageSheets must convert the cumulative 6,400-twip extent and retain Calc's inclusive rectangle unit"
        );

        let authored = {
            let mut workbook = Workbook::new();
            workbook.add_sheet("authored").write(0, 4, "A");
            workbook
        };
        let (_, authored_columns) =
            measure_sheet_axes(&authored.sheets[0], RenderRange::new(0, 0, 0, 4), &options)
                .unwrap();
        assert!(authored_columns
            .iter()
            .all(|column| column.size == options.default_column_width));
    }

    #[test]
    fn ooxml_defaults_distinguish_absent_defaulted_base_and_explicit_widths() {
        const PINNED_NOTO_SANS_CJK_KR_11_MDW: Fixed = Fixed::from_raw(8_336);
        let imported = |sheet_format: &str| {
            imported_xlsx(
                "<styleSheet/>",
                &format!(
                    r#"<worksheet>{sheet_format}<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#
                ),
            )
        };
        let absent = imported("");
        let defaulted_base = imported(r#"<sheetFormatPr/>"#);
        let explicit_8_5 = imported(r#"<sheetFormatPr defaultColWidth="8.5"/>"#);
        let explicit_8 = imported(r#"<sheetFormatPr defaultColWidth="8"/>"#);
        let base_8 = imported(r#"<sheetFormatPr baseColWidth="8"/>"#);
        let range = RenderRange::new(0, 0, 0, 0);

        assert_eq!(absent.sheets[0].default_column_width(), None);
        assert_eq!(absent.sheets[0].implicit_ooxml_column_width(), Some(None));
        assert!(!absent.sheets[0].ooxml_uses_defaulted_base_column_width());
        assert_eq!(
            defaulted_base.sheets[0].implicit_ooxml_column_width(),
            Some(None)
        );
        assert!(defaulted_base.sheets[0].ooxml_uses_defaulted_base_column_width());
        assert_eq!(
            calc_ooxml_wrap_column_twips(&absent.sheets[0], 0, 122),
            Some(1_037)
        );
        assert_eq!(
            calc_ooxml_wrap_column_twips(&defaulted_base.sheets[0], 0, 122),
            Some(1_051)
        );
        assert_eq!(explicit_8_5.sheets[0].default_column_width(), Some(8.5));
        assert_eq!(explicit_8_5.sheets[0].implicit_ooxml_column_width(), None);
        assert_eq!(base_8.sheets[0].default_column_width(), None);
        assert_eq!(
            base_8.sheets[0].implicit_ooxml_column_width(),
            Some(Some(8.0))
        );
        assert_eq!(
            xlsb_digits_to_fixed(
                OOXML_APPLICATION_DEFAULT_COLUMN_WIDTH_256,
                PINNED_NOTO_SANS_CJK_KR_11_MDW,
                0,
            ),
            Some(Fixed::from_raw(70_793))
        );

        let approximate = build_scene(
            &absent,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(range),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        assert_eq!(approximate.scene.width, Fixed::from_pixels(64));

        let absent_width = build_scene(&absent, 0, &outlined_options(range))
            .unwrap()
            .scene
            .width;
        let explicit_8_5_width = build_scene(&explicit_8_5, 0, &outlined_options(range))
            .unwrap()
            .scene
            .width;
        let explicit_8_width = build_scene(&explicit_8, 0, &outlined_options(range))
            .unwrap()
            .scene
            .width;
        let base_8_width = build_scene(&base_8, 0, &outlined_options(range))
            .unwrap()
            .scene
            .width;
        assert_eq!(absent_width, Fixed::from_raw(76_049));
        assert_eq!(explicit_8_5_width, Fixed::from_pixels(70));
        assert_eq!(explicit_8_width, Fixed::from_pixels(66));
        assert_eq!(base_8_width, Fixed::from_pixels(71));
        assert_eq!(
            base_8_width.checked_sub(explicit_8_width),
            Some(Fixed::from_pixels(5))
        );

        let five_columns = RenderRange::new(0, 0, 0, 4);
        let pack = synthetic_test_pack();
        let exact_options = RenderOptions {
            selection: RenderSelection::Range(five_columns),
            gridlines: false,
            default_font_family: pack.default_family().to_string(),
            default_font_size: Fixed::from_raw(13_893),
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let ordinary_total = |sheet: &Sheet| {
            measure_sheet_axes(sheet, five_columns, &exact_options)
                .unwrap()
                .1
                .iter()
                .map(|column| column.size.raw())
                .sum::<i64>()
        };
        assert_eq!(ordinary_total(&absent.sheets[0]), 353_965);
        assert_eq!(ordinary_total(&defaulted_base.sheets[0]), 358_740);
        assert_eq!(
            build_single_page_sheet_scene(&absent.sheets[0], 0, &exact_options)
                .unwrap()
                .scene
                .width,
            Fixed::from_raw(354_011)
        );
        assert_eq!(
            build_single_page_sheet_scene(&defaulted_base.sheets[0], 0, &exact_options)
                .unwrap()
                .scene
                .width,
            Fixed::from_raw(358_771)
        );

        let fallback_options = RenderOptions {
            selection: RenderSelection::Range(five_columns),
            gridlines: false,
            ..RenderOptions::default()
        };
        for sheet in [&absent.sheets[0], &defaulted_base.sheets[0]] {
            let ordinary = measure_sheet_axes(sheet, five_columns, &fallback_options)
                .unwrap()
                .1
                .iter()
                .map(|column| column.size.raw())
                .sum::<i64>();
            assert_eq!(ordinary, 5 * Fixed::from_pixels(64).raw());
            // Calc still converts the cumulative 4,800-twip position to
            // Map100thMM and applies tools::Rectangle's inclusive endpoint,
            // even when every source track used the renderer fallback.
            assert_eq!(
                build_single_page_sheet_scene(sheet, 0, &fallback_options)
                    .unwrap()
                    .scene
                    .width,
                Fixed::from_raw(327_732)
            );
        }
    }

    #[test]
    fn xlsb_digit_widths_match_calc_twips_for_explicit_defaults_hidden_and_font_mdw() {
        const CARLITO_11_MDW: Fixed = Fixed::from_raw(7_612);
        const CARLITO_EQUIVALENT_SYNTHETIC_SIZE: Fixed = Fixed::from_raw(12_687);
        let from_twips = |twips: i64| {
            Fixed::from_raw(
                (twips * FIXED_UNITS_PER_PIXEL + i64::try_from(TWIPS_PER_CSS_PIXEL / 2).unwrap())
                    / i64::try_from(TWIPS_PER_CSS_PIXEL).unwrap(),
            )
        };
        let ceil_pixels =
            |width: Fixed| (width.raw() + FIXED_UNITS_PER_PIXEL - 1) / FIXED_UNITS_PER_PIXEL;

        assert_eq!(
            xlsb_digits_to_fixed(18 * 256, CARLITO_11_MDW, 0),
            Some(from_twips(1_998))
        );
        assert_eq!(
            xlsb_digits_to_fixed(14 * 256, CARLITO_11_MDW, 0),
            Some(from_twips(1_554))
        );
        assert_eq!(
            xlsb_digits_to_fixed(8 * 256 + 128, CARLITO_11_MDW, 0),
            Some(from_twips(944))
        );
        assert_eq!(
            xlsb_digits_to_fixed(8 * 256, CARLITO_11_MDW, 5),
            Some(from_twips(963))
        );
        assert_eq!(
            xlsb_digits_to_fixed(18 * 256, Fixed::from_pixels(8), 0),
            Some(Fixed::from_pixels(144))
        );

        let explicit =
            imported_width_xlsb(None, &[(0, 0, 18 * 256, false), (1, 4, 14 * 256, false)]);
        assert_eq!(
            explicit.sheets[0].xlsb_column_widths_256().get(&0),
            Some(&(18 * 256))
        );
        assert_eq!(
            explicit.sheets[0].xlsb_column_widths_256().get(&4),
            Some(&(14 * 256))
        );
        assert_eq!(
            explicit.sheets[0].xlsb_default_column_width(),
            Some(XlsbDefaultColumnWidth::ApplicationDefault)
        );

        let range = RenderRange::new(0, 0, 0, 4);
        let pack = synthetic_test_pack();
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: pack.default_family().to_string(),
            default_font_size: CARLITO_EQUIVALENT_SYNTHETIC_SIZE,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let (_, explicit_columns) =
            measure_sheet_axes(&explicit.sheets[0], range, &options).unwrap();
        assert_eq!(
            explicit_columns
                .iter()
                .map(|column| column.size.raw())
                .collect::<Vec<_>>(),
            [136_397, 106_086, 106_086, 106_086, 106_086]
        );
        let explicit_total = explicit_columns
            .iter()
            .map(|column| column.size.raw())
            .sum::<i64>();
        assert_eq!(explicit_total, 560_741);
        assert_eq!(ceil_pixels(Fixed::from_raw(explicit_total)), 548);

        let implicit = imported_width_xlsb(None, &[]);
        let (_, implicit_columns) =
            measure_sheet_axes(&implicit.sheets[0], range, &options).unwrap();
        assert!(implicit_columns
            .iter()
            .all(|column| column.size.raw() == 64_444));
        let implicit_total = implicit_columns
            .iter()
            .map(|column| column.size.raw())
            .sum::<i64>();
        assert_eq!(implicit_total, 322_220);
        assert_eq!(ceil_pixels(Fixed::from_raw(implicit_total)), 315);
        assert_eq!(
            build_single_page_sheet_scene(&implicit.sheets[0], 0, &options)
                .unwrap()
                .scene
                .width,
            Fixed::from_raw(322_275),
            "SinglePageSheets must convert the cumulative XLSB twips and retain Calc's inclusive rectangle unit"
        );

        let numeric_default = imported_width_xlsb(Some((14 * 256, 42)), &[]);
        assert_eq!(
            numeric_default.sheets[0].xlsb_default_column_width(),
            Some(XlsbDefaultColumnWidth::Digits256(14 * 256))
        );
        let (_, default_columns) =
            measure_sheet_axes(&numeric_default.sheets[0], range, &options).unwrap();
        assert!(default_columns
            .iter()
            .all(|column| column.size.raw() == 106_086));
        assert_eq!(
            ceil_pixels(Fixed::from_raw(
                default_columns.iter().map(|column| column.size.raw()).sum()
            )),
            518
        );

        let base_default = imported_width_xlsb(Some((u32::MAX, 8)), &[]);
        assert_eq!(
            base_default.sheets[0].xlsb_default_column_width(),
            Some(XlsbDefaultColumnWidth::BaseCharacters(8))
        );
        let (_, base_columns) =
            measure_sheet_axes(&base_default.sheets[0], range, &options).unwrap();
        assert!(base_columns
            .iter()
            .all(|column| column.size.raw() == 65_741));

        let hidden = imported_width_xlsb(
            None,
            &[
                (0, 0, 18 * 256, false),
                (1, 1, 14 * 256, true),
                (2, 4, 14 * 256, false),
            ],
        );
        let (_, visible_columns) = measure_sheet_axes(&hidden.sheets[0], range, &options).unwrap();
        assert_eq!(
            visible_columns
                .iter()
                .map(|column| column.index)
                .collect::<Vec<_>>(),
            [0, 2, 3, 4]
        );
        assert_eq!(
            ceil_pixels(Fixed::from_raw(
                visible_columns.iter().map(|column| column.size.raw()).sum()
            )),
            444
        );
        let mut include_hidden = options.clone();
        include_hidden.include_hidden = true;
        let (_, all_columns) =
            measure_sheet_axes(&hidden.sheets[0], range, &include_hidden).unwrap();
        assert_eq!(
            all_columns
                .iter()
                .map(|column| column.size.raw())
                .sum::<i64>(),
            560_741
        );

        let mut wider_font = options.clone();
        wider_font.default_font_size = Fixed::from_raw(13_653);
        let (_, wider_columns) =
            measure_sheet_axes(&explicit.sheets[0], range, &wider_font).unwrap();
        assert_eq!(
            wider_columns
                .iter()
                .map(|column| column.size)
                .collect::<Vec<_>>(),
            [
                Fixed::from_pixels(144),
                Fixed::from_pixels(112),
                Fixed::from_pixels(112),
                Fixed::from_pixels(112),
                Fixed::from_pixels(112),
            ]
        );
    }

    #[test]
    fn ooxml_implicit_row_height_is_calc_specific_not_a_global_fallback() {
        let imported = |sheet_format: &str| {
            imported_xlsx(
                "<styleSheet/>",
                &format!(
                    r#"<worksheet>{sheet_format}<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#
                ),
            )
        };
        let implicit = imported("");
        let explicit = imported(r#"<sheetFormatPr defaultRowHeight="15"/>"#);
        let mut overridden = imported("");
        overridden.sheets[0].set_default_row_height(12.0);
        let mut authored = Workbook::new();
        authored.add_sheet("authored").write(0, 0, 1.0);

        assert_eq!(implicit.sheets[0].default_row_height(), None);
        assert!(implicit.sheets[0].has_implicit_ooxml_row_height());
        assert_eq!(explicit.sheets[0].default_row_height(), Some(15.0));
        assert!(!explicit.sheets[0].has_implicit_ooxml_row_height());
        assert_eq!(overridden.sheets[0].default_row_height(), Some(12.0));
        assert!(!overridden.sheets[0].has_implicit_ooxml_row_height());
        assert!(!authored.sheets[0].has_implicit_ooxml_row_height());

        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            gridlines: false,
            default_row_height: Fixed::from_pixels(37),
            ..RenderOptions::default()
        };
        let implicit_rows =
            measure_sheet_axes(&implicit.sheets[0], RenderRange::new(0, 0, 0, 0), &options)
                .unwrap()
                .0;
        let explicit_rows =
            measure_sheet_axes(&explicit.sheets[0], RenderRange::new(0, 0, 0, 0), &options)
                .unwrap()
                .0;
        let overridden_rows = measure_sheet_axes(
            &overridden.sheets[0],
            RenderRange::new(0, 0, 0, 0),
            &options,
        )
        .unwrap()
        .0;
        let authored_rows =
            measure_sheet_axes(&authored.sheets[0], RenderRange::new(0, 0, 0, 0), &options)
                .unwrap()
                .0;

        assert_eq!(implicit_rows[0].size, OOXML_APPLICATION_DEFAULT_ROW_HEIGHT);
        assert_eq!(implicit_rows[0].size.raw(), 19_351);
        assert_eq!(explicit_rows[0].size, Fixed::from_pixels(20));
        assert_eq!(overridden_rows[0].size, Fixed::from_pixels(16));
        assert_eq!(authored_rows[0].size, Fixed::from_pixels(37));
    }

    #[test]
    fn ooxml_implicit_row_defaults_round_only_cumulative_single_page_boundaries() {
        let unverified = |sheet_format: &str| {
            imported_xlsx(
                "<styleSheet/>",
                &format!(r#"<worksheet>{sheet_format}<sheetData/></worksheet>"#),
            )
        };
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let verified_styles = format!(
            r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let verified = |sheet_format: &str| {
            imported_xlsx(
                &verified_styles,
                &format!(r#"<worksheet>{sheet_format}<sheetData/></worksheet>"#),
            )
        };
        let range = RenderRange::new(0, 0, 4, 0);
        let fallback_options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            ..RenderOptions::default()
        };
        let verified_options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };

        for sheet_format in ["", "<sheetFormatPr/>"] {
            let unverified = unverified(sheet_format);
            let unverified_sheet = &unverified.sheets[0];
            let ordinary = measure_sheet_axes(unverified_sheet, range, &fallback_options)
                .unwrap()
                .0
                .iter()
                .map(|row| row.size.raw())
                .sum::<i64>();
            assert_eq!(ordinary, 96_755);
            assert_eq!(
                build_single_page_sheet_scene(unverified_sheet, 0, &fallback_options)
                    .unwrap()
                    .scene
                    .height,
                Fixed::from_raw(96_640)
            );

            let verified = verified(sheet_format);
            let verified_sheet = &verified.sheets[0];
            let ordinary = measure_sheet_axes(verified_sheet, range, &verified_options)
                .unwrap()
                .0
                .iter()
                .map(|row| row.size.raw())
                .sum::<i64>();
            assert_eq!(ordinary, 94_210);
            assert_eq!(
                build_single_page_sheet_scene(verified_sheet, 0, &verified_options)
                    .unwrap()
                    .scene
                    .height,
                Fixed::from_raw(94_240)
            );
        }
    }

    #[test]
    fn verified_wrapped_implicit_xlsx_quantizes_height_and_shares_wrap_with_painting() {
        const TEXT: &str = "aa aa aa aa aa aa";

        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let workbook = |row_attributes: &str| {
            imported_xlsx(
                &styles,
                &format!(
                    r#"<worksheet><cols><col min="1" max="1" width="4" customWidth="1"/></cols><sheetData><row r="1"{row_attributes}><c r="A1" s="1" t="inlineStr"><is><t>{TEXT}</t></is></c></row></sheetData></worksheet>"#
                ),
            )
        };
        let range = RenderRange::new(0, 0, 0, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family.clone(),
            font_pack: Some(pack),
            ..RenderOptions::default()
        };

        let automatic = workbook("");
        let sheet = &automatic.sheets[0];
        let style = sheet.resolved_cell_style(0, 0).expect("wrapped style");
        assert_eq!(sheet.verified_xlsx_cell_font_size_pt(0, 0), Some(11));
        assert_eq!(
            cell_line_layout_policy(
                sheet,
                CellCoordinate { row: 0, col: 0 },
                Some(&style),
                None,
                CalcLineLayoutEvidence {
                    is_plain_text: true,
                    has_adjustable_row: true,
                    wrap_space_available: true,
                },
                &options,
            ),
            CellLineLayoutPolicy::CalcEditEngine
        );
        assert_eq!(
            cell_line_layout_policy(
                sheet,
                CellCoordinate { row: 0, col: 0 },
                Some(&style),
                None,
                CalcLineLayoutEvidence {
                    is_plain_text: true,
                    has_adjustable_row: true,
                    wrap_space_available: false,
                },
                &options,
            ),
            CellLineLayoutPolicy::Native
        );
        let mut indented_style = style.clone();
        indented_style.align.as_mut().unwrap().indent = 1;
        assert_eq!(
            cell_line_layout_policy(
                sheet,
                CellCoordinate { row: 0, col: 0 },
                Some(&indented_style),
                None,
                CalcLineLayoutEvidence {
                    is_plain_text: true,
                    has_adjustable_row: true,
                    wrap_space_available: true,
                },
                &options,
            ),
            CellLineLayoutPolicy::Native
        );

        let (rows, columns) = measure_sheet_axes(sheet, range, &options).unwrap();
        let mut digit_warnings = Warnings::default();
        let mut digit_statistics = TypographyStats::default();
        let maximum_digit_width = maximum_digit_width(
            &RenderStyleSnapshot::new(sheet),
            &options,
            &mut digit_warnings,
            &mut digit_statistics,
        )
        .unwrap();
        let region = Region {
            source: CellCoordinate { row: 0, col: 0 },
            rect: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: columns[0].size,
                height: rows[0].size,
            },
            is_merged: false,
            line_layout_policy: CellLineLayoutPolicy::CalcEditEngine,
            calc_wrap_space: calc_ooxml_wrap_space(sheet, [0], maximum_digit_width).unwrap(),
            style: Some(style.clone()),
            conditional: ConditionalPaint::default(),
            text: TEXT.to_string(),
            rich_text: None,
            hyperlink: None,
            numeric_default: false,
            text_can_overflow: false,
            ods_fixed_height_row: false,
            print_vertical_overflow: false,
            vertical_margin: CALC_CELL_VERTICAL_MARGIN,
        };
        let text_style = text_style(&region, &options, false);
        let mut statistics = TypographyStats::default();
        let prepared = prepare_styled_text(
            options.font_pack.as_ref().unwrap(),
            &region,
            &text_style,
            false,
            true,
            &options,
            &mut statistics,
        )
        .unwrap();
        assert!(prepared.lines.len() >= 2);
        let line_heights = prepared
            .lines
            .iter()
            .map(|line| {
                line_height_from_metrics(line.metrics, CellLineLayoutPolicy::CalcEditEngine)
            })
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let expected_height = calc_automatic_cell_height(&prepared.lines).unwrap();
        assert_eq!(rows[0].size, expected_height);
        assert_eq!(
            prepared.available_width,
            inner_width(region.rect.width, prepared.horizontal_padding).unwrap()
        );

        let automatic_scene = build_scene(&automatic, 0, &options).unwrap();
        let automatic_run = glyph_run(&automatic_scene.scene, TEXT);
        let automatic_baselines = automatic_run
            .cluster_metrics
            .iter()
            .map(|metric| metric.baseline_y.raw())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(automatic_baselines.len(), prepared.lines.len());
        for (window, expected) in automatic_baselines.windows(2).zip(line_heights) {
            assert_eq!(window[1] - window[0], expected.raw());
        }
        let mut painted_partitions = BTreeMap::<i64, (u64, u64)>::new();
        for (cluster, metrics) in automatic_run
            .clusters
            .iter()
            .zip(&automatic_run.cluster_metrics)
        {
            let partition = painted_partitions
                .entry(metrics.baseline_y.raw())
                .or_insert((cluster.source_start, cluster.source_end));
            partition.0 = partition.0.min(cluster.source_start);
            partition.1 = partition.1.max(cluster.source_end);
        }
        assert_eq!(
            painted_partitions.into_values().collect::<Vec<_>>(),
            prepared
                .lines
                .iter()
                .map(|line| {
                    (
                        u64::try_from(line.source.start).unwrap(),
                        u64::try_from(line.source.end).unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        );

        let merged = imported_xlsx(
            &styles,
            &format!(
                r#"<worksheet><cols><col min="1" max="2" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{TEXT}</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#
            ),
        );
        let painted_source_partitions = |selection: RenderRange| {
            let scene = build_scene(
                &merged,
                0,
                &RenderOptions {
                    selection: RenderSelection::Range(selection),
                    ..options.clone()
                },
            )
            .unwrap();
            let mut partitions = BTreeMap::<i64, (u64, u64)>::new();
            for (cluster, metrics) in glyph_run(&scene.scene, TEXT)
                .clusters
                .iter()
                .zip(&glyph_run(&scene.scene, TEXT).cluster_metrics)
            {
                let partition = partitions
                    .entry(metrics.baseline_y.raw())
                    .or_insert((cluster.source_start, cluster.source_end));
                partition.0 = partition.0.min(cluster.source_start);
                partition.1 = partition.1.max(cluster.source_end);
            }
            partitions.into_values().collect::<Vec<_>>()
        };
        assert_eq!(
            painted_source_partitions(RenderRange::new(0, 0, 0, 0)),
            painted_source_partitions(RenderRange::new(0, 0, 0, 1)),
            "selection clipping must not change a merged cell's source paper"
        );

        let explicit = workbook(r#" ht="42" customHeight="1""#);
        let explicit_sheet = &explicit.sheets[0];
        let explicit_style = explicit_sheet
            .resolved_cell_style(0, 0)
            .expect("explicit wrapped style");
        assert_eq!(
            cell_line_layout_policy(
                explicit_sheet,
                CellCoordinate { row: 0, col: 0 },
                Some(&explicit_style),
                None,
                CalcLineLayoutEvidence {
                    is_plain_text: true,
                    has_adjustable_row: false,
                    wrap_space_available: true,
                },
                &options,
            ),
            CellLineLayoutPolicy::Native
        );
        let (explicit_rows, _) = measure_sheet_axes(explicit_sheet, range, &options).unwrap();
        assert_eq!(explicit_rows[0].size, points_to_fixed(42.0).unwrap());
        let explicit_scene = build_scene(&explicit, 0, &options).unwrap();
        let explicit_baselines = glyph_run(&explicit_scene.scene, TEXT)
            .cluster_metrics
            .iter()
            .map(|metric| metric.baseline_y.raw())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let native_step =
            line_height_from_metrics(prepared.lines[0].metrics, CellLineLayoutPolicy::Native)
                .unwrap();
        assert!(explicit_baselines.len() >= 2);
        assert_eq!(
            explicit_baselines[1] - explicit_baselines[0],
            native_step.raw()
        );

        let huge_width = imported_xlsx(
            &styles,
            &format!(
                r#"<worksheet><cols><col min="1" max="1" width="10000000000000000" customWidth="1"/></cols><sheetData><row r="1" ht="42" customHeight="1"><c r="A1" s="1" t="inlineStr"><is><t>{TEXT}</t></is></c></row></sheetData></worksheet>"#
            ),
        );
        let huge_width_scene = build_scene(&huge_width, 0, &options).unwrap();
        assert_eq!(
            huge_width_scene.scene.height,
            points_to_fixed(42.0).unwrap()
        );

        assert_eq!(
            cell_line_layout_policy(
                sheet,
                CellCoordinate { row: 0, col: 0 },
                Some(&style),
                None,
                CalcLineLayoutEvidence {
                    is_plain_text: false,
                    has_adjustable_row: true,
                    wrap_space_available: true,
                },
                &options,
            ),
            CellLineLayoutPolicy::Native,
            "numeric and other non-text cells cannot enter Calc's text wrapper"
        );
        let filtered = imported_xlsx(
            &styles,
            &format!(
                r#"<worksheet><cols><col min="1" max="1" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{TEXT}</t></is></c></row></sheetData><autoFilter ref="A1:A10"/></worksheet>"#
            ),
        );
        let filtered_sheet = &filtered.sheets[0];
        let filtered_style = filtered_sheet
            .resolved_cell_style(0, 0)
            .expect("filtered wrapped style");
        assert_eq!(
            cell_line_layout_policy(
                filtered_sheet,
                CellCoordinate { row: 0, col: 0 },
                Some(&filtered_style),
                None,
                CalcLineLayoutEvidence {
                    is_plain_text: true,
                    has_adjustable_row: true,
                    wrap_space_available: true,
                },
                &options,
            ),
            CellLineLayoutPolicy::Native,
            "Calc reserves the filter-button width in header cells"
        );
        build_scene(&filtered, 0, &options).unwrap();

        let conditional_styles = format!(
            r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf><alignment textRotation="30"/></dxf></dxfs></styleSheet>"#
        );
        let conditional = imported_xlsx(
            &conditional_styles,
            &format!(
                r#"<worksheet><cols><col min="1" max="1" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{TEXT}</t></is></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="expression" dxfId="0" priority="1"><formula>1=1</formula></cfRule></conditionalFormatting></worksheet>"#
            ),
        );
        let conditional_sheet = &conditional.sheets[0];
        assert!(has_conditional_text_layout_overlay(conditional_sheet));
        let conditional_style = conditional_sheet
            .resolved_cell_style(0, 0)
            .expect("conditionally overlaid wrapped style");
        let calc_line_layout_available = verified_implicit_ooxml(conditional_sheet, &options)
            && !has_conditional_text_layout_overlay(conditional_sheet);
        assert_eq!(
            cell_line_layout_policy(
                conditional_sheet,
                CellCoordinate { row: 0, col: 0 },
                Some(&conditional_style),
                None,
                CalcLineLayoutEvidence {
                    is_plain_text: true,
                    has_adjustable_row: true,
                    wrap_space_available: calc_line_layout_available,
                },
                &options,
            ),
            CellLineLayoutPolicy::Native
        );
        build_scene(&conditional, 0, &options).unwrap();
    }

    #[test]
    fn geometry_conditional_outside_selection_keeps_measurement_and_painting_on_native_wrap() {
        const TEXT: &str = "aa aa aa aa aa aa";

        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf><font><sz val="24"/></font></dxf></dxfs></styleSheet>"#
        );
        let workbook = imported_xlsx(
            &styles,
            &format!(
                r#"<worksheet><cols><col min="1" max="1" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{TEXT}</t></is></c><c r="B1"><v>1</v></c></row></sheetData><conditionalFormatting sqref="B1"><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting></worksheet>"#
            ),
        );
        let sheet = &workbook.sheets[0];
        let range = RenderRange::new(0, 0, 0, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        assert!(has_conditional_text_layout_overlay(sheet));
        assert!(!calc_line_layout_available(sheet, &options));

        let (rows, columns) = measure_sheet_axes(sheet, range, &options).unwrap();
        let style = sheet.resolved_cell_style(0, 0).expect("wrapped style");
        let mut digit_warnings = Warnings::default();
        let mut digit_statistics = TypographyStats::default();
        let maximum_digit_width = maximum_digit_width(
            &RenderStyleSnapshot::new(sheet),
            &options,
            &mut digit_warnings,
            &mut digit_statistics,
        )
        .unwrap();
        let calc_wrap_space = calc_ooxml_wrap_space(sheet, [0], maximum_digit_width)
            .unwrap()
            .expect("verified Calc wrap space");
        let native_region = Region {
            source: CellCoordinate { row: 0, col: 0 },
            rect: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: columns[0].size,
                height: rows[0].size,
            },
            is_merged: false,
            line_layout_policy: CellLineLayoutPolicy::Native,
            calc_wrap_space: None,
            style: Some(style),
            conditional: ConditionalPaint::default(),
            text: TEXT.to_string(),
            rich_text: None,
            hyperlink: None,
            numeric_default: false,
            text_can_overflow: false,
            ods_fixed_height_row: false,
            print_vertical_overflow: false,
            vertical_margin: CALC_CELL_VERTICAL_MARGIN,
        };
        let mut native_statistics = TypographyStats::default();
        let native_height = measure_automatic_cell_height(
            options.font_pack.as_ref().unwrap(),
            &native_region,
            false,
            &options,
            &mut native_statistics,
            None,
            None,
        )
        .unwrap();
        let mut calc_region = native_region.clone();
        calc_region.line_layout_policy = CellLineLayoutPolicy::CalcEditEngine;
        calc_region.calc_wrap_space = Some(calc_wrap_space);
        let mut calc_statistics = TypographyStats::default();
        let calc_height = measure_automatic_cell_height(
            options.font_pack.as_ref().unwrap(),
            &calc_region,
            false,
            &options,
            &mut calc_statistics,
            None,
            None,
        )
        .unwrap();
        assert_ne!(
            native_height, calc_height,
            "the fixture must distinguish the native and Calc measurement paths"
        );
        assert_eq!(
            rows[0].size, native_height,
            "automatic-row measurement must use the same conservative native wrapper as painting"
        );

        let native_style = text_style(&native_region, &options, false);
        let mut prepared_statistics = TypographyStats::default();
        let prepared = prepare_styled_text(
            options.font_pack.as_ref().unwrap(),
            &native_region,
            &native_style,
            false,
            true,
            &options,
            &mut prepared_statistics,
        )
        .unwrap();
        let scene = build_scene(&workbook, 0, &options).unwrap();
        let baselines = glyph_run(&scene.scene, TEXT)
            .cluster_metrics
            .iter()
            .map(|metric| metric.baseline_y.raw())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(baselines.len(), prepared.lines.len());
        for (window, line) in baselines.windows(2).zip(&prepared.lines) {
            assert_eq!(
                window[1] - window[0],
                line_height_from_metrics(line.metrics, CellLineLayoutPolicy::Native)
                    .unwrap()
                    .raw()
            );
        }
    }

    #[test]
    fn verified_ooxml_normal_font_drives_calc_implicit_row_twips() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let imported = |family: &str, points: u16, worksheet: &str| {
            imported_xlsx(
                &format!(
                    r#"<styleSheet><fonts count="1"><font><sz val="{points}"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
                ),
                worksheet,
            )
        };
        let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>plain</t></is></c></row></sheetData></worksheet>"#;
        let explicit_default_column_worksheet = r#"<worksheet><sheetFormatPr defaultColWidth="8.5"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>plain</t></is></c></row></sheetData></worksheet>"#;
        let range = RenderRange::new(0, 0, 0, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family.clone(),
            font_pack: Some(pack.clone()),
            ..RenderOptions::default()
        };

        for (points, expected_twips, expected_raw) in [(11, 276_i64, 18_842_i64), (12, 300, 20_480)]
        {
            for source in [worksheet, explicit_default_column_worksheet] {
                let workbook = imported(&family, points, source);
                let sheet = &workbook.sheets[0];
                assert!(sheet.has_implicit_ooxml_row_height());
                assert_eq!(sheet.verified_xlsx_normal_font_size_pt(), Some(points));
                assert_eq!(
                    calc_ooxml_implicit_row_height(sheet, &options),
                    Some(Fixed::from_raw(expected_raw))
                );
                let (rows, _) = measure_sheet_axes(sheet, range, &options).unwrap();
                assert_eq!(rows[0].size, Fixed::from_raw(expected_raw));
                assert_eq!(
                    rows[0].size.raw(),
                    (expected_twips * FIXED_UNITS_PER_PIXEL
                        + i64::try_from(TWIPS_PER_CSS_PIXEL / 2).unwrap())
                        / i64::try_from(TWIPS_PER_CSS_PIXEL).unwrap()
                );
            }
        }

        for source_size in ["0", "-1", "11.5", "409.55", "410", "1e309"] {
            let workbook = imported_xlsx(
                &format!(
                    r#"<styleSheet><fonts count="1"><font><sz val="{source_size}"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
                ),
                worksheet,
            );
            assert_eq!(
                workbook.sheets[0].verified_xlsx_normal_font_size_pt(),
                None,
                "{source_size}"
            );
            assert_eq!(
                calc_ooxml_implicit_row_height(&workbook.sheets[0], &options),
                None,
                "{source_size}"
            );
            assert_eq!(
                fallback_row_height(&workbook.sheets[0], &options),
                OOXML_APPLICATION_DEFAULT_ROW_HEIGHT,
                "{source_size}"
            );
        }

        let mismatched_normal = imported_xlsx(
            &format!(
                r#"<styleSheet><fonts count="2"><font><sz val="12"/><name val="{family}"/></font><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="1"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
            ),
            worksheet,
        );
        assert_eq!(
            mismatched_normal.sheets[0].verified_xlsx_normal_font_size_pt(),
            None
        );
        assert_eq!(
            calc_ooxml_implicit_row_height(&mismatched_normal.sheets[0], &options),
            None
        );

        let unavailable_style = imported_xlsx(
            &format!(
                r#"<styleSheet><fonts count="1"><font><sz val="12"/><name val="{family}"/><b/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
            ),
            worksheet,
        );
        assert_eq!(
            unavailable_style.sheets[0].verified_xlsx_normal_font_size_pt(),
            Some(12)
        );
        assert_eq!(
            verified_ooxml_normal_font_size(&unavailable_style.sheets[0], &options),
            None,
            "an exact family with a substituted style is not verified"
        );

        let substituted = imported("Unavailable Normal", 12, worksheet);
        assert_eq!(
            calc_ooxml_implicit_row_height(&substituted.sheets[0], &options),
            None
        );
        assert_eq!(
            fallback_row_height(&substituted.sheets[0], &options),
            OOXML_APPLICATION_DEFAULT_ROW_HEIGHT
        );

        let mut authored = Workbook::new();
        authored
            .add_sheet("authored")
            .set_default_format(&Format::new().set_font_name(&family).set_font_size(12));
        assert_eq!(
            verified_ooxml_normal_font_size(&authored.sheets[0], &options),
            None,
            "authored and non-OOXML sheets must retain their prior auto-height path"
        );

        let mut xlsb = Workbook::open(include_bytes!(
            "../../tests/fixtures/xlsb/reader-basic.xlsb"
        ))
        .expect("imported XLSB fixture");
        assert!(xlsb.sheets[0].has_implicit_ooxml_row_height());
        xlsb.sheets[0].set_default_col_width(8.5);
        assert_eq!(
            verified_ooxml_normal_font_size(&xlsb.sheets[0], &options),
            None,
            "mutable XLSB width provenance must not masquerade as XLSX"
        );

        let verified = imported(&family, 12, worksheet);
        let mut mutated_default = verified.clone();
        mutated_default.sheets[0]
            .set_default_format(&Format::new().set_font_name(&family).set_font_size(12));
        assert_eq!(
            mutated_default.sheets[0].verified_xlsx_normal_font_size_pt(),
            None
        );
        assert_eq!(
            calc_ooxml_implicit_row_height(&mutated_default.sheets[0], &options),
            None,
            "authoring a new default format invalidates retained source-font evidence"
        );
        let explicit_default_row = imported(
            &family,
            12,
            r#"<worksheet><sheetFormatPr defaultRowHeight="15"/><sheetData/></worksheet>"#,
        );
        assert_eq!(
            calc_ooxml_implicit_row_height(&explicit_default_row.sheets[0], &options),
            None,
            "an explicit default row height must not be recalibrated"
        );
        let without_pack = RenderOptions {
            font_pack: None,
            ..options
        };
        assert_eq!(
            calc_ooxml_implicit_row_height(&verified.sheets[0], &without_pack),
            None
        );
        assert_eq!(
            measure_sheet_axes(&verified.sheets[0], range, &without_pack)
                .unwrap()
                .0[0]
                .size,
            OOXML_APPLICATION_DEFAULT_ROW_HEIGHT
        );
    }

    #[test]
    fn verified_normal_size_suppresses_only_default_plain_single_line_expansion() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="2"><font><sz val="12"/><name val="{family}"/></font><font><sz val="14"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="3"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0"/><xf fontId="0" xfId="0"><alignment wrapText="1"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let worksheet = r#"<worksheet><sheetFormatPr defaultRowHeight="15" customHeight="0"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>plain Normal</t></is></c></row><row r="2"><c r="A2" s="1" t="inlineStr"><is><t>larger plain</t></is></c></row><row r="3"><c r="A3" s="2" t="inlineStr"><is><t>wrapped</t></is></c></row><row r="4" ht="21" customHeight="1"><c r="A4" s="2" t="inlineStr"><is><t>explicit wrapped</t></is></c></row><row r="5" hidden="1"><c r="A5" s="2" t="inlineStr"><is><t>hidden wrapped</t></is></c></row></sheetData></worksheet>"#;
        let workbook = imported_xlsx(&styles, worksheet);
        let workbook_with_explicit_column = imported_xlsx(
            &styles,
            &worksheet.replace(
                r#"defaultRowHeight="15""#,
                r#"defaultRowHeight="15" defaultColWidth="8.5""#,
            ),
        );
        let range = RenderRange::new(0, 0, 4, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let (rows, _) = measure_sheet_axes(&workbook.sheets[0], range, &options).unwrap();
        let (rows_with_explicit_column, _) =
            measure_sheet_axes(&workbook_with_explicit_column.sheets[0], range, &options).unwrap();

        assert_eq!(rows[0].size, Fixed::from_pixels(20));
        assert!(rows[1].size > Fixed::from_pixels(20));
        assert!(rows[2].size > Fixed::from_pixels(20));
        assert_eq!(rows[3].size, Fixed::from_pixels(28));
        assert!(rows.iter().all(|row| row.index != 4));
        assert_eq!(
            rows_with_explicit_column, rows,
            "column defaults must not change automatic row-font identity"
        );

        let included = RenderOptions {
            include_hidden: true,
            ..options
        };
        let (rows, _) = measure_sheet_axes(&workbook.sheets[0], range, &included).unwrap();
        assert!(rows[4].size > Fixed::from_pixels(20));
    }

    #[test]
    fn implicit_xlsx_plain_rows_use_declared_font_height_across_font_facets() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="5"><font><sz val="11"/><name val="{family}"/></font><font><b/><sz val="11"/><name val="{family}"/></font><font><i/><sz val="11"/><name val="{family}"/></font><font><sz val="11"/><name val="RTL Sans"/></font><font><sz val="14"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="5"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0" applyFont="1"/><xf fontId="2" xfId="0" applyFont="1"/><xf fontId="3" xfId="0" applyFont="1"/><xf fontId="4" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>regular automatic row</t></is></c></row><row r="2"><c r="A2" s="1" t="inlineStr"><is><t>bold automatic row</t></is></c></row><row r="3"><c r="A3" s="2" t="inlineStr"><is><t>italic automatic row</t></is></c></row><row r="4"><c r="A4" s="3" t="inlineStr"><is><t>שלום</t></is></c></row><row r="5"><c r="A5" s="4" t="inlineStr"><is><t>large automatic row</t></is></c></row></sheetData></worksheet>"#;
        let workbook = imported_xlsx(&styles, worksheet);
        let range = RenderRange::new(0, 0, 4, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };

        let (rows, _) = measure_sheet_axes(&workbook.sheets[0], range, &options).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.size).collect::<Vec<_>>(),
            [
                Fixed::from_raw(18_842),
                Fixed::from_raw(18_842),
                Fixed::from_raw(18_842),
                Fixed::from_raw(18_842),
                Fixed::from_raw(23_689),
            ]
        );
        assert_eq!(
            calc_ooxml_row_height_from_points(14),
            Some(Fixed::from_raw(23_689)),
            "14pt must use Calc's 347-twip declared-font row height"
        );
    }

    #[test]
    fn implicit_xlsx_declared_height_requires_exact_cell_font_provenance() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let range = RenderRange::new(0, 0, 0, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family.clone(),
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let measure = |font_record: &str, cell_xf: &str| {
            let styles = format!(
                r#"<styleSheet><fonts count="2"><font><sz val="11"/><name val="{family}"/></font><font>{font_record}<name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/>{cell_xf}</cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
            );
            let workbook = imported_xlsx(
                &styles,
                r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>plain automatic row</t></is></c></row></sheetData></worksheet>"#,
            );
            let sheet = &workbook.sheets[0];
            let mut snapshot = RenderStyleSnapshot::new(sheet);
            snapshot.capture_range(sheet, range, &options).unwrap();
            let measured = measure_sheet_axes_inner(
                sheet,
                range,
                &snapshot,
                &options,
                None,
                &mut Warnings::default(),
            )
            .unwrap();
            (
                sheet.verified_xlsx_cell_font_size_pt(0, 0),
                sheet
                    .resolved_cell_style(0, 0)
                    .and_then(|style| style.font)
                    .and_then(|font| font.size_pt),
                measured,
            )
        };

        let (provenance, rounded_points, exact) =
            measure(r#"<sz val="14"/>"#, r#"<xf fontId="1" xfId="0"/>"#);
        assert_eq!(provenance, Some(14));
        assert_eq!(rounded_points, Some(14));
        assert_eq!(exact.rows[0].size, Fixed::from_raw(23_689));
        assert_eq!(exact.typography.shaped_runs, 0);

        for (label, font_record, cell_xf, expected_rounded) in [
            (
                "fractional",
                r#"<sz val="13.5"/>"#,
                r#"<xf fontId="1" xfId="0"/>"#,
                Some(14),
            ),
            (
                "duplicate size",
                r#"<sz val="13.5"/><sz val="14"/>"#,
                r#"<xf fontId="1" xfId="0"/>"#,
                Some(14),
            ),
            (
                "malformed size",
                r#"<sz val="malformed"/>"#,
                r#"<xf fontId="1" xfId="0"/>"#,
                None,
            ),
            (
                "ambiguous cell XF",
                r#"<sz val="14"/>"#,
                r#"<xf fontId="1" fontId="0" xfId="0"/>"#,
                Some(14),
            ),
        ] {
            let (provenance, rounded_points, measured) = measure(font_record, cell_xf);
            assert_eq!(provenance, None, "{label}");
            assert_eq!(rounded_points, expected_rounded, "{label}");
            assert!(
                measured.typography.shaped_runs > 0,
                "{label} must take the bounded shaped-height path"
            );
        }
    }

    #[test]
    fn inherited_fractional_row_font_cannot_reuse_exact_cell_xf_provenance() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="2"><font><sz val="11"/><name val="{family}"/></font><font><sz val="10.5"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let workbook = imported_xlsx(
            &styles,
            r#"<worksheet><sheetData><row r="1" s="1" customFormat="1"><c r="A1" t="inlineStr"><is><t>plain automatic row</t></is></c></row></sheetData></worksheet>"#,
        );
        let sheet = &workbook.sheets[0];
        assert_eq!(
            sheet
                .resolved_cell_style(0, 0)
                .and_then(|style| style.font)
                .and_then(|font| font.size_pt),
            Some(11),
            "the public model deliberately rounds the inherited 10.5pt source"
        );
        assert_eq!(sheet.verified_xlsx_cell_font_size_pt(0, 0), None);

        let range = RenderRange::new(0, 0, 0, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot.capture_range(sheet, range, &options).unwrap();
        let measured = measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &options,
            None,
            &mut Warnings::default(),
        )
        .unwrap();
        assert!(measured.typography.shaped_runs > 0);
    }

    #[test]
    fn later_style_zero_duplicate_clears_direct_font_provenance() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="2"><font><sz val="11"/><name val="{family}"/></font><font><sz val="10.5"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="3"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0"/><xf fontId="0" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let workbook = imported_xlsx(
            &styles,
            r#"<worksheet><sheetData><row r="1" s="1" customFormat="1"><c r="A1" s="2" t="inlineStr"><is><t>earlier direct cell</t></is></c><c r="A1" t="inlineStr"><is><t>effective plain cell</t></is></c></row></sheetData></worksheet>"#,
        );
        let sheet = &workbook.sheets[0];
        assert_eq!(
            sheet.display_cells().next().map(|cell| cell.formatted),
            Some("effective plain cell")
        );
        assert_eq!(
            sheet
                .resolved_cell_style(0, 0)
                .and_then(|style| style.font)
                .and_then(|font| font.size_pt),
            Some(11),
            "the inherited 10.5pt row font collides after public rounding"
        );
        assert_eq!(sheet.verified_xlsx_cell_font_size_pt(0, 0), None);

        let range = RenderRange::new(0, 0, 0, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot.capture_range(sheet, range, &options).unwrap();
        let measured = measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &options,
            None,
            &mut Warnings::default(),
        )
        .unwrap();
        assert!(measured.typography.shaped_runs > 0);
    }

    #[test]
    fn implicit_xlsx_mixed_western_asian_normal_row_uses_primary_calc_metrics() {
        const MIXED_TEXT: &str = "한국어 자동 줄바꿈 English 日本語 中文 0123456789 한국어 자동 줄바꿈 English 日本語 中文 0123456789 한국어 자동 줄바꿈 English 日本語 中文 0123456789";

        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="2"><font><sz val="11"/><name val="{family}"/></font><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let worksheet = format!(
            r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{MIXED_TEXT}</t></is></c></row></sheetData></worksheet>"#
        );
        let workbook = imported_xlsx(&styles, &worksheet);
        let sheet = &workbook.sheets[0];
        let range = RenderRange::new(0, 0, 0, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot.capture_range(sheet, range, &options).unwrap();
        let measured = measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &options,
            None,
            &mut Warnings::default(),
        )
        .unwrap();

        assert!(has_mixed_calc_script_classes(MIXED_TEXT));
        assert!(!has_mixed_calc_script_classes("bold automatic row"));
        assert!(!has_mixed_calc_script_classes("日本語かなカナ"));
        assert!(!has_mixed_calc_script_classes("한국어 中文"));
        assert!(!has_mixed_calc_script_classes("Latin Ελληνικά"));
        assert!(!has_mixed_calc_script_classes("שלום العربية"));
        assert!(has_mixed_calc_script_classes("123 한국어"));
        assert!(has_mixed_calc_script_classes("Latin。"));
        assert!(has_mixed_calc_script_classes("Latin Ａ"));
        assert!(!has_mixed_calc_script_classes(
            " \u{00a0}\u{00b2}\u{00b3}\u{00b9}한국어"
        ));
        assert!(!has_mixed_calc_script_classes(
            "\u{0001}\u{0002}\u{02c7}\u{02ca}\u{02cb}\u{02d9}한국어"
        ));
        assert!(!has_mixed_calc_script_classes("Latin \u{2c80}"));
        assert_eq!(calc_script_class('1'), Some(CalcScriptClass::Western));
        assert_eq!(calc_script_class('１'), Some(CalcScriptClass::Asian));
        assert_eq!(
            calc_script_class('\u{2c80}'),
            Some(CalcScriptClass::Western)
        );
        let semantic_text = "한국어 렌더링 사례 0006";
        let groups = calc_edit_engine_semantic_groups(semantic_text, &[]).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            &semantic_text[groups[0].source_start as usize..groups[0].source_end as usize],
            "한국어 렌더링 사례 "
        );
        assert_eq!(
            &semantic_text[groups[1].source_start as usize..groups[1].source_end as usize],
            "0006"
        );
        assert!(calc_edit_engine_semantic_groups("Latin only 0006", &[])
            .unwrap()
            .is_empty());
        assert_eq!(
            measured.rows[0].size,
            calc_ooxml_row_height_from_points(11).unwrap(),
            "the synthetic primary font has Calc's declared 11pt row height"
        );
        assert_eq!(
            measured.typography.shaped_runs, 1,
            "mixed-script rows must still shape their glyphs before applying primary Calc metrics"
        );
        assert!(
            measured.typography.text_work >= MIXED_TEXT.chars().count() as u64,
            "script calibration and shaping must account for every inspected scalar"
        );
    }

    #[test]
    fn heading_row_mixed_only_across_uniform_cells_keeps_the_pattern_row_height() {
        // Each cell is internally single-script, so Calc keeps every one of
        // them on the pattern font height and the row stays exactly as tall as
        // the same sheet without it. Only a cell that itself mixes script
        // classes (or one re-resolved by a conditional format) leaves the
        // pattern, so a row that is "mixed" merely because separate cells carry
        // separate scripts must not grow.
        //
        // This is a consistency check, not the regression gate: the synthetic
        // pack resolves every script to one face, so shaped and pattern metrics
        // coincide here and the assertion below holds either way. The behaviour
        // is gated for real by the hosted OOXML row-diagnostic ratchet, whose
        // `auto_heading_western_asian`/`auto_heading_western_complex` cohorts
        // measure this against Calc with the pinned multi-face font pack.
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let worksheet = concat!(
            r#"<worksheet><sheetData><row r="1">"#,
            r#"<c r="A1" t="inlineStr"><is><t>Project review</t></is></c>"#,
            r#"<c r="B1" t="inlineStr"><is><t>한국어 검토</t></is></c>"#,
            r#"<c r="C1" t="inlineStr"><is><t>日本語確認</t></is></c>"#,
            r#"<c r="D1" t="inlineStr"><is><t>中文复核</t></is></c>"#,
            r#"</row></sheetData></worksheet>"#
        )
        .to_string();
        let workbook = imported_xlsx(&styles, &worksheet);
        let sheet = &workbook.sheets[0];
        let range = RenderRange::new(0, 0, 0, 3);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot.capture_range(sheet, range, &options).unwrap();
        let measured = measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &options,
            None,
            &mut Warnings::default(),
        )
        .unwrap();

        // Every heading cell is internally uniform even though the row is not.
        assert!(!has_mixed_calc_script_classes("Project review"));
        assert!(!has_mixed_calc_script_classes("한국어 검토"));
        assert!(!has_mixed_calc_script_classes("日本語確認"));
        assert!(!has_mixed_calc_script_classes("中文复核"));
        assert_eq!(
            measured.rows[0].size,
            calc_ooxml_row_height_from_points(11).unwrap(),
            "a row mixed only across internally-uniform cells keeps Calc's pattern row height"
        );
    }

    #[test]
    fn implicit_xlsx_unattested_complex_base_falls_back_without_inflation() {
        const MIXED_TEXT: &str = "العربية 0123456789";

        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="2"><font><sz val="11"/><name val="{family}"/></font><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let worksheet = format!(
            r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{MIXED_TEXT}</t></is></c></row></sheetData></worksheet>"#
        );
        let workbook = imported_xlsx(&styles, &worksheet);
        let sheet = &workbook.sheets[0];
        let range = RenderRange::new(0, 0, 0, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot.capture_range(sheet, range, &options).unwrap();
        let measured = measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &options,
            None,
            &mut Warnings::default(),
        )
        .unwrap();

        assert!(has_mixed_calc_script_classes(MIXED_TEXT));
        assert!(
            measured.typography.shaped_runs > 0,
            "a Western+Complex mix must stay on the glyph-aware height path"
        );
        assert_eq!(
            measured.rows[0].size,
            calc_ooxml_row_height_from_points(11).unwrap(),
            "an unattested synthetic pack must keep the conservative measured fallback"
        );
    }

    #[test]
    fn color_only_conditional_format_preserves_calc_layout_and_sizes_affected_rows() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="3"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyFont="1"/><xf fontId="0" xfId="0" applyFont="1" applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf><fill><patternFill patternType="solid"><fgColor rgb="FFFFC7CE"/><bgColor indexed="64"/></patternFill></fill><font><color rgb="FFFF0000"/></font></dxf></dxfs></styleSheet>"#
        );
        let conditional_worksheet = r#"<worksheet><cols><col min="2" max="2" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1"><v>123</v></c><c r="B1" s="2" t="inlineStr"><is><t>wrapped text remains on Calc line layout</t></is></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting></worksheet>"#;
        let inactive_worksheet = r#"<worksheet><cols><col min="2" max="2" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1"><v>123</v></c><c r="B1" s="2" t="inlineStr"><is><t>wrapped text remains on Calc line layout</t></is></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>1000</formula></cfRule></conditionalFormatting></worksheet>"#;
        let control_worksheet = r#"<worksheet><cols><col min="2" max="2" width="4" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="1"><v>123</v></c><c r="B1" s="2" t="inlineStr"><is><t>wrapped text remains on Calc line layout</t></is></c></row></sheetData></worksheet>"#;
        let conditional = imported_xlsx(&styles, conditional_worksheet);
        let inactive = imported_xlsx(&styles, inactive_worksheet);
        let control = imported_xlsx(&styles, control_worksheet);
        let sheet = &conditional.sheets[0];
        assert!(
            !has_conditional_text_layout_overlay(sheet),
            "font color does not change text geometry"
        );
        let wrapped_style = sheet.resolved_cell_style(0, 1).expect("wrapped cell style");
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 1)),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let candidates = sheet.display_cells().collect::<Vec<_>>();
        let active = active_color_only_conditional_cells(
            sheet,
            &candidates,
            &options,
            &mut Warnings::default(),
        )
        .unwrap();
        assert!(active.requires_individual_metrics(CellCoordinate { row: 0, col: 0 }));
        assert!(!active.requires_individual_metrics(CellCoordinate { row: 0, col: 1 }));
        let inactive_candidates = inactive.sheets[0].display_cells().collect::<Vec<_>>();
        assert!(
            !active_color_only_conditional_cells(
                &inactive.sheets[0],
                &inactive_candidates,
                &options,
                &mut Warnings::default(),
            )
            .unwrap()
            .requires_individual_metrics(CellCoordinate { row: 0, col: 0 }),
            "a false color-only rule must not request individual automatic height"
        );
        assert_eq!(
            cell_line_layout_policy(
                sheet,
                CellCoordinate { row: 0, col: 1 },
                Some(&wrapped_style),
                None,
                CalcLineLayoutEvidence {
                    is_plain_text: true,
                    has_adjustable_row: true,
                    wrap_space_available: true,
                },
                &options,
            ),
            CellLineLayoutPolicy::CalcEditEngine,
            "an unrelated color-only rule must not disable Calc wrapping"
        );

        let measure = |workbook: &Workbook| {
            let sheet = &workbook.sheets[0];
            let mut snapshot = RenderStyleSnapshot::new(sheet);
            snapshot
                .capture_range(sheet, RenderRange::new(0, 0, 0, 1), &options)
                .unwrap();
            measure_sheet_axes_inner(
                sheet,
                RenderRange::new(0, 0, 0, 1),
                &snapshot,
                &options,
                None,
                &mut Warnings::default(),
            )
            .unwrap()
        };
        let conditional_measured = measure(&conditional);
        let inactive_measured = measure(&inactive);
        let control_measured = measure(&control);
        assert!(
            conditional_measured.typography.shaped_runs > control_measured.typography.shaped_runs,
            "the conditionally affected automatic numeric cell must use individual Calc metrics"
        );
        assert_eq!(
            conditional_measured.rows[0].size, control_measured.rows[0].size,
            "color-only paint must not inflate the row in the synthetic primary font"
        );
        assert_eq!(
            inactive_measured.typography.shaped_runs, control_measured.typography.shaped_runs,
            "an inactive color-only rule must preserve the declared-height shortcut"
        );
    }

    #[test]
    fn lossy_conditional_font_cannot_use_the_color_only_exemption() {
        let styles = r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="Test Sans"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf><font><b val="0"/><color rgb="FFFF0000"/></font></dxf></dxfs></styleSheet>"#;
        let reset_only_styles = r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="Test Sans"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf><font><b val="0"/></font></dxf></dxfs></styleSheet>"#;
        let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting></worksheet>"#;
        let workbook = imported_xlsx(styles, worksheet);
        let reset_only_workbook = imported_xlsx(reset_only_styles, worksheet);
        let sheet = &workbook.sheets[0];
        let [metadata] = sheet.conditional_format_metadata() else {
            panic!("one conditional metadata row");
        };
        assert!(!metadata.style_losses.is_empty());
        assert!(
            has_conditional_text_layout_overlay(sheet),
            "an ambiguous font reset must conservatively disable Calc text layout"
        );
        let reset_only_sheet = &reset_only_workbook.sheets[0];
        let [reset_only_metadata] = reset_only_sheet.conditional_format_metadata() else {
            panic!("one reset-only conditional metadata row");
        };
        assert!(reset_only_metadata
            .differential_style
            .as_ref()
            .is_some_and(|style| style.font.is_none()));
        assert!(reset_only_metadata
            .style_losses
            .iter()
            .any(|loss| loss.kind == StyleLossKind::UnsupportedProperty));
        assert!(
            has_conditional_text_layout_overlay(reset_only_sheet),
            "an unretained reset-only font must conservatively disable Calc text layout"
        );
    }

    #[test]
    fn conditional_layout_styles_drive_axes_and_unresolved_text_styles_fail_closed() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let style_sheet = |dxf: &str| {
            format!(
                r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf>{dxf}</dxf></dxfs></styleSheet>"#
            )
        };
        let conditional_sheet = |formula: &str| {
            format!(
                r#"<worksheet><sheetData><row r="1"><c r="A1" s="1"><v>1</v></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>{formula}</formula></cfRule></conditionalFormatting></worksheet>"#
            )
        };
        let control = imported_xlsx(
            &style_sheet("<font><sz val=\"24\"/></font>"),
            r#"<worksheet><sheetData><row r="1"><c r="A1" s="1"><v>1</v></c></row></sheetData></worksheet>"#,
        );
        let active = imported_xlsx(
            &style_sheet("<font><sz val=\"24\"/></font>"),
            &conditional_sheet("0"),
        );
        let inactive = imported_xlsx(
            &style_sheet("<font><sz val=\"24\"/></font>"),
            &conditional_sheet("100"),
        );
        let reset = imported_xlsx(
            &style_sheet("<font><b val=\"0\"/></font>"),
            &conditional_sheet("0"),
        );
        let inactive_reset = imported_xlsx(
            &style_sheet("<font><b val=\"0\"/></font>"),
            &conditional_sheet("100"),
        );
        let unresolved_color = imported_xlsx(
            &style_sheet("<font><color theme=\"99\"/></font>"),
            &conditional_sheet("0"),
        );
        let inactive_unresolved_color = imported_xlsx(
            &style_sheet("<font><color theme=\"99\"/></font>"),
            &conditional_sheet("100"),
        );
        let number_format = imported_xlsx(
            &style_sheet(r#"<numFmt numFmtId="165" formatCode="yyyy-mm-dd hh:mm:ss"/>"#),
            &conditional_sheet("0"),
        );
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let measure = |workbook: &Workbook| {
            let sheet = &workbook.sheets[0];
            let mut snapshot = RenderStyleSnapshot::new(sheet);
            snapshot
                .capture_range(sheet, RenderRange::new(0, 0, 0, 0), &options)
                .unwrap();
            measure_sheet_axes_inner(
                sheet,
                RenderRange::new(0, 0, 0, 0),
                &snapshot,
                &options,
                None,
                &mut Warnings::default(),
            )
        };
        let control = measure(&control).unwrap();
        let active = measure(&active).unwrap();
        let inactive = measure(&inactive).unwrap();
        assert!(
            active.rows[0].size > control.rows[0].size,
            "an active 24pt differential font must grow the automatic row"
        );
        assert_eq!(
            inactive.rows[0].size, control.rows[0].size,
            "an inactive differential font must not alter row geometry"
        );
        assert_eq!(
            measure(&reset).map(|_| ()),
            Err(RenderError::Typography {
                reason: "conditional_text_layout_unresolved",
            })
        );
        assert_eq!(
            measure(&inactive_reset).unwrap().rows[0].size,
            control.rows[0].size,
            "a proven-inactive reset-only rule must not block measurement"
        );
        assert_eq!(
            measure(&unresolved_color).map(|_| ()),
            Err(RenderError::Typography {
                reason: "conditional_text_layout_unresolved",
            })
        );
        assert_eq!(
            measure(&inactive_unresolved_color).unwrap().rows[0].size,
            control.rows[0].size,
            "a proven-inactive unresolved font color must not block measurement"
        );
        assert_eq!(
            measure(&number_format).map(|_| ()),
            Err(RenderError::Typography {
                reason: "conditional_number_format_layout_unresolved",
            })
        );
    }

    #[test]
    fn conditional_evaluation_budget_is_shared_by_axes_and_paint() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="1"><dxf><font><color rgb="FFFF0000"/></font></dxf></dxfs></styleSheet>"#
        );
        let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting></worksheet>"#;
        let workbook = imported_xlsx(&styles, worksheet);
        let mut options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        options.limits.max_conditional_evaluations = 1;
        assert_eq!(
            render_sheet_svg(&workbook, 0, &options).map(|_| ()),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::ConditionalEvaluations,
                limit: 1,
                actual: 2,
            })
        );
        options.limits.max_conditional_evaluations = 2;
        render_sheet_svg(&workbook, 0, &options).unwrap();
    }

    #[test]
    fn prepared_asian_metric_face_ignores_complex_only_fallback_runs() {
        let pack = synthetic_test_pack();
        let options = RenderOptions {
            default_font_family: "Legacy Sans".to_string(),
            font_pack: Some(pack.clone()),
            ..RenderOptions::default()
        };
        let make_region = |text: &str| Region {
            source: CellCoordinate { row: 0, col: 0 },
            rect: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(400),
                height: Fixed::from_pixels(20),
            },
            is_merged: false,
            line_layout_policy: CellLineLayoutPolicy::Native,
            calc_wrap_space: None,
            style: Some(CellStyle::new().font_name("Legacy Sans").size(11)),
            conditional: ConditionalPaint::default(),
            text: text.to_string(),
            rich_text: None,
            hyperlink: None,
            numeric_default: false,
            text_can_overflow: false,
            ods_fixed_height_row: false,
            print_vertical_overflow: false,
            vertical_margin: CALC_CELL_VERTICAL_MARGIN,
        };
        let mut statistics = TypographyStats::default();
        let region = make_region("Latin 한국어 العربية");
        let base = text_style(&region, &options, false);
        let prepared = prepare_styled_text(
            &pack,
            &region,
            &base,
            false,
            true,
            &options,
            &mut statistics,
        )
        .unwrap();
        assert_eq!(
            prepared_asian_face(&prepared, &region.text, &options, &mut statistics).unwrap(),
            PreparedAsianFace::Verified(FontId(0)),
            "the actual Asian run selects Wide Sans while the Arabic run's RTL face is ignored"
        );

        let mut statistics = TypographyStats::default();
        let region = make_region("Latin العربية");
        let base = text_style(&region, &options, false);
        let prepared = prepare_styled_text(
            &pack,
            &region,
            &base,
            false,
            true,
            &options,
            &mut statistics,
        )
        .unwrap();
        assert_eq!(
            prepared_asian_face(&prepared, &region.text, &options, &mut statistics).unwrap(),
            PreparedAsianFace::None
        );
    }

    #[test]
    fn bounded_script_summary_inspects_late_complex_scalars() {
        let mut options = RenderOptions::default();
        options.limits.max_text_runs = 3;
        let mut typography = TypographyStats::default();
        let summary = calc_script_class_summary_bounded("한1ع", &options, &mut typography).unwrap();
        assert!(summary.mixed);
        assert!(summary.has_complex);
        assert!(!calc_edit_engine_uses_only_complex_role("한1ع", summary, &options).unwrap());
        assert_eq!(typography.text_work, 3);

        options.limits.max_text_runs = 2;
        let mut typography = TypographyStats::default();
        assert_eq!(
            calc_script_class_summary_bounded("한1ع", &options, &mut typography).map(|_| ()),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::TextRuns,
                limit: 2,
                actual: 3,
            })
        );
    }

    #[test]
    fn calc_complex_role_metric_source_replays_bounded_logical_bidi_runs() {
        let options = RenderOptions::default();
        let analyze = |text: &str| {
            let summary =
                calc_script_class_summary_bounded(text, &options, &mut TypographyStats::default())
                    .unwrap();
            let edit_engine_uses_only_complex_role =
                calc_edit_engine_uses_only_complex_role(text, summary, &options).unwrap();
            (
                summary,
                CalcCellScriptAnalysis {
                    edit_engine_uses_only_complex_role,
                },
            )
        };
        for text in [
            "مرحبا بالعالم 0009",
            "0009 مرحبا بالعالم",
            "مرحبا!",
            "مرحبا-0009",
            "עברית (123)",
        ] {
            let (summary, analysis) = analyze(text);
            assert!(analysis.edit_engine_uses_only_complex_role, "{text}");
            for source in [
                OoxmlImplicitRowHeight::XlsxApplicationDefault,
                OoxmlImplicitRowHeight::XlsbApplicationDefault,
            ] {
                assert_eq!(
                    calc_automatic_metric_source(
                        Some(source),
                        true,
                        true,
                        Some(&summary),
                        Some(&analysis),
                    ),
                    Some(CalcAutomaticMetricSource::CalcComplexRole),
                    "{source:?}: {text}"
                );
            }
        }

        for text in [
            "مرحبا بالعالم",
            "0009",
            "Latin مرحبا 0009",
            "مرحبا 0009 한국어",
            "हिन्दी 0009",
            "مرحبا abc 0009",
            "مرحبا \u{2066}0009\u{2069}",
            "مرحبا \u{2067}0009\u{2069}",
            "مرحبا \u{2068}0009\u{2069}",
            "مرحبا \u{202a}0009\u{202c}",
            "مرحبا \u{202b}0009\u{202c}",
            "مرحبا \u{202d}0009\u{202c}",
            "مرحبا \u{202e}0009\u{202c}",
        ] {
            let (_, analysis) = analyze(text);
            assert!(
                !analysis.edit_engine_uses_only_complex_role,
                "unsupported mixed form must fail closed: {text}"
            );
        }
        let (qualified, qualified_analysis) = analyze("مرحبا 0009");
        assert_eq!(
            calc_automatic_metric_source(
                Some(OoxmlImplicitRowHeight::XlsxApplicationDefault),
                false,
                true,
                Some(&qualified),
                Some(&qualified_analysis),
            ),
            None,
            "the source-specific metric never bypasses normal eligibility"
        );
        assert_eq!(
            calc_automatic_metric_source(
                Some(OoxmlImplicitRowHeight::XlsbApplicationDefault),
                true,
                false,
                Some(&qualified),
                Some(&qualified_analysis),
            ),
            None,
            "an unattested workbook font never selects the pinned CTL metric"
        );

        let text = "مرحبا!";
        let mut limited = RenderOptions::default();
        limited.limits.max_text_bytes = text.len() as u64 - 1;
        let summary =
            calc_script_class_summary_bounded(text, &limited, &mut TypographyStats::default())
                .unwrap();
        assert_eq!(
            calc_edit_engine_uses_only_complex_role(text, summary, &limited),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::TextBytes,
                limit: text.len() as u64 - 1,
                actual: text.len() as u64,
            })
        );
    }

    #[test]
    fn declared_height_classification_and_off_range_candidates_obey_exact_limits() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let workbook = imported_xlsx(
            &styles,
            r#"<worksheet><sheetData><row r="1"><c r="F1" t="inlineStr"><is><t>Latin</t></is></c></row></sheetData></worksheet>"#,
        );
        let sheet = &workbook.sheets[0];
        let range = RenderRange::new(0, 0, 0, 0);
        let mut options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot.capture_range(sheet, range, &options).unwrap();
        let candidates = sheet.display_cells().collect::<Vec<_>>();

        options.limits.max_cells = 1;
        options.limits.max_text_bytes = 5;
        options.limits.max_text_runs = 5;
        let measured = measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &options,
            Some(&candidates),
            &mut Warnings::default(),
        )
        .unwrap();
        assert_eq!(measured.typography.text_bytes, 5);
        assert_eq!(measured.typography.text_work, 5);
        assert_eq!(measured.typography.shaped_runs, 0);

        let mut limited = options.clone();
        limited.limits.max_text_runs = 4;
        assert_eq!(
            measure_sheet_axes_inner(
                sheet,
                range,
                &snapshot,
                &limited,
                Some(&candidates),
                &mut Warnings::default(),
            )
            .map(|_| ()),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::TextRuns,
                limit: 4,
                actual: 5,
            })
        );

        let mut limited = options.clone();
        limited.limits.max_text_bytes = 4;
        assert_eq!(
            measure_sheet_axes_inner(
                sheet,
                range,
                &snapshot,
                &limited,
                Some(&candidates),
                &mut Warnings::default(),
            )
            .map(|_| ()),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::TextBytes,
                limit: 4,
                actual: 5,
            })
        );

        let mut limited = options;
        limited.limits.max_cells = 0;
        assert_eq!(
            measure_sheet_axes_inner(
                sheet,
                range,
                &snapshot,
                &limited,
                Some(&candidates),
                &mut Warnings::default(),
            )
            .map(|_| ()),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::Cells,
                limit: 0,
                actual: 1,
            })
        );
    }

    #[test]
    fn skipped_default_plain_candidates_are_charged_before_text_scans() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let mut workbook = Workbook::new();
        workbook.add_sheet("bounded").write(0, 5, "bounded");
        let sheet = &workbook.sheets[0];
        let range = RenderRange::new(0, 0, 0, 0);
        let mut options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot.capture_range(sheet, range, &options).unwrap();
        let candidates = sheet.display_cells().collect::<Vec<_>>();

        options.limits.max_cells = 1;
        options.limits.max_text_bytes = 7;
        let measured = measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &options,
            Some(&candidates),
            &mut Warnings::default(),
        )
        .unwrap();
        assert_eq!(measured.typography.text_bytes, 7);
        assert_eq!(measured.typography.text_work, 0);
        assert_eq!(measured.typography.shaped_runs, 0);

        let mut limited = options.clone();
        limited.limits.max_text_bytes = 6;
        assert_eq!(
            measure_sheet_axes_inner(
                sheet,
                range,
                &snapshot,
                &limited,
                Some(&candidates),
                &mut Warnings::default(),
            )
            .map(|_| ()),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::TextBytes,
                limit: 6,
                actual: 7,
            })
        );

        let mut limited = options;
        limited.limits.max_cells = 0;
        assert_eq!(
            measure_sheet_axes_inner(
                sheet,
                range,
                &snapshot,
                &limited,
                Some(&candidates),
                &mut Warnings::default(),
            )
            .map(|_| ()),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::Cells,
                limit: 0,
                actual: 1,
            })
        );
    }

    #[test]
    fn implicit_xlsx_rotation_shapes_but_shrink_uses_declared_auto_height() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="3"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyAlignment="1"><alignment textRotation="30"/></xf><xf fontId="0" xfId="0" applyAlignment="1"><alignment shrinkToFit="1"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>rotated automatic row</t></is></c></row><row r="2"><c r="A2" s="2" t="inlineStr"><is><t>shrunk automatic row</t></is></c></row></sheetData></worksheet>"#;
        let workbook = imported_xlsx(&styles, worksheet);
        let sheet = &workbook.sheets[0];
        let range = RenderRange::new(0, 0, 1, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot.capture_range(sheet, range, &options).unwrap();
        let measured = measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &options,
            None,
            &mut Warnings::default(),
        )
        .unwrap();

        assert!(
            measured.rows[0].size > Fixed::from_raw(18_842),
            "rotation must remain on the shaped height path"
        );
        assert_eq!(
            measured.rows[1].size,
            Fixed::from_raw(18_842),
            "Calc's standard-height path does not exclude shrink-to-fit"
        );
        assert!(measured.typography.shaped_runs >= 1);
    }

    #[test]
    fn implicit_xlsx_superscript_and_subscript_keep_measured_auto_height() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="3"><font><sz val="11"/><name val="{family}"/></font><font><vertAlign val="superscript"/><sz val="11"/><name val="{family}"/></font><font><vertAlign val="subscript"/><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="3"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0" applyFont="1"/><xf fontId="2" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>superscript automatic row</t></is></c></row><row r="2"><c r="A2" s="2" t="inlineStr"><is><t>subscript automatic row</t></is></c></row></sheetData></worksheet>"#;
        let workbook = imported_xlsx(&styles, worksheet);
        let sheet = &workbook.sheets[0];
        assert_eq!(
            sheet
                .resolved_cell_style(0, 0)
                .and_then(|style| style.font)
                .map(|font| font.script),
            Some(FormatScript::Superscript)
        );
        assert_eq!(
            sheet
                .resolved_cell_style(1, 0)
                .and_then(|style| style.font)
                .map(|font| font.script),
            Some(FormatScript::Subscript)
        );

        let range = RenderRange::new(0, 0, 1, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let mut snapshot = RenderStyleSnapshot::new(sheet);
        snapshot.capture_range(sheet, range, &options).unwrap();
        let measured = measure_sheet_axes_inner(
            sheet,
            range,
            &snapshot,
            &options,
            None,
            &mut Warnings::default(),
        )
        .unwrap();

        assert!(
            measured.typography.shaped_runs >= 2,
            "superscript and subscript must not take the declared-font shortcut"
        );
    }

    #[test]
    fn verified_normal_auto_height_skip_requires_the_same_effective_font() {
        let pack = synthetic_test_pack();
        let styles = r#"<styleSheet><fonts count="2"><font><sz val="12"/><name val="Wide Sans"/></font><font><sz val="12"/><name val="RTL Sans"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="1" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#;
        let workbook = |style_index: u8| {
            imported_xlsx(
                styles,
                &format!(
                    r#"<worksheet><sheetFormatPr defaultRowHeight="15"/><sheetData><row r="1"><c r="A1" s="{style_index}" t="inlineStr"><is><t>plain</t></is></c></row></sheetData></worksheet>"#
                ),
            )
        };
        let range = RenderRange::new(0, 0, 0, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: "Wide Sans".to_string(),
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let measure = |workbook: &Workbook| {
            let sheet = &workbook.sheets[0];
            let mut snapshot = RenderStyleSnapshot::new(sheet);
            snapshot.capture_range(sheet, range, &options).unwrap();
            measure_sheet_axes_inner(
                sheet,
                range,
                &snapshot,
                &options,
                None,
                &mut Warnings::default(),
            )
            .unwrap()
        };

        let default_font = measure(&workbook(0));
        let alternate_font = measure(&workbook(1));
        assert!(
            alternate_font.typography.shaped_runs > default_font.typography.shaped_runs,
            "a same-size alternate family must still be measured for automatic height"
        );
        assert_eq!(default_font.rows[0].size, Fixed::from_pixels(20));
        assert!(alternate_font.rows[0].size >= Fixed::from_pixels(20));
    }

    #[test]
    fn verified_ooxml_implicit_rows_keep_sparse_origin_and_explicit_hidden_geometry() {
        let pack = synthetic_test_pack();
        let family = pack.default_family().to_string();
        let styles = format!(
            r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="1"><xf fontId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let worksheet = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="4" ht="21" customHeight="1"/><row r="5" hidden="1"/><row r="8"><c r="A8"><v>8</v></c></row></sheetData></worksheet>"#;
        let workbook = imported_xlsx(&styles, worksheet);
        let sheet = &workbook.sheets[0];
        let range = RenderRange::new(0, 0, 7, 0);
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: family,
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let (rows, _) = measure_sheet_axes(sheet, range, &options).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| (row.index, row.size))
                .collect::<Vec<_>>(),
            [
                (0, Fixed::from_raw(18_842)),
                (1, Fixed::from_raw(18_842)),
                (2, Fixed::from_raw(18_842)),
                (3, Fixed::from_pixels(28)),
                (5, Fixed::from_raw(18_842)),
                (6, Fixed::from_raw(18_842)),
                (7, Fixed::from_raw(18_842)),
            ]
        );
        let mut warnings = Warnings::default();
        assert_eq!(
            sheet_grid_origin(
                sheet,
                RenderRange::new(7, 0, 7, 0),
                Fixed::from_pixels(7),
                &options,
                &mut warnings,
            )
            .unwrap()
            .1,
            Fixed::from_raw(18_842 * 5 + 28 * FIXED_UNITS_PER_PIXEL)
        );
    }

    #[test]
    fn biff_application_default_row_height_is_calc_specific_not_a_global_fallback() {
        let candidates = [
            include_bytes!("../../tests/fixtures/xls/reader-basic.xls").as_slice(),
            include_bytes!("../../tests/fixtures/xls/korean-unicode-biff8.xls").as_slice(),
            include_bytes!("../../tests/fixtures/xls/korean-cp949-biff5.xls").as_slice(),
        ];
        let implicit = candidates
            .into_iter()
            .map(|bytes| Workbook::open(bytes).expect("imported BIFF fixture"))
            .find(|workbook| {
                workbook
                    .sheets
                    .first()
                    .is_some_and(|sheet| sheet.biff_uses_application_default_row_height())
            })
            .expect("at least one BIFF fixture without DEFAULTROWHEIGHT");
        let mut overridden = implicit.clone();
        overridden.sheets[0].set_default_row_height(12.0);

        assert!(implicit.sheets[0].biff_uses_application_default_row_height());
        assert!(!overridden.sheets[0].biff_uses_application_default_row_height());

        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            gridlines: false,
            default_row_height: Fixed::from_pixels(37),
            ..RenderOptions::default()
        };
        let measured = |workbook: &Workbook| {
            measure_sheet_axes(&workbook.sheets[0], RenderRange::new(0, 0, 0, 0), &options)
                .unwrap()
                .0[0]
                .size
        };

        assert_eq!(measured(&implicit), BIFF_APPLICATION_DEFAULT_ROW_HEIGHT);
        assert_eq!(measured(&overridden), Fixed::from_pixels(16));
    }

    #[test]
    fn biff_default_funsynced_controls_multiline_auto_height() {
        let automatic = imported_biff8_default_row(false, "first line\nsecond line");
        let manual = imported_biff8_default_row(true, "first line\nsecond line");
        assert!(!automatic.sheets[0].default_row_height_is_manual());
        assert!(manual.sheets[0].default_row_height_is_manual());

        let pack = synthetic_test_pack();
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            gridlines: false,
            default_font_family: pack.default_family().to_string(),
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let measured = |workbook: &Workbook| {
            measure_sheet_axes(&workbook.sheets[0], RenderRange::new(0, 0, 0, 0), &options)
                .unwrap()
                .0[0]
                .size
        };

        assert!(
            measured(&automatic) > Fixed::from_pixels(20),
            "automatic BIFF default must allow multiline expansion"
        );
        assert_eq!(
            measured(&manual),
            Fixed::from_pixels(20),
            "manual BIFF default must retain its exact 300-twip height"
        );
    }

    #[test]
    fn implicit_ooxml_row_height_drives_image_and_chart_anchor_geometry() {
        for kind in [DrawingObjectKind::Image, DrawingObjectKind::Chart] {
            let workbook = imported_two_cell_drawing(kind, (0, 0));
            assert!(workbook.sheets[0].has_implicit_ooxml_row_height());

            let build = build_scene(
                &workbook,
                0,
                &RenderOptions {
                    gridlines: false,
                    default_row_height: Fixed::from_pixels(37),
                    ..RenderOptions::default()
                },
            )
            .unwrap();
            assert_eq!(build.report.range, RenderRange::new(3, 2, 6, 4));
            assert_eq!(
                build.scene.height,
                Fixed::from_raw(OOXML_APPLICATION_DEFAULT_ROW_HEIGHT.raw() * 4),
                "{kind:?}"
            );
            assert!(
                build.scene.nodes.iter().any(|node| matches!(
                    node,
                    SceneNode::Rect(RectNode {
                        rect: Rect {
                            x: Fixed::ZERO,
                            y: Fixed::ZERO,
                            width,
                            height,
                        },
                        ..
                    }) if *width == build.scene.width && *height == build.scene.height
                )),
                "{kind:?} anchor did not retain the exact implicit-row rectangle"
            );
        }
    }

    #[test]
    fn imported_ooxml_drawing_anchors_use_calc_standard_row_tracks() {
        let mut workbook = imported_xlsx("<styleSheet/>", "<worksheet><sheetData/></worksheet>");
        workbook.sheets[0]
            .add_image(Image::new([137, 80, 78, 71], ImageFmt::Png, (0, 0)).with_to((4, 4)));
        assert!(workbook.sheets[0].has_implicit_ooxml_row_height());
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let frame = build
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Rect(RectNode {
                    rect,
                    fill: Some(color),
                    ..
                }) if *color == Rgb::new(242, 242, 242) => Some(*rect),
                _ => None,
            })
            .expect("imported image placeholder frame");
        assert_eq!(
            frame.height,
            Fixed::from_raw(CALC_OOXML_DRAWING_DEFAULT_ROW_HEIGHT.raw() * 4)
        );
    }

    #[test]
    fn ooxml_default_hidden_rows_drive_selection_sparse_origin_and_drawing_geometry() {
        let worksheet = r#"<worksheet><sheetFormatPr defaultRowHeight="15" zeroHeight="1"/><sheetData><row r="2"/><row r="4"/><row r="6"/></sheetData><drawing r:id="rIdDrawing"/></worksheet>"#;
        for kind in [DrawingObjectKind::Image, DrawingObjectKind::Chart] {
            let workbook = imported_two_cell_drawing_with_worksheet(kind, (0, 0), worksheet);
            let sheet = &workbook.sheets[0];
            assert_eq!(
                sheet
                    .default_hidden_row_exceptions()
                    .expect("zeroHeight provenance")
                    .iter()
                    .copied()
                    .collect::<Vec<_>>(),
                [1, 3, 5]
            );

            let mut warnings = Warnings::default();
            let hidden_origin = sheet_grid_origin(
                sheet,
                RenderRange::new(7, 0, 7, 0),
                Fixed::from_pixels(7),
                &RenderOptions::default(),
                &mut warnings,
            )
            .unwrap()
            .1;
            assert_eq!(hidden_origin, Fixed::from_pixels(60), "{kind:?}");

            for (include_hidden, expected_last_row, expected_height, expected_origin) in [
                (false, 5, Fixed::from_pixels(40), Fixed::from_pixels(60)),
                (true, 6, Fixed::from_pixels(80), Fixed::from_pixels(140)),
            ] {
                let options = RenderOptions {
                    gridlines: false,
                    include_hidden,
                    ..RenderOptions::default()
                };
                let build = build_scene(&workbook, 0, &options).unwrap();
                assert_eq!(
                    build.report.range,
                    RenderRange::new(3, 2, expected_last_row, 4)
                );
                assert_eq!(build.scene.height, expected_height, "{kind:?}");

                let mut origin_warnings = Warnings::default();
                assert_eq!(
                    sheet_grid_origin(
                        sheet,
                        RenderRange::new(7, 0, 7, 0),
                        Fixed::from_pixels(7),
                        &options,
                        &mut origin_warnings,
                    )
                    .unwrap()
                    .1,
                    expected_origin,
                    "{kind:?}"
                );
                assert!(
                    build.scene.nodes.iter().any(|node| matches!(
                        node,
                        SceneNode::Rect(RectNode {
                            rect: Rect {
                                x: Fixed::ZERO,
                                y: Fixed::ZERO,
                                width,
                                height,
                            },
                            ..
                        }) if *width == build.scene.width && *height == build.scene.height
                    )),
                    "{kind:?} anchor did not follow effective row visibility"
                );
            }
        }
    }

    #[test]
    fn worksheet_view_gridlines_use_light_gray_one_pixel_strokes() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("grid").write(0, 0, "A");
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let grid = build
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Line(node) if node.color == Rgb::GRIDLINE => Some(node),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(grid.len(), 4);
        assert!(grid.iter().all(|line| line.width == Fixed::from_pixels(1)));
    }

    #[test]
    fn single_page_print_gridlines_use_calc_hairline_mapping_and_frame() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("print-grid");
        sheet.write(0, 0, "A");
        sheet.write(1, 1, "B");

        let build = build_single_page_sheet_scene_for_print(
            sheet,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 1, 1)),
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let print_grid = build
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Line(line)
                    if line.color == Rgb::BLACK && line.width == Fixed::from_raw(137) =>
                {
                    Some(line)
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(print_grid.len(), 2);
        assert!(!build
            .scene
            .nodes
            .iter()
            .any(|node| matches!(node, SceneNode::Line(line) if line.color == Rgb::GRIDLINE)));
        assert!(print_grid.iter().any(|line| {
            line.x1 == PRINT_GRIDLINE_FRAME_LEFT_INSET
                && line.x2 == PRINT_GRIDLINE_FRAME_LEFT_INSET
                && line.y1 == PRINT_GRIDLINE_FRAME_TOP_INSET
                && line.y2
                    == build
                        .scene
                        .height
                        .checked_sub(PRINT_GRIDLINE_FRAME_TRAILING_INSET)
                        .unwrap()
        }));
        assert!(print_grid.iter().any(|line| {
            line.x1 == PRINT_GRIDLINE_FRAME_LEFT_INSET
                && line.x2
                    == build
                        .scene
                        .width
                        .checked_sub(PRINT_GRIDLINE_FRAME_TRAILING_INSET)
                        .unwrap()
                && line.y1 == PRINT_GRIDLINE_FRAME_TOP_INSET
                && line.y2 == PRINT_GRIDLINE_FRAME_TOP_INSET
        }));
    }

    #[test]
    fn print_gridline_modes_keep_view_authored_and_calc_geometry_distinct() {
        fn print_gridlines(scene: &Scene) -> Vec<&LineNode> {
            scene
                .nodes
                .iter()
                .filter_map(|node| match node {
                    SceneNode::Line(line)
                        if line.color == Rgb::BLACK && line.width == PRINT_GRIDLINE_WIDTH =>
                    {
                        Some(line)
                    }
                    _ => None,
                })
                .collect()
        }

        let mut workbook = Workbook::new();
        workbook.add_sheet("print-grid");
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 17, 5)),
            ..RenderOptions::default()
        };

        let view = build_sheet_scene(&workbook.sheets[0], 0, &options).unwrap();
        let view_gridlines = view
            .scene
            .nodes
            .iter()
            .filter(|node| {
                matches!(node, SceneNode::Line(line) if line.color == Rgb::GRIDLINE && line.width == Fixed::from_pixels(1))
            })
            .count();
        assert_eq!(view_gridlines, 26);
        assert!(print_gridlines(&view.scene).is_empty());

        let authored = build_sheet_scene_for_print(&workbook.sheets[0], 0, &options).unwrap();
        assert_eq!(print_gridlines(&authored.scene).len(), 26);
        assert!(!authored
            .scene
            .nodes
            .iter()
            .any(|node| matches!(node, SceneNode::Line(line) if line.color == Rgb::GRIDLINE)));

        let single_page =
            build_single_page_sheet_scene_for_print(&workbook.sheets[0], 0, &options).unwrap();
        assert_eq!(print_gridlines(&single_page.scene).len(), 8);

        workbook.sheets[0].set_right_to_left(true);
        let rtl =
            build_single_page_sheet_scene_for_print(&workbook.sheets[0], 0, &options).unwrap();
        assert_eq!(print_gridlines(&rtl.scene).len(), 2);
    }

    #[test]
    fn print_gridline_precedence_retains_shared_unfilled_edge() {
        fn vertical_line_at(scene: &Scene, x: Fixed, color: Rgb, width: Fixed) -> bool {
            scene.nodes.iter().any(|node| {
                matches!(node, SceneNode::Line(line) if line.x1 == x && line.x2 == x && line.color == color && line.width == width)
            })
        }

        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("fill-boundary");
        sheet.write_with_format(
            0,
            0,
            "filled",
            &Format::new().set_background_color([0xFF, 0xCC, 0x00]),
        );
        sheet.write(0, 1, "plain");
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 1)),
            ..RenderOptions::default()
        };

        let view = build_sheet_scene(sheet, 0, &options).unwrap();
        let filled = view
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Rect(rect) if rect.fill.is_some() => Some(rect.rect),
                _ => None,
            })
            .expect("filled cell rectangle");
        let shared_x = filled.x.checked_add(filled.width).unwrap();
        assert!(!vertical_line_at(
            &view.scene,
            shared_x,
            Rgb::GRIDLINE,
            Fixed::from_pixels(1)
        ));

        let print = build_sheet_scene_for_print(sheet, 0, &options).unwrap();
        assert!(vertical_line_at(
            &print.scene,
            shared_x,
            Rgb::BLACK,
            PRINT_GRIDLINE_WIDTH
        ));
    }

    #[test]
    fn shared_grid_edges_are_painted_once_and_coalesced_per_axis() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("grid").write(0, 0, "A");
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 1, 1)),
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let grid = build
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Line(node) if node.color == Rgb::GRIDLINE => Some(node),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            grid.len(),
            6,
            "a 2x2 selection has three coalesced lines on each axis"
        );
        assert_eq!(
            grid.iter()
                .filter(|line| line.x1 == line.x2)
                .map(|line| (line.y2.raw() - line.y1.raw()).abs())
                .collect::<BTreeSet<_>>()
                .len(),
            1
        );
        assert_eq!(
            grid.iter()
                .filter(|line| line.y1 == line.y2)
                .map(|line| (line.x2.raw() - line.x1.raw()).abs())
                .collect::<BTreeSet<_>>()
                .len(),
            1
        );
    }

    #[test]
    fn two_by_two_grid_passes_at_its_exact_coalesced_scene_node_limit() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("bounded-grid");
        let mut options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 1, 1)),
            ..RenderOptions::default()
        };
        options.limits.max_scene_nodes = 6;
        let build = build_scene(&workbook, 0, &options).unwrap();
        assert_eq!(build.report.scene_nodes, 6);
        assert_eq!(
            build
                .scene
                .nodes
                .iter()
                .filter(|node| matches!(node, SceneNode::Line(_)))
                .count(),
            6
        );

        options.limits.max_scene_nodes = 5;
        assert_eq!(
            build_scene(&workbook, 0, &options),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::SceneNodes,
                limit: 5,
                actual: 6,
            })
        );
    }

    #[test]
    fn filled_and_conditionally_filled_cells_suppress_gridline_segments() {
        let fill = Color::rgb(10, 20, 30);
        let mut filled = Workbook::new();
        filled.add_sheet("filled").write_styled(
            0,
            0,
            "A",
            &CellStyle {
                fill: Some(fill),
                pattern_fill: Some(rxls::Fill::solid(fill)),
                ..CellStyle::default()
            },
        );
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            ..RenderOptions::default()
        };
        let filled_scene = build_scene(&filled, 0, &options).unwrap();
        assert!(!filled_scene
            .scene
            .nodes
            .iter()
            .any(|node| matches!(node, SceneNode::Line(line) if line.color == Rgb::GRIDLINE)));

        let mut conditional = Workbook::new();
        let sheet = conditional.add_sheet("conditional");
        sheet.write_number(0, 0, 1);
        sheet.add_conditional_format(CondFormat::new(
            (0, 0, 0, 0),
            CfRule::cell_is(DvOp::GreaterThan, "0", None::<&str>, fill),
        ));
        let conditional_scene = build_scene(&conditional, 0, &options).unwrap();
        assert!(!conditional_scene
            .scene
            .nodes
            .iter()
            .any(|node| matches!(node, SceneNode::Line(line) if line.color == Rgb::GRIDLINE)));
    }

    #[test]
    fn explicit_shared_border_wins_once_over_neighbor_and_gridline() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("borders");
        sheet.write_styled(
            0,
            0,
            "left",
            &CellStyle {
                border: Some(
                    Border::new()
                        .with_right(BorderStyle::Thin)
                        .with_right_color(Color::rgb(255, 0, 0)),
                ),
                ..CellStyle::default()
            },
        );
        sheet.write_styled(
            0,
            1,
            "right",
            &CellStyle {
                border: Some(
                    Border::new()
                        .with_left(BorderStyle::Thick)
                        .with_left_color(Color::rgb(0, 0, 255)),
                ),
                ..CellStyle::default()
            },
        );
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 1)),
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let explicit = build
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Line(line) if line.color != Rgb::GRIDLINE => Some(line),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(explicit.len(), 1);
        assert_eq!(explicit[0].color, Rgb::new(0, 0, 255));
        assert_eq!(explicit[0].width, Fixed::from_pixels(3));
        assert!(!build.scene.nodes.iter().any(|node| {
            matches!(
                node,
                SceneNode::Line(line)
                    if line.color == Rgb::GRIDLINE
                        && line.x1 == explicit[0].x1
                        && line.x2 == explicit[0].x2
                        && line.y1 == explicit[0].y1
                        && line.y2 == explicit[0].y2
            )
        }));
    }

    #[test]
    fn gridlines_remain_below_text_while_explicit_borders_remain_above_it() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("edge-layers").write_styled(
            0,
            0,
            "overflowing text",
            &CellStyle {
                border: Some(
                    Border::new()
                        .with_right(BorderStyle::Thin)
                        .with_right_color(Color::rgb(255, 0, 0)),
                ),
                ..CellStyle::default()
            },
        );
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 1)),
                ..RenderOptions::default()
            },
        )
        .unwrap();

        let text_index = build
            .scene
            .nodes
            .iter()
            .position(|node| matches!(node, SceneNode::Text(_)))
            .unwrap();
        let last_gridline = build
            .scene
            .nodes
            .iter()
            .rposition(|node| matches!(node, SceneNode::Line(line) if line.color == Rgb::GRIDLINE))
            .unwrap();
        let explicit_border = build
            .scene
            .nodes
            .iter()
            .position(
                |node| matches!(node, SceneNode::Line(line) if line.color == Rgb::new(255, 0, 0)),
            )
            .unwrap();
        assert!(last_gridline < text_index);
        assert!(text_index < explicit_border);
    }

    #[test]
    fn double_border_is_retained_as_two_parallel_single_pixel_lines() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("double").write_styled(
            0,
            0,
            "A",
            &CellStyle {
                border: Some(
                    Border::new()
                        .with_top(BorderStyle::Double)
                        .with_top_color(Color::rgb(1, 2, 3)),
                ),
                ..CellStyle::default()
            },
        );
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let lines = build
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Line(line) => Some(line),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|line| line.color == Rgb::new(1, 2, 3)
            && line.width == Fixed::from_pixels(1)
            && line.y1 == line.y2));
        assert_eq!(
            (lines[1].y1.raw() - lines[0].y1.raw()).abs(),
            Fixed::from_pixels(2).raw()
        );
        assert!(!build
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::DoubleBorderSimplified));
    }

    #[test]
    fn shared_double_border_geometry_does_not_depend_on_the_authoring_neighbor() {
        fn shared_lines(author_on_left: bool) -> Vec<(i64, i64, i64, i64)> {
            let mut workbook = Workbook::new();
            let sheet = workbook.add_sheet("shared-double");
            let (left, right) = if author_on_left {
                (
                    CellStyle {
                        border: Some(Border::new().with_right(BorderStyle::Double)),
                        ..CellStyle::default()
                    },
                    CellStyle::default(),
                )
            } else {
                (
                    CellStyle::default(),
                    CellStyle {
                        border: Some(Border::new().with_left(BorderStyle::Double)),
                        ..CellStyle::default()
                    },
                )
            };
            sheet.write_styled(0, 0, "A", &left);
            sheet.write_styled(0, 1, "B", &right);
            build_scene(
                &workbook,
                0,
                &RenderOptions {
                    selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 1)),
                    gridlines: false,
                    ..RenderOptions::default()
                },
            )
            .unwrap()
            .scene
            .nodes
            .into_iter()
            .filter_map(|node| match node {
                SceneNode::Line(line) => {
                    Some((line.x1.raw(), line.y1.raw(), line.x2.raw(), line.y2.raw()))
                }
                _ => None,
            })
            .collect()
        }

        let left_authored = shared_lines(true);
        let right_authored = shared_lines(false);
        assert_eq!(left_authored, right_authored);
        assert_eq!(left_authored.len(), 2);
    }

    #[test]
    fn ods_physical_column_width_precedes_character_projection() {
        use std::io::Write;

        use zip::write::SimpleFileOptions;

        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Physical"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles><style:default-style style:family="table-column"><style:table-column-properties style:column-width="1in"/></style:default-style></office:styles></office:document-styles>"#;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        zip.start_file("mimetype", options).unwrap();
        zip.write_all(b"application/vnd.oasis.opendocument.spreadsheet")
            .unwrap();
        zip.start_file("content.xml", options).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
        zip.start_file("styles.xml", options).unwrap();
        zip.write_all(styles.as_bytes()).unwrap();
        let bytes = zip.finish().unwrap().into_inner();

        let workbook = Workbook::open(&bytes).expect("ODS workbook");
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.physical_column_widths().get(&0), Some(&72.0));
        assert!(sheet.column_widths()[&0] > 13.0);

        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        assert_eq!(build.scene.width, Fixed::from_pixels(96));
    }

    #[test]
    fn ods_physical_width_drives_exact_automatic_cjk_row_height() {
        use std::io::Write;

        use zip::write::SimpleFileOptions;

        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Auto"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>한글中文</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:default-style style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap"/></style:default-style><style:default-style style:family="table-column"><style:table-column-properties style:column-width="0.1875in"/></style:default-style></office:styles></office:document-styles>"#;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let zip_options = SimpleFileOptions::default();
        zip.start_file("mimetype", zip_options).unwrap();
        zip.write_all(b"application/vnd.oasis.opendocument.spreadsheet")
            .unwrap();
        zip.start_file("content.xml", zip_options).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
        zip.start_file("styles.xml", zip_options).unwrap();
        zip.write_all(styles.as_bytes()).unwrap();
        let workbook = Workbook::open(&zip.finish().unwrap().into_inner()).expect("ODS workbook");
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.physical_column_widths().get(&0), Some(&13.5));

        let range = RenderRange::new(0, 0, 0, 0);
        let options = outlined_options(range);
        let (rows, columns) = measure_sheet_axes(sheet, range, &options).unwrap();
        assert_eq!(columns[0].size, Fixed::from_pixels(18));
        assert_eq!(rows[0].size.raw(), 74_140);
        let build = build_scene(&workbook, 0, &options).unwrap();
        assert_eq!(build.scene.width, Fixed::from_pixels(18));
        assert_eq!(build.scene.height.raw(), 74_140);
    }

    #[test]
    fn ods_fixed_height_wrapped_text_keeps_implicit_start_and_explicit_alignment_controls() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:automatic-styles><style:style style:name="co" style:family="table-column"><style:table-column-properties style:column-width="0.45in"/></style:style><style:style style:name="ro" style:family="table-row"><style:table-row-properties style:row-height="0.25in" style:use-optimal-row-height="false"/></style:style><style:style style:name="ce-default" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap"/></style:style><style:style style:name="ce-top" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap" style:vertical-align="top"/></style:style><style:style style:name="ce-middle" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap" style:vertical-align="middle"/></style:style><style:style style:name="ce-bottom" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap" style:vertical-align="bottom"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Clip"><table:table-column table:style-name="co"/><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce-default" office:value-type="string"><text:p>implicit one two three four five six seven</text:p></table:table-cell></table:table-row><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce-top" office:value-type="string"><text:p>top one two three four five six seven</text:p></table:table-cell></table:table-row><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce-middle" office:value-type="string"><text:p>middle one two three four five six seven</text:p></table:table-cell></table:table-row><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce-bottom" office:value-type="string"><text:p>bottom one two three four five six seven</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
        let workbook = ods_workbook(content, styles);
        let build = build_scene(
            &workbook,
            0,
            &outlined_options(RenderRange::new(0, 0, 3, 0)),
        )
        .unwrap();

        for text in [
            "implicit one two three four five six seven",
            "top one two three four five six seven",
            "middle one two three four five six seven",
            "bottom one two three four five six seven",
        ] {
            let run = glyph_run(&build.scene, text);
            assert_eq!(
                run.semantic_groups,
                [GlyphSemanticGroup {
                    source_start: 0,
                    source_end: u64::try_from(text.len()).unwrap(),
                }],
                "fixed-height ODS wrapping must retain Calc's complete logical paragraph"
            );
        }

        let span = |text: &str| {
            let run = glyph_run(&build.scene, text);
            let minimum = run
                .cluster_metrics
                .iter()
                .map(|metric| metric.baseline_y)
                .min()
                .unwrap();
            let maximum = run
                .cluster_metrics
                .iter()
                .map(|metric| metric.baseline_y)
                .max()
                .unwrap();
            (run.clip_bounds, minimum, maximum)
        };
        let (implicit_clip, implicit_min, implicit_max) =
            span("implicit one two three four five six seven");
        let (top_clip, top_min, top_max) = span("top one two three four five six seven");
        let (middle_clip, middle_min, middle_max) =
            span("middle one two three four five six seven");
        let (bottom_clip, bottom_min, bottom_max) =
            span("bottom one two three four five six seven");
        let bottom = |clip: Rect| clip.y.checked_add(clip.height).unwrap();

        assert!(implicit_min >= implicit_clip.y);
        assert!(implicit_max > bottom(implicit_clip));
        assert!(top_min >= top_clip.y);
        assert!(top_max > bottom(top_clip));
        assert!(middle_min < middle_clip.y);
        assert!(middle_max > bottom(middle_clip));
        assert!(bottom_min < bottom_clip.y);
        assert!(bottom_max <= bottom(bottom_clip));
    }

    #[test]
    fn ods_fixed_height_multiline_text_that_fits_does_not_expand_semantics() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:automatic-styles><style:style style:name="co" style:family="table-column"><style:table-column-properties style:column-width="0.45in"/></style:style><style:style style:name="ro" style:family="table-row"><style:table-row-properties style:row-height="0.75in" style:use-optimal-row-height="false"/></style:style><style:style style:name="ce" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap" style:vertical-align="top"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Fits"><table:table-column table:style-name="co"/><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce" office:value-type="string"><text:p>tall one two</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
        let workbook = ods_workbook(content, styles);
        let build = build_scene(
            &workbook,
            0,
            &outlined_options(RenderRange::new(0, 0, 0, 0)),
        )
        .unwrap();
        let run = glyph_run(&build.scene, "tall one two");
        assert!(
            run.cluster_metrics
                .windows(2)
                .any(|pair| pair[0].baseline_y != pair[1].baseline_y),
            "the fixed-height control must actually wrap onto multiple lines"
        );
        assert!(
            run.semantic_groups.is_empty(),
            "a multiline paragraph that fits its fixed row must retain ordinary clip semantics"
        );
        let clip_bottom = run
            .clip_bounds
            .y
            .checked_add(run.clip_bounds.height)
            .unwrap();
        assert!(run.cluster_metrics.iter().all(|metrics| {
            metrics.baseline_y.checked_sub(metrics.ascent).unwrap() >= run.clip_bounds.y
                && metrics.baseline_y.checked_sub(metrics.descent).unwrap() <= clip_bottom
        }));
    }

    #[test]
    fn ods_rotated_fixed_height_wrap_retains_generic_bottom_alignment() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:automatic-styles><style:style style:name="co" style:family="table-column"><style:table-column-properties style:column-width="0.45in"/></style:style><style:style style:name="ro" style:family="table-row"><style:table-row-properties style:row-height="0.25in" style:use-optimal-row-height="false"/></style:style><style:style style:name="ce-implicit" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap" style:rotation-angle="45"/></style:style><style:style style:name="ce-bottom" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap" style:rotation-angle="45" style:vertical-align="bottom"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Rotated"><table:table-column table:style-name="co"/><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce-implicit" office:value-type="string"><text:p>rotated one two three four five six seven</text:p></table:table-cell></table:table-row><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce-bottom" office:value-type="string"><text:p>rotated one two three four five six seven</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
        let workbook = ods_workbook(content, styles);
        let build = build_scene(
            &workbook,
            0,
            &outlined_options(RenderRange::new(0, 0, 1, 0)),
        )
        .unwrap();
        let runs = build
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::GlyphRun(run)
                    if run.text == "rotated one two three four five six seven" =>
                {
                    Some(run)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().all(|run| run.rotation_degrees == 45));

        let relative_baselines = |run: &GlyphRunNode| {
            run.cluster_metrics
                .iter()
                .map(|metric| metric.baseline_y.raw() - run.clip_bounds.y.raw())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            relative_baselines(runs[0]),
            relative_baselines(runs[1]),
            "implicit rotated ODS text must retain the generic bottom-aligned path"
        );
        assert!(
            runs[0]
                .cluster_metrics
                .iter()
                .map(|metric| metric.baseline_y)
                .min()
                .unwrap()
                < runs[0].clip_bounds.y,
            "a multi-line bottom-aligned block begins above a fixed row's clip"
        );
    }

    #[test]
    fn ods_optimal_height_wrapped_text_expands_instead_of_using_the_fixed_clip_override() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:automatic-styles><style:style style:name="co" style:family="table-column"><style:table-column-properties style:column-width="0.45in"/></style:style><style:style style:name="ro" style:family="table-row"><style:table-row-properties style:row-height="0.25in" style:use-optimal-row-height="true"/></style:style><style:style style:name="ce" style:family="table-cell"><style:table-cell-properties fo:wrap-option="wrap"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Optimal"><table:table-column table:style-name="co"/><table:table-row table:style-name="ro"><table:table-cell table:style-name="ce" office:value-type="string"><text:p>automatic one two three four five six seven</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
        let workbook = ods_workbook(content, styles);
        let sheet = &workbook.sheets[0];
        assert!(!sheet.row_height_is_manual(0));

        let range = RenderRange::new(0, 0, 0, 0);
        let options = outlined_options(range);
        let (rows, _) = measure_sheet_axes(sheet, range, &options).unwrap();
        assert!(rows[0].size > points_to_fixed(18.0).unwrap());

        let build = build_scene(&workbook, 0, &options).unwrap();
        let run = glyph_run(&build.scene, "automatic one two three four five six seven");
        assert!(
            run.semantic_groups.is_empty(),
            "an expanded ODS row must use ordinary per-cluster visibility"
        );
        let clip_bottom = run
            .clip_bounds
            .y
            .checked_add(run.clip_bounds.height)
            .unwrap();
        for metrics in &run.cluster_metrics {
            assert!(
                metrics.baseline_y.checked_sub(metrics.ascent).unwrap() >= run.clip_bounds.y,
                "an optimal row must retain the first glyph outline inside its expanded clip"
            );
            assert!(
                metrics.baseline_y.checked_sub(metrics.descent).unwrap() <= clip_bottom,
                "an optimal row must retain the last glyph outline inside its expanded clip"
            );
        }
    }

    fn ods_workbook(content: &str, styles: &str) -> Workbook {
        use std::io::Write;

        use zip::write::SimpleFileOptions;

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        zip.start_file("mimetype", options).unwrap();
        zip.write_all(b"application/vnd.oasis.opendocument.spreadsheet")
            .unwrap();
        zip.start_file("content.xml", options).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
        zip.start_file("styles.xml", options).unwrap();
        zip.write_all(styles.as_bytes()).unwrap();
        Workbook::open(&zip.finish().unwrap().into_inner()).expect("ODS workbook")
    }

    #[test]
    fn single_page_ods_undeclared_row_uses_calc_application_default_height() {
        // A row with no table:style-name, and no default-style declared for
        // table-row anywhere in styles.xml, has no explicit height anywhere
        // in the document. `src/ods.rs` used to leave
        // `imported_default_row_axis_measure` as `None` for every ODS sheet,
        // so this row's contribution to the SinglePageSheets native
        // cumulative extent silently fell back to the renderer's generic
        // 15pt Excel-style default (`RenderOptions::default_row_height`)
        // requantized through a lossy CSS-pixel round trip, instead of
        // Calc's real 0.5 cm (14.173228 pt) no-information application
        // default -- the same oracle-pinned constant OOXML already uses for
        // its own equivalent "no information" row
        // (`OOXML_APPLICATION_DEFAULT_ROW_HEIGHT`).
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Plain"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
        let workbook = ods_workbook(content, styles);
        let sheet = &workbook.sheets[0];
        assert_eq!(
            sheet.imported_default_row_axis_measure(),
            Some(ImportedAxisMeasure::MillimeterHundredths(500)),
            "ods.rs must expose Calc's native no-information row default, \
             mirroring the unconditional 64-point column default"
        );

        let range = RenderRange::new(0, 0, 0, 0);
        let opts = outlined_options(range);
        let single_page = build_single_page_sheet_scene(sheet, 0, &opts).unwrap();
        assert_eq!(
            single_page.scene.height,
            Fixed::from_raw(19_351),
            "an undeclared ODS row must resolve the single-page page-box \
             height through Calc's native 0.5 cm application default"
        );
    }

    #[test]
    fn single_page_ods_undeclared_rows_accumulate_exact_native_extent() {
        // Five stacked undeclared rows exercise the SourceAxisCursor's
        // cumulative twips accounting rather than a single-row endpoint, the
        // shape of drift that showed up as page-box error scaling with sheet
        // size in the hosted pilot.
        let mut rows = String::new();
        for _ in 0..5 {
            rows.push_str(
                r#"<table:table-row><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row>"#,
            );
        }
        let content = format!(
            r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Plain"><table:table-column/>{rows}</table:table></office:spreadsheet></office:body></office:document-content>"#
        );
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
        let workbook = ods_workbook(&content, styles);
        let sheet = &workbook.sheets[0];

        let range = RenderRange::new(0, 0, 4, 0);
        let opts = outlined_options(range);
        let single_page = build_single_page_sheet_scene(sheet, 0, &opts).unwrap();
        assert_eq!(
            single_page.scene.height,
            Fixed::from_raw(96_640),
            "five undeclared ODS rows must accumulate through the same \
             twips-native cursor as a single row, not drift by a \
             per-row CSS-pixel rounding residual"
        );
    }

    #[test]
    fn fallback_row_height_prefers_ods_native_default_over_generic_placeholder() {
        // Companion to `single_page_ods_undeclared_row_uses_calc_application_default_height`
        // above, but exercising `fallback_row_height` directly -- the shared
        // helper behind the ordinary (`AxisEndpointPolicy::PerTrackFixed`) row
        // path, not just the single-page SourceNative cursor. Before this fix
        // ODS sheets fell through every branch (BIFF, then OOXML implicit) to
        // `options.default_row_height`, the renderer's generic 15pt
        // Excel-style placeholder, ignoring the sheet's own populated
        // `imported_default_row_axis_measure`.
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Plain"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
        let workbook = ods_workbook(content, styles);
        let sheet = &workbook.sheets[0];
        assert_eq!(
            sheet.imported_default_row_axis_measure(),
            Some(ImportedAxisMeasure::MillimeterHundredths(500))
        );
        assert_eq!(sheet.default_row_height(), None);
        assert!(!sheet.biff_uses_application_default_row_height());
        assert_eq!(sheet.implicit_ooxml_row_height_source(), None);

        let range = RenderRange::new(0, 0, 0, 0);
        let opts = outlined_options(range);
        assert_eq!(
            fallback_row_height(sheet, &opts),
            OOXML_APPLICATION_DEFAULT_ROW_HEIGHT,
            "an ODS sheet's own native no-information row default must win \
             over the renderer's generic RenderOptions::default_row_height \
             placeholder"
        );
    }

    #[test]
    fn ordinary_path_ods_undeclared_row_uses_calc_application_default_height() {
        // The ordinary (non-single-page) render path -- `build_sheet_scene`,
        // `AxisEndpointPolicy::PerTrackFixed` -- measures row height through
        // `fallback_row_height` directly, unlike SourceNative which only uses
        // it as a last-resort fallback behind the twips cursor. This is the
        // ordinary-path counterpart to
        // `single_page_ods_undeclared_row_uses_calc_application_default_height`.
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Plain"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
        let workbook = ods_workbook(content, styles);
        let sheet = &workbook.sheets[0];

        let range = RenderRange::new(0, 0, 0, 0);
        let opts = outlined_options(range);
        let ordinary = build_sheet_scene(sheet, 0, &opts).unwrap();
        assert_eq!(
            ordinary.scene.height,
            Fixed::from_raw(19_351),
            "an undeclared ODS row must resolve the ordinary-path page-box \
             height through Calc's native 0.5 cm application default, the \
             same value the single-page path already resolves to"
        );
    }

    #[test]
    fn ordinary_and_single_page_ods_paths_agree_on_undeclared_row_height() {
        // The bug class this tranche fixes is exactly the two endpoint
        // policies disagreeing about an ODS sheet's undeclared row height:
        // SourceNative (single-page) already consumed the sheet's native
        // default after the prior tranche, while PerTrackFixed (ordinary)
        // still fell back to the unrelated generic placeholder. For a single
        // row, both paths must now resolve to the identical page-box height.
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Plain"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>A</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
        let workbook = ods_workbook(content, styles);
        let sheet = &workbook.sheets[0];

        let range = RenderRange::new(0, 0, 0, 0);
        let opts = outlined_options(range);
        let ordinary = build_sheet_scene(sheet, 0, &opts).unwrap();
        let single_page = build_single_page_sheet_scene(sheet, 0, &opts).unwrap();
        assert_eq!(
            ordinary.scene.height, single_page.scene.height,
            "the ordinary and single-page paths must not disagree about an \
             undeclared ODS row's height"
        );
    }

    #[test]
    fn outlined_typography_limits_are_typed_and_exact_at_the_boundary() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("limits").write(0, 0, "A");
        let range = RenderRange::new(0, 0, 0, 0);
        let baseline = build_scene(&workbook, 0, &outlined_options(range)).unwrap();
        let command_count = glyph_run(&baseline.scene, "A").commands.len() as u64;
        assert!(command_count > 1);

        let mut exact = outlined_options(range);
        exact.limits.max_glyphs = 1;
        exact.limits.max_text_runs = 3;
        exact.limits.max_text_lines = 1;
        exact.limits.max_path_commands = command_count;
        assert_eq!(build_scene(&workbook, 0, &exact).unwrap(), baseline);

        let mut limited = exact.clone();
        limited.limits.max_text_runs = 2;
        assert_eq!(
            build_scene(&workbook, 0, &limited),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::TextRuns,
                limit: 2,
                actual: 3,
            })
        );

        let mut limited = exact.clone();
        limited.limits.max_text_lines = 0;
        assert_eq!(
            build_scene(&workbook, 0, &limited),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::TextLines,
                limit: 0,
                actual: 1,
            })
        );

        let mut limited = exact;
        limited.limits.max_path_commands = command_count - 1;
        assert_eq!(
            build_scene(&workbook, 0, &limited),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::PathCommands,
                limit: command_count - 1,
                actual: command_count,
            })
        );
    }

    #[test]
    fn substitution_and_missing_glyphs_are_aggregated_without_host_fallback() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("warnings").write_styled(
            0,
            0,
            "A😀",
            &CellStyle::new().font_name("Host Font Must Not Be Read"),
        );
        let build = build_scene(
            &workbook,
            0,
            &outlined_options(RenderRange::new(0, 0, 0, 0)),
        )
        .unwrap();
        assert!(build.report.warnings.iter().any(|warning| {
            warning.code == WarningCode::FontFamilySubstituted && warning.occurrences == 1
        }));
        assert!(build.report.warnings.iter().any(|warning| {
            warning.code == WarningCode::MissingGlyph && warning.occurrences == 1
        }));
    }

    #[test]
    fn numeric_overflow_uses_hashes_but_wrap_and_shrink_remain_authoritative() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("overflow");
        sheet.set_col_width(0, 1.0);
        sheet.write_number(0, 0, 123_456_789);
        sheet.write_styled(1, 0, 123_456_789, &CellStyle::new().wrap());
        sheet.write_styled(2, 0, 123_456_789, &CellStyle::new().shrink_to_fit());
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 2, 0)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let texts = build
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Text(node) => Some(node.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(texts, ["#", "123456789", "123456789"]);
        assert!(build.report.warnings.iter().any(|warning| {
            warning.code == WarningCode::NumericOverflowHashed && warning.occurrences == 1
        }));
    }

    #[test]
    fn date_overflow_uses_effective_format_for_authored_numbers_and_cached_formulas() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("date-overflow");
        sheet.set_col_width(0, 1.0);
        sheet.set_col_width(1, 20.0);
        sheet.set_col_width(2, 1.0);
        sheet.set_col_width(3, 1.0);
        let format = Format::new().set_num_format("yyyy-mm-dd");
        // These are deliberately authored as ordinary numeric cells. Their
        // effective resolved number format, not their storage variant, gives
        // them date display semantics.
        sheet.write_number_with_format(0, 0, 45_366.0, &format);
        sheet.write_number_with_format(0, 1, 45_366.0, &format);
        sheet.write_with_format(
            0,
            2,
            Cell::Formula {
                formula: "B1".to_string(),
                cached: Box::new(Cell::Number(45_366.0)),
            },
            &format,
        );
        sheet.write_number(0, 3, 123_456_789);
        let build = build_scene(
            &workbook,
            0,
            &outlined_options(RenderRange::new(0, 0, 0, 3)),
        )
        .unwrap();
        let texts = build
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::GlyphRun(node) => Some(node.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(texts, ["###", "2024-03-15", "###", "#"]);
        assert!(build.report.warnings.iter().any(|warning| {
            warning.code == WarningCode::NumericOverflowHashed && warning.occurrences == 3
        }));
    }

    #[test]
    fn date_overflow_is_stable_across_xlsx_serialize_and_reopen() {
        let mut authored = Workbook::new();
        let sheet = authored.add_sheet("roundtrip-date-overflow");
        sheet.set_col_width(0, 1.0);
        sheet.write_number_with_format(0, 0, 45_366.0, &Format::new().set_num_format("yyyy-mm-dd"));
        assert!(matches!(sheet.cell(0, 0), Some(Cell::Number(_))));

        let options = outlined_options(RenderRange::new(0, 0, 0, 0));
        let authored_build = build_scene(&authored, 0, &options).unwrap();
        assert_eq!(glyph_run(&authored_build.scene, "###").text, "###");

        let imported = Workbook::open(&authored.to_xlsx()).expect("round-tripped workbook");
        assert!(matches!(imported.sheets[0].cell(0, 0), Some(Cell::Date(_))));
        let imported_build = build_scene(&imported, 0, &options).unwrap();
        assert_eq!(glyph_run(&imported_build.scene, "###").text, "###");
    }

    #[test]
    fn imported_formula_dates_and_ods_formula_times_use_fixed_three_hashes() {
        let xlsx = imported_xlsx(
            r#"<styleSheet><numFmts count="1"><numFmt numFmtId="165" formatCode="yyyy-mm-dd"/></numFmts><cellXfs count="1"><xf numFmtId="165" applyNumberFormat="1"/></cellXfs></styleSheet>"#,
            r#"<worksheet><cols><col min="1" max="1" width="1" customWidth="1"/></cols><sheetData><row r="1"><c r="A1" s="0"><f>TODAY()</f><v>45366</v></c></row></sheetData></worksheet>"#,
        );
        match xlsx.sheets[0].cell(0, 0).expect("formula date") {
            Cell::Formula { cached, .. } => assert!(matches!(cached.as_ref(), Cell::Date(_))),
            other => panic!("expected imported formula, got {other:?}"),
        }
        let xlsx_build =
            build_scene(&xlsx, 0, &outlined_options(RenderRange::new(0, 0, 0, 0))).unwrap();
        assert_eq!(glyph_run(&xlsx_build.scene, "###").text, "###");

        let ods_content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:automatic-styles><style:style style:name="co" style:family="table-column"><style:table-column-properties style:column-width="0.08in"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Time"><table:table-column table:style-name="co"/><table:table-row><table:table-cell table:formula="of:=TIME(12;0;0)" office:value-type="time" office:time-value="PT12H"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let ods_styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles/></office:document-styles>"#;
        let ods = ods_workbook(ods_content, ods_styles);
        match ods.sheets[0].cell(0, 0).expect("formula time") {
            Cell::Formula { cached, .. } => assert!(matches!(cached.as_ref(), Cell::Date(_))),
            other => panic!("expected imported ODS formula, got {other:?}"),
        }
        let ods_build =
            build_scene(&ods, 0, &outlined_options(RenderRange::new(0, 0, 0, 0))).unwrap();
        assert_eq!(glyph_run(&ods_build.scene, "###").text, "###");
    }

    #[test]
    fn typed_conditional_formats_resolve_priority_scales_ranks_and_bars() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("conditional");
        for row in 0..3 {
            for col in 0..4 {
                sheet.write_number(row, col, f64::from(row * 50));
            }
        }
        sheet.write_number(0, 2, 100);
        sheet.write_number(1, 2, 100);
        sheet.write_number(2, 2, 50);
        sheet.add_conditional_format(CondFormat::new(
            (0, 0, 2, 0),
            CfRule::color_scale2(Color::rgb(255, 0, 0), Color::rgb(0, 255, 0)),
        ));
        sheet.add_conditional_format(CondFormat::new(
            (0, 1, 2, 1),
            CfRule::cell_is(
                DvOp::GreaterThan,
                "50",
                None::<&str>,
                Color::rgb(255, 255, 0),
            ),
        ));
        sheet.add_conditional_format(CondFormat::new(
            (0, 2, 2, 2),
            CfRule::top_bottom(1, false, false, Color::rgb(255, 192, 0)),
        ));
        sheet.add_conditional_format(CondFormat::new(
            (0, 3, 2, 3),
            CfRule::data_bar(Color::rgb(68, 114, 196)),
        ));
        sheet.add_conditional_format(CondFormat::new(
            (0, 0, 2, 3),
            CfRule::expression("A1>0", Color::rgb(1, 2, 3)),
        ));
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 2, 3)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let rectangles = build
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Rect(node) => Some(node),
                _ => None,
            })
            .collect::<Vec<_>>();
        for color in [
            Rgb::new(255, 0, 0),
            Rgb::new(128, 128, 0),
            Rgb::new(0, 255, 0),
        ] {
            assert!(rectangles.iter().any(|node| node.fill == Some(color)));
        }
        assert_eq!(
            rectangles
                .iter()
                .filter(|node| node.fill == Some(Rgb::new(255, 192, 0)))
                .count(),
            2,
            "top-N includes all values tied at the threshold"
        );
        let bars = rectangles
            .iter()
            .filter(|node| node.fill == Some(Rgb::new(68, 114, 196)))
            .collect::<Vec<_>>();
        assert_eq!(bars.len(), 2, "the minimum-value data bar has zero width");
        assert_eq!(bars[0].rect.width, Fixed::from_pixels(31));
        assert_eq!(bars[1].rect.width, Fixed::from_pixels(62));
        assert!(build.report.warnings.iter().any(|warning| {
            warning.code == WarningCode::ConditionalDataBarSimplified && warning.occurrences == 1
        }));
        assert!(!build
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::ConditionalFormattingDeferred));
        assert_eq!(
            rectangles
                .iter()
                .filter(|node| node.fill == Some(Rgb::new(1, 2, 3)))
                .count(),
            4,
            "the strict numeric comparison expression uses relative A1 references"
        );
    }

    #[test]
    fn conditional_rule_statistics_include_cells_outside_the_render_selection() {
        let render_middle = |workbook: &Workbook| {
            build_scene(
                workbook,
                0,
                &RenderOptions {
                    selection: RenderSelection::Range(RenderRange::new(1, 0, 1, 0)),
                    gridlines: false,
                    ..RenderOptions::default()
                },
            )
            .unwrap()
        };

        let mut scale = Workbook::new();
        let sheet = scale.add_sheet("scale");
        for (row, value) in [0.0, 50.0, 100.0].into_iter().enumerate() {
            sheet.write_number(row as u32, 0, value);
        }
        sheet.add_conditional_format(CondFormat::new(
            (0, 0, 2, 0),
            CfRule::color_scale2(Color::rgb(255, 0, 0), Color::rgb(0, 255, 0)),
        ));
        let scale = render_middle(&scale);
        assert!(
            scale.scene.nodes.iter().any(|node| {
                matches!(
                    node,
                    SceneNode::Rect(RectNode {
                        fill: Some(Rgb {
                            red: 128,
                            green: 128,
                            blue: 0
                        }),
                        ..
                    })
                )
            }),
            "the selected midpoint must be scaled against the off-selection minimum and maximum"
        );

        let mut ranked = Workbook::new();
        let sheet = ranked.add_sheet("ranked");
        for (row, value) in [100.0, 50.0, 100.0].into_iter().enumerate() {
            sheet.write_number(row as u32, 0, value);
        }
        let top_fill = Rgb::new(255, 192, 0);
        let below_average_fill = Rgb::new(0, 176, 80);
        sheet.add_conditional_format(CondFormat::new(
            (0, 0, 2, 0),
            CfRule::top_bottom(1, false, false, Color::rgb(255, 192, 0)),
        ));
        sheet.add_conditional_format(CondFormat::new(
            (0, 0, 2, 0),
            CfRule::above_average(true, Color::rgb(0, 176, 80)),
        ));
        let ranked = render_middle(&ranked);
        let fills = ranked
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Rect(node) => node.fill,
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !fills.contains(&top_fill),
            "the selected 50 is not a top-one value when off-selection 100s are included"
        );
        assert!(
            fills.contains(&below_average_fill),
            "the selected 50 is below the full rule-range average"
        );

        let mut duplicate = Workbook::new();
        let sheet = duplicate.add_sheet("duplicate");
        for (row, value) in ["Alpha", "alpha", "Beta"].into_iter().enumerate() {
            sheet.write(row as u32, 0, value);
        }
        let duplicate_fill = Rgb::new(68, 114, 196);
        sheet.add_conditional_format(CondFormat::new(
            (0, 0, 2, 0),
            CfRule::duplicate_values(false, Color::rgb(68, 114, 196)),
        ));
        let duplicate = render_middle(&duplicate);
        assert!(
            duplicate.scene.nodes.iter().any(|node| matches!(
                node,
                SceneNode::Rect(node) if node.fill == Some(duplicate_fill)
            )),
            "duplicate classification must include the matching off-selection value"
        );
        assert!(
            !duplicate
                .report
                .warnings
                .iter()
                .any(|warning| warning.code == WarningCode::ConditionalFormattingDeferred),
            "a fully supported rule range must not become deferred only because it is clipped"
        );
    }

    #[test]
    fn imported_table_region_snapshot_precedes_direct_and_conditional_layers_deterministically() {
        let workbook = imported_table_xlsx(
            r#"<styleSheet>
                <fonts count="3"><font><name val="Base"/></font><font><b/></font><font><i/></font></fonts>
                <fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="solid"><fgColor rgb="FF636363"/></patternFill></fill></fills>
                <borders count="1"><border/></borders>
                <cellXfs count="4">
                    <xf numFmtId="2" fontId="0" fillId="0" borderId="0"/>
                    <xf numFmtId="2" fontId="1" fillId="0" borderId="0" applyFont="1"/>
                    <xf numFmtId="2" fontId="2" fillId="0" borderId="0" applyFont="1"/>
                    <xf numFmtId="2" fontId="0" fillId="1" borderId="0" applyFill="1"/>
                </cellXfs>
                <dxfs count="7">
                    <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF0A0A0A"/></patternFill></fill></dxf>
                    <dxf><font><b/><color rgb="FFFFFFFF"/></font><fill><patternFill patternType="solid"><fgColor rgb="FF141414"/></patternFill></fill></dxf>
                    <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF1E1E1E"/></patternFill></fill></dxf>
                    <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF282828"/></patternFill></fill></dxf>
                    <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF323232"/></patternFill></fill></dxf>
                    <dxf><font><color rgb="FF3C3C3C"/></font></dxf>
                    <dxf><fill><patternFill patternType="solid"><fgColor rgb="FFC8C8C8"/></patternFill></fill></dxf>
                </dxfs>
                <tableStyles count="1"><tableStyle name="RenderedLayers" count="6">
                    <tableStyleElement type="wholeTable" dxfId="0"/>
                    <tableStyleElement type="headerRow" dxfId="1"/>
                    <tableStyleElement type="totalRow" dxfId="2"/>
                    <tableStyleElement type="firstRowStripe" dxfId="3"/>
                    <tableStyleElement type="secondRowStripe" dxfId="4"/>
                    <tableStyleElement type="firstColumn" dxfId="5"/>
                </tableStyle></tableStyles>
            </styleSheet>"#,
            r#"<worksheet><cols><col min="1" max="1" style="1"/></cols><sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>Left</t></is></c><c r="B1" t="inlineStr"><is><t>Right</t></is></c></row>
                <row r="2" s="2" customFormat="1"><c r="A2" s="3"><v>1</v></c><c r="B2"><v>2</v></c></row>
                <row r="3"><c r="A3"><v>3</v></c><c r="B3"><v>4</v></c></row>
                <row r="4"><c r="A4"><v>5</v></c><c r="B4"><v>6</v></c></row>
            </sheetData>
            <conditionalFormatting sqref="B2"><cfRule type="cellIs" dxfId="6" priority="1" stopIfTrue="1" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting>
            <tableParts count="1"><tablePart r:id="rIdTable"/></tableParts></worksheet>"#,
            r#"<table id="1" name="RenderedTable" displayName="RenderedTable" ref="A1:B4" headerRowCount="1" totalsRowCount="1"><tableColumns count="2"><tableColumn id="1" name="Left"/><tableColumn id="2" name="Right"/></tableColumns><tableStyleInfo name="RenderedLayers" showFirstColumn="1" showLastColumn="0" showRowStripes="1" showColumnStripes="0"/></table>"#,
        );
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 3, 1)),
            gridlines: false,
            ..RenderOptions::default()
        };
        let first = render_sheet_svg(&workbook, 0, &options).unwrap();
        let second = render_sheet_svg(&workbook, 0, &options).unwrap();
        assert_eq!(first.scene, second.scene);
        assert_eq!(first.svg.as_bytes(), second.svg.as_bytes());

        let fills = first
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Rect(node) => node.fill,
                _ => None,
            })
            .collect::<Vec<_>>();
        for (color, expected) in [
            (Rgb::new(0x14, 0x14, 0x14), 2),
            (Rgb::new(0x63, 0x63, 0x63), 1),
            (Rgb::new(0xC8, 0xC8, 0xC8), 1),
            (Rgb::new(0x32, 0x32, 0x32), 2),
            (Rgb::new(0x1E, 0x1E, 0x1E), 2),
        ] {
            assert_eq!(
                fills.iter().filter(|fill| **fill == color).count(),
                expected,
                "fills: {fills:?}"
            );
        }
        assert_eq!(
            fills
                .iter()
                .filter(|fill| **fill == Rgb::new(0x28, 0x28, 0x28))
                .count(),
            0,
            "direct and conditional layers cover both first-stripe cells"
        );
        let direct_text = first
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Text(node) if node.text == "1.00" => Some(node),
                _ => None,
            })
            .expect("direct cell text");
        assert!(
            !direct_text.style.bold,
            "the resolved row XF replaces the lower-precedence column XF"
        );
        assert!(direct_text.style.italic, "row style must survive");
        assert_eq!(direct_text.style.color, Rgb::new(0x3C, 0x3C, 0x3C));
    }

    #[test]
    fn used_selection_preflights_sparse_materialized_cells_before_dense_extent_work() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("sparse-preflight");
        sheet.write(0, 0, "a");
        sheet.write(0, 1, "b");
        sheet.write(1, 0, "c");
        let mut options = RenderOptions::default();
        options.limits.max_cells = 2;
        assert_eq!(
            build_scene(&workbook, 0, &options),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::Cells,
                limit: 2,
                actual: 3,
            }),
            "the sparse preflight must fail at the third materialized cell, before the 2x2 extent"
        );
    }

    #[test]
    fn imported_conditional_priority_stop_and_dxf_overlay_are_exact_and_deterministic() {
        let mut workbook = imported_xlsx(
            r#"<styleSheet>
                <fonts count="2"><font/><font><b/><color rgb="FF112233"/></font></fonts>
                <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
                <borders count="2"><border/><border><left style="thin"><color rgb="FF010203"/></left></border></borders>
                <cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="1"/></cellXfs>
                <dxfs count="3">
                    <dxf><fill><patternFill patternType="solid"><fgColor rgb="FFFF0000"/></patternFill></fill></dxf>
                    <dxf><font><color rgb="FF663399"/></font><fill><patternFill patternType="solid"><fgColor rgb="FF0000FF"/></patternFill></fill><border><bottom style="medium"><color rgb="FFAABBCC"/></bottom></border><numFmt numFmtId="2" formatCode="0.00"/><protection locked="0"/></dxf>
                    <dxf><font><i/></font><fill><patternFill patternType="solid"><fgColor rgb="FF00FF00"/></patternFill></fill></dxf>
                </dxfs>
            </styleSheet>"#,
            r#"<worksheet><sheetData><row r="1"><c r="A1" s="1"><v>5</v></c></row></sheetData>
                <conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="0" priority="10" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting>
                <conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="1" priority="1" stopIfTrue="1" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting>
                <conditionalFormatting sqref="A1"><cfRule type="cellIs" dxfId="2" priority="2" operator="greaterThan"><formula>0</formula></cfRule></conditionalFormatting>
            </worksheet>"#,
        );
        workbook.sheets[0].write_styled(
            0,
            0,
            5,
            &CellStyle {
                font: Some(
                    rxls::Font::new()
                        .bold()
                        .with_color(Color::rgb(0x11, 0x22, 0x33)),
                ),
                border: Some(
                    Border::new()
                        .with_left(BorderStyle::Thin)
                        .with_left_color(Color::rgb(1, 2, 3)),
                ),
                ..CellStyle::default()
            },
        );
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            gridlines: false,
            ..RenderOptions::default()
        };
        assert_eq!(
            workbook.sheets[0]
                .cell_style(0, 0)
                .and_then(|style| style.border.as_ref())
                .map(|border| (border.left, border.left_color)),
            Some((BorderStyle::Thin, Some(Color::rgb(1, 2, 3))))
        );
        let first = render_sheet_svg(&workbook, 0, &options).unwrap();
        let second = render_sheet_svg(&workbook, 0, &options).unwrap();
        assert_eq!(first.scene, second.scene);
        assert_eq!(first.svg.as_bytes(), second.svg.as_bytes());

        let fill = first.scene.nodes.iter().find_map(|node| match node {
            SceneNode::Rect(node) => node.fill,
            _ => None,
        });
        assert_eq!(fill, Some(Rgb::new(0, 0, 255)));
        let text = first
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Text(node) => Some(node),
                _ => None,
            })
            .expect("conditional text");
        assert!(
            text.style.bold,
            "base bold font must survive a color-only dxf font"
        );
        assert!(
            !text.style.italic,
            "stopIfTrue must block the lower-priority italic dxf"
        );
        assert_eq!(text.style.color, Rgb::new(0x66, 0x33, 0x99));
        let lines = first
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Line(line) => Some(line),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            lines.iter().any(|line| line.color == Rgb::new(1, 2, 3)),
            "line colors: {:?}",
            lines.iter().map(|line| line.color).collect::<Vec<_>>()
        );
        assert!(lines
            .iter()
            .any(|line| line.color == Rgb::new(0xAA, 0xBB, 0xCC)));
        assert!(first.report.warnings.iter().any(|warning| {
            warning.code == WarningCode::ConditionalFormattingDeferred && warning.occurrences == 2
        }));
    }

    #[test]
    fn authored_and_round_tripped_imported_style_snapshots_match() {
        let mut authored = Workbook::new();
        let style = CellStyle {
            font: Some(
                rxls::Font::new()
                    .with_name("Liberation Sans")
                    .bold()
                    .with_color(Color::rgb(12, 34, 56)),
            ),
            fill: Some(Color::rgb(210, 220, 230)),
            pattern_fill: Some(rxls::Fill::solid(Color::rgb(210, 220, 230))),
            border: Some(
                Border::new()
                    .with_all(BorderStyle::Thin)
                    .with_color(Color::rgb(70, 80, 90)),
            ),
            ..CellStyle::default()
        };
        let sheet = authored.add_sheet("snapshot");
        // Pin geometry explicitly: a re-opened OOXML sheet with no
        // sheetFormatPr intentionally carries Calc's imported application
        // default, while a purely authored sheet retains the caller fallback.
        sheet.set_default_row_height(15.0);
        sheet.write_styled(0, 0, 5, &style);
        sheet.add_conditional_format(CondFormat::new(
            (0, 0, 0, 0),
            CfRule::cell_is(DvOp::GreaterThan, "0", None::<&str>, Color::rgb(1, 2, 3)),
        ));
        let imported = Workbook::open(&authored.to_xlsx()).expect("round-tripped workbook");
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            gridlines: false,
            ..RenderOptions::default()
        };
        let authored_scene = build_scene(&authored, 0, &options).unwrap();
        let imported_scene = build_scene(&imported, 0, &options).unwrap();
        assert_eq!(authored_scene.scene, imported_scene.scene);
    }

    #[test]
    fn conditional_references_and_duplicate_values_are_exact_for_bounded_subset() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("O'Brien");
        for (row, (left, right)) in [(10, 5), (20, 25), (20, 15)].into_iter().enumerate() {
            sheet.write_number(row as u32, 0, left);
            sheet.write_number(row as u32, 1, right);
        }
        for (row, value) in ["Alpha", "alpha", "Beta"].into_iter().enumerate() {
            sheet.write(row as u32, 2, value);
        }
        sheet.add_conditional_format(CondFormat::new(
            (0, 1, 2, 1),
            CfRule::cell_is(
                DvOp::GreaterThan,
                "'O''Brien'!$A1",
                None::<&str>,
                Color::rgb(255, 0, 0),
            ),
        ));
        sheet.add_conditional_format(CondFormat::new(
            (0, 2, 2, 2),
            CfRule::duplicate_values(false, Color::rgb(0, 255, 0)),
        ));
        sheet.add_conditional_format(CondFormat::new(
            (0, 0, 2, 0),
            CfRule::expression("$B1>$A1", Color::rgb(0, 0, 255)),
        ));

        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 2, 2)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let fills = build
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Rect(node) => node.fill,
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            fills
                .iter()
                .filter(|color| **color == Rgb::new(255, 0, 0))
                .count(),
            1,
            "only B2 is greater than the row-relative absolute-column A reference"
        );
        assert_eq!(
            fills
                .iter()
                .filter(|color| **color == Rgb::new(0, 255, 0))
                .count(),
            2,
            "ASCII duplicate matching is case-insensitive"
        );
        assert_eq!(
            fills
                .iter()
                .filter(|color| **color == Rgb::new(0, 0, 255))
                .count(),
            1,
            "only A2 has B greater than A"
        );
        assert!(!build
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::ConditionalFormattingDeferred));
    }

    #[test]
    fn duplicate_wildcards_and_sparse_ranges_remain_deferred() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("deferred");
        sheet.write(0, 0, "*");
        sheet.write(1, 0, "*");
        sheet.add_conditional_format(CondFormat::new(
            (0, 0, 1, 0),
            CfRule::duplicate_values(false, Color::rgb(1, 2, 3)),
        ));
        sheet.add_conditional_format(CondFormat::new(
            (0, 1, 1, 1),
            CfRule::duplicate_values(true, Color::rgb(4, 5, 6)),
        ));
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 1, 1)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        assert!(build.report.warnings.iter().any(|warning| {
            warning.code == WarningCode::ConditionalFormattingDeferred && warning.occurrences == 2
        }));
    }

    #[test]
    fn conditional_and_media_limits_fail_before_expansion() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("limits");
        sheet.write_number(0, 0, 1);
        sheet.add_conditional_format(CondFormat::new(
            (0, 0, 0, 0),
            CfRule::cell_is(DvOp::Equal, "1", None::<&str>, Color::rgb(1, 2, 3)),
        ));
        sheet.add_image(Image::new([137, 80, 78, 71], ImageFmt::Png, (0, 0)));

        let mut options = RenderOptions::default();
        options.limits.max_conditional_rules = 0;
        assert_eq!(
            build_scene(&workbook, 0, &options),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::ConditionalRules,
                limit: 0,
                actual: 1,
            })
        );
        options.limits.max_conditional_rules = 1;
        options.limits.max_media_bytes = 3;
        assert_eq!(
            build_scene(&workbook, 0, &options),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::MediaBytes,
                limit: 3,
                actual: 4,
            })
        );
        options.limits.max_media_bytes = 4;
        options.limits.max_conditional_evaluations = 0;
        assert_eq!(
            build_scene(&workbook, 0, &options),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::ConditionalEvaluations,
                limit: 0,
                actual: 1,
            })
        );
    }

    #[test]
    fn images_charts_and_sparklines_use_deterministic_geometric_placeholders() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("drawings");
        sheet.write(3, 3, "extent");
        sheet.add_image(Image::new([137, 80, 78, 71], ImageFmt::Png, (0, 0)).with_to((2, 2)));
        sheet.add_chart(Chart::new(ChartKind::Line, (1, 1), (3, 3)));
        sheet.add_sparkline(
            Sparkline::new((0, 3), "drawings!$A$1:$A$3").with_kind(SparklineKind::Column),
        );
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 3, 3)),
            gridlines: false,
            ..RenderOptions::default()
        };
        let first = build_scene(&workbook, 0, &options).unwrap();
        let second = build_scene(&workbook, 0, &options).unwrap();
        assert_eq!(first, second);
        for code in [
            WarningCode::ImagePlaceholder,
            WarningCode::ChartPlaceholder,
            WarningCode::SparklinePlaceholder,
        ] {
            assert!(first
                .report
                .warnings
                .iter()
                .any(|warning| warning.code == code && warning.occurrences == 1));
        }
        assert!(first.scene.nodes.iter().any(|node| matches!(
            node,
            SceneNode::Rect(RectNode {
                rect: Rect {
                    width,
                    height,
                    ..
                },
                fill: Some(Rgb {
                    red: 242,
                    green: 242,
                    blue: 242,
                }),
                ..
            }) if *width == Fixed::from_pixels(128) && *height == Fixed::from_pixels(40)
        )));
    }

    #[test]
    fn drawings_continue_across_explicit_range_and_print_tile_boundaries() {
        fn contains_placeholder(nodes: &[SceneNode]) -> bool {
            nodes.iter().any(|node| match node {
                SceneNode::ClipGroup(group) => contains_placeholder(&group.nodes),
                SceneNode::Rect(RectNode {
                    fill:
                        Some(Rgb {
                            red: 242,
                            green: 242,
                            blue: 242,
                        }),
                    ..
                }) => true,
                _ => false,
            })
        }

        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("continued-drawing");
        sheet.add_image(Image::new([137, 80, 78, 71], ImageFmt::Png, (0, 0)).with_to((4, 4)));
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(2, 2, 4, 4)),
            gridlines: false,
            ..RenderOptions::default()
        };
        let build = build_scene(&workbook, 0, &options).unwrap();
        assert!(build.report.warnings.iter().any(|warning| {
            warning.code == WarningCode::ImagePlaceholder && warning.occurrences == 1
        }));
        assert!(!build
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::DrawingAnchorUnavailable));
        assert!(build.scene.nodes.iter().any(|node| matches!(
            node,
            SceneNode::Rect(RectNode {
                rect: Rect {
                    x,
                    y,
                    width,
                    height,
                },
                fill: Some(Rgb {
                    red: 242,
                    green: 242,
                    blue: 242,
                }),
                ..
            }) if *x == Fixed::ZERO
                && *y == Fixed::ZERO
                && *width == Fixed::from_pixels(128)
                && *height == Fixed::from_pixels(40)
        )));

        let mut paginated = Workbook::new();
        let sheet = paginated.add_sheet("continued-print-drawing");
        sheet.add_image(Image::new([137, 80, 78, 71], ImageFmt::Png, (0, 0)).with_to((8, 4)));
        sheet.set_page_setup(
            PageSetup::new()
                .with_print_area((0, 0, 8, 4))
                .with_paper_size(1)
                .with_scale(400),
        );
        let document = build_print_document(
            &paginated,
            0,
            &PrintOptions {
                omit_sparse_pages: false,
                ..PrintOptions::default()
            },
        )
        .unwrap();
        assert!(document.pages.len() > 1);
        assert!(document
            .pages
            .iter()
            .skip(1)
            .any(|page| contains_placeholder(&page.scene.nodes)));
    }

    #[test]
    fn valid_png_images_decode_to_backend_neutral_rgba_nodes() {
        let rgba = [
            255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 64, 255, 255, 255, 0,
        ];
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("decoded-image");
        sheet.add_image(
            Image::new(test_rgba_png(2, 2, &rgba), ImageFmt::Png, (0, 0)).with_to((1, 1)),
        );
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 1, 1)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let image = build
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Image(node) => Some(node),
                _ => None,
            })
            .expect("decoded image node");
        assert_eq!((image.pixel_width, image.pixel_height), (2, 2));
        assert_eq!(image.rgba.as_ref(), rgba);
        assert_eq!(
            image.rect,
            Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(64),
                height: Fixed::from_pixels(20),
            }
        );
        assert!(!build
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::ImagePlaceholder));

        let mut limited = RenderOptions::default();
        limited.limits.max_image_pixels = 3;
        assert_eq!(
            build_scene(&workbook, 0, &limited),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::ImagePixels,
                limit: 3,
                actual: 4,
            })
        );
    }

    #[test]
    fn same_sheet_a1_charts_and_sparklines_render_real_bounded_geometry() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("charts");
        for (row, (label, value, x, size)) in [
            ("Jan", 10.0, 1.0, 4.0),
            ("Feb", 20.0, 2.0, 16.0),
            ("Mar", 15.0, 3.0, 9.0),
        ]
        .into_iter()
        .enumerate()
        {
            sheet.write(row as u32, 0, label);
            sheet.write_number(row as u32, 1, value);
            sheet.write_number(row as u32, 2, x);
            sheet.write_number(row as u32, 3, size);
        }
        let categorical = || {
            Series::new("charts!$B$1:$B$3")
                .with_categories("charts!$A$1:$A$3")
                .with_name("Revenue")
        };
        sheet.add_chart(
            Chart::new(ChartKind::Line, (0, 4), (8, 10))
                .with_title("Line")
                .with_x_axis_title("Month")
                .with_y_axis_title("Value")
                .with_legend(true)
                .with_data_labels(true)
                .add_series(categorical()),
        );
        sheet.add_chart(
            Chart::new(ChartKind::Pie, (9, 4), (17, 10))
                .with_title("Pie")
                .with_legend(true)
                .with_data_labels(true)
                .add_series(categorical()),
        );
        sheet.add_chart(
            Chart::new(ChartKind::Scatter, (18, 4), (26, 10))
                .with_title("Scatter")
                .add_series(
                    Series::new("charts!$B$1:$B$3")
                        .with_categories("charts!$C$1:$C$3")
                        .with_name("XY"),
                ),
        );
        sheet.add_chart(
            Chart::new(ChartKind::Bar, (27, 4), (35, 10))
                .with_title("Columns")
                .add_series(categorical()),
        );
        sheet.add_chart(
            Chart::new(ChartKind::Area, (36, 4), (44, 10))
                .with_title("Area")
                .add_series(categorical()),
        );
        sheet.add_chart(
            Chart::new(ChartKind::Doughnut, (45, 4), (53, 10))
                .with_title("Doughnut")
                .add_series(categorical()),
        );
        sheet.add_chart(
            Chart::new(ChartKind::Radar, (54, 4), (62, 10))
                .with_title("Radar")
                .add_series(categorical()),
        );
        sheet.add_chart(
            Chart::new(ChartKind::Bubble, (63, 4), (71, 10))
                .with_title("Bubble")
                .add_series(
                    Series::new("charts!$B$1:$B$3")
                        .with_categories("charts!$C$1:$C$3")
                        .with_bubble_sizes("charts!$D$1:$D$3")
                        .with_name("Bubbles"),
                ),
        );
        for (row, kind) in [
            SparklineKind::Line,
            SparklineKind::Column,
            SparklineKind::WinLoss,
        ]
        .into_iter()
        .enumerate()
        {
            sheet.add_sparkline(
                Sparkline::new((row as u32, 11), "charts!$B$1:$B$3").with_kind(kind),
            );
        }

        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 71, 11)),
            gridlines: false,
            ..RenderOptions::default()
        };
        let build = build_scene(&workbook, 0, &options).unwrap();
        assert!(!build.report.warnings.iter().any(|warning| matches!(
            warning.code,
            WarningCode::ChartPlaceholder | WarningCode::SparklinePlaceholder
        )));
        assert!(
            build
                .scene
                .nodes
                .iter()
                .any(|node| matches!(node, SceneNode::Path(_))),
            "pie wedges use filled paths"
        );
        for title in [
            "Line", "Pie", "Scatter", "Columns", "Area", "Doughnut", "Radar", "Bubble",
        ] {
            assert!(build.scene.nodes.iter().any(|node| match node {
                SceneNode::Text(node) => node.text == title,
                _ => false,
            }));
        }
        for (text, role) in [
            ("Line", ChartTextRole::ChartTitle),
            ("Month", ChartTextRole::AxisTitle),
            ("Value", ChartTextRole::AxisTitle),
            ("Revenue", ChartTextRole::Legend),
            ("Jan", ChartTextRole::AxisLabel),
            ("10", ChartTextRole::DataLabel),
        ] {
            assert!(
                build.scene.nodes.iter().any(|node| match node {
                    SceneNode::Text(node) => {
                        node.text == text
                            && node.style.family == options.default_font_family
                            && node.style.size == role.size()
                            && node.style.bold == role.bold()
                    }
                    _ => false,
                }),
                "missing exact {role:?} style for {text:?}"
            );
        }

        let mut limited = options;
        limited.limits.max_chart_points = 5;
        assert_eq!(
            build_scene(&workbook, 0, &limited),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::ChartPoints,
                limit: 5,
                actual: 6,
            })
        );
    }

    #[test]
    fn imported_sparklines_defer_paint_when_calc_omits_ooxml_extensions() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("imported_sparkline");
        for (col, value) in [10.0, 20.0, 15.0, 25.0].into_iter().enumerate() {
            sheet.write_number(0, (col + 1) as u16, value);
        }
        sheet.add_sparkline(Sparkline::new((0, 0), "imported_sparkline!$B$1:$E$1"));
        let workbook = Workbook::open(&workbook.to_xlsx()).expect("reopen imported sparkline");
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 4)),
            gridlines: false,
            ..RenderOptions::default()
        };
        let build = build_scene(&workbook, 0, &options).unwrap();
        assert!(!build
            .scene
            .nodes
            .iter()
            .any(|node| { matches!(node, SceneNode::Line(_) | SceneNode::Path(_)) }));
        assert!(!build
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::SparklinePlaceholder));
    }

    #[test]
    fn authored_chart_text_uses_point_defaults_with_the_configured_family() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Render");
        for (column, category) in ["Q1", "Q2", "Q3", "Q4"].into_iter().enumerate() {
            sheet.write(5, column as u16 + 1, category);
        }
        sheet.write(6, 0, "Series 0000");
        for (column, value) in [28.0, 41.0, 54.0, 67.0].into_iter().enumerate() {
            sheet.write_number(6, column as u16 + 1, value);
        }
        sheet.add_chart(
            Chart::new(ChartKind::Line, (7, 0), (18, 6)).add_series(
                Series::new("Render!$B$7:$E$7")
                    .with_categories("Render!$B$6:$E$6")
                    .with_name("Series 0000"),
            ),
        );
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 18, 6)),
            gridlines: false,
            default_font_family: "Noto Sans CJK KR".to_string(),
            ..RenderOptions::default()
        };
        let build = build_scene(&workbook, 0, &options).unwrap();
        assert!(!build
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::ChartPlaceholder));

        let chart_size = points_to_fixed(10.0).unwrap();
        for expected in [
            "Q1", "Q2", "Q3", "Q4", "0", "10", "20", "30", "40", "50", "60", "70", "80",
        ] {
            assert!(
                build.scene.nodes.iter().any(|node| match node {
                    SceneNode::Text(node) => {
                        node.text == expected
                            && node.style.family == options.default_font_family
                            && node.style.size == chart_size
                            && !node.style.bold
                    }
                    _ => false,
                }),
                "missing xlsx-0000-like chart label {expected:?}"
            );
        }
        assert!(build.scene.nodes.iter().any(|node| match node {
            SceneNode::Text(node) => {
                node.text == "Q1"
                    && node.style.family == "Noto Sans CJK KR"
                    && node.style.size == options.default_font_size
            }
            _ => false,
        }));

        let horizontal_axis = build
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Line(line)
                    if line.color == Rgb::BLACK && line.y1 == line.y2 && line.x1 < line.x2 =>
                {
                    Some(line)
                }
                _ => None,
            })
            .expect("chart horizontal axis");
        let vertical_axis = build
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Line(line)
                    if line.color == Rgb::BLACK && line.x1 == line.x2 && line.y1 < line.y2 =>
                {
                    Some(line)
                }
                _ => None,
            })
            .expect("chart vertical axis");
        assert_eq!(vertical_axis.x1, horizontal_axis.x1);
        assert_eq!(vertical_axis.y2, horizontal_axis.y1);
        assert!(build
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Line(line) if line.color == Rgb::new(0x44, 0x72, 0xC4) => Some(line),
                _ => None,
            })
            .all(|line| {
                line.x1 >= horizontal_axis.x1
                    && line.x1 <= horizontal_axis.x2
                    && line.x2 >= horizontal_axis.x1
                    && line.x2 <= horizontal_axis.x2
                    && line.y1 >= vertical_axis.y1
                    && line.y1 <= vertical_axis.y2
                    && line.y2 >= vertical_axis.y1
                    && line.y2 <= vertical_axis.y2
            }));
    }

    #[test]
    fn chart_text_roles_are_source_exact_and_do_not_change_non_chart_text() {
        for (role, points, bold) in [
            (ChartTextRole::ChartTitle, 18.0, true),
            (ChartTextRole::AxisTitle, 10.0, true),
            (ChartTextRole::AxisLabel, 10.0, false),
            (ChartTextRole::Legend, 10.0, false),
            (ChartTextRole::DataLabel, 10.0, false),
        ] {
            let style = role.style("Theme Sans", TextAnchor::Start, 0);
            assert_eq!(style.family, "Theme Sans");
            assert_eq!(style.size, points_to_fixed(points).unwrap());
            assert_eq!(style.bold, bold);
        }

        let mut workbook = Workbook::new();
        workbook.add_sheet("control").write(0, 0, "worksheet-only");
        let options = RenderOptions {
            selection: RenderSelection::Range(RenderRange::new(0, 0, 0, 0)),
            gridlines: false,
            default_font_family: "Noto Sans CJK KR".to_string(),
            default_font_size: points_to_fixed(11.0).unwrap(),
            ..RenderOptions::default()
        };
        let build = build_scene(&workbook, 0, &options).unwrap();
        let text = build
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Text(node) if node.text == "worksheet-only" => Some(node),
                _ => None,
            })
            .expect("non-chart control text");
        assert_eq!(text.style.family, options.default_font_family);
        assert_eq!(text.style.size, options.default_font_size);
        assert!(!text.style.bold);
    }

    #[test]
    fn chart_gutters_use_verified_shaped_advances_when_a_pack_is_present() {
        let pack = synthetic_test_pack();
        let options = RenderOptions {
            default_font_family: "worksheet-family-must-not-leak".to_string(),
            font_pack: Some(pack.clone()),
            ..RenderOptions::default()
        };
        let role = ChartTextRole::AxisLabel;
        let style = role.resolved_style(CALC_MISSING_THEME_CHART_LATIN_FAMILY);
        let shaped = shape_text(
            &pack,
            "WWW",
            style.request(),
            BaseDirection::LeftToRight,
            &options,
        )
        .unwrap();
        let expected_width = shaped_width(&pack, &shaped, style.size)
            .unwrap()
            .checked_add(Fixed::from_pixels(4))
            .unwrap();
        let expected_height = line_height_from_metrics(
            styled_line_metrics(
                &pack,
                &shaped,
                std::slice::from_ref(&style),
                CellLineLayoutPolicy::Native,
                1,
                1,
            )
            .unwrap(),
            CellLineLayoutPolicy::Native,
        )
        .unwrap()
        .checked_add(Fixed::from_pixels(4))
        .unwrap();
        let measured = measure_chart_text(
            "WWW",
            role,
            CALC_MISSING_THEME_CHART_LATIN_FAMILY,
            &options,
            None,
        )
        .unwrap();
        assert_eq!(
            measured,
            ChartTextMetrics {
                width: expected_width,
                height: expected_height,
            }
        );

        let mut nodes = Vec::new();
        let mut text_bytes = 0;
        let mut glyphs = 0;
        let mut typography_stats = TypographyStats::default();
        push_chart_text(
            &mut nodes,
            "WWW".to_string(),
            Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: measured.width,
                height: measured.height,
            },
            TextAnchor::Start,
            0,
            role,
            CALC_MISSING_THEME_CHART_LATIN_FAMILY,
            &mut text_bytes,
            &mut glyphs,
            &mut typography_stats,
            &options,
        )
        .unwrap();
        let SceneNode::GlyphRun(run) = &nodes[0] else {
            panic!("verified chart text must be outlined");
        };
        assert_eq!(run.clip_bounds.width, measured.width);
        assert_eq!(run.clip_bounds.height, measured.height);
        assert!(run.glyphs.iter().all(|glyph| glyph.size == role.size()));
    }

    #[test]
    fn packless_chart_gutters_use_the_same_proportional_helvetica_advances_as_pdf() {
        let options = RenderOptions::default();
        let role = ChartTextRole::AxisLabel;
        let wide = measure_chart_text("WWW", role, "Chart Sans", &options, None).unwrap();
        let narrow = measure_chart_text("iii", role, "Chart Sans", &options, None).unwrap();
        let padding = Fixed::from_pixels(4);
        let expected_wide = Fixed::from_raw(role.size().raw() * (944 * 3) / 1_000)
            .checked_add(padding)
            .unwrap();
        let expected_narrow = Fixed::from_raw(role.size().raw() * (222 * 3) / 1_000)
            .checked_add(padding)
            .unwrap();

        assert_eq!(wide.width, expected_wide);
        assert_eq!(narrow.width, expected_narrow);
        assert!(wide.width > narrow.width);
    }

    #[test]
    fn packless_chart_kerning_falls_back_with_explicit_approximation_provenance() {
        let options = RenderOptions::default();
        let mut style = ResolvedChartTextStyle::for_role(ChartTextRole::AxisLabel, "Chart Sans");
        style.kerning_minimum_hundredths_of_point =
            Some(style.size_hundredths_of_point.saturating_add(1));
        assert!(!style.kerning());
        assert!(measure_chart_text_with_style("AV", &style, &options, None).is_ok());
        let node = build_auxiliary_text_node_with_kerning(
            "AV".to_string(),
            Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(80),
                height: Fixed::from_pixels(24),
            },
            Fixed::from_pixels(2),
            style.text_style(TextAnchor::Start, 0),
            style.kerning(),
            &options,
        )
        .unwrap();
        assert!(matches!(node, SceneNode::Text(_)));

        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("chart");
        for (row, (category, value)) in [("A", 1.0), ("B", 2.0)].into_iter().enumerate() {
            sheet.write(row as u32, 0, category);
            sheet.write_number(row as u32, 1, value);
        }
        let chart = Chart::new(ChartKind::Line, (0, 0), (10, 8))
            .add_series(Series::new("chart!$B$1:$B$2").with_categories("chart!$A$1:$A$2"));
        let warning_cell = CellCoordinate { row: 0, col: 0 };
        let mut nodes = Vec::new();
        let mut warnings = Warnings::default();
        assert!(try_push_chart(
            &mut nodes,
            Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(500),
                height: Fixed::from_pixels(300),
            },
            &chart,
            None,
            sheet,
            &mut 0,
            &mut 0,
            &mut 0,
            &mut TypographyStats::default(),
            &options,
            &mut warnings,
            warning_cell,
        )
        .unwrap());
        assert!(nodes.iter().any(|node| matches!(node, SceneNode::Text(_))));
        assert_eq!(
            warnings.finish(),
            [RenderWarning {
                code: WarningCode::ApproximateTextMetrics,
                occurrences: 1,
                first_cell: Some(warning_cell),
            }]
        );

        let pie = Chart::new(ChartKind::Pie, (0, 0), (10, 8))
            .add_series(Series::new("chart!$B$1:$B$2").with_categories("chart!$A$1:$A$2"));
        let mut pie_warnings = Warnings::default();
        assert!(try_push_chart(
            &mut Vec::new(),
            Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(500),
                height: Fixed::from_pixels(300),
            },
            &pie,
            None,
            sheet,
            &mut 0,
            &mut 0,
            &mut 0,
            &mut TypographyStats::default(),
            &options,
            &mut pie_warnings,
            warning_cell,
        )
        .unwrap());
        assert!(pie_warnings.finish().is_empty());
    }

    #[test]
    fn scatter_and_bubble_points_share_the_nice_x_axis_used_by_labels() {
        let mut style = ChartSeriesStyle::default();
        style.marker = ChartMarkerSymbol::None;
        style.line_color = Some(Color::rgb(0x12, 0x34, 0x56));
        let series = [ResolvedChartSeries {
            name: "Series".to_string(),
            values: vec![1.0, 2.0],
            x_values: Some(vec![11.0, 19.0]),
            labels: Vec::new(),
            bubble_sizes: Some(vec![1.0, 1.0]),
            style,
        }];
        let axis = chart_nice_x_axis(&series).unwrap();
        assert_eq!((axis.minimum, axis.maximum), (10.0, 20.0));
        assert_eq!(axis.ticks.first(), Some(&axis.minimum));
        assert_eq!(axis.ticks.last(), Some(&axis.maximum));
        let data_bounds = chart_x_data_bounds(&series).unwrap();
        let retained_ticks = axis
            .ticks
            .iter()
            .copied()
            .filter(|value| *value >= data_bounds.0 && *value <= data_bounds.1)
            .collect::<Vec<_>>();
        assert_eq!(retained_ticks.first(), Some(&11.0));
        assert_eq!(retained_ticks.last(), Some(&19.0));
        let plot = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(100),
            height: Fixed::from_pixels(100),
        };
        let options = RenderOptions::default();

        let mut scatter_nodes = Vec::new();
        push_scatter_chart(
            &mut scatter_nodes,
            plot,
            &series,
            (0.0, 2.0),
            &axis,
            &[],
            false,
            &mut Vec::new(),
            &mut TypographyStats::default(),
            &options,
        )
        .unwrap();
        let scatter_line = scatter_nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Line(line) if line.color == Rgb::new(0x12, 0x34, 0x56) => Some(line),
                _ => None,
            })
            .expect("retained scatter line");
        assert_eq!(scatter_line.x1, Fixed::from_pixels(10));
        assert_eq!(scatter_line.x2, Fixed::from_pixels(90));

        let mut bubble_nodes = Vec::new();
        push_bubble_chart(
            &mut bubble_nodes,
            plot,
            &series,
            (0.0, 2.0),
            &axis,
            &[],
            false,
            &mut Vec::new(),
            &mut TypographyStats::default(),
            &options,
        )
        .unwrap();
        let bubble_centers = bubble_nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Path(path) => {
                    let x_values = path.commands.iter().filter_map(|command| match command {
                        PathCommand::MoveTo { x, .. }
                        | PathCommand::LineTo { x, .. }
                        | PathCommand::QuadraticTo { x, .. }
                        | PathCommand::CubicTo { x, .. } => Some(x.raw()),
                        PathCommand::Close => None,
                    });
                    let (minimum, maximum) = x_values
                        .fold((i64::MAX, i64::MIN), |(minimum, maximum), value| {
                            (minimum.min(value), maximum.max(value))
                        });
                    Some(Fixed::from_raw(minimum + (maximum - minimum) / 2))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            bubble_centers,
            [Fixed::from_pixels(10), Fixed::from_pixels(90)]
        );
    }

    #[test]
    fn chart_category_sampling_preserves_endpoints_without_exceeding_its_bound() {
        for count in 0..=4_096_usize {
            let stride = chart_category_label_stride(count);
            let retained = (0..count)
                .filter(|index| chart_category_label_is_retained(*index, count, stride))
                .collect::<Vec<_>>();
            assert!(retained.len() <= MAX_CHART_CATEGORY_LABELS, "count={count}");
            if count == 0 {
                assert!(retained.is_empty());
            } else {
                assert_eq!(retained.first(), Some(&0), "count={count}");
                assert_eq!(retained.last(), Some(&(count - 1)), "count={count}");
                if count <= MAX_CHART_CATEGORY_LABELS {
                    assert_eq!(retained.len(), count, "count={count}");
                }
            }
        }
    }

    #[test]
    fn imported_line_chart_value_axis_keeps_calc_zero_baseline() {
        let mut style = ChartSeriesStyle::default();
        style.marker = ChartMarkerSymbol::None;
        let series = [ResolvedChartSeries {
            name: "Imported line".to_string(),
            values: vec![51.0, 64.0, 77.0, 90.0],
            x_values: None,
            labels: vec![
                "Q1".to_string(),
                "Q2".to_string(),
                "Q3".to_string(),
                "Q4".to_string(),
            ],
            bubble_sizes: None,
            style,
        }];
        let authored = chart_nice_value_axis(&series, false).unwrap();
        let imported = chart_nice_value_axis(&series, true).unwrap();
        assert_eq!((authored.minimum, authored.maximum), (45.0, 95.0));
        assert_eq!((imported.minimum, imported.maximum), (0.0, 100.0));
        assert_eq!(imported.ticks, vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0]);
    }

    #[test]
    fn finite_extreme_chart_axes_fail_closed_before_nonfinite_geometry() {
        let mut style = ChartSeriesStyle::default();
        style.marker = ChartMarkerSymbol::None;
        let constant_extreme_value = [ResolvedChartSeries {
            name: "Constant extreme value".to_string(),
            values: vec![f64::MAX],
            x_values: None,
            labels: vec!["A".to_string()],
            bubble_sizes: None,
            style: style.clone(),
        }];
        assert!(chart_nice_value_axis(&constant_extreme_value, false).is_none());
        let derived_extreme_value_span = [ResolvedChartSeries {
            name: "Derived extreme value span".to_string(),
            values: vec![-f64::MAX, f64::MAX],
            x_values: None,
            labels: vec!["A".to_string(), "B".to_string()],
            bubble_sizes: None,
            style: style.clone(),
        }];
        assert!(chart_nice_value_axis(&derived_extreme_value_span, false).is_none());
        let subnormal_value_span = [ResolvedChartSeries {
            name: "Subnormal value span".to_string(),
            values: vec![0.0, f64::from_bits(1)],
            x_values: None,
            labels: vec!["A".to_string(), "B".to_string()],
            bubble_sizes: None,
            style: style.clone(),
        }];
        assert!(chart_nice_value_axis(&subnormal_value_span, false).is_none());
        let extreme_x = [ResolvedChartSeries {
            name: "Extreme x".to_string(),
            values: vec![1.0, 2.0],
            x_values: Some(vec![-f64::MAX, f64::MAX]),
            labels: Vec::new(),
            bubble_sizes: None,
            style,
        }];
        assert!(chart_nice_x_axis(&extreme_x).is_none());

        let options = RenderOptions::default();
        let rect = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(500),
            height: Fixed::from_pixels(300),
        };
        let mut value_workbook = Workbook::new();
        let value_sheet = value_workbook.add_sheet("extreme_value");
        for (row, (category, value)) in [("A", -f64::MAX), ("B", f64::MAX)].into_iter().enumerate()
        {
            value_sheet.write(row as u32, 0, category);
            value_sheet.write_number(row as u32, 1, value);
        }
        let value_chart = Chart::new(ChartKind::Line, (0, 0), (10, 8)).add_series(
            Series::new("extreme_value!$B$1:$B$2").with_categories("extreme_value!$A$1:$A$2"),
        );
        let mut chart_points = 0;
        assert!(!try_push_chart(
            &mut Vec::new(),
            rect,
            &value_chart,
            None,
            value_sheet,
            &mut chart_points,
            &mut 0,
            &mut 0,
            &mut TypographyStats::default(),
            &options,
            &mut Warnings::default(),
            CellCoordinate { row: 0, col: 0 },
        )
        .unwrap());
        assert_eq!(chart_points, 0);

        let mut x_workbook = Workbook::new();
        let x_sheet = x_workbook.add_sheet("extreme_x");
        for (row, (x, y)) in [(-f64::MAX, 1.0), (f64::MAX, 2.0)].into_iter().enumerate() {
            x_sheet.write_number(row as u32, 0, x);
            x_sheet.write_number(row as u32, 1, y);
        }
        let x_chart = Chart::new(ChartKind::Scatter, (0, 0), (10, 8))
            .add_series(Series::new("extreme_x!$B$1:$B$2").with_categories("extreme_x!$A$1:$A$2"));
        assert!(!try_push_chart(
            &mut Vec::new(),
            rect,
            &x_chart,
            None,
            x_sheet,
            &mut chart_points,
            &mut 0,
            &mut 0,
            &mut TypographyStats::default(),
            &options,
            &mut Warnings::default(),
            CellCoordinate { row: 0, col: 0 },
        )
        .unwrap());
        assert_eq!(chart_points, 0);
    }

    #[test]
    fn pie_and_doughnut_nonfinite_totals_fail_closed_to_placeholders() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("extreme_totals");
        for (row, category) in ["A", "B"].into_iter().enumerate() {
            sheet.write(row as u32, 0, category);
            sheet.write_number(row as u32, 1, f64::MAX);
        }
        let series =
            || Series::new("extreme_totals!$B$1:$B$2").with_categories("extreme_totals!$A$1:$A$2");
        sheet.add_chart(Chart::new(ChartKind::Pie, (0, 2), (10, 8)).add_series(series()));
        sheet.add_chart(Chart::new(ChartKind::Doughnut, (11, 2), (21, 8)).add_series(series()));

        let rect = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(500),
            height: Fixed::from_pixels(300),
        };
        let options = RenderOptions::default();
        for chart in sheet.charts() {
            let mut nodes = Vec::new();
            let mut chart_points = 0;
            assert!(!try_push_chart(
                &mut nodes,
                rect,
                chart,
                None,
                sheet,
                &mut chart_points,
                &mut 0,
                &mut 0,
                &mut TypographyStats::default(),
                &options,
                &mut Warnings::default(),
                CellCoordinate { row: 0, col: 0 },
            )
            .unwrap());
            assert_eq!(chart_points, 0);
            assert!(nodes.is_empty());
        }

        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 21, 8)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        assert!(build.report.warnings.iter().any(|warning| {
            warning.code == WarningCode::ChartPlaceholder && warning.occurrences == 2
        }));
    }

    #[test]
    fn pie_and_doughnut_finite_extreme_totals_normalize_before_multiplication() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("finite_extreme_total");
        sheet.write(0, 0, "A");
        sheet.write_number(0, 1, f64::MAX);
        let series = || {
            Series::new("finite_extreme_total!$B$1").with_categories("finite_extreme_total!$A$1")
        };
        let charts = [
            Chart::new(ChartKind::Pie, (0, 0), (10, 8)).add_series(series()),
            Chart::new(ChartKind::Doughnut, (0, 0), (10, 8)).add_series(series()),
        ];
        let rect = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(500),
            height: Fixed::from_pixels(300),
        };
        let options = RenderOptions::default();
        for chart in &charts {
            let mut nodes = Vec::new();
            assert!(try_push_chart(
                &mut nodes,
                rect,
                chart,
                None,
                sheet,
                &mut 0,
                &mut 0,
                &mut 0,
                &mut TypographyStats::default(),
                &options,
                &mut Warnings::default(),
                CellCoordinate { row: 0, col: 0 },
            )
            .unwrap());
            assert!(nodes.iter().any(|node| matches!(node, SceneNode::Path(_))));
        }
    }

    #[test]
    fn radar_axes_gridlines_and_labels_obey_visibility_and_bounds() {
        let mut style = ChartSeriesStyle::default();
        style.marker = ChartMarkerSymbol::None;
        let category_count = 128_usize;
        let series = [ResolvedChartSeries {
            name: "Radar".to_string(),
            values: (1..=category_count).map(|value| value as f64).collect(),
            x_values: None,
            labels: (0..category_count)
                .map(|index| format!("C{index:03}"))
                .collect(),
            bubble_sizes: None,
            style,
        }];
        let axis = chart_nice_value_axis(&series, true).unwrap();
        let plot = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(400),
            height: Fixed::from_pixels(300),
        };
        let options = RenderOptions::default();
        let warning_cell = CellCoordinate { row: 4, col: 2 };
        let mut nodes = Vec::new();
        let mut category_labels = Vec::new();
        let mut value_labels = Vec::new();
        let mut warnings = Warnings::default();
        push_radar_chart(
            &mut nodes,
            plot,
            &series,
            &axis,
            &[],
            true,
            true,
            false,
            false,
            &mut category_labels,
            &mut value_labels,
            false,
            &mut Vec::new(),
            &mut TypographyStats::default(),
            &options,
            &mut warnings,
            warning_cell,
        )
        .unwrap();
        assert!(category_labels.len() <= MAX_CHART_CATEGORY_LABELS);
        assert_eq!(
            category_labels.first().map(|label| label.text.as_str()),
            Some("C000")
        );
        assert_eq!(
            category_labels.last().map(|label| label.text.as_str()),
            Some("C127")
        );
        assert_eq!(value_labels.len(), axis.ticks.len());
        assert!(!nodes.iter().any(|node| match node {
            SceneNode::Line(line) => line.color == Rgb::new(205, 205, 205),
            SceneNode::Path(path) => path.stroke == Some(Rgb::new(205, 205, 205)),
            _ => false,
        }));
        let retained = category_labels.len();
        assert_eq!(
            warnings.finish(),
            [RenderWarning {
                code: WarningCode::ChartMetadataSimplified,
                occurrences: category_count.saturating_sub(retained) as u64,
                first_cell: Some(warning_cell),
            }]
        );

        let mut hidden_nodes = Vec::new();
        let mut hidden_category_labels = Vec::new();
        let mut hidden_value_labels = Vec::new();
        push_radar_chart(
            &mut hidden_nodes,
            plot,
            &series,
            &axis,
            &[],
            false,
            false,
            true,
            true,
            &mut hidden_category_labels,
            &mut hidden_value_labels,
            false,
            &mut Vec::new(),
            &mut TypographyStats::default(),
            &options,
            &mut Warnings::default(),
            warning_cell,
        )
        .unwrap();
        assert!(hidden_category_labels.is_empty());
        assert!(hidden_value_labels.is_empty());
        assert!(!hidden_nodes.iter().any(|node| match node {
            SceneNode::Line(line) => line.color == Rgb::new(205, 205, 205),
            SceneNode::Path(path) => path.stroke == Some(Rgb::new(205, 205, 205)),
            _ => false,
        }));

        let mut grid_nodes = Vec::new();
        push_radar_chart(
            &mut grid_nodes,
            plot,
            &series,
            &axis,
            &[],
            true,
            true,
            true,
            true,
            &mut Vec::new(),
            &mut Vec::new(),
            false,
            &mut Vec::new(),
            &mut TypographyStats::default(),
            &options,
            &mut Warnings::default(),
            warning_cell,
        )
        .unwrap();
        assert_eq!(
            grid_nodes
                .iter()
                .filter(|node| {
                    matches!(
                        node,
                        SceneNode::Line(line) if line.color == Rgb::new(205, 205, 205)
                    )
                })
                .count(),
            category_count
        );
        assert!(grid_nodes.iter().any(|node| {
            matches!(
                node,
                SceneNode::Path(path) if path.stroke == Some(Rgb::new(205, 205, 205))
            )
        }));

        let mut limited = options;
        limited.limits.max_path_commands = 16;
        assert_eq!(
            push_radar_chart(
                &mut Vec::new(),
                plot,
                &series,
                &axis,
                &[],
                true,
                true,
                false,
                true,
                &mut Vec::new(),
                &mut Vec::new(),
                false,
                &mut Vec::new(),
                &mut TypographyStats::default(),
                &limited,
                &mut Warnings::default(),
                warning_cell,
            ),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::PathCommands,
                limit: 16,
                actual: 129,
            })
        );
    }

    #[test]
    fn chart_measurement_aggregates_limits_faces_and_warnings() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("chart_accounting");
        for (row, (label, value)) in [("A", 1.0), ("B", 2.0)].into_iter().enumerate() {
            sheet.write(row as u32, 0, label);
            sheet.write_number(row as u32, 1, value);
        }
        let chart = Chart::new(ChartKind::Line, (0, 0), (10, 8))
            .with_title("📈")
            .add_series(
                Series::new("chart_accounting!$B$1:$B$2")
                    .with_categories("chart_accounting!$A$1:$A$2")
                    .with_name("Legacy series"),
            );
        let mut metadata = DrawingMetadata::default();
        metadata.chart_default_latin_font_family = Some("Legacy Sans".to_string());
        let pack = synthetic_test_pack();
        let options = RenderOptions {
            default_font_family: pack.default_family().to_string(),
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let rect = Rect {
            x: Fixed::from_pixels(10),
            y: Fixed::from_pixels(20),
            width: Fixed::from_pixels(500),
            height: Fixed::from_pixels(300),
        };
        let warning_cell = CellCoordinate { row: 0, col: 0 };
        let mut nodes = Vec::new();
        let mut chart_points = 0;
        let mut text_bytes = 0;
        let mut glyphs = 0;
        let mut typography = TypographyStats::default();
        let mut warnings = Warnings::default();
        assert!(try_push_chart(
            &mut nodes,
            rect,
            &chart,
            Some(&metadata),
            sheet,
            &mut chart_points,
            &mut text_bytes,
            &mut glyphs,
            &mut typography,
            &options,
            &mut warnings,
            warning_cell,
        )
        .unwrap());

        assert!(typography
            .clone()
            .finish_font_faces()
            .iter()
            .any(|face| face.family == "Wide Sans" && face.substituted));
        let warnings = warnings.finish();
        assert!(warnings
            .iter()
            .any(|warning| warning.code == WarningCode::FontFamilySubstituted));
        assert!(warnings
            .iter()
            .any(|warning| warning.code == WarningCode::MissingGlyph));

        let mut limited = options;
        limited.limits.max_text_runs = 7;
        let style = ResolvedChartTextStyle::for_role(ChartTextRole::AxisLabel, "Legacy Sans");
        let mut limited_typography = TypographyStats::default();
        let mut limited_warnings = Warnings::default();
        assert_eq!(
            max_chart_text_metrics_with_style(
                ["A", "BC"],
                &style,
                &limited,
                &mut limited_typography,
                &mut limited_warnings,
                warning_cell,
            ),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::TextRuns,
                limit: 7,
                actual: 8,
            })
        );
        assert_eq!(limited_typography.text_work, 8);
        assert_eq!(limited_typography.text_lines, 1);
    }

    #[test]
    fn short_chart_legend_columnizes_without_leaving_the_frame() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("legend");
        let mut chart = Chart::new(ChartKind::Line, (0, 0), (4, 14)).with_legend(true);
        for index in 0_u16..12 {
            sheet.write_number(0, index, f64::from(index + 1));
            let column = char::from(b'A' + u8::try_from(index).unwrap());
            chart = chart.add_series(
                Series::new(format!("legend!${column}$1")).with_name(format!("S{index:02}")),
            );
        }
        let rect = Rect {
            x: Fixed::from_pixels(7),
            y: Fixed::from_pixels(11),
            width: Fixed::from_pixels(900),
            height: Fixed::from_pixels(100),
        };
        let options = RenderOptions::default();
        let mut nodes = Vec::new();
        assert!(try_push_chart(
            &mut nodes,
            rect,
            &chart,
            None,
            sheet,
            &mut 0,
            &mut 0,
            &mut 0,
            &mut TypographyStats::default(),
            &options,
            &mut Warnings::default(),
            CellCoordinate { row: 0, col: 0 },
        )
        .unwrap());

        let legend_text = nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Text(text) if text.text.len() == 3 && text.text.starts_with('S') => {
                    Some(text)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(legend_text.len(), 12);
        assert!(
            legend_text
                .iter()
                .map(|text| text.bounds.x.raw())
                .collect::<BTreeSet<_>>()
                .len()
                > 1,
            "a short chart must use more than one legend column"
        );
        assert!(legend_text
            .iter()
            .all(|text| rect_contains(rect, text.bounds)));

        let swatches = nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Rect(node)
                    if node.rect.width == Fixed::from_pixels(10)
                        && node.rect.height == Fixed::from_pixels(10) =>
                {
                    Some(node.rect)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(swatches.len(), 12);
        assert!(swatches
            .iter()
            .copied()
            .all(|swatch| rect_contains(rect, swatch)));
    }

    #[test]
    fn vertical_axis_endpoint_labels_stay_inside_a_short_chart() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("axis_bounds");
        for (row, (label, value)) in [("A", 0.0), ("B", 100.0)].into_iter().enumerate() {
            sheet.write(row as u32, 0, label);
            sheet.write_number(row as u32, 1, value);
        }
        let chart = Chart::new(ChartKind::Line, (0, 0), (4, 8)).add_series(
            Series::new("axis_bounds!$B$1:$B$2").with_categories("axis_bounds!$A$1:$A$2"),
        );
        let rect = Rect {
            x: Fixed::from_pixels(13),
            y: Fixed::from_pixels(17),
            width: Fixed::from_pixels(500),
            height: Fixed::from_pixels(80),
        };
        let options = RenderOptions::default();
        let mut nodes = Vec::new();
        assert!(try_push_chart(
            &mut nodes,
            rect,
            &chart,
            None,
            sheet,
            &mut 0,
            &mut 0,
            &mut 0,
            &mut TypographyStats::default(),
            &options,
            &mut Warnings::default(),
            CellCoordinate { row: 0, col: 0 },
        )
        .unwrap());

        let vertical_axis = nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Line(line)
                    if line.color == Rgb::BLACK && line.x1 == line.x2 && line.y1 < line.y2 =>
                {
                    Some(line)
                }
                _ => None,
            })
            .expect("vertical chart axis");
        let mut endpoint_centers = nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Text(text) if text.style.anchor == TextAnchor::End => {
                    assert!(rect_contains(rect, text.bounds), "{:?}", text.bounds);
                    Some(
                        text.bounds
                            .y
                            .checked_add(Fixed::from_raw(text.bounds.height.raw() / 2))
                            .unwrap(),
                    )
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        endpoint_centers.sort_unstable_by_key(|center| center.raw());
        assert_eq!(endpoint_centers.first(), Some(&vertical_axis.y1));
        assert_eq!(endpoint_centers.last(), Some(&vertical_axis.y2));
    }

    #[test]
    fn pinned_missing_theme_chart_roles_resolve_to_the_verified_arimo_alias() {
        let Some(manifest) = std::env::var_os("RXLS_TEST_FONT_PACK_MANIFEST") else {
            return;
        };
        let pack = FontPack::load_manifest(manifest).expect("load pinned render font pack");
        for role in [
            ChartTextRole::ChartTitle,
            ChartTextRole::AxisTitle,
            ChartTextRole::AxisLabel,
            ChartTextRole::Legend,
            ChartTextRole::DataLabel,
        ] {
            let request = role.resolved_style(CALC_MISSING_THEME_CHART_LATIN_FAMILY);
            let resolution = pack.resolve(request.request());
            assert!(!resolution.exact_family, "{role:?}");
            assert!(resolution.declared_alias, "{role:?}");
            assert!(resolution.exact_style, "{role:?}");
            let identity = pack.selected_face_identity(resolution.id).unwrap();
            assert_eq!(identity.family, "Arimo", "{role:?}");
            assert_eq!(identity.weight, if role.bold() { 700 } else { 400 });
        }
    }

    #[test]
    fn chart_text_rotation_bounds_are_exact_for_representative_angles() {
        let metrics = ChartTextMetrics {
            width: Fixed::from_raw(100),
            height: Fixed::from_raw(40),
        };
        for (angle, width, height) in [
            (0, 100, 40),
            (30, 107, 85),
            (45, 99, 99),
            (90, 40, 100),
            (135, 99, 99),
        ] {
            assert_eq!(
                metrics.rotated(angle).unwrap(),
                ChartTextMetrics {
                    width: Fixed::from_raw(width),
                    height: Fixed::from_raw(height),
                },
                "angle {angle}"
            );
        }
    }

    #[test]
    fn chart_text_rotation_bounds_normalize_turns_and_negative_angles() {
        let metrics = ChartTextMetrics {
            width: Fixed::from_raw(137),
            height: Fixed::from_raw(53),
        };
        for equivalent in [390, -330] {
            assert_eq!(metrics.rotated(equivalent), metrics.rotated(30));
        }
        for equivalent in [-30, 330, -390] {
            assert_eq!(metrics.rotated(equivalent), metrics.rotated(30));
        }
        for equivalent in [495, -225] {
            assert_eq!(metrics.rotated(equivalent), metrics.rotated(135));
        }
    }

    #[test]
    fn chart_text_rotation_coefficients_saturate_and_extents_fail_closed() {
        assert_eq!(CHART_TEXT_SINE_Q62_CEIL[0], 0);
        assert_eq!(
            u128::from(CHART_TEXT_SINE_Q62_CEIL[90]),
            CHART_TEXT_TRIG_SCALE
        );
        assert!(CHART_TEXT_SINE_Q62_CEIL
            .windows(2)
            .all(|pair| pair[0] < pair[1]));
        assert!(CHART_TEXT_SINE_Q62_CEIL
            .iter()
            .all(|coefficient| u128::from(*coefficient) <= CHART_TEXT_TRIG_SCALE));

        let maximum_width = ChartTextMetrics {
            width: Fixed::from_raw(i64::MAX),
            height: Fixed::ZERO,
        };
        assert_eq!(maximum_width.rotated(0).unwrap(), maximum_width);
        assert_eq!(
            maximum_width.rotated(90).unwrap(),
            ChartTextMetrics {
                width: Fixed::ZERO,
                height: Fixed::from_raw(i64::MAX),
            }
        );
        assert!(maximum_width.rotated(1).is_ok());

        let maximum_square = ChartTextMetrics {
            width: Fixed::from_raw(i64::MAX),
            height: Fixed::from_raw(i64::MAX),
        };
        assert_eq!(
            maximum_square.rotated(45),
            Err(RenderError::CoordinateOverflow)
        );
        assert_eq!(
            ChartTextMetrics {
                width: Fixed::from_raw(-1),
                height: Fixed::ZERO,
            }
            .rotated(30),
            Err(RenderError::CoordinateOverflow)
        );
    }

    fn imported_rtl_circle_chart() -> Workbook {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (name, body) in [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Render" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetViews><sheetView rightToLeft="1"/></sheetViews><sheetData>
                  <row r="1"><c r="A1" t="inlineStr"><is><t>Q1</t></is></c><c r="B1"><v>10</v></c></row>
                  <row r="2"><c r="A2" t="inlineStr"><is><t>Q2</t></is></c><c r="B2"><v>23</v></c></row>
                  <row r="3"><c r="A3" t="inlineStr"><is><t>Q3</t></is></c><c r="B3"><v>36</v></c></row>
                  <row r="4"><c r="A4" t="inlineStr"><is><t>Q4</t></is></c><c r="B4"><v>49</v></c></row>
                </sheetData><drawing r:id="rIdDraw"/></worksheet>"#,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
            ),
            (
                "xl/drawings/drawing1.xml",
                r#"<wsDr><twoCellAnchor><from><col>0</col><row>7</row></from><to><col>6</col><row>18</row></to><graphicFrame><graphic><graphicData><chart r:id="rIdChart"/></graphicData></graphic></graphicFrame></twoCellAnchor></wsDr>"#,
            ),
            (
                "xl/drawings/_rels/drawing1.xml.rels",
                r#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#,
            ),
            (
                "xl/charts/chart1.xml",
                r#"<chartSpace><chart><plotArea><lineChart><ser><idx val="0"/><order val="0"/>
                  <marker><symbol val="circle"/><size val="3"/></marker>
                  <cat><strRef><f>Render!$A$1:$A$4</f></strRef></cat>
                  <val><numRef><f>Render!$B$1:$B$4</f></numRef></val>
                </ser><axId val="1"/><axId val="2"/></lineChart>
                <catAx><axId val="1"/><crossAx val="2"/></catAx>
                <valAx><axId val="2"/><crossAx val="1"/></valAx>
                </plotArea></chart></chartSpace>"#,
            ),
        ] {
            writer.start_file(name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        Workbook::open(&writer.finish().unwrap().into_inner()).expect("imported RTL circle chart")
    }

    #[test]
    fn rtl_chart_continuation_keeps_signed_tile_coordinates() {
        let workbook = imported_rtl_circle_chart();
        assert_eq!(workbook.sheets[0].charts().len(), 1);

        let rows = (0_u32..=18)
            .map(|index| MeasuredAxisSlot {
                index,
                offset: Fixed::from_pixels(i64::from(index) * 20),
                size: Fixed::from_pixels(20),
            })
            .collect::<Vec<_>>();
        let columns = (0_u16..=6)
            .map(|index| MeasuredAxisSlot {
                index,
                offset: Fixed::from_pixels(i64::from(index) * 64),
                size: Fixed::from_pixels(64),
            })
            .collect::<Vec<_>>();
        let options = outlined_options(RenderRange::new(1, 0, 8, 2));
        let build = build_sheet_scene_with_geometry(
            &workbook.sheets[0],
            0,
            &options,
            SheetGeometryOverride::new(&rows, &columns),
        )
        .expect("a continued chart may retain negative pre-clip coordinates");

        assert!(build
            .report
            .warnings
            .iter()
            .all(|warning| warning.code != WarningCode::ChartPlaceholder));
        let chart_group = build
            .scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::ClipGroup(group)
                    if group.nodes.iter().any(|node| {
                        matches!(
                            node,
                            SceneNode::Path(path)
                                if path.commands.iter().filter(|command| {
                                    matches!(command, PathCommand::CubicTo { .. })
                                }).count() == 4
                        )
                    }) =>
                {
                    Some(group)
                }
                _ => None,
            })
            .expect("the continued imported chart must retain its clipped circle markers");
        assert!(chart_group.clip.x.raw() >= 0);
        let circle_markers = chart_group
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Path(path)
                    if path
                        .commands
                        .iter()
                        .filter(|command| matches!(command, PathCommand::CubicTo { .. }))
                        .count()
                        == 4 =>
                {
                    Some(path)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(circle_markers.len(), 4);
        assert!(
            circle_markers
                .iter()
                .flat_map(|path| &path.commands)
                .any(|command| match command {
                    PathCommand::MoveTo { x, .. }
                    | PathCommand::LineTo { x, .. }
                    | PathCommand::QuadraticTo { x, .. }
                    | PathCommand::CubicTo { x, .. } => x.raw() < 0,
                    PathCommand::Close => false,
                }),
            "a real circle marker must retain a negative pre-clip x coordinate"
        );
    }

    #[test]
    fn chart_coordinate_conversion_is_signed_and_fail_closed() {
        assert_eq!(pixels_as_fixed(0.0).unwrap(), Fixed::ZERO);
        assert_eq!(
            pixels_as_fixed(-0.25).unwrap(),
            Fixed::from_raw(-FIXED_UNITS_PER_PIXEL / 4)
        );
        assert_eq!(
            pixels_as_fixed(-9_007_199_254_740_992.0).unwrap(),
            Fixed::from_raw(i64::MIN)
        );
        assert_eq!(
            pixels_as_fixed(9_007_199_254_740_991.0).unwrap(),
            Fixed::from_raw(i64::MAX - (FIXED_UNITS_PER_PIXEL - 1))
        );

        for invalid in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            9_007_199_254_740_992.0,
            -9_007_199_254_740_994.0,
        ] {
            assert_eq!(
                pixels_as_fixed(invalid),
                Err(RenderError::CoordinateOverflow)
            );
        }
        for boundary in [i64::MIN, i64::MAX] {
            assert_eq!(
                interpolate_fixed(Fixed::from_raw(boundary), Fixed::ZERO, 0.0).unwrap(),
                Fixed::from_raw(boundary)
            );
        }
        assert_eq!(
            interpolate_fixed(Fixed::from_raw(i64::MIN), Fixed::from_raw(i64::MAX), 1.0).unwrap(),
            Fixed::from_raw(-1)
        );
        assert_eq!(
            interpolate_fixed(
                Fixed::from_raw((1_i64 << 62) - 1),
                Fixed::from_raw((1_i64 << 62) + 1),
                1.0
            ),
            Err(RenderError::CoordinateOverflow)
        );
        for (start, extent, ratio) in [
            (i64::MAX, 1, 1.0),
            (i64::MIN, -1, 1.0),
            (i64::MAX, 1, 0.5),
            (i64::MIN, -1, 0.5),
            (0, 1_i64 << 62, 2.0),
        ] {
            assert_eq!(
                interpolate_fixed(Fixed::from_raw(start), Fixed::from_raw(extent), ratio),
                Err(RenderError::CoordinateOverflow)
            );
        }
        for ratio in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                interpolate_fixed(Fixed::ZERO, Fixed::ZERO, ratio),
                Err(RenderError::CoordinateOverflow)
            );
        }
    }

    #[test]
    fn clipped_circle_chart_marker_keeps_negative_coordinates() {
        let mut nodes = Vec::new();
        let mut typography_stats = TypographyStats::default();
        let mut style = ChartSeriesStyle::default();
        style.marker = ChartMarkerSymbol::Circle;
        style.marker_size = Some(3);

        push_chart_marker(
            &mut nodes,
            pixels_as_fixed(-188.25).unwrap(),
            Fixed::from_pixels(12),
            Rgb::new(0x12, 0x34, 0x56),
            &style,
            &mut typography_stats,
            &RenderOptions::default(),
        )
        .expect("a clipped circular marker may retain negative pre-clip coordinates");

        let SceneNode::Path(path) = &nodes[0] else {
            panic!("a circular marker must render as a path");
        };
        assert!(path.commands.iter().any(|command| match command {
            PathCommand::MoveTo { x, .. }
            | PathCommand::LineTo { x, .. }
            | PathCommand::QuadraticTo { x, .. }
            | PathCommand::CubicTo { x, .. } => x.raw() < 0,
            PathCommand::Close => false,
        }));
    }

    #[test]
    fn retained_emu_width_drives_line_scatter_and_area_scene_strokes() {
        let mut style = ChartSeriesStyle::default();
        style.marker = ChartMarkerSymbol::None;
        style.line_color = Some(Color::rgb(0x12, 0x34, 0x56));
        style.line_width_emu = Some(19_050);
        let series = [ResolvedChartSeries {
            name: "Series".to_string(),
            values: vec![1.0, 2.0],
            x_values: Some(vec![1.0, 2.0]),
            labels: vec!["A".to_string(), "B".to_string()],
            bubble_sizes: None,
            style,
        }];
        let plot = Rect {
            x: Fixed::ZERO,
            y: Fixed::ZERO,
            width: Fixed::from_pixels(100),
            height: Fixed::from_pixels(100),
        };
        let options = RenderOptions::default();

        let mut line_nodes = Vec::new();
        push_line_chart(
            &mut line_nodes,
            plot,
            &series,
            (0.0, 2.0),
            false,
            &[],
            false,
            &mut Vec::new(),
            &mut TypographyStats::default(),
            &options,
        )
        .unwrap();
        assert!(line_nodes.iter().any(|node| {
            matches!(
                node,
                SceneNode::Line(line)
                    if line.color == Rgb::new(0x12, 0x34, 0x56)
                        && line.width == Fixed::from_pixels(2)
            )
        }));

        let mut scatter_nodes = Vec::new();
        let x_axis = chart_nice_x_axis(&series).unwrap();
        push_scatter_chart(
            &mut scatter_nodes,
            plot,
            &series,
            (0.0, 2.0),
            &x_axis,
            &[],
            false,
            &mut Vec::new(),
            &mut TypographyStats::default(),
            &options,
        )
        .unwrap();
        assert!(scatter_nodes.iter().any(|node| {
            matches!(
                node,
                SceneNode::Line(line)
                    if line.color == Rgb::new(0x12, 0x34, 0x56)
                        && line.width == Fixed::from_pixels(2)
            )
        }));

        let mut area_nodes = Vec::new();
        push_area_chart(
            &mut area_nodes,
            plot,
            &series,
            (0.0, 2.0),
            false,
            &[],
            false,
            &mut Vec::new(),
            &mut TypographyStats::default(),
            &options,
        )
        .unwrap();
        assert!(area_nodes.iter().any(|node| {
            matches!(
                node,
                SceneNode::Path(path)
                    if path.stroke == Some(Rgb::new(0x12, 0x34, 0x56))
                        && path.stroke_width == Fixed::from_pixels(2)
            )
        }));
    }

    #[test]
    fn shifted_category_positions_use_band_centers_and_legacy_positions_keep_endpoints() {
        let shifted = (0..4)
            .map(|index| chart_category_ratio(index, 4, true))
            .collect::<Vec<_>>();
        assert_eq!(shifted, [0.125, 0.375, 0.625, 0.875]);

        let legacy = (0..4)
            .map(|index| chart_category_ratio(index, 4, false))
            .collect::<Vec<_>>();
        assert_eq!(legacy, [0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0]);
        assert_eq!(chart_category_ratio(0, 1, true), 0.5);
        assert_eq!(chart_category_ratio(0, 1, false), 0.5);
    }

    #[test]
    fn shifted_category_data_plot_keeps_calc_frame_padding() {
        let plot = Rect {
            x: Fixed::from_pixels(10),
            y: Fixed::from_pixels(20),
            width: Fixed::from_pixels(100),
            height: Fixed::from_pixels(40),
        };
        let shifted = chart_category_data_plot(plot, true, false).unwrap();
        assert_eq!(shifted.x, Fixed::from_pixels(18));
        assert_eq!(shifted.y, plot.y);
        assert_eq!(shifted.width, Fixed::from_pixels(84));
        assert_eq!(shifted.height, plot.height);
        assert_eq!(chart_category_data_plot(plot, false, false).unwrap(), plot);
        assert_eq!(chart_category_data_plot(plot, true, true).unwrap(), plot);
    }

    #[test]
    fn imported_bar_chart_renders_real_geometry() {
        let mut authored = Workbook::new();
        let sheet = authored.add_sheet("imported_bar");
        for (row, (label, value)) in [("A", 2.0), ("B", 5.0), ("C", 3.0)].into_iter().enumerate() {
            sheet.write(row as u32, 0, label);
            sheet.write_number(row as u32, 1, value);
        }
        sheet.add_chart(
            Chart::new(ChartKind::Bar, (0, 3), (10, 9))
                .with_title("Imported columns")
                .add_series(
                    Series::new("imported_bar!$B$1:$B$3").with_categories("imported_bar!$A$1:$A$3"),
                ),
        );
        let imported = Workbook::open(&authored.to_xlsx()).expect("reopen authored chart");
        assert_ne!(imported.sheets[0].style_fidelity(), StyleFidelity::Authored);
        let mut resolved_points = 0;
        assert_eq!(
            resolve_numeric_a1_range(
                &imported.sheets[0],
                "imported_bar!$B$1:$B$3",
                &mut resolved_points,
                &RenderOptions::default(),
                true,
            )
            .unwrap(),
            Some(vec![2.0, 5.0, 3.0])
        );
        assert_eq!(
            resolve_label_a1_range(
                &imported.sheets[0],
                "imported_bar!$A$1:$A$3",
                &mut resolved_points,
                &RenderOptions::default(),
            )
            .unwrap(),
            Some(vec!["A".into(), "B".into(), "C".into()])
        );
        let build = build_scene(
            &imported,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 10, 9)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        assert!(
            !build
                .report
                .warnings
                .iter()
                .any(|warning| warning.code == WarningCode::ChartPlaceholder),
            "charts={:?} report={:?}",
            imported.sheets[0].charts(),
            build.report
        );
        assert!(build.scene.nodes.iter().any(|node| matches!(
            node,
            SceneNode::Rect(RectNode {
                fill: Some(Rgb {
                    red: 68,
                    green: 114,
                    blue: 196,
                }),
                ..
            })
        )));
    }

    #[test]
    fn imported_cross_sheet_chart_uses_complete_cache_and_theme_palette() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Host" r:id="rId1"/><sheet name="Data" r:id="rId2"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="worksheets/sheet2.xml"/><Relationship Id="rIdTheme" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/></Relationships>"#,
            ),
            (
                "xl/theme/theme1.xml",
                r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:themeElements><a:clrScheme><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:accent1><a:srgbClr val="123456"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme><a:majorFont><a:latin typeface="Liberation Sans"/></a:majorFont><a:minorFont><a:latin typeface="Liberation Sans"/></a:minorFont></a:fontScheme></a:themeElements></a:theme>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData/><drawing r:id="rIdDraw"/></worksheet>"#,
            ),
            (
                "xl/worksheets/sheet2.xml",
                r#"<worksheet><sheetData/></worksheet>"#,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
            ),
            (
                "xl/drawings/drawing1.xml",
                r#"<wsDr><twoCellAnchor><from><col>0</col><row>0</row></from><to><col>8</col><row>12</row></to><graphicFrame><graphic><graphicData><chart r:id="rIdChart"/></graphicData></graphic></graphicFrame></twoCellAnchor></wsDr>"#,
            ),
            (
                "xl/drawings/_rels/drawing1.xml.rels",
                r#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#,
            ),
            (
                "xl/charts/chart1.xml",
                r#"<chartSpace><chart><plotArea><barChart><barDir val="bar"/><ser><idx val="0"/><order val="0"/><tx><strRef><f>Data!$C$1</f><strCache><pt idx="0"><v>Cached revenue</v></pt></strCache></strRef></tx><cat><strRef><f>Data!$A$1:$A$3</f><strCache><pt idx="0"><v>A</v></pt><pt idx="1"><v>B</v></pt><pt idx="2"><v>C</v></pt></strCache></strRef></cat><val><numRef><f>Data!$B$1:$B$3</f><numCache><pt idx="0"><v>2</v></pt><pt idx="1"><v>5</v></pt><pt idx="2"><v>3</v></pt></numCache></numRef></val></ser><axId val="1"/><axId val="2"/></barChart><catAx><axId val="1"/><crossAx val="2"/></catAx><valAx><axId val="2"/><crossAx val="1"/></valAx></plotArea><legend/></chart></chartSpace>"#,
            ),
        ];
        for (name, body) in parts {
            writer.start_file(name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        let workbook = Workbook::open(&bytes).expect("cached chart workbook");
        let metadata = workbook.sheets[0]
            .drawing_metadata()
            .iter()
            .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
            .expect("chart sidecar");
        assert_eq!(metadata.chart_bar_direction, ChartBarDirection::Horizontal);
        assert_eq!(
            metadata.chart_default_latin_font_family.as_deref(),
            Some(CALC_MISSING_THEME_CHART_LATIN_FAMILY)
        );
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 12, 8)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        assert!(
            !build
                .report
                .warnings
                .iter()
                .any(|warning| warning.code == WarningCode::ChartPlaceholder),
            "chart reasons: {:?}",
            metadata.chart_unsupported_reasons
        );
        assert!(build.scene.nodes.iter().any(|node| match node {
            SceneNode::Text(text) => {
                text.text == "Cached revenue"
                    && text.style.family == CALC_MISSING_THEME_CHART_LATIN_FAMILY
            }
            _ => false,
        }));
        let horizontal_bars = build
            .scene
            .nodes
            .iter()
            .filter_map(|node| match node {
                SceneNode::Rect(RectNode {
                    rect,
                    fill:
                        Some(Rgb {
                            red: 0x12,
                            green: 0x34,
                            blue: 0x56,
                        }),
                    ..
                }) if rect.width > rect.height => Some(*rect),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(horizontal_bars.len(), 3);
    }

    #[test]
    fn unsupported_imported_chart_constructs_are_explicit_placeholders() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Host" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="2"><c r="A2"><v>2</v></c></row></sheetData><drawing r:id="rIdDraw"/></worksheet>"#,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
            ),
            (
                "xl/drawings/drawing1.xml",
                r#"<wsDr><twoCellAnchor><from><col>0</col><row>0</row></from><to><col>8</col><row>12</row></to><graphicFrame><graphic><graphicData><chart r:id="rIdChart"/></graphicData></graphic></graphicFrame></twoCellAnchor></wsDr>"#,
            ),
            (
                "xl/drawings/_rels/drawing1.xml.rels",
                r#"<Relationships><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#,
            ),
            (
                "xl/charts/chart1.xml",
                r#"<chartSpace><pivotSource/><externalData/><chart><view3D/><plotArea><barChart><ser><val><numRef><f>Host!$A$1:$A$2</f></numRef></val></ser></barChart><lineChart><ser><val><numRef><f>Host!$A$1:$A$2</f></numRef></val></ser></lineChart></plotArea></chart></chartSpace>"#,
            ),
        ];
        for (name, body) in parts {
            writer.start_file(name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        let workbook = Workbook::open(&writer.finish().unwrap().into_inner()).unwrap();
        let metadata = workbook.sheets[0]
            .drawing_metadata()
            .iter()
            .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
            .expect("retained unsupported chart");
        let expected_reasons = [
            rxls::ChartUnsupportedReason::Combo,
            rxls::ChartUnsupportedReason::ThreeDimensional,
            rxls::ChartUnsupportedReason::Pivot,
            rxls::ChartUnsupportedReason::ExternalData,
            rxls::ChartUnsupportedReason::UnsupportedAxisTopology,
            rxls::ChartUnsupportedReason::UnsupportedPlotSemantics,
        ];
        assert_eq!(
            metadata.chart_unsupported_reasons.len(),
            expected_reasons.len(),
            "unexpected reasons: {:?}",
            metadata.chart_unsupported_reasons
        );
        for reason in expected_reasons {
            assert!(
                metadata.chart_unsupported_reasons.contains(&reason),
                "missing {reason:?} in {:?}",
                metadata.chart_unsupported_reasons
            );
        }
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                selection: RenderSelection::Range(RenderRange::new(0, 0, 12, 8)),
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        assert!(build.report.warnings.iter().any(|warning| {
            warning.code == WarningCode::ChartPlaceholder && warning.occurrences == 1
        }));
    }

    #[test]
    fn used_and_single_page_bounds_expand_to_visible_drawing_anchors() {
        let mut workbook = Workbook::new();
        workbook
            .add_sheet("drawing-only")
            .add_image(Image::new([137, 80, 78, 71], ImageFmt::Png, (5, 3)).with_to((15, 7)));
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();
        assert_eq!(build.report.range, RenderRange::new(5, 3, 15, 7));
        assert_eq!(build.scene.width, Fixed::from_pixels(320));
        assert_eq!(build.scene.height, Fixed::from_pixels(220));
        assert!(build.report.warnings.iter().any(|warning| {
            warning.code == WarningCode::ImagePlaceholder && warning.occurrences == 1
        }));
        assert!(!build
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::DrawingAnchorUnavailable));

        let document = build_print_document(
            &workbook,
            0,
            &PrintOptions {
                single_page_sheets: true,
                render: RenderOptions {
                    gridlines: false,
                    ..RenderOptions::default()
                },
                ..PrintOptions::default()
            },
        )
        .unwrap();
        assert_eq!(document.pages[0].scene.width, Fixed::from_raw(327_732));
        assert_eq!(document.pages[0].scene.height, Fixed::from_raw(225_325));
        assert_eq!(document.report.source.range, RenderRange::new(5, 3, 15, 7));
    }

    #[test]
    fn imported_two_cell_drawing_end_markers_use_the_last_visibly_occupied_cell() {
        for kind in [DrawingObjectKind::Image, DrawingObjectKind::Chart] {
            for (offset, expected) in [
                ((0, 0), RenderRange::new(3, 2, 6, 4)),
                ((1, 0), RenderRange::new(3, 2, 6, 5)),
                ((0, 1), RenderRange::new(3, 2, 7, 4)),
                ((1, 1), RenderRange::new(3, 2, 7, 5)),
            ] {
                let workbook = imported_two_cell_drawing(kind, offset);
                let build = build_scene(
                    &workbook,
                    0,
                    &RenderOptions {
                        gridlines: false,
                        ..RenderOptions::default()
                    },
                )
                .unwrap();
                assert_eq!(build.report.range, expected, "{kind:?} at {offset:?}");
            }
        }
    }

    #[test]
    fn calc_ooxml_closed_terminal_boundaries_round_trip_through_mm100_exactly() {
        for (twips, mm100, closed_twips) in [
            (5_185, 9_146, 5_185),
            (6_222, 10_975, 6_221),
            (9_028, 15_924, 9_027),
            (10_065, 17_754, 10_065),
        ] {
            assert_eq!(round_unsigned_ratio(twips, 127, 72), Some(mm100));
            assert_eq!(round_unsigned_ratio(mm100 - 1, 72, 127), Some(closed_twips));
        }
    }

    #[test]
    fn single_page_ooxml_zero_offset_terminal_columns_follow_calc_physical_bounds_only() {
        let pack = synthetic_test_pack();
        let render_options = RenderOptions {
            gridlines: false,
            default_font_family: pack.default_family().to_string(),
            // The synthetic face has a 600/1000 digit advance, yielding the
            // hosted Noto-equivalent 8_336 raw / 122-twip digit width.
            default_font_size: Fixed::from_raw(13_893),
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        for (
            hidden_terminal_column,
            explicit_prefix_widths,
            ordinary_last_column,
            single_page_last_column,
            label,
        ) in [
            (true, false, 4, 6, "hidden implicit F"),
            (false, false, 5, 5, "visible implicit F"),
            (true, true, 4, 4, "hidden explicit-prefix F"),
            (false, true, 5, 6, "visible explicit-prefix F"),
        ] {
            let workbook = imported_single_page_terminal_column_drawing(
                hidden_terminal_column,
                explicit_prefix_widths,
                0,
                6,
            );
            let sheet = &workbook.sheets[0];
            assert_eq!(sheet.implicit_ooxml_column_width(), Some(None), "{label}");
            assert_eq!(sheet.xlsb_default_column_width(), None, "{label}");
            assert!(sheet.xlsb_column_widths_256().is_empty(), "{label}");
            let metadata = &sheet.drawing_metadata()[0];
            assert_eq!(metadata.from_cell, Some((0, 0)), "{label}");
            assert_eq!(metadata.to_cell, Some((1, 6)), "{label}");
            assert_eq!(metadata.to_offset_emu, Some((0, 0)), "{label}");

            let ordinary = build_scene(&workbook, 0, &render_options).unwrap();
            assert_eq!(
                ordinary.report.range,
                RenderRange::new(0, 0, 0, ordinary_last_column),
                "ordinary Used bounds changed for {label}"
            );
            let single_page = build_print_document(
                &workbook,
                0,
                &PrintOptions {
                    single_page_sheets: true,
                    render: render_options.clone(),
                    ..PrintOptions::default()
                },
            )
            .unwrap();
            assert_eq!(
                single_page.report.source.range,
                RenderRange::new(0, 0, 0, single_page_last_column),
                "{label}"
            );
            assert_eq!(
                single_page.report.pages[0].body_range, single_page.report.source.range,
                "{label}"
            );
            if !hidden_terminal_column && explicit_prefix_widths {
                assert_eq!(
                    ordinary.report.range,
                    RenderRange::new(0, 0, 0, 5),
                    "visible explicit-prefix ordinary bounds remain A:F"
                );
                assert_eq!(
                    single_page.report.source.range,
                    RenderRange::new(0, 0, 0, 6),
                    "visible explicit-prefix SinglePageSheets bounds expand to A:G"
                );
                assert_eq!(
                    single_page.pages[0].scene.width,
                    Fixed::from_raw(757_947),
                    "visible explicit-prefix SinglePageSheets canvas is 555.136962890625 pt"
                );
            }
            let single_rect = shape_placeholder_rect(&single_page.pages[0].scene.nodes)
                .expect("single-page shape");
            let ordinary_rect =
                shape_placeholder_rect(&ordinary.scene.nodes).expect("ordinary shape");
            assert_eq!(single_rect.x, ordinary_rect.x, "{label}");
            assert_eq!(single_rect.y, ordinary_rect.y, "{label}");
            assert_eq!(single_rect.height, ordinary_rect.height, "{label}");
            if explicit_prefix_widths {
                let mut visible_twips = 2_196_i128 + 4 * 1_708;
                if !hidden_terminal_column {
                    visible_twips += 1_037;
                }
                let native_width = calc_twips_position_to_fixed(visible_twips).unwrap();
                let native_width = if hidden_terminal_column {
                    calc_inclusive_rectangle_extent(native_width).unwrap()
                } else {
                    native_width
                };
                assert_eq!(
                    single_rect.width, native_width,
                    "OOXML explicit prefixes must use rounded default-font digit twips for {label}"
                );
            } else {
                let visible_columns = if hidden_terminal_column { 5 } else { 6 };
                assert_eq!(
                    ordinary_rect.width,
                    Fixed::from_raw(70_793 * visible_columns),
                    "ordinary layout must retain per-track rounding for {label}"
                );
                let native_width = calc_twips_position_to_fixed(
                    i128::from(visible_columns).checked_mul(1_037).unwrap(),
                )
                .unwrap();
                let native_width = if hidden_terminal_column {
                    native_width
                } else {
                    calc_inclusive_rectangle_extent(native_width).unwrap()
                };
                assert_eq!(
                    single_rect.width, native_width,
                    "single-page layout must round the cumulative Calc source width once for {label}"
                );
            }
        }
    }

    #[test]
    fn single_page_ooxml_physical_bounds_keep_sparse_high_columns_within_span_limits() {
        let pack = synthetic_test_pack();
        let workbook = imported_single_page_terminal_column_drawing(false, false, 1_024, 1_030);
        let document = build_print_document(
            &workbook,
            0,
            &PrintOptions {
                single_page_sheets: true,
                render: RenderOptions {
                    gridlines: false,
                    default_font_family: pack.default_family().to_string(),
                    default_font_size: Fixed::from_raw(13_893),
                    font_pack: Some(pack),
                    limits: RenderLimits {
                        max_columns: 6,
                        ..RenderLimits::default()
                    },
                    ..RenderOptions::default()
                },
                ..PrintOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            document.report.source.range,
            RenderRange::new(0, 1_024, 0, 1_029)
        );
    }

    #[test]
    fn hidden_axes_keep_move_only_size_but_shrink_move_and_size_used_bounds() {
        for kind in [
            DrawingObjectKind::Image,
            DrawingObjectKind::Chart,
            DrawingObjectKind::Shape,
        ] {
            for right_to_left in [false, true] {
                let move_and_size = imported_hidden_two_cell_drawing(kind, false, right_to_left);
                let metadata = &move_and_size.sheets[0].drawing_metadata()[0];
                assert_eq!(metadata.behavior, DrawingAnchorBehavior::MoveAndSize);
                assert_eq!(metadata.to_offset_emu, Some((0, 0)));
                assert_eq!(metadata.absolute_size_emu, None);
                let resized = build_scene(
                    &move_and_size,
                    0,
                    &RenderOptions {
                        gridlines: false,
                        ..RenderOptions::default()
                    },
                )
                .unwrap();
                assert_eq!(
                    resized.report.range,
                    RenderRange::new(3, 2, 4, 2),
                    "{kind:?} MoveAndSize rtl={right_to_left}"
                );
                assert_eq!(resized.scene.width, Fixed::from_pixels(64));
                assert_eq!(resized.scene.height, Fixed::from_pixels(40));

                let move_only = imported_hidden_two_cell_drawing(kind, true, right_to_left);
                let metadata = &move_only.sheets[0].drawing_metadata()[0];
                assert_eq!(metadata.behavior, DrawingAnchorBehavior::MoveOnly);
                assert_eq!(metadata.to_offset_emu, Some((0, 0)));
                assert_eq!(metadata.absolute_size_emu, Some((1_619_250, 666_750)));
                let fixed = build_scene(
                    &move_only,
                    0,
                    &RenderOptions {
                        gridlines: false,
                        ..RenderOptions::default()
                    },
                )
                .unwrap();
                assert_eq!(
                    fixed.report.range,
                    RenderRange::new(3, 2, 9, 7),
                    "{kind:?} MoveOnly rtl={right_to_left}"
                );
                assert_eq!(fixed.scene.width, Fixed::from_pixels(192));
                assert_eq!(fixed.scene.height, Fixed::from_pixels(80));
                assert_eq!(
                    fixed_drawing_outer_rect(&fixed.scene.nodes),
                    Rect {
                        x: Fixed::from_pixels(if right_to_left { 22 } else { 0 }),
                        y: Fixed::ZERO,
                        width: Fixed::from_pixels(170),
                        height: Fixed::from_pixels(70),
                    }
                );

                let (drawing_extents, has_absolute) = prepared_drawing_geometry_extent(
                    &move_only.sheets[0],
                    &[RenderRange::new(8, 6, 9, 7)],
                    &RenderOptions {
                        gridlines: false,
                        ..RenderOptions::default()
                    },
                )
                .unwrap();
                assert!(!has_absolute);
                assert_eq!(
                    drawing_extents,
                    vec![RenderRange::new(3, 2, 9, 7)],
                    "the print continuation after the raw F8 marker must retain full geometry"
                );

                let document = build_print_document(
                    &move_only,
                    0,
                    &PrintOptions {
                        single_page_sheets: true,
                        render: RenderOptions {
                            gridlines: false,
                            ..RenderOptions::default()
                        },
                        ..PrintOptions::default()
                    },
                )
                .unwrap();
                assert_eq!(document.report.source.range, RenderRange::new(3, 2, 9, 7));
                assert_eq!(document.pages[0].scene.width, Fixed::from_raw(190_493));
                assert_eq!(document.pages[0].scene.height, Fixed::from_raw(81_933));
                assert_eq!(
                    fixed_drawing_outer_rect(&document.pages[0].scene.nodes),
                    Rect {
                        x: if right_to_left {
                            Fixed::from_raw(16_413)
                        } else {
                            Fixed::ZERO
                        },
                        y: Fixed::ZERO,
                        width: Fixed::from_pixels(170),
                        height: Fixed::from_pixels(70),
                    },
                    "SinglePageSheets must reflect the fixed-size object inside Calc's cumulative imported width"
                );
            }
        }
    }

    #[test]
    fn used_bounds_render_cell_anchored_shapes_as_explicit_placeholders() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Shapes" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData/><drawing r:id="rIdDraw"/></worksheet>"#,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
            ),
            (
                "xl/drawings/drawing1.xml",
                r#"<wsDr><twoCellAnchor><from><col>1</col><row>2</row></from><to><col>4</col><row>5</row></to><sp><nvSpPr><cNvPr id="1" name="Callout"/></nvSpPr></sp></twoCellAnchor></wsDr>"#,
            ),
        ];
        for (name, body) in parts {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        let workbook = Workbook::open(&writer.finish().unwrap().into_inner()).unwrap();
        let build = build_scene(
            &workbook,
            0,
            &RenderOptions {
                gridlines: false,
                ..RenderOptions::default()
            },
        )
        .unwrap();

        assert_eq!(build.report.range, RenderRange::new(2, 1, 5, 4));
        assert!(build.report.warnings.iter().any(|warning| {
            warning.code == WarningCode::ShapePlaceholder && warning.occurrences == 1
        }));
        assert!(!build
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::ShapeAnchorUnavailable));
        assert!(build.scene.nodes.iter().any(|node| matches!(
            node,
            SceneNode::Rect(RectNode {
                fill: Some(Rgb {
                    red: 221,
                    green: 235,
                    blue: 247,
                }),
                ..
            })
        )));
    }

    #[test]
    fn single_page_native_rows_use_global_hidden_prefix_and_final_dimension() {
        let workbook = imported_xlsx(
            "<styleSheet/>",
            r#"<worksheet><sheetFormatPr defaultRowHeight="12.85"/><sheetData><row r="2" hidden="1"/><row r="3"><c r="A3" t="inlineStr"><is><t>visible</t></is></c></row></sheetData></worksheet>"#,
        );
        let sheet = &workbook.sheets[0];
        let selected = RenderRange::new(2, 0, 2, 0);
        let base = RenderOptions {
            selection: RenderSelection::Range(selected),
            gridlines: false,
            default_column_width: Fixed::from_pixels(1),
            ..RenderOptions::default()
        };

        let ordinary = build_sheet_scene(sheet, 0, &base).unwrap();
        let single_page = build_single_page_sheet_scene(sheet, 0, &base).unwrap();
        assert_eq!(ordinary.scene.height, Fixed::from_raw(17_545));
        assert_eq!(
            single_page.scene.height,
            Fixed::from_raw(17_610),
            "row 3 must inherit the cumulative endpoint phase of visible row 1, skip hidden row 2, and retain the inclusive rectangle unit"
        );

        let tight = RenderOptions {
            limits: RenderLimits {
                max_dimension_raw: 17_571,
                ..RenderLimits::default()
            },
            ..base.clone()
        };
        assert_eq!(
            build_single_page_sheet_scene(sheet, 0, &tight),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::Dimension,
                limit: 17_571,
                actual: 17_610,
            })
        );
        assert_eq!(
            build_sheet_scene(sheet, 0, &tight).unwrap().scene.height,
            Fixed::from_raw(17_545)
        );

        let full_range = RenderRange::new(0, 0, 2, 0);
        let full = RenderOptions {
            selection: RenderSelection::Range(full_range),
            include_hidden: true,
            ..base
        };
        let ordinary = build_sheet_scene(sheet, 0, &full).unwrap();
        let single_page = build_single_page_sheet_scene(sheet, 0, &full).unwrap();
        assert_eq!(ordinary.scene.height, Fixed::from_raw(52_635));
        assert_eq!(single_page.scene.height, Fixed::from_raw(52_674));

        let measured = measure_sheet_axes_for_ranges(sheet, &[full_range], &full).unwrap();
        assert_eq!(
            measured[0]
                .0
                .iter()
                .map(|slot| slot.size)
                .collect::<Vec<_>>(),
            vec![Fixed::from_raw(17_545); 3],
            "prepared paginated geometry must remain per-track Fixed"
        );
        let replay = build_sheet_scene_with_geometry(
            sheet,
            0,
            &full,
            SheetGeometryOverride::new(&measured[0].0, &measured[0].1),
        )
        .unwrap();
        assert_eq!(replay.scene.height, Fixed::from_raw(52_635));
    }

    #[test]
    fn single_page_native_row_baselines_precede_merged_auto_height() {
        let pack = synthetic_test_pack();
        let family = pack.default_family();
        let styles = format!(
            r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let text = "한글中文한글中文한글中文한글中文한글中文";
        let unmerged = imported_xlsx(
            &styles,
            &format!(
                r#"<worksheet><sheetFormatPr defaultRowHeight="12.85" defaultColWidth="2"/><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{text}</t></is></c></row></sheetData></worksheet>"#
            ),
        );
        let merged = imported_xlsx(
            &styles,
            &format!(
                r#"<worksheet><sheetFormatPr defaultRowHeight="12.85" defaultColWidth="2"/><sheetData><row r="1"><c r="A1" s="1" t="inlineStr"><is><t>{text}</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:A3"/></mergeCells></worksheet>"#
            ),
        );
        let render = |workbook: &Workbook, range| {
            build_single_page_sheet_scene(
                &workbook.sheets[0],
                0,
                &RenderOptions {
                    selection: RenderSelection::Range(range),
                    gridlines: false,
                    default_font_family: family.to_string(),
                    font_pack: Some(pack.clone()),
                    ..RenderOptions::default()
                },
            )
            .unwrap()
            .scene
            .height
        };
        let required = render(&unmerged, RenderRange::new(0, 0, 0, 0));
        let merged_height = render(&merged, RenderRange::new(0, 0, 2, 0));
        assert!(required > Fixed::from_raw(52_634));
        assert_eq!(
            merged_height, required,
            "native baseline residuals must be resolved before the merged-row deficit"
        );
    }

    #[test]
    fn imported_row_manuality_controls_single_page_auto_height_expansion() {
        let pack = synthetic_test_pack();
        let family = pack.default_family();
        let styles = format!(
            r#"<styleSheet><fonts count="1"><font><sz val="11"/><name val="{family}"/></font></fonts><cellStyleXfs count="1"><xf fontId="0"/></cellStyleXfs><cellXfs count="2"><xf fontId="0" xfId="0"/><xf fontId="0" xfId="0" applyAlignment="1"><alignment wrapText="1" vertical="top"/></xf></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        );
        let text = "한글中文한글中文한글中文한글中文한글中文";
        let height = |default_custom: bool, row_height: Option<&str>, row_custom: Option<&str>| {
            let row_height = row_height
                .map(|height| format!(r#" ht="{height}""#))
                .unwrap_or_default();
            let row_custom = row_custom
                .map(|value| format!(r#" customHeight="{value}""#))
                .unwrap_or_default();
            let workbook = imported_xlsx(
                &styles,
                &format!(
                    r#"<worksheet><sheetFormatPr defaultRowHeight="12.85" customHeight="{}" defaultColWidth="2"/><sheetData><row r="1"{row_height}{row_custom}><c r="A1" s="1" t="inlineStr"><is><t>{text}</t></is></c></row></sheetData></worksheet>"#,
                    u8::from(default_custom)
                ),
            );
            build_single_page_sheet_scene(
                &workbook.sheets[0],
                0,
                &RenderOptions {
                    gridlines: false,
                    default_font_family: family.to_string(),
                    font_pack: Some(pack.clone()),
                    ..RenderOptions::default()
                },
            )
            .unwrap()
            .scene
            .height
        };

        let manual_row = height(false, Some("12.85"), Some("1"));
        let automatic_row = height(false, Some("12.85"), Some("0"));
        assert!(automatic_row > manual_row);
        assert_eq!(
            height(false, Some("12.85"), None),
            automatic_row,
            "an absent customHeight flag retains a cached automatic baseline"
        );
        assert_eq!(
            height(true, None, None),
            manual_row,
            "a manual default fixes rows without an explicit cached height"
        );
        assert_eq!(
            height(true, Some("12.85"), Some("0")),
            automatic_row,
            "an explicit automatic row overrides an inherited manual default"
        );
    }

    #[test]
    fn single_page_fixed_extent_and_absolute_origin_share_native_boundaries() {
        let workbook = imported_xlsx(
            "<styleSheet/>",
            r#"<worksheet><sheetFormatPr defaultRowHeight="12.85"/><sheetData/></worksheet>"#,
        );
        let sheet = &workbook.sheets[0];
        let options = RenderOptions::default();
        let maximum_digit_width = Fixed::from_pixels(7);

        assert_eq!(
            fixed_size_used_row(
                sheet,
                0,
                0,
                489_598,
                maximum_digit_width,
                &options,
                AxisEndpointPolicy::PerTrackFixed,
            )
            .unwrap(),
            2
        );
        assert_eq!(
            fixed_size_used_row(
                sheet,
                0,
                0,
                489_598,
                maximum_digit_width,
                &options,
                AxisEndpointPolicy::SourceNative,
            )
            .unwrap(),
            2,
            "drawing anchors and SinglePageSheets share the same cumulative twip endpoints before the page rectangle's terminal inclusive unit"
        );

        let range = RenderRange::new(3, 0, 3, 0);
        let mut warnings = Warnings::default();
        assert_eq!(
            sheet_grid_origin_with_policy(
                sheet,
                range,
                maximum_digit_width,
                &options,
                AxisEndpointPolicy::PerTrackFixed,
                &mut warnings,
            )
            .unwrap()
            .1,
            Fixed::from_raw(52_635)
        );
        let mut warnings = Warnings::default();
        assert_eq!(
            sheet_grid_origin_with_policy(
                sheet,
                range,
                maximum_digit_width,
                &options,
                AxisEndpointPolicy::SourceNative,
                &mut warnings,
            )
            .unwrap()
            .1,
            Fixed::from_raw(52_635)
        );
    }

    #[test]
    fn source_axis_ratio_variants_quantize_to_calc_twips() {
        assert_eq!(
            imported_axis_measure_twips(
                ImportedAxisMeasure::PointRatio(15, 1),
                Fixed::from_pixels(7),
            ),
            Some(300)
        );
        assert_eq!(
            imported_axis_measure_twips(
                ImportedAxisMeasure::CharacterWidthRatio(843, 100),
                Fixed::from_pixels(7),
            ),
            Some(885)
        );
    }

    #[test]
    fn source_character_widths_follow_format_specific_calc_importers() {
        const NOTO_11_MDW: Fixed = Fixed::from_raw(8_336);
        const CARLITO_11_MDW: Fixed = Fixed::from_raw(7_612);

        assert_eq!(
            imported_axis_measure_twips(
                ImportedAxisMeasure::CharacterWidth256(18 * 256),
                NOTO_11_MDW,
            ),
            Some(2_195),
            "BIFF truncates width times digit twips after subtracting one half"
        );
        assert_eq!(
            imported_axis_measure_twips(
                ImportedAxisMeasure::CharacterWidthRatio(18, 1),
                NOTO_11_MDW,
            ),
            Some(2_196),
            "OOXML rounds width times digit twips without BIFF's half-twip bias"
        );
        assert_eq!(
            imported_axis_measure_twips(
                ImportedAxisMeasure::DigitWidth256(18 * 256),
                CARLITO_11_MDW,
            ),
            Some(1_998),
            "XLSB uses the rounded standard-digit width"
        );
        assert_eq!(
            imported_axis_measure_twips(
                ImportedAxisMeasure::CharacterBaseWidth256(8 * 256),
                NOTO_11_MDW,
            ),
            Some(1_051),
            "OOXML base widths add five 96-DPI screen pixels"
        );
    }

    #[test]
    fn exact_character_widths_match_legacy_digit_pixel_quantization() {
        for maximum_digit_width in [Fixed::from_raw(7_612), Fixed::from_raw(8_336)] {
            for (numerator, denominator) in [(2_160_u64, 256_u64), (843, 100)] {
                let characters = numerator as f32 / denominator as f32;
                let expected = column_chars_to_fixed(
                    characters,
                    maximum_digit_width,
                    IMPORTED_COLUMN_PADDING_PIXELS,
                )
                .unwrap();
                let pixels = character_width_ratio_to_pixels(
                    numerator,
                    denominator,
                    maximum_digit_width,
                    IMPORTED_COLUMN_PADDING_PIXELS,
                )
                .unwrap();
                assert_eq!(
                    pixels * i128::from(FIXED_UNITS_PER_PIXEL),
                    i128::from(expected.raw()),
                    "{numerator}/{denominator} characters at {maximum_digit_width:?}"
                );
            }
        }
    }

    #[test]
    fn source_native_endpoints_follow_calc_cumulative_twip_boundaries() {
        fn sizes(measure: ImportedAxisMeasure, count: u64, prefix_count: u64) -> Vec<(i64, i64)> {
            let contribution = imported_axis_measure_twips(measure, Fixed::from_pixels(7)).unwrap();
            let prefix = contribution.checked_mul(i128::from(prefix_count)).unwrap();
            let mut cursor = SourceAxisCursor::new(prefix).unwrap();
            (0..count)
                .map(|_| {
                    let (offset, size, _) = cursor.advance(contribution).unwrap();
                    (offset.raw(), size.raw())
                })
                .collect()
        }

        assert_eq!(
            sizes(ImportedAxisMeasure::Twips(280), 3, 0),
            [(0, 19_119), (19_119, 19_119), (38_238, 19_119)]
        );
        assert_eq!(
            sizes(ImportedAxisMeasure::MillimeterHundredths(2_000), 3, 0),
            [(0, 77_405), (77_405, 77_443), (154_848, 77_405)]
        );
        assert_eq!(
            sizes(ImportedAxisMeasure::DigitWidth256(2_432), 4, 0),
            [
                (0, 68_116),
                (68_116, 68_155),
                (136_271, 68_116),
                (204_387, 68_116),
            ]
        );
        assert_eq!(
            sizes(ImportedAxisMeasure::Twips(280), 2, 1),
            [(0, 19_119), (19_119, 19_119)],
            "a nonzero selection must retain the global rounding phase"
        );

        for measure in [
            ImportedAxisMeasure::Twips(280),
            ImportedAxisMeasure::MillimeterHundredths(2_000),
            ImportedAxisMeasure::PointRatio(14, 1),
            ImportedAxisMeasure::CharacterWidth256(2_048),
            ImportedAxisMeasure::CharacterWidthRatio(843, 100),
            ImportedAxisMeasure::CharacterBaseWidth256(2_048),
            ImportedAxisMeasure::DigitWidth256(2_432),
            ImportedAxisMeasure::DigitBaseWidth256(2_048),
        ] {
            assert!(
                imported_axis_measure_twips(measure, Fixed::from_pixels(7))
                    .is_some_and(|twips| twips > 0),
                "missing exact conversion for {measure:?}"
            );
        }
    }
}
