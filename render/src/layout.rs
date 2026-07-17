//! Bounded worksheet-to-scene layout.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::sync::Arc;

use rxls::{
    Border, BorderStyle, Cell, CellStyle, CfRule, Chart, ChartBarDirection, ChartCachedPoint,
    ChartKind, ChartMarkerSymbol, ChartSeriesStyle, Color, DisplayCell, DrawingAnchorBehavior,
    DrawingMetadata, DrawingObjectKind, DvOp, FormatPattern, FormatScript, HAlign, Sheet,
    Sparkline, SparklineKind, StyleFidelity, VAlign, Workbook,
};

use crate::error::{LimitKind, RenderError};
use crate::font::{
    BaseDirection, FontOutlineCommand, FontPack, FontPackError, FontRequest, ShapeOptions,
    ShapedText, StyledFontRequest, FONT_OUTLINE_UNITS,
};
use crate::media::decode_image;
use crate::scene::{
    ClipGroupNode, Fixed, GlyphCluster, GlyphPaint, GlyphRunNode, ImageNode, LineNode, PathCommand,
    PathNode, Rect, RectNode, Rgb, Scene, SceneNode, TextAnchor, TextBaseline, TextNode, TextStyle,
    FIXED_UNITS_PER_PIXEL,
};
use crate::typography::wrap_text_ranges;
use unicode_bidi::{bidi_class, BidiClass};

/// Largest supported zero-based worksheet row (Excel row 1,048,576).
pub const MAX_WORKSHEET_ROW: u32 = 1_048_575;
/// Largest supported zero-based worksheet column (Excel column XFD).
pub const MAX_WORKSHEET_COLUMN: u16 = 16_383;
/// Deterministic default character count used with verified font metrics.
const DEFAULT_COLUMN_CHARACTERS: f32 = 10.0;
/// LibreOffice Calc's import geometry adds two device pixels to explicit
/// Excel character widths. Missing-width fallback keeps the ECMA five-pixel
/// allowance because it is derived from the verified default font instead.
const IMPORTED_COLUMN_PADDING_PIXELS: f64 = 2.0;
const DEFAULT_COLUMN_PADDING_PIXELS: f64 = 5.0;
/// Calc's application default when an OOXML worksheet omits sheet-format
/// width metadata. Its import is byte-for-byte equivalent to an explicit 8.5.
const OOXML_APPLICATION_DEFAULT_COLUMN_CHARACTERS: f32 = 8.5;
/// `baseColWidth` / XLSB `cchDefColWidth` exclude the four margin pixels and
/// one gridline pixel included when deriving a default column width.
const OOXML_BASE_COLUMN_EXTRA_PADDING_PIXELS: f64 = 5.0;
/// Calc leaves a small, DPI-stable vertical inset around optimally sized
/// multiline cells. The pinned LibreOffice oracle rounds this to about two CSS
/// pixels at 96 DPI; keeping it fixed avoids platform font or UI dependencies.
const AUTO_ROW_VERTICAL_PADDING_PIXELS: i64 = 2;

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
    /// Paint worksheet gridlines unless the source sheet explicitly hides them.
    pub gridlines: bool,
    /// Give hidden rows and columns their normal geometry instead of omitting them.
    pub include_hidden: bool,
    /// Canvas background.
    pub background: Rgb,
    /// Fallback pixel width when the workbook has no width metadata or verified font.
    pub default_column_width: Fixed,
    /// Fallback row height when the workbook has no height metadata.
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

struct AxisMeasurement {
    rows: Vec<MeasuredAxisSlot<u32>>,
    columns: Vec<MeasuredAxisSlot<u16>>,
    maximum_digit_width: Fixed,
    typography: TypographyStats,
}

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
pub(crate) fn measure_sheet_axes(
    sheet: &Sheet,
    range: RenderRange,
    options: &RenderOptions,
) -> Result<MeasuredAxes, RenderError> {
    let range = range.validate()?;
    let cells = (u64::from(range.last_row) - u64::from(range.first_row) + 1)
        .checked_mul(u64::from(range.last_col) - u64::from(range.first_col) + 1)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(LimitKind::Cells, options.limits.max_cells, cells)?;
    let mut style_snapshot = RenderStyleSnapshot::new(sheet);
    style_snapshot.capture_range(sheet, range, options)?;
    let mut warnings = Warnings::default();
    let measured = measure_sheet_axes_inner(sheet, range, &style_snapshot, options, &mut warnings)?;
    Ok((measured.rows, measured.columns))
}

fn measure_sheet_axes_inner(
    sheet: &Sheet,
    range: RenderRange,
    style_snapshot: &RenderStyleSnapshot,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<AxisMeasurement, RenderError> {
    let range = range.validate()?;
    let row_count = u64::from(range.last_row) - u64::from(range.first_row) + 1;
    let column_count = u64::from(range.last_col) - u64::from(range.first_col) + 1;
    enforce(LimitKind::Rows, options.limits.max_rows, row_count)?;
    enforce(LimitKind::Columns, options.limits.max_columns, column_count)?;
    let mut typography = TypographyStats::default();
    let maximum_digit_width =
        maximum_digit_width(style_snapshot, options, warnings, &mut typography)?;
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
        x = x.checked_add(size).ok_or(RenderError::CoordinateOverflow)?;
        enforce_dimension(x, options)?;
    }

    let mut row_sizes = BTreeMap::new();
    for row in range.first_row..=range.last_row {
        if !options.include_hidden && sheet.hidden_rows().contains(&row) {
            continue;
        }
        row_sizes.insert(row, row_height(sheet, row, options, warnings));
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
    )?;

    let mut rows = Vec::with_capacity(row_sizes.len());
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
    Ok(AxisMeasurement {
        rows,
        columns,
        maximum_digit_width,
        typography,
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
    let Some(font_pack) = options.font_pack.as_ref() else {
        return Ok(SceneNode::Text(TextNode {
            text,
            bounds,
            clip_bounds: bounds,
            horizontal_padding,
            style,
            hyperlink: None,
        }));
    };
    let region = Region {
        source: CellCoordinate { row: 0, col: 0 },
        rect: bounds,
        is_merged: false,
        style: None,
        conditional: ConditionalPaint::default(),
        text,
        rich_text: None,
        hyperlink: None,
        numeric_default: false,
        text_can_overflow: false,
    };
    let mut auxiliary_options = options.clone();
    auxiliary_options.horizontal_padding = horizontal_padding;
    let mut statistics = TypographyStats::default();
    let mut warnings = Warnings::default();
    Ok(SceneNode::GlyphRun(build_glyph_run(
        font_pack,
        &region,
        bounds,
        &style,
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
}

#[derive(Debug, Clone)]
struct Region {
    source: CellCoordinate,
    rect: Rect,
    is_merged: bool,
    style: Option<CellStyle>,
    conditional: ConditionalPaint,
    text: String,
    rich_text: Option<Vec<rxls::TextRun>>,
    hyperlink: Option<String>,
    numeric_default: bool,
    text_can_overflow: bool,
}

#[derive(Debug, Clone, Default)]
struct ConditionalPaint {
    style: Option<CellStyle>,
    data_bar: Option<DataBarPaint>,
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

#[derive(Default)]
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

#[derive(Default)]
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
    build_sheet_scene_inner(sheet, sheet_index, options)
}

fn build_sheet_scene_inner(
    sheet: &Sheet,
    sheet_index: usize,
    options: &RenderOptions,
) -> Result<SceneBuild, RenderError> {
    let mut style_snapshot = RenderStyleSnapshot::new(sheet);
    let used_selection = matches!(options.selection, RenderSelection::Used);
    let used_extent = match options.selection {
        RenderSelection::Used => {
            style_snapshot.capture_sparse_visual_candidates(sheet, options)?;
            Some(render_used_extent(sheet, &style_snapshot)?)
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
    let measured = measure_sheet_axes_inner(sheet, range, &style_snapshot, options, &mut warnings)?;
    let mut row_slots = measured.rows;
    let mut col_slots = measured.columns;
    let maximum_digit_width = measured.maximum_digit_width;
    let mut typography_stats = measured.typography;
    let hidden_rows_skipped = rows_considered.saturating_sub(row_slots.len() as u64);
    let hidden_columns_skipped = columns_considered.saturating_sub(col_slots.len() as u64);
    let y = axis_slots_end(&row_slots)?;
    let x = axis_slots_end(&col_slots)?;
    let viewport = drawing_layout_viewport(
        sheet,
        range,
        x,
        y,
        maximum_digit_width,
        used_selection,
        options,
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

    let display_cells = sheet
        .display_cells()
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
    let mut regions = Vec::new();
    for row in &row_slots {
        for col in visual_col_slots {
            let coordinate = CellCoordinate {
                row: row.index,
                col: col.index,
            };
            let (source, rect, is_merged) = if let Some(&merge_index) = merge_cover.get(&coordinate)
            {
                let merge = &merge_layouts[merge_index];
                if coordinate != merge.owner {
                    continue;
                }
                (merge.anchor, merge.rect, true)
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
            regions.push(Region {
                source,
                rect,
                is_merged,
                style,
                conditional: ConditionalPaint::default(),
                text,
                rich_text,
                hyperlink,
                numeric_default,
                text_can_overflow,
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
    let show_gridlines = options.gridlines && !sheet.sheet_view().hide_gridlines;
    resolve_conditional_paints(sheet, &display_cells, &mut regions, options, &mut warnings)?;
    for region in &regions {
        let fill = resolve_fill(region.style.as_ref(), region.source, &mut warnings);
        if fill.is_some() || show_gridlines {
            push_node(
                &mut nodes,
                SceneNode::Rect(RectNode {
                    rect: region.rect,
                    fill,
                    stroke: show_gridlines.then_some(Rgb::GRIDLINE),
                    stroke_width: Fixed::from_pixels(1),
                }),
                options,
            )?;
        }
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
        let clip_bounds = text_clip_bounds(region_index, &regions, &row_regions, &style)?;
        let node = match options.font_pack.as_ref() {
            Some(font_pack) => SceneNode::GlyphRun(build_glyph_run(
                font_pack,
                region,
                clip_bounds,
                &style,
                sheet_right_to_left,
                options,
                &mut typography_stats,
                &mut warnings,
            )?),
            None => SceneNode::Text(TextNode {
                text: region.text.clone(),
                bounds: region.rect,
                clip_bounds,
                horizontal_padding: options.horizontal_padding,
                style,
                hyperlink: region.hyperlink.clone(),
            }),
        };
        push_node(&mut nodes, node, options)?;
    }
    for region in &regions {
        if let Some(border) = region
            .style
            .as_ref()
            .and_then(|style| style.border.as_ref())
        {
            push_borders(
                &mut nodes,
                region.rect,
                border,
                region.source,
                options,
                &mut warnings,
            )?;
        }
    }
    push_drawing_placeholders(
        &mut nodes,
        sheet,
        &row_slots,
        &col_slots,
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
) -> Result<UsedRenderExtent, RenderError> {
    let mut extent = UsedRenderExtent::default();
    let mut retained_cells = BTreeMap::<u32, BTreeSet<u16>>::new();
    let metadata_index = DrawingMetadataIndex::new(sheet);

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
        let to = drawing_visible_to(image.from, to, metadata);
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
        let to = drawing_visible_to(chart.from, chart.to, metadata);
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
        if let Some(to) = metadata.to_cell {
            let to = drawing_visible_to(from, to, Some(metadata));
            include_render_coordinate(
                &mut extent.range,
                to.0.min(MAX_WORKSHEET_ROW),
                to.1.min(MAX_WORKSHEET_COLUMN),
            );
        }
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

fn drawing_visible_to(
    from: (u32, u16),
    to: (u32, u16),
    metadata: Option<&DrawingMetadata>,
) -> (u32, u16) {
    let Some((column_offset, row_offset)) = metadata.and_then(|metadata| metadata.to_offset_emu)
    else {
        return to;
    };
    let row = if row_offset > 0 {
        to.0
    } else {
        to.0.saturating_sub(1).max(from.0)
    };
    let column = if column_offset > 0 {
        to.1
    } else {
        to.1.saturating_sub(1).max(from.1)
    };
    (row, column)
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

fn cell_style_has_visible_blank_paint(style: &CellStyle) -> bool {
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
    let mut range = render_used_extent(sheet, &style_snapshot)?
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
    let explicit_chars = sheet
        .column_widths()
        .get(&col)
        .copied()
        .or_else(|| sheet.default_column_width());
    let (measured, invalid_source_geometry) = match explicit_chars {
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
            Some(None) if options.font_pack.is_some() => (
                column_chars_to_fixed(
                    OOXML_APPLICATION_DEFAULT_COLUMN_CHARACTERS,
                    maximum_digit_width,
                    IMPORTED_COLUMN_PADDING_PIXELS,
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
            options.default_row_height.max(Fixed::from_raw(1))
        }
    }
}

#[derive(Debug)]
struct AutoMergeHeight {
    rows: Vec<u32>,
    adjustable_row: u32,
    required: Fixed,
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
) -> Result<(), RenderError> {
    let Some(pack) = options.font_pack.as_ref() else {
        return Ok(());
    };

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
    let mut measured_cells = 0_u64;

    for cell in sheet.display_cells() {
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
        let (visible_rows, adjustable_row, width, is_merged) =
            if let Some((r0, c0, r1, c1)) = merged {
                let visible_rows = row_sizes
                    .range(r0.max(range.first_row)..=r1.min(range.last_row))
                    .map(|(&row, _)| row)
                    .collect::<Vec<_>>();
                let Some(adjustable_row) = visible_rows
                    .iter()
                    .copied()
                    .find(|row| !sheet.row_heights().contains_key(row))
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
                (visible_rows, adjustable_row, width, true)
            } else {
                if !row_sizes.contains_key(&cell.row)
                    || sheet.row_heights().contains_key(&cell.row)
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
                (vec![cell.row], cell.row, width, false)
            };

        let style = style_snapshot.owned_style(source);
        let alignment = style.as_ref().and_then(|style| style.align.as_ref());
        let font_size = style
            .as_ref()
            .and_then(|style| style.font.as_ref())
            .and_then(|font| font.size_pt)
            .and_then(|points| points_to_fixed(points as f32))
            .unwrap_or(options.default_font_size);
        let rich_text = cell.rich_text.filter(|runs| !runs.is_empty());
        if !alignment.is_some_and(|alignment| alignment.wrap)
            && !contains_mandatory_line_break(cell.formatted)
            && rich_text.is_none()
            && font_size <= options.default_font_size
        {
            continue;
        }

        measured_cells = measured_cells
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(LimitKind::Cells, options.limits.max_cells, measured_cells)?;
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
        let region = Region {
            source,
            rect: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width,
                height: Fixed::from_raw(1),
            },
            is_merged,
            style,
            conditional: ConditionalPaint::default(),
            text,
            rich_text,
            hyperlink: None,
            numeric_default: false,
            text_can_overflow: false,
        };
        let required = measure_automatic_cell_height(
            pack,
            &region,
            sheet.sheet_view().right_to_left,
            options,
            typography,
        )?;
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
    padding_pixels: f64,
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
        + padding_pixels;
    float_pixels_to_fixed(pixels)
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
        let count = available.raw().max(1) / hash_width.raw().max(1);
        let count = usize::try_from(count.max(1)).map_err(|_| RenderError::CoordinateOverflow)?;
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
) -> Result<(), RenderError> {
    let mut paints = BTreeMap::<CellCoordinate, ConditionalPaint>::new();
    let mut stopped = BTreeSet::<CellCoordinate>::new();
    let mut evaluations = 0_u64;
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
                if style.num_fmt.take().is_some() {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                }
                if style.protection.take().is_some() {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                }
                style
            });
        let has_imported_differential =
            rule_metadata.is_some_and(|metadata| metadata.differential_style.is_some());
        let range = conditional.sqref;
        if range.0 > range.2 || range.1 > range.3 {
            warnings.add(WarningCode::ConditionalFormattingDeferred, None);
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
                    continue;
                };
                let second = match op {
                    DvOp::Between | DvOp::NotBetween => {
                        let Some(second) = formula2.as_deref().and_then(parse_conditional_operand)
                        else {
                            warnings.add(WarningCode::ConditionalFormattingDeferred, None);
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
                    bump_conditional_evaluations(&mut evaluations, options)?;
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
                    continue;
                }
                for coordinate in matches {
                    apply_conditional_paint(
                        &mut paints,
                        &mut stopped,
                        coordinate,
                        Some(conditional_fill_overlay(
                            rgb(*fill),
                            differential_style.as_ref(),
                            has_imported_differential,
                        )),
                        None,
                        stop_if_true,
                    );
                }
            }
            CfRule::ColorScale2 { min, max } => {
                let values =
                    conditional_numeric_values(display_cells, range, &mut evaluations, options)?;
                let Some((minimum, maximum)) = numeric_bounds(&values) else {
                    continue;
                };
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(&mut evaluations, options)?;
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
                        Some(conditional_fill_overlay(
                            interpolate_rgb(rgb(*min), rgb(*max), ratio),
                            differential_style.as_ref(),
                            has_imported_differential,
                        )),
                        None,
                        stop_if_true,
                    );
                }
            }
            CfRule::ColorScale3 { min, mid, max } => {
                let mut values =
                    conditional_numeric_values(display_cells, range, &mut evaluations, options)?;
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
                    bump_conditional_evaluations(&mut evaluations, options)?;
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
                        Some(conditional_fill_overlay(
                            color,
                            differential_style.as_ref(),
                            has_imported_differential,
                        )),
                        None,
                        stop_if_true,
                    );
                }
            }
            CfRule::DataBar { color } => {
                let values =
                    conditional_numeric_values(display_cells, range, &mut evaluations, options)?;
                let Some((minimum, maximum)) = numeric_bounds(&values) else {
                    continue;
                };
                warnings.add(WarningCode::ConditionalDataBarSimplified, None);
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(&mut evaluations, options)?;
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
                        differential_style.clone(),
                        Some(DataBarPaint {
                            color: rgb(*color),
                            width_ppm: normalized_ppm(value, minimum, maximum),
                        }),
                        stop_if_true,
                    );
                }
            }
            CfRule::TopBottom {
                rank,
                bottom,
                percent,
                fill,
            } => {
                let mut values =
                    conditional_numeric_values(display_cells, range, &mut evaluations, options)?;
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
                    bump_conditional_evaluations(&mut evaluations, options)?;
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
                            Some(conditional_fill_overlay(
                                rgb(*fill),
                                differential_style.as_ref(),
                                has_imported_differential,
                            )),
                            None,
                            stop_if_true,
                        );
                    }
                }
            }
            CfRule::AboveAverage { below, fill } => {
                let values =
                    conditional_numeric_values(display_cells, range, &mut evaluations, options)?;
                if values.is_empty() {
                    continue;
                }
                let sum = values.iter().try_fold(0.0_f64, |sum, value| {
                    let next = sum + value;
                    next.is_finite().then_some(next)
                });
                let Some(sum) = sum else {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    continue;
                };
                let average = sum / values.len() as f64;
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(&mut evaluations, options)?;
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
                            Some(conditional_fill_overlay(
                                rgb(*fill),
                                differential_style.as_ref(),
                                has_imported_differential,
                            )),
                            None,
                            stop_if_true,
                        );
                    }
                }
            }
            CfRule::DuplicateValues { unique, fill } => {
                let Some(keys) =
                    conditional_value_keys(display_cells, range, &mut evaluations, options)?
                else {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
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
                    bump_conditional_evaluations(&mut evaluations, options)?;
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
                            Some(conditional_fill_overlay(
                                rgb(*fill),
                                differential_style.as_ref(),
                                has_imported_differential,
                            )),
                            None,
                            stop_if_true,
                        );
                    }
                }
            }
            CfRule::Expression { formula, fill } => {
                let Some(expression) = parse_conditional_expression(formula) else {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    continue;
                };
                let mut matches = Vec::new();
                let mut deferred = false;
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(&mut evaluations, options)?;
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
                    continue;
                }
                for coordinate in matches {
                    apply_conditional_paint(
                        &mut paints,
                        &mut stopped,
                        coordinate,
                        Some(conditional_fill_overlay(
                            rgb(*fill),
                            differential_style.as_ref(),
                            has_imported_differential,
                        )),
                        None,
                        stop_if_true,
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
    Ok(())
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
    display_cells: &BTreeMap<CellCoordinate, DisplayCell<'_>>,
    range: (u32, u16, u32, u16),
    evaluations: &mut u64,
    options: &RenderOptions,
) -> Result<Vec<f64>, RenderError> {
    let mut values = Vec::new();
    for (coordinate, cell) in display_cells {
        bump_conditional_evaluations(evaluations, options)?;
        if coordinate_in_range(*coordinate, range) {
            if let Some(value) = numeric_cell_value(cell.value) {
                values.push(value);
            }
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
            if reference.sheet.as_deref().is_some_and(|name| {
                name != sheet.name
                    && !(name.is_ascii()
                        && sheet.name.is_ascii()
                        && name.eq_ignore_ascii_case(&sheet.name))
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
            sheet.cell(row, col).and_then(numeric_cell_value)
        }
    }
}

fn offset_a1_axis(target: u64, reference: u64, origin: u64, maximum: u64) -> Option<u64> {
    let value = i128::from(target)
        .checked_add(i128::from(reference))?
        .checked_sub(i128::from(origin))?;
    (0..=i128::from(maximum))
        .contains(&value)
        .then_some(value as u64)
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
    display_cells: &BTreeMap<CellCoordinate, DisplayCell<'_>>,
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

    let mut keys = BTreeMap::new();
    for row in range.0..=range.2 {
        for col in range.1..=range.3 {
            let coordinate = CellCoordinate { row, col };
            let Some(key) = display_cells
                .get(&coordinate)
                .and_then(|cell| conditional_value_key(cell.value))
            else {
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
    style: Option<CellStyle>,
    data_bar: Option<DataBarPaint>,
    stop_if_true: bool,
) {
    if stopped.contains(&coordinate) {
        return;
    }
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
    let mut placeholders = Vec::<DrawingPlaceholder>::new();
    let mut ordinal = 0_u64;
    for (index, image) in sheet.images().iter().enumerate() {
        let metadata = metadata_index.get(DrawingObjectKind::Image, index);
        let to = image.to.unwrap_or((
            image.from.0.saturating_add(10),
            image.from.1.saturating_add(4),
        ));
        match drawing_rect(
            row_slots,
            col_slots,
            cell_viewport,
            sheet_viewport,
            scene_width,
            DrawingObjectKind::Image,
            image.from,
            to,
            metadata,
            right_to_left,
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
                clip: is_sheet_absolute_metadata(metadata).then_some(Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width: scene_width,
                    height: scene_height,
                }),
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
                clip: is_sheet_absolute_metadata(metadata).then_some(Rect {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    width: scene_width,
                    height: scene_height,
                }),
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
            row_slots,
            col_slots,
            cell_viewport,
            sheet_viewport,
            scene_width,
            DrawingObjectKind::Shape,
            from,
            to,
            Some(metadata),
            right_to_left,
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
                clip: None,
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
    for (index, sparkline) in sheet.sparklines().iter().enumerate() {
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
    grid_width: Fixed,
    grid_height: Fixed,
    maximum_digit_width: Fixed,
    used_selection: bool,
    options: &RenderOptions,
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
    let (grid_x, grid_y) = sheet_grid_origin(sheet, range, maximum_digit_width, options, warnings)?;
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

fn sheet_grid_origin(
    sheet: &Sheet,
    range: RenderRange,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<(Fixed, Fixed), RenderError> {
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
        None => options.default_row_height.max(Fixed::from_raw(1)),
    };
    let hidden_rows = if options.include_hidden {
        0_u64
    } else {
        sheet.hidden_rows().range(..range.first_row).count() as u64
    };
    let visible_rows = u64::from(range.first_row).saturating_sub(hidden_rows);
    let mut y_raw = i128::from(base_row_height.raw())
        .checked_mul(i128::from(visible_rows))
        .ok_or(RenderError::CoordinateOverflow)?;
    for (&row, _) in sheet.row_heights().range(..range.first_row) {
        if !options.include_hidden && sheet.hidden_rows().contains(&row) {
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

pub(crate) fn absolute_drawings_intersect_range(
    sheet: &Sheet,
    range: RenderRange,
    width: Fixed,
    height: Fixed,
    options: &RenderOptions,
) -> Result<bool, RenderError> {
    if width <= Fixed::ZERO || height <= Fixed::ZERO {
        return Ok(false);
    }
    let mut warnings = Warnings::default();
    let mut typography = TypographyStats::default();
    let style_snapshot = RenderStyleSnapshot::new(sheet);
    let maximum_digit_width =
        maximum_digit_width(&style_snapshot, options, &mut warnings, &mut typography)?;
    let (x, y) = sheet_grid_origin(
        sheet,
        range.validate()?,
        maximum_digit_width,
        options,
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
        metadata
            .chart_series_styles
            .iter()
            .map(|style| style.losses.len() as u64)
            .sum()
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
                series.values.iter().any(|value| *value < 0.0)
                    || series.values.iter().sum::<f64>() <= 0.0
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

    let title_height = if chart.title.is_some() { 24 } else { 8 };
    let legend_width = if chart.legend { 96 } else { 8 };
    let left = rect
        .x
        .checked_add(Fixed::from_pixels(40))
        .ok_or(RenderError::CoordinateOverflow)?;
    let top = rect
        .y
        .checked_add(Fixed::from_pixels(title_height))
        .ok_or(RenderError::CoordinateOverflow)?;
    let right = rect
        .x
        .checked_add(rect.width)
        .and_then(|value| value.checked_sub(Fixed::from_pixels(legend_width)))
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .and_then(|value| value.checked_sub(Fixed::from_pixels(30)))
        .ok_or(RenderError::CoordinateOverflow)?;
    if right <= left || bottom <= top {
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
    let palette = metadata.map_or(&[][..], |metadata| metadata.chart_palette.as_slice());
    let cartesian = matches!(
        chart.kind,
        ChartKind::Bar | ChartKind::Line | ChartKind::Scatter | ChartKind::Area | ChartKind::Bubble
    );
    push_placeholder_frame(nodes, rect, Rgb::WHITE, options)?;
    let mut labels = Vec::<ChartLabel>::new();
    let horizontal_bar = chart.kind == ChartKind::Bar
        && metadata
            .is_some_and(|metadata| metadata.chart_bar_direction == ChartBarDirection::Horizontal);
    let cartesian_axis = cartesian.then(|| {
        chart_nice_value_axis(
            &series,
            matches!(chart.kind, ChartKind::Bar | ChartKind::Area),
        )
    });
    if let Some(axis) = cartesian_axis.as_ref() {
        push_cartesian_chart_axes(
            nodes,
            plot,
            chart.kind,
            horizontal_bar,
            axis,
            &series,
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
            let bounds = chart_value_bounds(&series, true);
            push_radar_chart(
                nodes,
                plot,
                &series,
                bounds,
                palette,
                chart.data_labels,
                &mut labels,
                typography_stats,
                options,
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
                    plot,
                    &series,
                    bounds,
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
                            plot,
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
                    plot,
                    &series,
                    bounds,
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

    if let Some(title) = chart.title.as_ref() {
        let bounds = Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: Fixed::from_pixels(22),
        };
        push_chart_text(
            nodes,
            title.clone(),
            bounds,
            TextAnchor::Middle,
            0,
            Fixed::from_pixels(13),
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    if let Some(title) = chart.x_axis_title.as_ref() {
        push_chart_text(
            nodes,
            title.clone(),
            Rect {
                x: left,
                y: bottom
                    .checked_add(Fixed::from_pixels(14))
                    .ok_or(RenderError::CoordinateOverflow)?,
                width: plot.width,
                height: Fixed::from_pixels(16),
            },
            TextAnchor::Middle,
            0,
            Fixed::from_pixels(10),
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    if let Some(title) = chart.y_axis_title.as_ref() {
        push_chart_text(
            nodes,
            title.clone(),
            Rect {
                x: rect.x,
                y: top,
                width: Fixed::from_pixels(18),
                height: plot.height,
            },
            TextAnchor::Middle,
            -90,
            Fixed::from_pixels(10),
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    if chart.legend {
        let entries = if matches!(chart.kind, ChartKind::Pie | ChartKind::Doughnut) {
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
        };
        for (row, (index, name)) in entries.into_iter().enumerate() {
            let y = top
                .checked_add(Fixed::from_pixels(row as i64 * 16))
                .ok_or(RenderError::CoordinateOverflow)?;
            push_solid_rect(
                nodes,
                right
                    .checked_add(Fixed::from_pixels(8))
                    .ok_or(RenderError::CoordinateOverflow)?,
                y.checked_add(Fixed::from_pixels(3))
                    .ok_or(RenderError::CoordinateOverflow)?,
                right
                    .checked_add(Fixed::from_pixels(18))
                    .ok_or(RenderError::CoordinateOverflow)?,
                y.checked_add(Fixed::from_pixels(13))
                    .ok_or(RenderError::CoordinateOverflow)?,
                chart_color(index, palette),
                options,
            )?;
            push_chart_text(
                nodes,
                name,
                Rect {
                    x: right
                        .checked_add(Fixed::from_pixels(20))
                        .ok_or(RenderError::CoordinateOverflow)?,
                    y,
                    width: Fixed::from_pixels(72),
                    height: Fixed::from_pixels(16),
                },
                TextAnchor::Start,
                0,
                Fixed::from_pixels(9),
                text_bytes,
                glyphs,
                typography_stats,
                options,
            )?;
        }
    }
    for label in labels {
        push_chart_text(
            nodes,
            label.text,
            Rect {
                x: Fixed::from_raw(label.x.raw() - Fixed::from_pixels(24).raw()),
                y: Fixed::from_raw(label.y.raw() - Fixed::from_pixels(7).raw()),
                width: Fixed::from_pixels(48),
                height: Fixed::from_pixels(14),
            },
            TextAnchor::Middle,
            0,
            Fixed::from_pixels(8),
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

fn nice_chart_step(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return 1.0;
    }
    let exponent = value.log10().floor();
    let magnitude = 10_f64.powf(exponent);
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
    factor * magnitude
}

fn chart_nice_value_axis(series: &[ResolvedChartSeries], force_zero: bool) -> NiceChartAxis {
    let mut raw_minimum = f64::INFINITY;
    let mut raw_maximum = f64::NEG_INFINITY;
    for value in series.iter().flat_map(|series| series.values.iter()) {
        raw_minimum = raw_minimum.min(*value);
        raw_maximum = raw_maximum.max(*value);
    }
    if raw_maximum <= raw_minimum {
        let padding = raw_maximum.abs().max(1.0) * 0.5;
        raw_minimum -= padding;
        raw_maximum += padding;
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
    let span = (data_maximum - data_minimum).max(f64::EPSILON);
    let major = nice_chart_step(span / CHART_AXIS_TARGET_INTERVALS);
    let padding = span * 0.05;
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
    let mut minimum = (padded_minimum / major).floor() * major;
    let mut maximum = (padded_maximum / major).ceil() * major;
    if include_zero {
        minimum = minimum.min(0.0);
        maximum = maximum.max(0.0);
    }
    if maximum <= minimum {
        maximum = minimum + major;
    }
    let intervals =
        (((maximum - minimum) / major).round() as usize).clamp(1, MAX_CHART_AXIS_INTERVALS);
    maximum = minimum + major * intervals as f64;
    let ticks = (0..=intervals)
        .map(|index| {
            let value = minimum + major * index as f64;
            if value.abs() < major.abs() * 1e-10 {
                0.0
            } else {
                value
            }
        })
        .collect();
    NiceChartAxis {
        minimum,
        maximum,
        major,
        ticks,
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

fn chart_value_bounds(series: &[ResolvedChartSeries], include_zero: bool) -> (f64, f64) {
    let mut minimum = if include_zero { 0.0 } else { f64::INFINITY };
    let mut maximum = if include_zero { 0.0 } else { f64::NEG_INFINITY };
    for value in series.iter().flat_map(|series| series.values.iter()) {
        minimum = minimum.min(*value);
        maximum = maximum.max(*value);
    }
    if maximum <= minimum {
        (minimum - 0.5, maximum + 0.5)
    } else {
        (minimum, maximum)
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

#[allow(clippy::too_many_arguments)]
fn push_cartesian_chart_axes(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    chart_kind: ChartKind,
    horizontal_bar: bool,
    axis: &NiceChartAxis,
    series: &[ResolvedChartSeries],
    text_bytes: &mut u64,
    glyphs: &mut u64,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
    warnings: &mut Warnings,
    warning_cell: CellCoordinate,
) -> Result<(), RenderError> {
    let plot_right = plot
        .x
        .checked_add(plot.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let plot_bottom = plot
        .y
        .checked_add(plot.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let grid = Rgb::new(217, 217, 217);
    for value in &axis.ticks {
        if horizontal_bar {
            let x = chart_x(
                plot,
                (*value - axis.minimum) / (axis.maximum - axis.minimum),
            )?;
            push_placeholder_line(nodes, x, plot.y, x, plot_bottom, grid, options)?;
            push_chart_text(
                nodes,
                chart_axis_number(*value, axis.major),
                Rect {
                    x: Fixed::from_raw(x.raw() - Fixed::from_pixels(24).raw()),
                    y: plot_bottom,
                    width: Fixed::from_pixels(48),
                    height: Fixed::from_pixels(14),
                },
                TextAnchor::Middle,
                0,
                Fixed::from_pixels(9),
                text_bytes,
                glyphs,
                typography_stats,
                options,
            )?;
        } else {
            let y = chart_y(plot, *value, (axis.minimum, axis.maximum))?;
            push_placeholder_line(nodes, plot.x, y, plot_right, y, grid, options)?;
            push_chart_text(
                nodes,
                chart_axis_number(*value, axis.major),
                Rect {
                    x: Fixed::from_raw(plot.x.raw() - Fixed::from_pixels(38).raw()),
                    y: Fixed::from_raw(y.raw() - Fixed::from_pixels(7).raw()),
                    width: Fixed::from_pixels(34),
                    height: Fixed::from_pixels(14),
                },
                TextAnchor::End,
                0,
                Fixed::from_pixels(9),
                text_bytes,
                glyphs,
                typography_stats,
                options,
            )?;
        }
    }
    push_placeholder_line(
        nodes,
        plot.x,
        plot.y,
        plot.x,
        plot_bottom,
        Rgb::BLACK,
        options,
    )?;
    push_placeholder_line(
        nodes,
        plot.x,
        plot_bottom,
        plot_right,
        plot_bottom,
        Rgb::BLACK,
        options,
    )?;

    if matches!(chart_kind, ChartKind::Scatter | ChartKind::Bubble) {
        return Ok(());
    }
    let Some(categories) = series.first().map(|series| series.labels.as_slice()) else {
        return Ok(());
    };
    if categories.is_empty() {
        return Ok(());
    }
    let stride = categories.len().div_ceil(MAX_CHART_CATEGORY_LABELS).max(1);
    let retained = categories
        .iter()
        .enumerate()
        .filter(|(index, _)| index % stride == 0 || index + 1 == categories.len())
        .count();
    warnings.add_count(
        WarningCode::ChartMetadataSimplified,
        categories.len().saturating_sub(retained) as u64,
        Some(warning_cell),
    );
    for (index, category) in categories.iter().enumerate() {
        if index % stride != 0 && index + 1 != categories.len() {
            continue;
        }
        let (bounds, anchor) = if horizontal_bar {
            let ratio = (index as f64 + 0.5) / categories.len() as f64;
            let y = interpolate_fixed(plot.y, plot.height, ratio)?;
            (
                Rect {
                    x: Fixed::from_raw(plot.x.raw() - Fixed::from_pixels(38).raw()),
                    y: Fixed::from_raw(y.raw() - Fixed::from_pixels(7).raw()),
                    width: Fixed::from_pixels(34),
                    height: Fixed::from_pixels(14),
                },
                TextAnchor::End,
            )
        } else {
            let ratio = if chart_kind == ChartKind::Bar {
                (index as f64 + 0.5) / categories.len() as f64
            } else if categories.len() == 1 {
                0.5
            } else {
                index as f64 / (categories.len() - 1) as f64
            };
            let x = chart_x(plot, ratio)?;
            (
                Rect {
                    x: Fixed::from_raw(x.raw() - Fixed::from_pixels(24).raw()),
                    y: plot_bottom,
                    width: Fixed::from_pixels(48),
                    height: Fixed::from_pixels(14),
                },
                TextAnchor::Middle,
            )
        };
        push_chart_text(
            nodes,
            category.clone(),
            bounds,
            anchor,
            0,
            Fixed::from_pixels(9),
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
            let ratio = if series.values.len() == 1 {
                0.5
            } else {
                index as f64 / (series.values.len() - 1) as f64
            };
            let x = chart_x(plot, ratio)?;
            let y = chart_y(plot, *value, bounds)?;
            if series.style.line_visible {
                if let Some((previous_x, previous_y)) = previous {
                    push_placeholder_line(
                        nodes, previous_x, previous_y, x, y, line_color, options,
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
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let mut x_min = f64::INFINITY;
    let mut x_max = f64::NEG_INFINITY;
    for value in series
        .iter()
        .filter_map(|series| series.x_values.as_ref())
        .flatten()
    {
        x_min = x_min.min(*value);
        x_max = x_max.max(*value);
    }
    if x_max <= x_min {
        x_min -= 0.5;
        x_max += 0.5;
    }
    for (series_index, series) in series.iter().enumerate() {
        let x_values = series.x_values.as_ref().expect("scatter x values");
        for (x_value, y_value) in x_values.iter().zip(&series.values) {
            let x = chart_x(plot, (*x_value - x_min) / (x_max - x_min))?;
            let y = chart_y(plot, *y_value, y_bounds)?;
            let palette_color = chart_color(series_index, palette);
            let color = series.style.line_color.map_or(palette_color, |color| {
                let [red, green, blue] = color.as_rgb();
                Rgb::new(red, green, blue)
            });
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
            stroke_width: Fixed::from_pixels(1),
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
        let (first_ratio, last_ratio) = if series.values.len() == 1 {
            (0.5, 0.5)
        } else {
            (0.0, 1.0)
        };
        let first_x = chart_x(plot, first_ratio)?;
        let last_x = chart_x(plot, last_ratio)?;
        let mut commands = Vec::with_capacity(series.values.len().saturating_add(3));
        commands.push(PathCommand::MoveTo {
            x: first_x,
            y: baseline,
        });
        for (index, value) in series.values.iter().enumerate() {
            let ratio = if series.values.len() == 1 {
                0.5
            } else {
                index as f64 / (series.values.len() - 1) as f64
            };
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
        push_chart_path(
            nodes,
            commands,
            Some(light_chart_color(color)),
            Some(color),
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
            let end = start + std::f64::consts::TAU * *value / total;
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
    bounds: (f64, f64),
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
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
    for ring in 1..=4 {
        let mut commands = Vec::with_capacity(categories.saturating_add(2));
        for index in 0..categories {
            let (x, y) = polar(index, f64::from(ring) / 4.0)?;
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
    for (series_index, series) in series.iter().enumerate() {
        let mut commands = Vec::with_capacity(series.values.len().saturating_add(2));
        for (index, value) in series.values.iter().enumerate() {
            let scale = ((*value - bounds.0) / (bounds.1 - bounds.0)).clamp(0.0, 1.0);
            let (x, y) = polar(index, scale)?;
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
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let x_values = series
        .iter()
        .filter_map(|series| series.x_values.as_ref())
        .flatten();
    let mut x_min = f64::INFINITY;
    let mut x_max = f64::NEG_INFINITY;
    for value in x_values {
        x_min = x_min.min(*value);
        x_max = x_max.max(*value);
    }
    if x_max <= x_min {
        x_min -= 0.5;
        x_max += 0.5;
    }
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
            let x = chart_x(plot, (*x_value - x_min) / (x_max - x_min))?;
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
        let end = start + std::f64::consts::TAU * *value / total;
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
fn push_chart_text(
    nodes: &mut Vec<SceneNode>,
    text: String,
    bounds: Rect,
    anchor: TextAnchor,
    rotation_degrees: i16,
    size: Fixed,
    text_bytes: &mut u64,
    glyphs: &mut u64,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
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
    let node = build_auxiliary_text_node(
        text,
        bounds,
        Fixed::from_pixels(2),
        TextStyle {
            family: options.default_font_family.clone(),
            size,
            color: Rgb::BLACK,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            anchor,
            baseline: TextBaseline::Middle,
            rotation_degrees,
        },
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

fn pixels_as_fixed(value: f64) -> Result<Fixed, RenderError> {
    float_pixels_to_fixed(value).ok_or(RenderError::CoordinateOverflow)
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

fn row_or_column_boundary_row(slots: &[AxisSlot<u32>], index: u32, total: Fixed) -> Fixed {
    slots
        .iter()
        .find(|slot| slot.index >= index)
        .map_or(total, |slot| slot.offset)
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
    if !ratio.is_finite() {
        return Err(RenderError::CoordinateOverflow);
    }
    let raw = start.raw() as f64 + extent.raw() as f64 * ratio;
    if !raw.is_finite() || raw < i64::MIN as f64 || raw > i64::MAX as f64 {
        return Err(RenderError::CoordinateOverflow);
    }
    Ok(Fixed::from_raw(raw.round() as i64))
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

fn push_placeholder_line(
    nodes: &mut Vec<SceneNode>,
    x1: Fixed,
    y1: Fixed,
    x2: Fixed,
    y2: Fixed,
    color: Rgb,
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
            width: Fixed::from_pixels(1),
        }),
        options,
    )
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
        Some(VAlign::Bottom) => TextBaseline::Bottom,
        Some(VAlign::Middle) | None => TextBaseline::Middle,
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
    shaped: ShapedText,
    width: Fixed,
    metrics: CombinedLineMetrics,
}

struct PreparedText {
    styles: Vec<ResolvedRunStyle>,
    lines: Vec<PreparedLine>,
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
) -> Result<Fixed, RenderError> {
    stats.text_bytes = stats
        .text_bytes
        .checked_add(region.text.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::TextBytes,
        options.limits.max_text_bytes,
        stats.text_bytes,
    )?;
    let style = text_style(region, options, sheet_right_to_left);
    let prepared = prepare_styled_text(pack, region, &style, sheet_right_to_left, options, stats)?;
    sum_fixed(
        prepared
            .lines
            .iter()
            .map(|line| line_height_from_metrics(line.metrics))
            .collect::<Result<Vec<_>, _>>()?,
    )?
    .checked_add(Fixed::from_pixels(AUTO_ROW_VERTICAL_PADDING_PIXELS))
    .ok_or(RenderError::CoordinateOverflow)
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

fn line_height_from_metrics(metrics: CombinedLineMetrics) -> Result<Fixed, RenderError> {
    metrics
        .ascent
        .checked_sub(metrics.descent)
        .and_then(|height| height.checked_add(metrics.line_gap))
        .ok_or(RenderError::CoordinateOverflow)
        .map(|height| height.max(Fixed::from_raw(1)))
}

#[allow(clippy::too_many_arguments)]
fn build_glyph_run(
    pack: &FontPack,
    region: &Region,
    clip_bounds: Rect,
    style: &TextStyle,
    sheet_right_to_left: bool,
    options: &RenderOptions,
    stats: &mut TypographyStats,
    warnings: &mut Warnings,
) -> Result<GlyphRunNode, RenderError> {
    let alignment = region.style.as_ref().and_then(|style| style.align.as_ref());
    let mut prepared =
        prepare_styled_text(pack, region, style, sheet_right_to_left, options, stats)?;
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
                scale_numerator,
                scale_denominator,
            )?;
        }
    }

    let line_heights = prepared
        .lines
        .iter()
        .map(|line| line_height_from_metrics(line.metrics))
        .collect::<Result<Vec<_>, _>>()?;
    let block_height = sum_fixed(line_heights.iter().copied())?;
    let top = vertical_block_top(region.rect, block_height, style.baseline)?;

    let mut commands = Vec::new();
    let mut clusters = Vec::new();
    let mut paints = Vec::new();
    let mut decorations = Vec::new();
    let mut line_top = top;
    for (line, line_height) in prepared.lines.iter().zip(line_heights) {
        let baseline = line_top
            .checked_add(line.metrics.ascent)
            .ok_or(RenderError::CoordinateOverflow)?;
        let line_x = horizontal_line_start(
            region.rect,
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
            &mut paints,
            &mut decorations,
        )?;
        line_top = line_top
            .checked_add(line_height)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    let (pivot_x, pivot_y) = rotation_pivot(region.rect, prepared.horizontal_padding, style)?;
    let node = GlyphRunNode {
        text: region.text.clone(),
        clip_bounds,
        commands,
        clusters,
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
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<PreparedText, RenderError> {
    let (styles, spans) = resolve_rich_styles(region, base)?;
    let direction = text_base_direction(&region.text, sheet_right_to_left);
    let primary_size = styled_font_size(&styles[0], 1, 1)?;
    let horizontal_padding =
        outlined_horizontal_padding(pack, styles[0].request(), primary_size, region, options)?;
    let available_width = inner_width(region.rect.width, horizontal_padding)?;
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
    let line_ranges = wrap_text_ranges(
        &region.text,
        wrap,
        available_width,
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
                options,
            )?;
            styled_shaped_width(pack, &shaped, &styles, 1, 1)
        },
    )?;

    let mut lines = Vec::with_capacity(line_ranges.len());
    let mut max_width = Fixed::ZERO;
    let mut missing_glyphs = 0_u64;
    let mut family_substituted = false;
    for source in line_ranges {
        let shaped = shape_styled_range(
            pack,
            &region.text,
            source.clone(),
            &spans,
            &styles,
            direction,
            options,
        )?;
        account_shaping(pack, &shaped, options, stats)?;
        let width = styled_shaped_width(pack, &shaped, &styles, 1, 1)?;
        let metrics = styled_line_metrics(pack, &shaped, &styles, 1, 1)?;
        max_width = max_width.max(width);
        missing_glyphs = missing_glyphs.saturating_add(shaped.missing_glyphs as u64);
        family_substituted |= !shaped.requested_family_matched;
        lines.push(PreparedLine {
            source,
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
    let glyph_limit = usize::try_from(options.limits.max_glyphs).unwrap_or(usize::MAX);
    let run_limit = usize::try_from(options.limits.max_text_runs).unwrap_or(usize::MAX);
    pack.shape(
        text,
        request,
        ShapeOptions {
            direction,
            max_glyphs: glyph_limit,
            max_runs: run_limit,
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
}

fn styled_line_metrics(
    pack: &FontPack,
    shaped: &ShapedText,
    styles: &[ResolvedRunStyle],
    scale_numerator: i64,
    scale_denominator: i64,
) -> Result<CombinedLineMetrics, RenderError> {
    let mut combined = CombinedLineMetrics {
        ascent: Fixed::ZERO,
        descent: Fixed::ZERO,
        line_gap: Fixed::ZERO,
    };
    let mut initialized = false;
    let mut include =
        |font_id: crate::font::FontId, style: &ResolvedRunStyle| -> Result<(), RenderError> {
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
            if !initialized {
                combined.ascent = ascent;
                combined.descent = descent;
                combined.line_gap = line_gap;
                initialized = true;
            } else {
                combined.ascent = combined.ascent.max(ascent);
                combined.descent = combined.descent.min(descent);
                combined.line_gap = combined.line_gap.max(line_gap);
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
    paints: &mut Vec<GlyphPaint>,
    decorations: &mut Vec<LineNode>,
) -> Result<(), RenderError> {
    let mut visual_cursor = line_x;
    for run in &shaped.runs {
        let style = styles.get(run.style_index).ok_or(RenderError::Typography {
            reason: "invalid_rich_style_index",
        })?;
        let metrics = pack.metrics(run.font_id).map_err(map_font_error)?;
        let font_size = styled_font_size(style, scale_numerator, scale_denominator)?;
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
        let run_command_start = output.len() as u64;
        let mut glyph_index = 0_usize;
        while glyph_index < run.glyphs.len() {
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
            let cluster_end = run
                .glyphs
                .iter()
                .filter_map(|glyph| usize::try_from(glyph.cluster).ok())
                .filter(|candidate| *candidate > cluster_start)
                .min()
                .unwrap_or(run.source.end);
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
            if let Some(previous) = clusters.last_mut() {
                if previous.source_start == source_start as u64
                    && previous.source_end == source_end as u64
                    && previous.command_end == command_start
                {
                    previous.command_end = command_end;
                } else {
                    clusters.push(GlyphCluster {
                        source_start: source_start as u64,
                        source_end: source_end as u64,
                        command_start,
                        command_end,
                    });
                }
            } else {
                clusters.push(GlyphCluster {
                    source_start: source_start as u64,
                    source_end: source_end as u64,
                    command_start,
                    command_end,
                });
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

fn cell_defaults_to_right_alignment(cell: &Cell) -> bool {
    match cell {
        Cell::Number(_) | Cell::Date(_) => true,
        Cell::Formula { cached, .. } => cell_defaults_to_right_alignment(cached),
        Cell::Text(_) | Cell::Bool(_) | Cell::Error(_) => false,
    }
}

fn cell_allows_horizontal_overflow(cell: &Cell) -> bool {
    match cell {
        Cell::Text(_) => true,
        Cell::Formula { cached, .. } => cell_allows_horizontal_overflow(cached),
        Cell::Number(_) | Cell::Date(_) | Cell::Bool(_) | Cell::Error(_) => false,
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

fn push_borders(
    nodes: &mut Vec<SceneNode>,
    rect: Rect,
    border: &Border,
    coordinate: CellCoordinate,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<(), RenderError> {
    let right = rect
        .x
        .checked_add(rect.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let edges = [
        (
            border.left,
            border.left_color.or(border.color),
            rect.x,
            rect.y,
            rect.x,
            bottom,
        ),
        (
            border.right,
            border.right_color.or(border.color),
            right,
            rect.y,
            right,
            bottom,
        ),
        (
            border.top,
            border.top_color.or(border.color),
            rect.x,
            rect.y,
            right,
            rect.y,
        ),
        (
            border.bottom,
            border.bottom_color.or(border.color),
            rect.x,
            bottom,
            right,
            bottom,
        ),
    ];
    for (style, color, x1, y1, x2, y2) in edges {
        let Some(width) = border_width(style) else {
            continue;
        };
        if style == BorderStyle::Double {
            warnings.add(WarningCode::DoubleBorderSimplified, Some(coordinate));
        }
        push_node(
            nodes,
            SceneNode::Line(LineNode {
                x1,
                y1,
                x2,
                y2,
                color: color.map(rgb).unwrap_or(Rgb::BLACK),
                width,
            }),
            options,
        )?;
    }
    Ok(())
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
        build_print_document, render_print_document_pdf, render_print_document_png_pages,
        render_sheet_svg, PrintOptions,
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
                br#"<worksheet><sheetData/><drawing r:id="rIdDrawing"/></worksheet>"#.as_slice(),
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
            // Poppler wraps strong RTL text in directional controls and emits
            // its visual glyph order; displaying this substring reads `אב`.
            assert!(extracted.contains("\u{202b} בא\u{202c}"), "{extracted:?}");
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
        let mut paints = Vec::new();
        let mut decorations = Vec::new();
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
            &mut paints,
            &mut decorations,
        )
        .unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].source_start, 0);
        assert_eq!(clusters[0].source_end, 2);
        assert_eq!(clusters[0].command_start, 0);
        assert_eq!(clusters[0].command_end, commands.len() as u64);

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
    fn ooxml_absent_explicit_and_base_default_widths_match_calc_import_geometry() {
        let imported = |sheet_format: &str| {
            imported_xlsx(
                "<styleSheet/>",
                &format!(
                    r#"<worksheet>{sheet_format}<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#
                ),
            )
        };
        let absent = imported("");
        let explicit_8_5 = imported(r#"<sheetFormatPr defaultColWidth="8.5"/>"#);
        let explicit_8 = imported(r#"<sheetFormatPr defaultColWidth="8"/>"#);
        let base_8 = imported(r#"<sheetFormatPr baseColWidth="8"/>"#);
        let range = RenderRange::new(0, 0, 0, 0);

        assert_eq!(absent.sheets[0].default_column_width(), None);
        assert_eq!(absent.sheets[0].implicit_ooxml_column_width(), Some(None));
        assert_eq!(explicit_8_5.sheets[0].default_column_width(), Some(8.5));
        assert_eq!(explicit_8_5.sheets[0].implicit_ooxml_column_width(), None);
        assert_eq!(base_8.sheets[0].default_column_width(), None);
        assert_eq!(
            base_8.sheets[0].implicit_ooxml_column_width(),
            Some(Some(8.0))
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
        assert_eq!(absent_width, explicit_8_5_width);
        assert_eq!(absent_width, Fixed::from_pixels(70));
        assert_eq!(base_8_width, Fixed::from_pixels(71));
        assert_eq!(
            base_8_width.checked_sub(explicit_8_width),
            Some(Fixed::from_pixels(5))
        );
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
            .find_map(|node| match node {
                SceneNode::Rect(node) if node.stroke.is_some() => Some(node),
                _ => None,
            })
            .expect("gridline rectangle");
        assert_eq!(grid.stroke, Some(Rgb::new(217, 217, 217)));
        assert_eq!(grid.stroke_width, Fixed::from_pixels(1));
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
            .any(|page| page.scene.nodes.iter().any(|node| matches!(
                node,
                SceneNode::Rect(RectNode {
                    fill: Some(Rgb {
                        red: 242,
                        green: 242,
                        blue: 242,
                    }),
                    ..
                })
            ))));
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
                r#"<theme><themeElements><clrScheme><accent1><srgbClr val="123456"/></accent1></clrScheme></themeElements></theme>"#,
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
                r#"<Relationships><Relationship Id="rIdChart" Target="../charts/chart1.xml"/></Relationships>"#,
            ),
            (
                "xl/charts/chart1.xml",
                r#"<chartSpace><chart><plotArea><barChart><barDir val="bar"/><ser><tx><strRef><f>Data!$C$1</f><strCache><pt idx="0"><v>Cached revenue</v></pt></strCache></strRef></tx><cat><strRef><f>Data!$A$1:$A$3</f><strCache><pt idx="0"><v>A</v></pt><pt idx="1"><v>B</v></pt><pt idx="2"><v>C</v></pt></strCache></strRef></cat><val><numRef><f>Data!$B$1:$B$3</f><numCache><pt idx="0"><v>2</v></pt><pt idx="1"><v>5</v></pt><pt idx="2"><v>3</v></pt></numCache></numRef></val></ser></barChart></plotArea><legend/></chart></chartSpace>"#,
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
        assert!(!build
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == WarningCode::ChartPlaceholder));
        assert!(build.scene.nodes.iter().any(|node| match node {
            SceneNode::Text(text) => text.text == "Cached revenue",
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
                r#"<Relationships><Relationship Id="rIdChart" Target="../charts/chart1.xml"/></Relationships>"#,
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
        assert_eq!(metadata.chart_unsupported_reasons.len(), 4);
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
        assert_eq!(document.pages[0].scene.width, Fixed::from_pixels(320));
        assert_eq!(document.pages[0].scene.height, Fixed::from_pixels(220));
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
}
