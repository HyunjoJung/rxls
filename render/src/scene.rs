//! Backend-neutral fixed-point scene primitives.

use std::fmt;
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
}

impl GlyphRunNode {
    /// Return whether source clusters and paint spans are safe, bounded, and
    /// internally consistent with this node's text and command vectors.
    pub fn metadata_is_valid(&self) -> bool {
        let command_len = self.commands.len() as u64;
        let text_len = self.text.len() as u64;
        let scalar_count = self.text.chars().count();
        if self.clusters.len() > scalar_count || self.paints.len() > self.commands.len() {
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

/// One backend-neutral scene operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SceneNode {
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
    }
}
