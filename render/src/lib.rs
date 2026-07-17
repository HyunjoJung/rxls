//! Deterministic, bounded worksheet rendering for `rxls`.
//!
//! Layout produces a backend-neutral [`Scene`] using fixed-point geometry. SVG
//! is one consumer of that scene, not the layout model itself. This keeps later
//! font shaping, raster, PDF, and LibreOffice differential work composable.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(missing_debug_implementations, rust_2018_idioms)]

mod error;
mod font;
mod layout;
mod media;
mod pdf;
mod png;
mod print;
mod scene;
mod svg;
mod typography;

pub use error::{LimitKind, RenderError};
pub use font::{FontFaceIdentity, FontPack, FontPackError, FontPackLimits, FontPackMember};
pub use layout::{
    build_scene, build_sheet_scene, CellCoordinate, RenderLimits, RenderOptions, RenderRange,
    RenderReport, RenderSelection, RenderWarning, RenderedFontFace, SceneBuild, WarningCode,
    MAX_WORKSHEET_COLUMN, MAX_WORKSHEET_ROW,
};
pub use pdf::render_print_document_pdf;
pub use png::{render_print_document_png_pages, render_print_page_png};
pub use print::{
    build_print_document, build_print_page, build_sheet_print_document, build_sheet_print_page,
    prepare_print_document, prepare_sheet_print_document, PageMapEntry, PaperGeometry,
    PreparedPrintDocument, PrintDocument, PrintLayoutOverride, PrintLimits, PrintOptions,
    PrintPage, PrintReport, PrintWarning, PrintWarningCode,
};
pub use scene::{
    ClipGroupNode, Fixed, GlyphCluster, GlyphPaint, GlyphRunNode, ImageNode, LineNode, PathCommand,
    PathNode, Rect, RectNode, Rgb, Scene, SceneNode, TextAnchor, TextBaseline, TextNode, TextStyle,
    FIXED_UNITS_PER_PIXEL,
};
pub use svg::render_scene_svg;

use rxls::Workbook;

/// SVG plus its backend-neutral scene and finalized report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOutput {
    /// Backend-neutral fixed-point scene.
    pub scene: Scene,
    /// Deterministic SVG document.
    pub svg: String,
    /// Layout and serialization report.
    pub report: RenderReport,
}

/// Render one workbook sheet as bounded deterministic SVG.
pub fn render_sheet_svg(
    workbook: &Workbook,
    sheet_index: usize,
    options: &RenderOptions,
) -> Result<RenderOutput, RenderError> {
    let build = build_scene(workbook, sheet_index, options)?;
    let svg = render_scene_svg(&build.scene, options.limits.max_output_bytes)?;
    let mut report = build.report;
    report.svg_bytes = svg.len() as u64;
    Ok(RenderOutput {
        scene: build.scene,
        svg,
        report,
    })
}
