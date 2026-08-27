//! Drawing, chart, and imported axis metadata.

use super::super::Color;

/// Kind of object described by [`DrawingMetadata`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DrawingObjectKind {
    /// An entry in [`crate::Sheet::images`].
    #[default]
    Image,
    /// An entry in [`crate::Sheet::charts`].
    Chart,
    /// A source drawing shape that is not represented by the Image/Chart APIs.
    Shape,
}

/// How a drawing responds when its anchor cells are moved or resized.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DrawingAnchorBehavior {
    /// Move and resize with the anchor cells.
    #[default]
    MoveAndSize,
    /// Move with cells while retaining an absolute size.
    MoveOnly,
    /// Use absolute page/sheet coordinates.
    Absolute,
}

/// Source drawing crop, normalized to parts per million of each edge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct DrawingCrop {
    /// Crop from the left edge.
    pub left_ppm: u32,
    /// Crop from the top edge.
    pub top_ppm: u32,
    /// Crop from the right edge.
    pub right_ppm: u32,
    /// Crop from the bottom edge.
    pub bottom_ppm: u32,
}

/// One indexed value retained from an OOXML chart cache.
///
/// Values remain strings so malformed or non-finite numeric cache entries can
/// be rejected by a renderer without weakening [`DrawingMetadata`]'s exact
/// equality semantics. Readers bound both the number and byte length of cached
/// points before storing them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChartCachedPoint {
    /// Zero-based point index declared by the chart part.
    pub index: u32,
    /// Cached display or numeric value.
    pub value: String,
}

/// Bounded cached inputs for one chart series.
///
/// The entry at index `n` corresponds to `Chart::series[n]`. A renderer may use
/// a complete, well-formed cache when a referenced worksheet range is missing
/// or belongs to another sheet; incomplete caches must not be silently padded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChartSeriesCache {
    /// Cached series-name points, normally containing index zero only.
    pub name: Vec<ChartCachedPoint>,
    /// Cached category labels or X values.
    pub categories: Vec<ChartCachedPoint>,
    /// Cached Y values.
    pub values: Vec<ChartCachedPoint>,
    /// Cached bubble-size values.
    pub bubble_sizes: Vec<ChartCachedPoint>,
}

/// Marker shape retained for one imported chart series.
///
/// [`Self::Automatic`] preserves the source application's default. Renderers
/// may use a deterministic fallback when no explicit marker was authored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChartMarkerSymbol {
    /// No explicit marker shape was retained.
    #[default]
    Automatic,
    /// Do not paint markers.
    None,
    /// Circular markers.
    Circle,
    /// Square markers.
    Square,
    /// Diamond markers.
    Diamond,
    /// Triangular markers.
    Triangle,
}

/// Typed reason that an imported chart-series style needs a rendering fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChartSeriesStyleLossKind {
    /// The source marker symbol is outside the retained subset.
    UnsupportedMarkerSymbol,
    /// The source marker size is absent from OOXML's bounded 2--72 point range.
    InvalidMarkerSize,
    /// The source line uses a paint or dash mode outside the retained subset.
    UnsupportedLinePaint,
    /// The source line width is not an OOXML `ST_LineWidth` value.
    InvalidLineWidth,
}

/// Bounded visual metadata retained for one imported chart series.
///
/// Entries are aligned with [`DrawingMetadata::chart_series_caches`] and the
/// matching chart's `series` collection. A missing entry means application
/// defaults: visible palette-colored lines and an automatic marker.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChartSeriesStyle {
    /// Marker shape.
    pub marker: ChartMarkerSymbol,
    /// Explicit marker diameter in points, constrained to 2--72.
    pub marker_size: Option<u8>,
    /// Whether the series line is visible.
    pub line_visible: bool,
    /// Explicit RGB line color, or `None` for the chart palette default.
    pub line_color: Option<Color>,
    /// Retained source-effective line width in EMUs (`1 pt = 12,700 EMUs`).
    ///
    /// OOXML constrains `a:ln/@w` to `0..=20,116,800`; chart imports retain
    /// LibreOffice's 12,700-EMU chart-line default when `a:ln` omits `w`.
    /// An explicit zero remains a visible device-hairline request.
    pub line_width_emu: Option<u32>,
    /// Deduplicated typed fallback boundaries observed while importing.
    pub losses: Vec<ChartSeriesStyleLossKind>,
}

impl Default for ChartSeriesStyle {
    fn default() -> Self {
        Self {
            marker: ChartMarkerSymbol::Automatic,
            marker_size: None,
            line_visible: true,
            line_color: None,
            line_width_emu: None,
            losses: Vec::new(),
        }
    }
}

/// Fill paint retained from an imported chart-space frame.
///
/// [`Self::Automatic`] preserves the renderer's authored-chart compatibility
/// default. Imported OOXML charts use [`Self::NoFill`] or [`Self::Solid`] when
/// `c:chartSpace/c:spPr` explicitly supplies a supported fill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChartFrameFill {
    /// No supported explicit chart-space fill was retained.
    #[default]
    Automatic,
    /// The chart-space frame explicitly requests `a:noFill`.
    NoFill,
    /// The chart-space frame explicitly requests a solid RGB-resolvable fill.
    Solid(Color),
}

/// Typed reason that an imported chart-frame style needs a rendering fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChartFrameStyleLossKind {
    /// The chart-space frame uses a paint outside the retained subset.
    UnsupportedPaint,
}

/// Uniform effective text style retained for one semantic imported-chart role.
///
/// OOXML rich text can vary formatting between individual runs. Readers retain
/// this compact style only when every painted run for the role resolves to the
/// same supported properties. Mixed or otherwise unrepresentable formatting is
/// reported through [`DrawingMetadata::chart_unsupported_reasons`] instead of
/// being flattened silently.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ChartTextStyle {
    /// Effective Latin font family, bounded to 255 UTF-8 bytes by readers.
    pub latin_font_family: String,
    /// Effective font size in hundredths of a point.
    ///
    /// Imported DrawingML values are constrained to the `ST_TextFontSize`
    /// range `100..=400_000`.
    pub size_hundredths_of_point: u32,
    /// Effective text color after supported theme-color transformations.
    pub color: Color,
    /// Whether the text is bold.
    pub bold: bool,
    /// Whether the text is italic.
    pub italic: bool,
    /// Whether the text has a single underline.
    pub underline: bool,
    /// Whether the text has a single strikethrough.
    pub strikethrough: bool,
    /// Minimum size in hundredths of a point at which pair kerning is enabled.
    ///
    /// `None` retains the shaping engine's default. DrawingML `kern` values are
    /// constrained to the same bounded point-size domain as other text metrics.
    pub kerning_minimum_hundredths_of_point: Option<u32>,
    /// Explicit integer text-body rotation in degrees, when present.
    ///
    /// `None` keeps the renderer's semantic-role orientation (for example, a
    /// vertical value-axis title). Fractional and unsupported vertical-text
    /// modes are surfaced as unsupported chart text instead.
    pub rotation_degrees: Option<i16>,
}

/// Fixed-cardinality text-style sidecar for semantic imported-chart roles.
///
/// Keeping category and value axes semantic avoids swapping their styling when
/// a horizontal bar chart places the value axis along the display X axis.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ChartTextStyles {
    /// Main chart-title style.
    pub chart_title: Option<ChartTextStyle>,
    /// Category-axis title style.
    pub category_axis_title: Option<ChartTextStyle>,
    /// Value-axis title style.
    pub value_axis_title: Option<ChartTextStyle>,
    /// Legend-entry style.
    pub legend: Option<ChartTextStyle>,
    /// Category-axis label style.
    pub category_axis_labels: Option<ChartTextStyle>,
    /// Value-axis label style.
    pub value_axis_labels: Option<ChartTextStyle>,
    /// Data-label style.
    pub data_labels: Option<ChartTextStyle>,
}

/// Unsupported source-chart construct retained for explicit placeholder rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChartUnsupportedReason {
    /// Multiple distinct plot kinds share one chart (a combo chart).
    Combo,
    /// A three-dimensional chart variant was requested.
    ThreeDimensional,
    /// The chart is backed by a pivot chart source.
    Pivot,
    /// Chart data refers to an external workbook or external chart-data part.
    ExternalData,
    /// The plot kind is not represented by [`crate::ChartKind`].
    UnsupportedKind,
    /// Painted runs or role-specific overrides resolve to different text styles.
    MixedTextStyle,
    /// A chart text property is invalid or outside the exactly rendered subset.
    UnsupportedTextStyle,
    /// Data-label policy cannot be represented as uniform value-only labels.
    UnsupportedDataLabels,
    /// An imported axis visibility value is malformed or contradictory.
    InvalidAxisVisibility,
    /// The imported chart uses an axis topology outside the retained primary pair.
    UnsupportedAxisTopology,
    /// Axis scaling, positioning, labels, ticks, or formatting cannot be rendered exactly.
    UnsupportedAxisPresentation,
    /// Plot grouping, ordering, spacing, smoothing, or kind-specific geometry is unsupported.
    UnsupportedPlotSemantics,
    /// Legend placement, entry filtering, or per-entry formatting is unsupported.
    UnsupportedLegend,
    /// A non-default built-in or external chart style is not represented exactly.
    UnsupportedChartStyle,
    /// Compatibility markup or a foreign namespace could not be selected safely.
    UnsupportedMarkup,
}

/// Orientation of an OOXML `barChart` retained beside [`crate::ChartKind::Bar`].
///
/// The public chart kind predates the distinction between vertical columns and
/// horizontal bars, so readers retain it in [`DrawingMetadata`] without
/// changing authored `Chart` literals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChartBarDirection {
    /// Values extend vertically from a horizontal baseline (`barDir="col"`).
    #[default]
    Column,
    /// Values extend horizontally from a vertical baseline (`barDir="bar"`).
    Horizontal,
}

/// Bounded rendering sidecar for a worksheet drawing object.
///
/// This intentionally leaves [`crate::Image`] and [`crate::Chart`] source-compatible while
/// retaining offsets and absolute geometry used by higher-fidelity renderers.
/// Strings are reader-bounded and object indexes always address the matching
/// image/chart slice for the declared [`DrawingObjectKind`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DrawingMetadata {
    /// Object kind.
    pub kind: DrawingObjectKind,
    /// Index into [`crate::Sheet::images`] or [`crate::Sheet::charts`].
    pub object_index: usize,
    /// Top-left cell marker retained for every placeable drawing kind.
    pub from_cell: Option<(u32, u16)>,
    /// Bottom-right cell marker for a two-cell anchor, when present.
    ///
    /// The marker is a boundary: a zero offset leaves the marker cell itself
    /// unoccupied.
    pub to_cell: Option<(u32, u16)>,
    /// Offset from the top-left anchor cell, in English Metric Units.
    pub from_offset_emu: Option<(i64, i64)>,
    /// Offset at the bottom-right anchor cell, in English Metric Units.
    pub to_offset_emu: Option<(i64, i64)>,
    /// Absolute width and height in English Metric Units.
    pub absolute_size_emu: Option<(u64, u64)>,
    /// Optional source crop.
    pub crop: Option<DrawingCrop>,
    /// Clockwise rotation in thousandths of a degree.
    pub rotation_mdeg: Option<i32>,
    /// Source stacking order.
    pub z_order: Option<i32>,
    /// Accessible alternative text, when present.
    pub alt_text: Option<String>,
    /// Source object name, when present.
    pub name: Option<String>,
    /// Move/resize behavior relative to cells.
    pub behavior: DrawingAnchorBehavior,
    /// Theme-derived chart series/slice palette in source order.
    ///
    /// Empty for non-chart drawings. Readers may use deterministic Office
    /// defaults for theme slots omitted by the source package.
    pub chart_palette: Vec<Color>,
    /// Effective Latin font family inherited by imported OOXML chart text.
    ///
    /// XLSX and XLSB readers retain the source theme's minor Latin typeface
    /// when it is non-empty and at most 255 UTF-8 bytes. When that source
    /// value or the package theme is absent, they retain Calc's
    /// `Liberation Sans` fallback instead. `None` preserves authored-chart,
    /// non-chart, and legacy-reader behavior.
    pub chart_default_latin_font_family: Option<String>,
    /// Uniform effective text styling retained by semantic imported-chart role.
    ///
    /// Empty for authored charts, non-chart drawings, and readers that do not
    /// expose OOXML chart text. Mixed or unsupported styles instead add a typed
    /// entry to [`Self::chart_unsupported_reasons`].
    pub chart_text_styles: ChartTextStyles,
    /// Whether the semantic category axis is visible for an imported chart.
    ///
    /// `None` preserves authored-chart, non-chart, and legacy-reader behavior;
    /// retained OOXML charts use `Some(false)` for a deleted axis.
    pub chart_category_axis_visible: Option<bool>,
    /// Whether imported category positions are shifted into category bands.
    ///
    /// OOXML `crossBetween="between"` and Calc's omitted default for line and
    /// column charts retain `Some(true)`. `midCat` retains `Some(false)`;
    /// `None` preserves authored-chart and legacy-reader endpoint behavior.
    pub chart_category_axis_shifted: Option<bool>,
    /// Whether the semantic value axis is visible for an imported chart.
    ///
    /// `None` preserves authored-chart, non-chart, and legacy-reader behavior;
    /// retained OOXML charts use `Some(false)` for a deleted axis.
    pub chart_value_axis_visible: Option<bool>,
    /// Bounded cached chart data, aligned with the matching chart's series.
    ///
    /// Empty for authored charts and readers that cannot retain chart caches.
    pub chart_series_caches: Vec<ChartSeriesCache>,
    /// Bounded per-series marker and line metadata in chart-series order.
    pub chart_series_styles: Vec<ChartSeriesStyle>,
    /// Chart-space frame fill retained from imported OOXML.
    pub chart_frame_fill: ChartFrameFill,
    /// Deduplicated typed fallback boundaries for the chart-space frame.
    pub chart_frame_style_losses: Vec<ChartFrameStyleLossKind>,
    /// Whether an imported category axis explicitly contains major gridlines.
    ///
    /// `None` preserves authored and legacy-reader renderer defaults.
    pub chart_category_major_gridlines: Option<bool>,
    /// Whether an imported value axis explicitly contains major gridlines.
    ///
    /// `None` preserves authored and legacy-reader renderer defaults.
    pub chart_value_major_gridlines: Option<bool>,
    /// Unsupported source constructs that require an explicit chart placeholder.
    pub chart_unsupported_reasons: Vec<ChartUnsupportedReason>,
    /// Column versus horizontal-bar orientation for [`crate::ChartKind::Bar`].
    pub chart_bar_direction: ChartBarDirection,
}

#[cfg_attr(not(feature = "xlsx"), allow(dead_code))]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) enum OoxmlImplicitColumnWidth {
    #[default]
    None,
    ApplicationDefault,
    BaseCharacters(f32),
}

/// XLSB sheet-wide column-width provenance retained for deterministic rendering.
///
/// This is an internal cross-crate contract for `rxls-render`. XLSB stores
/// widths in standard-digit units whose import conversion differs from XLSX's
/// character-width projection.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XlsbDefaultColumnWidth {
    /// The worksheet omitted an authoritative width, so 8.5 digits apply.
    ApplicationDefault,
    /// Raw `BrtWsFmtInfo.dxGCol` units, where 256 units equal one digit.
    Digits256(u32),
    /// `BrtWsFmtInfo.cchDefColWidth`, which also carries five screen pixels.
    BaseCharacters(u16),
}

/// Exact imported worksheet-axis geometry retained for deterministic rendering.
///
/// This is an internal cross-crate contract for `rxls-render`. It preserves the
/// source integer domain where the file format provides one, while the ordinary
/// public row-height and column-width APIs continue to expose compatibility
/// `f32` values.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportedAxisMeasure {
    /// Physical length in twentieths of a point.
    Twips(u32),
    /// Physical length in hundredths of a millimetre.
    MillimeterHundredths(u32),
    /// A positive physical length as a reduced numerator/denominator in points.
    PointRatio(u64, u64),
    /// BIFF character-width units, where 256 units equal one character.
    CharacterWidth256(u32),
    /// A positive OOXML character width as a reduced numerator/denominator.
    CharacterWidthRatio(u64, u64),
    /// OOXML base-character width, excluding its five device pixels.
    CharacterBaseWidth256(u32),
    /// XLSB standard-digit units, where 256 units equal one digit.
    DigitWidth256(u32),
    /// XLSB base-digit width, excluding its device-pixel allowance.
    DigitBaseWidth256(u32),
}

#[cfg_attr(not(any(feature = "xlsx", feature = "ods")), allow(dead_code))]
pub(crate) fn parse_decimal_ratio_u64(value: &str) -> Option<(u64, u64)> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('-') {
        return None;
    }
    let value = value.strip_prefix('+').unwrap_or(value);
    let exponent_at = value.find(['e', 'E']);
    let (mantissa, exponent) = match exponent_at {
        Some(index) => {
            let exponent = value.get(index + 1..)?;
            if exponent.is_empty() || exponent.contains(['e', 'E']) {
                return None;
            }
            (value.get(..index)?, exponent.parse::<i32>().ok()?)
        }
        None => (value, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if fraction.contains('.')
        || (whole.is_empty() && fraction.is_empty())
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }

    let digits = whole.bytes().chain(fraction.bytes());
    let total_digits = whole.len().checked_add(fraction.len())?;
    let leading_zeros = digits.clone().take_while(|byte| *byte == b'0').count();
    if leading_zeros == total_digits {
        return Some((0, 1));
    }
    let trailing_zeros = digits.rev().take_while(|byte| *byte == b'0').count();
    let significant_end = total_digits.checked_sub(trailing_zeros)?;
    let mut numerator = 0_u64;
    for byte in whole
        .bytes()
        .chain(fraction.bytes())
        .skip(leading_zeros)
        .take(significant_end.checked_sub(leading_zeros)?)
    {
        numerator = numerator
            .checked_mul(10)?
            .checked_add(u64::from(byte - b'0'))?;
    }

    let decimal_power = i64::from(exponent)
        .checked_sub(i64::try_from(fraction.len()).ok()?)?
        .checked_add(i64::try_from(trailing_zeros).ok()?)?;
    let mut denominator = 1_u64;
    if decimal_power >= 0 {
        numerator =
            numerator.checked_mul(10_u64.checked_pow(u32::try_from(decimal_power).ok()?)?)?;
    } else {
        denominator = 10_u64.checked_pow(u32::try_from(decimal_power.checked_neg()?).ok()?)?;
    }
    let divisor = gcd_u64(numerator, denominator);
    Some((numerator / divisor, denominator / divisor))
}

#[cfg_attr(not(any(feature = "xlsx", feature = "ods")), allow(dead_code))]
const fn gcd_u64(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    if left == 0 {
        1
    } else {
        left
    }
}

#[cfg_attr(not(feature = "ods"), allow(dead_code))]
pub(crate) fn parse_decimal_scaled_u32(value: &str, scale: u32) -> Option<u32> {
    let (numerator, denominator) = parse_decimal_ratio_u64(value)?;
    let scaled = u128::from(numerator).checked_mul(u128::from(scale))?;
    if scaled % u128::from(denominator) != 0 {
        return None;
    }
    u32::try_from(scaled / u128::from(denominator)).ok()
}

#[cfg_attr(not(any(feature = "xlsx", feature = "xlsb")), allow(dead_code))]
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OoxmlImplicitRowHeight {
    #[default]
    None,
    /// An XLSX worksheet omitted an authoritative default row height.
    XlsxApplicationDefault,
    /// An XLSB worksheet omitted an authoritative default row height.
    XlsbApplicationDefault,
}
