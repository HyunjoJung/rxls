//! Backend-neutral fixed-point scene primitives.

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

/// Number of fixed-point units in one CSS pixel.
pub const FIXED_UNITS_PER_PIXEL: i64 = 1_024;

/// A deterministic signed fixed-point CSS-pixel coordinate.
///
/// Geometry uses 1/1024 pixel units. This avoids backend-specific floating-point
/// formatting and gives SVG, raster, and PDF backends one shared layout surface.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fixed(i64);

impl Fixed {
    /// Zero pixels.
    pub const ZERO: Self = Self(0);

    /// Construct a coordinate from raw 1/1024-pixel units.
    pub const fn from_raw(raw: i64) -> Self {
        Self(raw)
    }

    /// Construct a coordinate from a whole number of CSS pixels.
    pub const fn from_pixels(pixels: i64) -> Self {
        Self(pixels.saturating_mul(FIXED_UNITS_PER_PIXEL))
    }

    /// Return the raw 1/1024-pixel value.
    pub const fn raw(self) -> i64 {
        self.0
    }

    pub(crate) fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    pub(crate) fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }

    pub(crate) fn max(self, other: Self) -> Self {
        Self(self.0.max(other.0))
    }
}

impl fmt::Debug for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fixed({})", format_fixed(*self))
    }
}

/// Format a fixed value as the shortest exact decimal representation.
pub(crate) fn format_fixed(value: Fixed) -> String {
    let raw = i128::from(value.raw());
    if raw == 0 {
        return "0".to_string();
    }
    let negative = raw < 0;
    let magnitude = raw.unsigned_abs();
    let scale = FIXED_UNITS_PER_PIXEL as u128;
    let whole = magnitude / scale;
    let mut remainder = magnitude % scale;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&whole.to_string());
    if remainder != 0 {
        out.push('.');
        while remainder != 0 {
            remainder *= 10;
            out.push(char::from(b'0' + (remainder / scale) as u8));
            remainder %= scale;
        }
    }
    out
}

/// An RGB color independent of the source workbook model and output backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb {
    /// Red channel.
    pub red: u8,
    /// Green channel.
    pub green: u8,
    /// Blue channel.
    pub blue: u8,
}

impl Rgb {
    /// Construct an RGB color.
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    /// White.
    pub const WHITE: Self = Self::new(255, 255, 255);
    /// Black.
    pub const BLACK: Self = Self::new(0, 0, 0);
    /// Default worksheet-view gridline gray.
    pub const GRIDLINE: Self = Self::new(217, 217, 217);
}

/// A fixed-point rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rect {
    /// Left coordinate.
    pub x: Fixed,
    /// Top coordinate.
    pub y: Fixed,
    /// Width.
    pub width: Fixed,
    /// Height.
    pub height: Fixed,
}

/// Horizontal text anchoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextAnchor {
    /// Anchor at the leading edge.
    Start,
    /// Anchor at the center.
    Middle,
    /// Anchor at the trailing edge.
    End,
}

/// Vertical text anchoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextBaseline {
    /// Anchor at the top edge.
    Top,
    /// Anchor at the vertical center.
    Middle,
    /// Anchor at the bottom edge.
    Bottom,
}

/// Backend-neutral text styling.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TextStyle {
    /// Ordered font-family request. This first slice stores one family name.
    pub family: String,
    /// Font size in CSS pixels.
    pub size: Fixed,
    /// Text color.
    pub color: Rgb,
    /// Bold text.
    pub bold: bool,
    /// Italic text.
    pub italic: bool,
    /// Underlined text.
    pub underline: bool,
    /// Struck-through text.
    pub strikethrough: bool,
    /// Horizontal anchoring within `bounds`.
    pub anchor: TextAnchor,
    /// Vertical anchoring within `bounds`.
    pub baseline: TextBaseline,
    /// Rotation in degrees around the resolved anchor point.
    pub rotation_degrees: i16,
}

/// A rectangle scene node.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RectNode {
    /// Rectangle geometry.
    pub rect: Rect,
    /// Optional fill paint.
    pub fill: Option<Rgb>,
    /// Optional uniform stroke paint.
    pub stroke: Option<Rgb>,
    /// Stroke width when `stroke` is present.
    pub stroke_width: Fixed,
}

/// A line scene node.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LineNode {
    /// Starting x coordinate.
    pub x1: Fixed,
    /// Starting y coordinate.
    pub y1: Fixed,
    /// Ending x coordinate.
    pub x2: Fixed,
    /// Ending y coordinate.
    pub y2: Fixed,
    /// Stroke paint.
    pub color: Rgb,
    /// Stroke width.
    pub width: Fixed,
}

/// A backend-neutral filled and/or stroked vector path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathNode {
    /// Absolute fixed-point path commands.
    pub commands: Vec<PathCommand>,
    /// Optional fill paint.
    pub fill: Option<Rgb>,
    /// Optional stroke paint.
    pub stroke: Option<Rgb>,
    /// Stroke width when `stroke` is present.
    pub stroke_width: Fixed,
}

/// A decoded, cropped RGBA image placed in worksheet coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageNode {
    /// Destination rectangle before rotation.
    pub rect: Rect,
    /// Decoded source width in pixels.
    pub pixel_width: u32,
    /// Decoded source height in pixels.
    pub pixel_height: u32,
    /// Straight-alpha pixels in row-major RGBA8 order.
    pub rgba: Arc<[u8]>,
    /// Clockwise rotation in thousandths of a degree around the rectangle center.
    pub rotation_mdeg: i32,
    /// Accessible image description retained from drawing metadata.
    pub alt_text: Option<String>,
}

/// One absolute fixed-point command in a filled vector path.
///
/// Outlined glyphs use these commands instead of delegating font selection or
/// shaping to an SVG consumer. This makes a scene independent of host fonts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathCommand {
    /// Begin a new contour.
    MoveTo {
        /// Destination x coordinate.
        x: Fixed,
        /// Destination y coordinate.
        y: Fixed,
    },
    /// Draw a straight segment.
    LineTo {
        /// Destination x coordinate.
        x: Fixed,
        /// Destination y coordinate.
        y: Fixed,
    },
    /// Draw a quadratic Bézier segment.
    QuadraticTo {
        /// Control-point x coordinate.
        control_x: Fixed,
        /// Control-point y coordinate.
        control_y: Fixed,
        /// Destination x coordinate.
        x: Fixed,
        /// Destination y coordinate.
        y: Fixed,
    },
    /// Draw a cubic Bézier segment.
    CubicTo {
        /// First control-point x coordinate.
        control1_x: Fixed,
        /// First control-point y coordinate.
        control1_y: Fixed,
        /// Second control-point x coordinate.
        control2_x: Fixed,
        /// Second control-point y coordinate.
        control2_y: Fixed,
        /// Destination x coordinate.
        x: Fixed,
        /// Destination y coordinate.
        y: Fixed,
    },
    /// Close the current contour.
    Close,
}

/// One source-text cluster mapped to its glyph-outline commands.
///
/// Source offsets are UTF-8 byte offsets into [`GlyphRunNode::text`]. Command
/// offsets are half-open indexes into [`GlyphRunNode::commands`]. Records are
/// stored in visual paint order; their source ranges may therefore move
/// backwards for bidirectional text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphCluster {
    /// Inclusive UTF-8 source byte offset.
    pub source_start: u64,
    /// Exclusive UTF-8 source byte offset.
    pub source_end: u64,
    /// Inclusive glyph path-command index.
    pub command_start: u64,
    /// Exclusive glyph path-command index.
    pub command_end: u64,
}

/// Nominal font geometry retained for one shaped source cluster.
///
/// Unlike outline ink bounds, these metrics describe the shaped text cursor
/// and the font's baseline-relative ascent and descent. PDF uses them for
/// semantic text boxes while replaying the original outline commands exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphClusterMetrics {
    /// Horizontal pen position before this cluster in scene coordinates.
    pub origin_x: Fixed,
    /// Signed horizontal cursor advance after this cluster.
    pub advance_x: Fixed,
    /// Alphabetic baseline in scene coordinates.
    pub baseline_y: Fixed,
    /// Positive distance from the baseline to the nominal font top.
    pub ascent: Fixed,
    /// Non-positive distance from the baseline to the nominal font bottom.
    pub descent: Fixed,
}

/// One bounded source-text semantic record.
///
/// Records before an optional zero-length layout divider are retention groups:
/// Calc can keep their complete source semantics when any member is visible.
/// Layout-produced runs append that divider at `text.len()`, followed by one
/// source range per prepared visual line in reading order. Keeping both record
/// classes in the existing bounded vector preserves compatibility with public
/// caller-authored scene literals while giving backends authoritative line
/// boundaries instead of asking them to infer lines from painted outlines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphSemanticGroup {
    /// Inclusive UTF-8 source byte offset.
    pub source_start: u64,
    /// Exclusive UTF-8 source byte offset.
    pub source_end: u64,
}

/// Visibility behavior attached to one semantic source span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum GlyphSemanticRetentionPolicy {
    /// Retain only source clusters whose nominal layout geometry is visible.
    ClipToNominalClusters,
    /// Retain the complete bounded source group when any member is visible.
    RetainCompleteSourceGroup,
}

/// One source span whose semantics use an explicit clipping policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlyphSemanticRetention {
    pub(crate) source: Range<usize>,
    pub(crate) policy: GlyphSemanticRetentionPolicy,
}

/// One source word or whitespace span inside an authoritative visual line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlyphSemanticWord {
    pub(crate) source: Range<usize>,
    pub(crate) visual_line_id: u32,
    pub(crate) reading_order: u32,
    pub(crate) cluster_indices: Vec<usize>,
    pub(crate) nominal_bounds: Option<Rect>,
    pub(crate) baselines: Vec<Fixed>,
    pub(crate) is_whitespace: bool,
    pub(crate) retention: GlyphSemanticRetentionPolicy,
}

/// One prepared visual line retained independently of paint-node adjacency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlyphSemanticLine {
    pub(crate) source: Range<usize>,
    pub(crate) visual_line_id: u32,
    pub(crate) reading_order: u32,
    pub(crate) cluster_indices: Vec<usize>,
    pub(crate) nominal_bounds: Option<Rect>,
    pub(crate) baselines: Vec<Fixed>,
    pub(crate) words: Vec<GlyphSemanticWord>,
}

/// Bounded text semantics materialized from one layout-produced glyph run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlyphSemanticLayout {
    pub(crate) lines: Vec<GlyphSemanticLine>,
    pub(crate) retention_groups: Vec<GlyphSemanticRetention>,
}

/// One contiguous outline paint span in visual command order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphPaint {
    /// Inclusive glyph path-command index.
    pub command_start: u64,
    /// Exclusive glyph path-command index.
    pub command_end: u64,
    /// Fill color for this command span.
    pub color: Rgb,
}

/// Identity of one font face referenced by shaped glyphs.
///
/// Scenes stay self-describing: the digest pins which face produced each
/// glyph without carrying the face bytes. A backend that embeds font programs
/// resolves these against the same pinned pack that produced the scene.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SceneFontFace {
    /// Resolved family name.
    pub family: String,
    /// Resolved OS/2 weight class.
    pub weight: u16,
    /// Resolved italic flag.
    pub italic: bool,
    /// Design units per em, needed to interpret glyph ids.
    pub units_per_em: u16,
    /// Lowercase hex SHA-256 of the face bytes, matching the pack manifest.
    pub face_sha256: String,
}

/// One shaped glyph retained alongside its replayed outline.
///
/// The outline commands remain authoritative for painting. These records add
/// the glyph identity that outlines discard, so a backend can embed a real
/// font program instead of outlining every glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShapedGlyph {
    /// Index into [`GlyphRunNode::font_faces`].
    pub face: u32,
    /// Index of the source cluster this glyph belongs to.
    ///
    /// A cluster can shape to several glyphs, and a glyph can paint no
    /// outline commands at all, so command ranges cannot recover this mapping.
    pub cluster: u32,
    /// Glyph id within that face.
    pub glyph_id: u16,
    /// Pen x position for this glyph in scene coordinates.
    pub origin_x: Fixed,
    /// Baseline y position for this glyph in scene coordinates.
    pub origin_y: Fixed,
    /// Rendered em size.
    pub size: Fixed,
    /// Whether synthetic bold or italic was applied to the painted outline.
    ///
    /// Synthetic styling cannot be reproduced by embedding the face alone, so
    /// backends must not embed a run whose glyphs set this.
    pub synthetic: bool,
}

/// Deterministically shaped text represented by absolute glyph outlines.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GlyphRunNode {
    /// Original display text retained for accessibility.
    pub text: String,
    /// Clip rectangle, including any legal spill into adjacent empty cells.
    pub clip_bounds: Rect,
    /// Concatenated absolute glyph-outline commands in visual paint order.
    pub commands: Vec<PathCommand>,
    /// Bounded source-cluster mappings in visual paint order.
    pub clusters: Vec<GlyphCluster>,
    /// Optional nominal metrics parallel to `clusters`.
    ///
    /// Layout-produced nodes always populate this vector. An empty vector
    /// retains compatibility with caller-authored scenes, whose PDF semantic
    /// geometry falls back to bounded outline placement.
    pub cluster_metrics: Vec<GlyphClusterMetrics>,
    /// Bounded source semantics and optional prepared visual-line records.
    ///
    /// Caller-authored non-empty vectors remain complete retention partitions.
    /// Layout-produced vectors append a zero-length divider at `text.len()`
    /// and then one ordered source range per non-empty visual line.
    pub semantic_groups: Vec<GlyphSemanticGroup>,
    /// Contiguous color spans covering every glyph-outline command.
    pub paints: Vec<GlyphPaint>,
    /// Underline and strike-through segments derived from pinned font metrics.
    pub decorations: Vec<LineNode>,
    /// Cell-level fallback color; exact outline colors are stored in `paints`.
    pub color: Rgb,
    /// Rotation in degrees around `pivot_x`,`pivot_y`.
    pub rotation_degrees: i16,
    /// Rotation pivot x coordinate.
    pub pivot_x: Fixed,
    /// Rotation pivot y coordinate.
    pub pivot_y: Fixed,
    /// Allowlisted hyperlink target, if this outlined text is interactive.
    pub hyperlink: Option<String>,
    /// Optional shaped-glyph identities parallel to the painted outlines.
    ///
    /// Layout-produced nodes populate this vector. An empty vector retains
    /// compatibility with caller-authored scenes, which fall back to outlined
    /// glyph emission.
    pub glyphs: Vec<ShapedGlyph>,
    /// Font faces referenced by [`ShapedGlyph::face`], in first-use order.
    ///
    /// Empty whenever `glyphs` is empty.
    pub font_faces: Vec<SceneFontFace>,
}

impl GlyphRunNode {
    fn semantic_layout_divider_index(&self) -> Option<usize> {
        self.semantic_groups
            .iter()
            .position(|group| group.source_start == group.source_end)
    }

    /// Retention records carried before the optional visual-line divider.
    ///
    /// Use this accessor instead of decoding [`Self::semantic_groups`]
    /// directly so caller-authored legacy partitions and layout-produced
    /// records remain distinguishable.
    pub fn semantic_retention_groups(&self) -> &[GlyphSemanticGroup] {
        match self.semantic_layout_divider_index() {
            Some(index) => &self.semantic_groups[..index],
            None => &self.semantic_groups,
        }
    }

    /// Prepared visible source fragments in stable visual-line reading order.
    ///
    /// `None` identifies caller-authored legacy metadata. `Some` identifies a
    /// layout-produced run, including the valid empty-line-record case. A
    /// printer-path source cutoff may omit complete leading or trailing
    /// clusters while outlines remain available for clipped paint replay.
    pub fn semantic_line_records(&self) -> Option<&[GlyphSemanticGroup]> {
        self.semantic_layout_divider_index()
            .map(|index| &self.semantic_groups[index + 1..])
    }

    fn source_record_range(&self, record: &GlyphSemanticGroup) -> Option<Range<usize>> {
        let start = usize::try_from(record.source_start).ok()?;
        let end = usize::try_from(record.source_end).ok()?;
        (start < end
            && end <= self.text.len()
            && self.text.is_char_boundary(start)
            && self.text.is_char_boundary(end))
        .then_some(start..end)
    }

    fn retention_partition_is_valid(&self, records: &[GlyphSemanticGroup]) -> bool {
        if records.is_empty() {
            return true;
        }
        let mut previous_end = 0_usize;
        for record in records {
            let Some(source) = self.source_record_range(record) else {
                return false;
            };
            if source.start != previous_end {
                return false;
            }
            previous_end = source.end;
        }
        previous_end == self.text.len()
    }

    fn line_records_are_valid(&self, records: &[GlyphSemanticGroup]) -> bool {
        if records.is_empty() {
            return self.text.is_empty();
        }
        let scalar_count = self.text.chars().count();
        if records.len() > scalar_count.max(1) {
            return false;
        }
        let mut previous_end = 0_usize;
        for (index, record) in records.iter().enumerate() {
            let Some(source) = self.source_record_range(record) else {
                return false;
            };
            if index != 0 && source.start < previous_end {
                return false;
            }
            previous_end = source.end;
        }

        self.clusters.iter().all(|cluster| {
            let start = cluster.source_start;
            let end = cluster.source_end;
            let index = records.partition_point(|record| record.source_end <= start);
            let Some(record) = records.get(index) else {
                return true;
            };
            if end <= record.source_start || record.source_end <= start {
                return true;
            }
            record.source_start <= start && start < end && end <= record.source_end
        })
    }

    /// Nominal layout rectangle for one source cluster.
    ///
    /// These bounds come from the shaped cursor and font metrics, never from
    /// glyph ink. Print projection already transforms `cluster_metrics`, so
    /// callers get page-local geometry without a second coordinate path.
    pub(crate) fn nominal_cluster_bounds(&self, index: usize) -> Option<Rect> {
        let metrics = *self.cluster_metrics.get(index)?;
        let advance_end = metrics.origin_x.checked_add(metrics.advance_x)?;
        let left = Fixed::from_raw(metrics.origin_x.raw().min(advance_end.raw()));
        let right = Fixed::from_raw(metrics.origin_x.raw().max(advance_end.raw()));
        let top = metrics.baseline_y.checked_sub(metrics.ascent)?;
        let bottom = metrics.baseline_y.checked_sub(metrics.descent)?;
        Some(Rect {
            x: left,
            y: top,
            width: right.checked_sub(left)?,
            height: bottom.checked_sub(top)?,
        })
    }

    fn semantic_bounds(&self, indices: &[usize]) -> Option<Rect> {
        let mut bounds = None::<Rect>;
        for &index in indices {
            let next = self.nominal_cluster_bounds(index)?;
            bounds = Some(match bounds {
                None => next,
                Some(current) => {
                    let current_right = current.x.checked_add(current.width)?;
                    let current_bottom = current.y.checked_add(current.height)?;
                    let next_right = next.x.checked_add(next.width)?;
                    let next_bottom = next.y.checked_add(next.height)?;
                    let left = Fixed::from_raw(current.x.raw().min(next.x.raw()));
                    let top = Fixed::from_raw(current.y.raw().min(next.y.raw()));
                    let right = Fixed::from_raw(current_right.raw().max(next_right.raw()));
                    let bottom = Fixed::from_raw(current_bottom.raw().max(next_bottom.raw()));
                    Rect {
                        x: left,
                        y: top,
                        width: right.checked_sub(left)?,
                        height: bottom.checked_sub(top)?,
                    }
                }
            });
        }
        bounds
    }

    fn semantic_baselines(&self, indices: &[usize]) -> Vec<Fixed> {
        let mut baselines = Vec::new();
        let mut seen = BTreeSet::new();
        for baseline in indices
            .iter()
            .filter_map(|index| self.cluster_metrics.get(*index))
            .map(|metrics| metrics.baseline_y)
        {
            if seen.insert(baseline) {
                baselines.push(baseline);
            }
        }
        baselines
    }

    /// Materialize the authoritative, bounded line and word semantics stored
    /// by layout. Legacy caller-authored nodes return `None` and keep backend
    /// compatibility fallbacks.
    pub(crate) fn semantic_text_layout(&self) -> Option<GlyphSemanticLayout> {
        let line_records = self.semantic_line_records()?;
        let retention_groups = self
            .semantic_retention_groups()
            .iter()
            .map(|record| {
                Some(GlyphSemanticRetention {
                    source: self.source_record_range(record)?,
                    policy: GlyphSemanticRetentionPolicy::RetainCompleteSourceGroup,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let mut clusters_by_line = vec![Vec::new(); line_records.len()];
        for (cluster_index, cluster) in self.clusters.iter().enumerate() {
            let line_index =
                line_records.partition_point(|record| record.source_end <= cluster.source_start);
            let Some(line) = line_records.get(line_index) else {
                continue;
            };
            if cluster.source_end <= line.source_start || line.source_end <= cluster.source_start {
                // Calc can shorten the semantic source recorded in its printer
                // metafile without changing the clipped glyph paint stream.
                continue;
            }
            if cluster.source_start < line.source_start || line.source_end < cluster.source_end {
                return None;
            }
            clusters_by_line[line_index].push(cluster_index);
        }
        let mut lines = Vec::with_capacity(line_records.len());
        for (line_index, (record, cluster_indices)) in
            line_records.iter().zip(clusters_by_line).enumerate()
        {
            let source = self.source_record_range(record)?;
            let visual_line_id = u32::try_from(line_index).ok()?;
            let source_words = semantic_source_spans(&self.text, source.clone())?;
            let mut clusters_by_word = vec![Vec::new(); source_words.len()];
            for &cluster_index in &cluster_indices {
                let cluster = self.clusters[cluster_index];
                let cluster_start = usize::try_from(cluster.source_start).ok()?;
                let cluster_end = usize::try_from(cluster.source_end).ok()?;
                let word_index =
                    source_words.partition_point(|(source, _)| source.end <= cluster_start);
                let (word_source, _) = source_words.get(word_index)?;
                if cluster_start < word_source.start || word_source.end < cluster_end {
                    return None;
                }
                clusters_by_word[word_index].push(cluster_index);
            }
            let mut words = Vec::with_capacity(source_words.len());
            for (word_index, ((word_source, is_whitespace), word_clusters)) in
                source_words.into_iter().zip(clusters_by_word).enumerate()
            {
                let retention = if retention_groups.is_empty() {
                    GlyphSemanticRetentionPolicy::ClipToNominalClusters
                } else {
                    GlyphSemanticRetentionPolicy::RetainCompleteSourceGroup
                };
                words.push(GlyphSemanticWord {
                    source: word_source,
                    visual_line_id,
                    reading_order: u32::try_from(word_index).ok()?,
                    nominal_bounds: self.semantic_bounds(&word_clusters),
                    baselines: self.semantic_baselines(&word_clusters),
                    cluster_indices: word_clusters,
                    is_whitespace,
                    retention,
                });
            }
            lines.push(GlyphSemanticLine {
                source,
                visual_line_id,
                reading_order: visual_line_id,
                nominal_bounds: self.semantic_bounds(&cluster_indices),
                baselines: self.semantic_baselines(&cluster_indices),
                cluster_indices,
                words,
            });
        }
        Some(GlyphSemanticLayout {
            lines,
            retention_groups,
        })
    }

    /// Return whether source clusters and paint spans are safe, bounded, and
    /// internally consistent with this node's text and command vectors.
    pub fn metadata_is_valid(&self) -> bool {
        let command_len = self.commands.len() as u64;
        let text_len = self.text.len() as u64;
        let scalar_count = self.text.chars().count();
        let max_semantic_records = scalar_count.saturating_mul(2).saturating_add(1);
        if self.clusters.len() > scalar_count
            || self.paints.len() > self.commands.len()
            || self.semantic_groups.len() > max_semantic_records
            || (!self.cluster_metrics.is_empty()
                && self.cluster_metrics.len() != self.clusters.len())
            || self.glyphs.is_empty() != self.font_faces.is_empty()
        {
            return false;
        }
        if self.cluster_metrics.iter().any(|metrics| {
            metrics.ascent <= Fixed::ZERO
                || metrics.descent > Fixed::ZERO
                || metrics.ascent <= metrics.descent
                || metrics.origin_x.checked_add(metrics.advance_x).is_none()
                || metrics.baseline_y.checked_sub(metrics.ascent).is_none()
                || metrics.baseline_y.checked_sub(metrics.descent).is_none()
        }) {
            return false;
        }

        let mut previous_command_end = 0_u64;
        for cluster in &self.clusters {
            let Ok(start) = usize::try_from(cluster.source_start) else {
                return false;
            };
            let Ok(end) = usize::try_from(cluster.source_end) else {
                return false;
            };
            if cluster.source_start >= cluster.source_end
                || cluster.source_end > text_len
                || !self.text.is_char_boundary(start)
                || !self.text.is_char_boundary(end)
                || cluster.command_start != previous_command_end
                || cluster.command_start > cluster.command_end
                || cluster.command_end > command_len
            {
                return false;
            }
            previous_command_end = cluster.command_end;
        }
        if previous_command_end != command_len {
            return false;
        }

        let dividers = self
            .semantic_groups
            .iter()
            .filter(|group| group.source_start == group.source_end)
            .count();
        if dividers > 1 {
            return false;
        }
        if let Some(index) = self.semantic_layout_divider_index() {
            let divider = self.semantic_groups[index];
            if divider.source_start != text_len
                || !self.retention_partition_is_valid(&self.semantic_groups[..index])
                || !self.line_records_are_valid(&self.semantic_groups[index + 1..])
            {
                return false;
            }
        } else if !self.retention_partition_is_valid(&self.semantic_groups) {
            return false;
        }

        if self.font_faces.iter().any(|face| {
            face.units_per_em == 0
                || face.face_sha256.len() != 64
                || !face
                    .face_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            return false;
        }
        if self.glyphs.iter().any(|glyph| {
            usize::try_from(glyph.face)
                .ok()
                .is_none_or(|index| index >= self.font_faces.len())
                || usize::try_from(glyph.cluster)
                    .ok()
                    .is_none_or(|index| index >= self.clusters.len())
                || glyph.size <= Fixed::ZERO
                || glyph.origin_x.checked_add(glyph.size).is_none()
                || glyph.origin_x.checked_sub(glyph.size).is_none()
                || glyph.origin_y.checked_add(glyph.size).is_none()
                || glyph.origin_y.checked_sub(glyph.size).is_none()
        }) {
            return false;
        }

        let mut previous_paint_end = 0_u64;
        for paint in &self.paints {
            if paint.command_start != previous_paint_end
                || paint.command_start >= paint.command_end
                || paint.command_end > command_len
            {
                return false;
            }
            previous_paint_end = paint.command_end;
        }
        previous_paint_end == command_len
    }

    /// Expand physical cluster visibility to complete retained semantic groups.
    pub(crate) fn expand_semantic_visibility(&self, visible: &mut [bool]) {
        let triggers = visible.to_vec();
        self.expand_semantic_visibility_from(visible, &triggers);
    }

    /// Expand retained groups selected by an independent visibility trigger.
    pub(crate) fn expand_semantic_visibility_from(&self, visible: &mut [bool], triggers: &[bool]) {
        // Legacy caller-authored runs may intentionally omit clusters while
        // still carrying a complete semantic partition. There is no physical
        // cluster to expand in that representation; backends handle its
        // single fallback glyph separately. Avoid indexing a visibility
        // vector whose length is the fallback-glyph count rather than the
        // cluster count.
        let semantic_groups = self.semantic_retention_groups();
        if semantic_groups.is_empty() || self.clusters.is_empty() {
            return;
        }
        debug_assert_eq!(visible.len(), self.clusters.len());
        debug_assert_eq!(triggers.len(), self.clusters.len());
        let overlapping_groups = |cluster: &GlyphCluster| {
            let start =
                semantic_groups.partition_point(|group| group.source_end <= cluster.source_start);
            let end = semantic_groups[start..]
                .partition_point(|group| group.source_start < cluster.source_end)
                + start;
            start..end
        };
        let mut retained = vec![false; semantic_groups.len()];
        for (cluster, trigger) in self.clusters.iter().zip(triggers) {
            if *trigger {
                retained[overlapping_groups(cluster)].fill(true);
            }
        }
        for (cluster, visible) in self.clusters.iter().zip(visible.iter_mut()) {
            if retained[overlapping_groups(cluster)]
                .iter()
                .any(|value| *value)
            {
                *visible = true;
            }
        }
    }
}

fn semantic_source_spans(text: &str, source: Range<usize>) -> Option<Vec<(Range<usize>, bool)>> {
    let selected = text.get(source.clone())?;
    let mut spans = Vec::new();
    let mut current = None::<(usize, bool)>;
    for (offset, character) in selected.char_indices() {
        let start = source.start.checked_add(offset)?;
        let whitespace = character.is_whitespace();
        match current {
            Some((span_start, previous)) if previous != whitespace => {
                spans.push((span_start..start, previous));
                current = Some((start, whitespace));
            }
            None => current = Some((start, whitespace)),
            _ => {}
        }
    }
    if let Some((span_start, whitespace)) = current {
        spans.push((span_start..source.end, whitespace));
    }
    Some(spans)
}

/// A clipped text scene node.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TextNode {
    /// Cell display text after invalid XML characters are normalized.
    pub text: String,
    /// The text's clipping and alignment rectangle.
    pub bounds: Rect,
    /// Clip rectangle, which can include legal spill into adjacent empty cells.
    pub clip_bounds: Rect,
    /// Insets from each horizontal cell edge.
    pub horizontal_padding: Fixed,
    /// Backend-neutral style.
    pub style: TextStyle,
    /// Allowlisted hyperlink target, if this text is interactive.
    pub hyperlink: Option<String>,
}

/// A deterministic rectangular clip applied to an ordered group of nodes.
///
/// Print tiles use this to retain full drawing geometry while preventing a
/// partially intersecting object from bleeding into adjacent page regions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClipGroupNode {
    /// Clip rectangle in the containing scene's coordinate system.
    pub clip: Rect,
    /// Ordered child paint operations.
    pub nodes: Vec<SceneNode>,
}

/// One backend-neutral scene operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SceneNode {
    /// Ordered child operations clipped as one rectangular group.
    ClipGroup(ClipGroupNode),
    /// Filled and/or stroked rectangle.
    Rect(RectNode),
    /// Independent line, used for explicit cell borders.
    Line(LineNode),
    /// Filled and/or stroked vector path.
    Path(PathNode),
    /// Decoded embedded raster image.
    Image(ImageNode),
    /// Text clipped to a cell or merged-cell rectangle.
    Text(TextNode),
    /// Font-shaped text painted as deterministic glyph outlines.
    GlyphRun(GlyphRunNode),
}

/// A complete fixed-point worksheet scene.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scene {
    /// Accessible scene title, normally the worksheet name.
    pub title: String,
    /// Canvas width.
    pub width: Fixed,
    /// Canvas height.
    pub height: Fixed,
    /// Canvas background.
    pub background: Rgb,
    /// Paint operations in deterministic back-to-front order.
    pub nodes: Vec<SceneNode>,
}

/// Canvas identity recorded by one backend replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackendCanvasTrace {
    pub(crate) width: Fixed,
    pub(crate) height: Fixed,
    pub(crate) background: Rgb,
}

/// One exact command range consumed by a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackendCommandRangeTrace {
    pub(crate) start: u64,
    pub(crate) end: u64,
}

/// One glyph-outline command consumed by a backend, including its source index
/// and resolved paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackendGlyphCommandTrace {
    pub(crate) index: u64,
    pub(crate) command: PathCommand,
    pub(crate) color: Rgb,
}

/// Hyperlink geometry acknowledged by one backend replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackendLinkTrace {
    pub(crate) rect: Rect,
    pub(crate) target: String,
}

/// Vector-path geometry recorded from one backend's own command-emission loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackendPathTrace {
    pub(crate) command_range: BackendCommandRangeTrace,
    pub(crate) commands: Vec<PathCommand>,
    pub(crate) fill: Option<Rgb>,
    pub(crate) stroke: Option<Rgb>,
    pub(crate) stroke_width: Fixed,
}

/// Raster-image geometry consumed by one backend replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackendImageTrace {
    pub(crate) rect: Rect,
    pub(crate) pixel_width: u32,
    pub(crate) pixel_height: u32,
    pub(crate) rotation_mdeg: i32,
}

/// Outlined-glyph geometry recorded by one backend replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackendGlyphTrace {
    pub(crate) clip_bounds: Rect,
    pub(crate) clusters: Vec<GlyphCluster>,
    pub(crate) paints: Vec<GlyphPaint>,
    pub(crate) commands: Vec<BackendGlyphCommandTrace>,
    pub(crate) decorations: Vec<LineNode>,
    pub(crate) rotation_degrees: i16,
    pub(crate) pivot_x: Fixed,
    pub(crate) pivot_y: Fixed,
    pub(crate) link: Option<BackendLinkTrace>,
}

/// Approximate-text geometry. PNG deliberately rejects this node kind before
/// replay, but SVG/PDF traces retain it for their backend-local tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackendTextTrace {
    pub(crate) bounds: Rect,
    pub(crate) clip_bounds: Rect,
    pub(crate) horizontal_padding: Fixed,
    pub(crate) style: TextStyle,
    pub(crate) link: Option<BackendLinkTrace>,
}

/// One scene node as independently consumed by a concrete backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BackendNodeTrace {
    ClipStart(Rect),
    ClipEnd,
    Rect(RectNode),
    Line(LineNode),
    Path(BackendPathTrace),
    Image(BackendImageTrace),
    Text(BackendTextTrace),
    Glyph(BackendGlyphTrace),
}

/// Deterministic source-coordinate readback from one concrete backend replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackendGeometryTrace {
    pub(crate) canvas: BackendCanvasTrace,
    pub(crate) nodes: Vec<BackendNodeTrace>,
}

impl BackendGeometryTrace {
    pub(crate) fn new(scene: &Scene) -> Self {
        Self {
            canvas: BackendCanvasTrace {
                width: scene.width,
                height: scene.height,
                background: scene.background,
            },
            nodes: Vec::with_capacity(scene.nodes.len()),
        }
    }

    pub(crate) fn push(&mut self, node: BackendNodeTrace) {
        self.nodes.push(node);
    }
}

/// Validating recorder used inside each backend's own path-command loop.
#[derive(Debug)]
pub(crate) struct BackendPathTraceBuilder<'a> {
    node: &'a PathNode,
    commands: Vec<PathCommand>,
}

impl<'a> BackendPathTraceBuilder<'a> {
    pub(crate) fn new(node: &'a PathNode) -> Self {
        Self {
            node,
            commands: Vec::with_capacity(node.commands.len()),
        }
    }

    pub(crate) fn record(
        &mut self,
        index: usize,
        command: PathCommand,
    ) -> Result<(), &'static str> {
        if index != self.commands.len() || self.node.commands.get(index) != Some(&command) {
            return Err("backend_path_trace_mismatch");
        }
        self.commands.push(command);
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<BackendPathTrace, &'static str> {
        if self.commands != self.node.commands {
            return Err("backend_path_trace_incomplete");
        }
        Ok(BackendPathTrace {
            command_range: BackendCommandRangeTrace {
                start: 0,
                end: self.commands.len() as u64,
            },
            commands: self.commands,
            fill: self.node.fill,
            stroke: self.node.stroke,
            stroke_width: self.node.stroke_width,
        })
    }
}

/// Validating recorder used inside each backend's glyph replay path.
#[derive(Debug)]
pub(crate) struct BackendGlyphTraceBuilder<'a> {
    node: &'a GlyphRunNode,
    clip_recorded: bool,
    commands: Vec<BackendGlyphCommandTrace>,
    decorations: Vec<LineNode>,
    link: Option<BackendLinkTrace>,
}

impl<'a> BackendGlyphTraceBuilder<'a> {
    pub(crate) fn new(node: &'a GlyphRunNode) -> Self {
        Self {
            node,
            clip_recorded: false,
            commands: Vec::with_capacity(node.commands.len()),
            decorations: Vec::with_capacity(node.decorations.len()),
            link: None,
        }
    }

    pub(crate) fn record_clip(&mut self, clip: Rect) -> Result<(), &'static str> {
        if self.clip_recorded || clip != self.node.clip_bounds {
            return Err("backend_glyph_clip_trace_mismatch");
        }
        self.clip_recorded = true;
        Ok(())
    }

    pub(crate) fn record_command(
        &mut self,
        index: u64,
        command: PathCommand,
        color: Rgb,
    ) -> Result<(), &'static str> {
        let expected_index = self.commands.len() as u64;
        let expected_command = usize::try_from(index)
            .ok()
            .and_then(|index| self.node.commands.get(index));
        let expected_color = self
            .node
            .paints
            .iter()
            .find(|paint| index >= paint.command_start && index < paint.command_end)
            .map(|paint| paint.color);
        if index != expected_index
            || expected_command != Some(&command)
            || expected_color != Some(color)
        {
            return Err("backend_glyph_command_trace_mismatch");
        }
        self.commands.push(BackendGlyphCommandTrace {
            index,
            command,
            color,
        });
        Ok(())
    }

    pub(crate) fn record_decoration(&mut self, line: &LineNode) -> Result<(), &'static str> {
        let index = self.decorations.len();
        if self.node.decorations.get(index) != Some(line) {
            return Err("backend_glyph_decoration_trace_mismatch");
        }
        self.decorations.push(line.clone());
        Ok(())
    }

    pub(crate) fn record_link(&mut self, rect: Rect, target: &str) -> Result<(), &'static str> {
        if self.link.is_some() || self.node.hyperlink.as_deref() != Some(target) {
            return Err("backend_glyph_link_trace_mismatch");
        }
        self.link = Some(BackendLinkTrace {
            rect,
            target: target.to_string(),
        });
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<BackendGlyphTrace, &'static str> {
        if !self.clip_recorded
            || self.commands.len() != self.node.commands.len()
            || self.decorations != self.node.decorations
        {
            return Err("backend_glyph_trace_incomplete");
        }
        Ok(BackendGlyphTrace {
            clip_bounds: self.node.clip_bounds,
            clusters: self.node.clusters.clone(),
            paints: self.node.paints.clone(),
            commands: self.commands,
            decorations: self.decorations,
            rotation_degrees: self.node.rotation_degrees,
            pivot_x: self.node.pivot_x,
            pivot_y: self.node.pivot_y,
            link: self.link,
        })
    }
}

pub(crate) fn backend_image_trace(node: &ImageNode) -> BackendImageTrace {
    BackendImageTrace {
        rect: node.rect,
        pixel_width: node.pixel_width,
        pixel_height: node.pixel_height,
        rotation_mdeg: node.rotation_mdeg,
    }
}

pub(crate) fn backend_text_trace(node: &TextNode, link: Option<&str>) -> BackendTextTrace {
    BackendTextTrace {
        bounds: node.bounds,
        clip_bounds: node.clip_bounds,
        horizontal_padding: node.horizontal_padding,
        style: node.style.clone(),
        link: link.map(|target| BackendLinkTrace {
            rect: node.clip_bounds,
            target: target.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(x: i64) -> PathCommand {
        PathCommand::MoveTo {
            x: Fixed::from_pixels(x),
            y: Fixed::ZERO,
        }
    }

    #[test]
    fn glyph_metadata_accepts_visual_bidi_order_and_rejects_unsafe_ranges() {
        let mut node = GlyphRunNode {
            glyphs: Vec::new(),
            font_faces: Vec::new(),
            text: "Aאב".to_string(),
            clip_bounds: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(10),
                height: Fixed::from_pixels(10),
            },
            commands: vec![command(0), command(1), command(2)],
            clusters: vec![
                GlyphCluster {
                    source_start: 0,
                    source_end: 1,
                    command_start: 0,
                    command_end: 1,
                },
                GlyphCluster {
                    source_start: 3,
                    source_end: 5,
                    command_start: 1,
                    command_end: 2,
                },
                GlyphCluster {
                    source_start: 1,
                    source_end: 3,
                    command_start: 2,
                    command_end: 3,
                },
            ],
            cluster_metrics: Vec::new(),
            semantic_groups: Vec::new(),
            paints: vec![GlyphPaint {
                command_start: 0,
                command_end: 3,
                color: Rgb::BLACK,
            }],
            decorations: Vec::new(),
            color: Rgb::BLACK,
            rotation_degrees: 0,
            pivot_x: Fixed::ZERO,
            pivot_y: Fixed::ZERO,
            hyperlink: None,
        };
        assert!(node.metadata_is_valid());

        node.clusters[1].source_start = 2;
        assert!(
            !node.metadata_is_valid(),
            "UTF-8 interior offsets are rejected"
        );
        node.clusters[1].source_start = 3;
        node.paints[0].command_start = 1;
        assert!(!node.metadata_is_valid(), "paint gaps are rejected");

        node.paints[0].command_start = 0;
        let metrics = GlyphClusterMetrics {
            origin_x: Fixed::ZERO,
            advance_x: Fixed::from_pixels(1),
            baseline_y: Fixed::from_pixels(8),
            ascent: Fixed::from_pixels(7),
            descent: Fixed::from_pixels(-2),
        };
        node.cluster_metrics = vec![metrics; node.clusters.len()];
        assert!(node.metadata_is_valid());
        node.cluster_metrics.pop();
        assert!(
            !node.metadata_is_valid(),
            "nominal metrics must remain parallel to source clusters"
        );
        node.cluster_metrics.push(metrics);
        node.semantic_groups = vec![GlyphSemanticGroup {
            source_start: 0,
            source_end: 1,
        }];
        assert!(
            !node.metadata_is_valid(),
            "semantic groups must partition the complete source"
        );
        node.semantic_groups = vec![
            GlyphSemanticGroup {
                source_start: 0,
                source_end: 1,
            },
            GlyphSemanticGroup {
                source_start: 1,
                source_end: 5,
            },
        ];
        assert!(node.metadata_is_valid());
        node.semantic_groups[1].source_start = 2;
        assert!(
            !node.metadata_is_valid(),
            "semantic group gaps are rejected"
        );
        node.semantic_groups.clear();
        node.cluster_metrics[0].origin_x = Fixed::from_raw(i64::MAX);
        assert!(
            !node.metadata_is_valid(),
            "nominal metric extents must not overflow"
        );

        node.cluster_metrics[0].origin_x = Fixed::ZERO;
        node.font_faces.push(SceneFontFace {
            family: "Test Sans".to_string(),
            weight: 400,
            italic: false,
            units_per_em: 1_000,
            face_sha256: "0123456789abcdef".repeat(4),
        });
        node.glyphs.push(ShapedGlyph {
            face: 0,
            cluster: 1,
            glyph_id: 1,
            origin_x: Fixed::from_pixels(1),
            origin_y: Fixed::from_pixels(8),
            size: Fixed::from_pixels(7),
            synthetic: false,
        });
        assert!(node.metadata_is_valid());

        node.glyphs[0].face = 1;
        assert!(!node.metadata_is_valid(), "font indexes must be in range");
        node.glyphs[0].face = 0;
        node.glyphs[0].cluster = 3;
        assert!(
            !node.metadata_is_valid(),
            "cluster indexes must be in range"
        );
        node.glyphs[0].cluster = 1;
        node.font_faces[0].face_sha256.make_ascii_uppercase();
        assert!(
            !node.metadata_is_valid(),
            "face identities must be canonical lowercase SHA-256"
        );
        node.font_faces[0].face_sha256 = "0123456789abcdef".repeat(4);
        node.glyphs.clear();
        assert!(
            !node.metadata_is_valid(),
            "font identities cannot remain without shaped glyphs"
        );
    }

    #[test]
    fn semantic_layout_records_materialize_bounded_lines_words_and_policy() {
        let text = "alpha beta";
        let metrics = |origin_x, advance_x, baseline_y| GlyphClusterMetrics {
            origin_x: Fixed::from_pixels(origin_x),
            advance_x: Fixed::from_pixels(advance_x),
            baseline_y: Fixed::from_pixels(baseline_y),
            ascent: Fixed::from_pixels(7),
            descent: Fixed::from_pixels(-2),
        };
        let mut node = GlyphRunNode {
            glyphs: Vec::new(),
            font_faces: Vec::new(),
            text: text.to_string(),
            clip_bounds: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(20),
                height: Fixed::from_pixels(24),
            },
            commands: vec![command(0), command(6)],
            clusters: vec![
                GlyphCluster {
                    source_start: 0,
                    source_end: 5,
                    command_start: 0,
                    command_end: 1,
                },
                GlyphCluster {
                    source_start: 5,
                    source_end: 6,
                    command_start: 1,
                    command_end: 1,
                },
                GlyphCluster {
                    source_start: 6,
                    source_end: 10,
                    command_start: 1,
                    command_end: 2,
                },
            ],
            cluster_metrics: vec![metrics(0, 5, 8), metrics(5, 1, 8), metrics(6, 4, 18)],
            semantic_groups: vec![
                GlyphSemanticGroup {
                    source_start: 0,
                    source_end: 10,
                },
                GlyphSemanticGroup {
                    source_start: 10,
                    source_end: 10,
                },
                GlyphSemanticGroup {
                    source_start: 0,
                    source_end: 6,
                },
                GlyphSemanticGroup {
                    source_start: 6,
                    source_end: 10,
                },
            ],
            paints: vec![GlyphPaint {
                command_start: 0,
                command_end: 2,
                color: Rgb::BLACK,
            }],
            decorations: Vec::new(),
            color: Rgb::BLACK,
            rotation_degrees: 0,
            pivot_x: Fixed::ZERO,
            pivot_y: Fixed::ZERO,
            hyperlink: None,
        };
        assert!(node.metadata_is_valid());

        let layout = node.semantic_text_layout().unwrap();
        assert_eq!(layout.retention_groups.len(), 1);
        assert_eq!(layout.retention_groups[0].source, 0..10);
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(layout.lines[0].source, 0..6);
        assert_eq!(layout.lines[0].visual_line_id, 0);
        assert_eq!(layout.lines[0].words.len(), 2);
        assert_eq!(layout.lines[0].words[0].source, 0..5);
        assert_eq!(layout.lines[0].words[1].source, 5..6);
        assert_eq!(layout.lines[1].source, 6..10);
        assert_eq!(layout.lines[1].baselines, [Fixed::from_pixels(18)]);
        assert_eq!(
            layout.lines[1].nominal_bounds,
            Some(Rect {
                x: Fixed::from_pixels(6),
                y: Fixed::from_pixels(11),
                width: Fixed::from_pixels(4),
                height: Fixed::from_pixels(9),
            })
        );
        assert!(layout
            .lines
            .iter()
            .flat_map(|line| &line.words)
            .all(|word| {
                word.retention == GlyphSemanticRetentionPolicy::RetainCompleteSourceGroup
            }));

        node.semantic_groups[1].source_start = 9;
        node.semantic_groups[1].source_end = 9;
        assert!(
            !node.metadata_is_valid(),
            "the layout divider must be exactly text.len()"
        );
        node.semantic_groups[1].source_start = 10;
        node.semantic_groups[1].source_end = 10;
        node.semantic_groups[3].source_start = 5;
        assert!(
            !node.metadata_is_valid(),
            "visual-line source ranges must stay ordered and disjoint"
        );

        node.semantic_groups = vec![
            GlyphSemanticGroup {
                source_start: 10,
                source_end: 10,
            },
            GlyphSemanticGroup {
                source_start: 0,
                source_end: 5,
            },
        ];
        assert!(
            node.metadata_is_valid(),
            "a printer-path cutoff may omit complete trailing clusters"
        );
        let clipped = node.semantic_text_layout().unwrap();
        assert!(clipped.retention_groups.is_empty());
        assert_eq!(clipped.lines.len(), 1);
        assert_eq!(clipped.lines[0].source, 0..5);
        assert_eq!(clipped.lines[0].cluster_indices, [0]);

        node.semantic_groups[1].source_end = 4;
        assert!(
            !node.metadata_is_valid(),
            "a semantic cutoff cannot split a shaped source cluster"
        );
    }

    #[test]
    fn semantic_visibility_ignores_unclustered_legacy_fallback_glyph() {
        let node = GlyphRunNode {
            text: "fallback".to_string(),
            clip_bounds: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(10),
                height: Fixed::from_pixels(10),
            },
            commands: Vec::new(),
            clusters: Vec::new(),
            cluster_metrics: Vec::new(),
            semantic_groups: vec![GlyphSemanticGroup {
                source_start: 0,
                source_end: 8,
            }],
            paints: Vec::new(),
            decorations: Vec::new(),
            color: Rgb::BLACK,
            rotation_degrees: 0,
            pivot_x: Fixed::ZERO,
            pivot_y: Fixed::ZERO,
            hyperlink: None,
            glyphs: Vec::new(),
            font_faces: Vec::new(),
        };
        assert!(node.metadata_is_valid());

        // PDF's legacy fallback path has one registered glyph even though the
        // scene node has no physical clusters. This must remain a no-op.
        let mut visible = vec![true];
        node.expand_semantic_visibility(&mut visible);
        assert_eq!(visible, vec![true]);
    }
}
