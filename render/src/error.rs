//! Typed rendering failures.

use std::error::Error;
use std::fmt;

/// A bounded resource category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitKind {
    /// Source rows in the selected range.
    Rows,
    /// Source columns in the selected range.
    Columns,
    /// Rectangular source cells before hidden-row filtering.
    Cells,
    /// Conditional-formatting rules retained for one worksheet.
    ConditionalRules,
    /// Cell/rule evaluations performed while resolving conditional formatting.
    ConditionalEvaluations,
    /// Drawing, chart, image, and sparkline objects retained for one worksheet.
    DrawingObjects,
    /// Embedded image payload bytes inspected before placeholder rendering.
    MediaBytes,
    /// Width or height declared by one decoded embedded image.
    ImageDimension,
    /// Decoded pixels declared by one embedded image.
    ImagePixels,
    /// Aggregate decoded RGBA bytes retained for embedded images.
    DecodedMediaBytes,
    /// Data series retained across charts on one worksheet.
    ChartSeries,
    /// Resolved source points retained across charts and sparklines.
    ChartPoints,
    /// Accumulated UTF-8 display-text bytes.
    TextBytes,
    /// Unicode scalar values passed to the text backend.
    Glyphs,
    /// Shaped visual text runs.
    TextRuns,
    /// Laid-out text lines.
    TextLines,
    /// Vector path commands generated from glyph outlines.
    PathCommands,
    /// Backend-neutral scene nodes.
    SceneNodes,
    /// Canvas width or height in raw 1/1024-pixel units.
    Dimension,
    /// Serialized SVG bytes.
    OutputBytes,
    /// Logical page slots before sparse omission.
    LogicalPages,
    /// Emitted print pages.
    Pages,
    /// Scene nodes on one print page.
    PageSceneNodes,
    /// Scene nodes accumulated across a print document.
    TotalSceneNodes,
    /// Backend vector commands.
    BackendCommands,
    /// Serialized PDF bytes.
    PdfBytes,
    /// Raster width or height in pixels.
    RasterDimension,
    /// Raster pixels on one page.
    RasterPixels,
    /// Encoded PNG bytes on one page.
    PngBytes,
}

impl LimitKind {
    /// Stable machine-readable identifier.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Rows => "rows",
            Self::Columns => "columns",
            Self::Cells => "cells",
            Self::ConditionalRules => "conditional_rules",
            Self::ConditionalEvaluations => "conditional_evaluations",
            Self::DrawingObjects => "drawing_objects",
            Self::MediaBytes => "media_bytes",
            Self::ImageDimension => "image_dimension",
            Self::ImagePixels => "image_pixels",
            Self::DecodedMediaBytes => "decoded_media_bytes",
            Self::ChartSeries => "chart_series",
            Self::ChartPoints => "chart_points",
            Self::TextBytes => "text_bytes",
            Self::Glyphs => "glyphs",
            Self::TextRuns => "text_runs",
            Self::TextLines => "text_lines",
            Self::PathCommands => "path_commands",
            Self::SceneNodes => "scene_nodes",
            Self::Dimension => "dimension",
            Self::OutputBytes => "output_bytes",
            Self::LogicalPages => "logical_pages",
            Self::Pages => "pages",
            Self::PageSceneNodes => "page_scene_nodes",
            Self::TotalSceneNodes => "total_scene_nodes",
            Self::BackendCommands => "backend_commands",
            Self::PdfBytes => "pdf_bytes",
            Self::RasterDimension => "raster_dimension",
            Self::RasterPixels => "raster_pixels",
            Self::PngBytes => "png_bytes",
        }
    }
}

/// A typed layout or serialization error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// The requested sheet does not exist.
    SheetIndexOutOfRange {
        /// Requested zero-based sheet index.
        requested: usize,
        /// Number of available sheets.
        sheet_count: usize,
    },
    /// The requested inclusive rectangle is reversed.
    InvalidRange {
        /// First row.
        first_row: u32,
        /// First column.
        first_col: u16,
        /// Last row.
        last_row: u32,
        /// Last column.
        last_col: u16,
    },
    /// The requested rectangle exceeds the spreadsheet grid.
    RangeOutsideGrid {
        /// Requested final row.
        last_row: u32,
        /// Requested final column.
        last_col: u16,
        /// Largest supported zero-based row.
        max_row: u32,
        /// Largest supported zero-based column.
        max_col: u16,
    },
    /// A configured resource cap was exceeded.
    LimitExceeded {
        /// Resource category.
        kind: LimitKind,
        /// Configured inclusive upper bound.
        limit: u64,
        /// Required amount, when exactly known.
        actual: u64,
    },
    /// Fixed-point coordinate arithmetic overflowed.
    CoordinateOverflow,
    /// The verified typography backend rejected otherwise bounded input.
    Typography {
        /// Stable, path-free reason code.
        reason: &'static str,
    },
    /// A deterministic output backend rejected the bounded scene.
    Backend {
        /// Stable, path-free reason code.
        reason: &'static str,
    },
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SheetIndexOutOfRange {
                requested,
                sheet_count,
            } => write!(
                f,
                "sheet index {requested} is out of range for {sheet_count} sheets"
            ),
            Self::InvalidRange {
                first_row,
                first_col,
                last_row,
                last_col,
            } => write!(
                f,
                "invalid inclusive range ({first_row},{first_col})..=({last_row},{last_col})"
            ),
            Self::RangeOutsideGrid {
                last_row,
                last_col,
                max_row,
                max_col,
            } => write!(
                f,
                "render range ends at ({last_row},{last_col}), beyond maximum ({max_row},{max_col})"
            ),
            Self::LimitExceeded {
                kind,
                limit,
                actual,
            } => write!(
                f,
                "render {} limit exceeded: limit {limit}, required {actual}",
                kind.code()
            ),
            Self::CoordinateOverflow => f.write_str("fixed-point coordinate overflow"),
            Self::Typography { reason } => write!(f, "typography backend failed: {reason}"),
            Self::Backend { reason } => write!(f, "render backend failed: {reason}"),
        }
    }
}

impl Error for RenderError {}
