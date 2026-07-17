//! Bounded WebAssembly facade for the standalone `rxls-render` engine.
//!
//! The facade is intentionally synchronous. The JavaScript package runs it in
//! a dedicated module worker, so parsing and layout never block the browser's
//! main thread. Every request is capped below the native renderer defaults.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use rxls::Workbook;
use rxls_render::{
    build_print_page, prepare_print_document, render_print_page_png, render_scene_svg,
    render_sheet_svg, FontPack, FontPackError, FontPackLimits, FontPackMember, LimitKind,
    PreparedPrintDocument, PrintDocument, PrintLimits, PrintOptions, RenderError, RenderLimits,
    RenderOptions, RenderRange, RenderSelection,
};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

/// Maximum workbook input copied into WebAssembly linear memory.
pub const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
/// Maximum total retained embedded-image bytes in one parsed workbook.
pub const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum embedded image count in one parsed workbook.
pub const MAX_IMAGES: u64 = 256;
/// Maximum verified in-memory font-pack bytes, including auxiliary members.
pub const MAX_FONT_BYTES: u64 = 64 * 1024 * 1024;

const MAX_ROWS: u64 = 4_096;
const MAX_COLUMNS: u64 = 512;
const MAX_CELLS: u64 = 250_000;
const MAX_CONDITIONAL_RULES: u64 = 2_048;
const MAX_CONDITIONAL_EVALUATIONS: u64 = 500_000;
const MAX_DRAWING_OBJECTS: u64 = 2_048;
const MAX_MEDIA_BYTES: u64 = MAX_IMAGE_BYTES;
const MAX_IMAGE_DIMENSION: u64 = 8_192;
const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_DECODED_MEDIA_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CHART_SERIES: u64 = 128;
const MAX_CHART_POINTS: u64 = 250_000;
const MAX_TEXT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GLYPHS: u64 = 1_000_000;
const MAX_TEXT_RUNS: u64 = 500_000;
const MAX_TEXT_LINES: u64 = 250_000;
const MAX_PATH_COMMANDS: u64 = 4_000_000;
const MAX_SCENE_NODES: u64 = 1_000_000;
const MAX_DIMENSION_RAW: u64 = 2_000_000 * 1_024;
const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SHEETS: u64 = 255;
const MAX_LOGICAL_PAGES: u64 = 2_048;
const MAX_PAGES: u64 = 512;
const MAX_TOTAL_SCENE_NODES: u64 = 2_000_000;
const MAX_BACKEND_COMMANDS: u64 = 4_000_000;
const MAX_RASTER_DIMENSION: u32 = 8_192;
const MAX_RASTER_PIXELS: u64 = 32 * 1024 * 1024;
const MAX_PNG_BYTES: u64 = 16 * 1024 * 1024;
const MIN_DPI: u32 = 36;
const MAX_DPI: u32 = 300;
const FONT_BUNDLE_MAGIC: &[u8; 8] = b"RXLSFPK1";
const MAX_FONT_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_FONT_FILES: u64 = 512;
const MAX_FONT_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FONT_AUXILIARY_BYTES: u64 = 1024 * 1024;
const MAX_FONT_BUNDLE_BYTES: u64 = MAX_FONT_BYTES + MAX_FONT_MANIFEST_BYTES + 64 * 1024;

#[wasm_bindgen(typescript_custom_section)]
const ERROR_TYPES: &str = r#"
/** Stable, path-neutral error thrown by rxls-render-wasm. */
export interface RxlsRenderError extends Error {
  readonly name: "RxlsRenderError";
  readonly code: string;
  readonly location: string;
  readonly resource: string | null;
  readonly limit: number | null;
  readonly actual: number | null;
}
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FacadeError {
    code: &'static str,
    message: String,
    location: &'static str,
    resource: Option<&'static str>,
    limit: Option<u64>,
    actual: Option<u64>,
}

impl FacadeError {
    fn simple(code: &'static str, message: impl Into<String>, location: &'static str) -> Self {
        Self {
            code,
            message: message.into(),
            location,
            resource: None,
            limit: None,
            actual: None,
        }
    }

    fn limit(resource: &'static str, limit: u64, actual: u64) -> Self {
        Self {
            code: "limit_exceeded",
            message: format!("{resource} limit exceeded: limit {limit}, required {actual}"),
            location: "limits",
            resource: Some(resource),
            limit: Some(limit),
            actual: Some(actual),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RangeRequest {
    first_row: u32,
    first_col: u16,
    last_row: u32,
    last_col: u16,
}

impl From<RangeRequest> for RenderRange {
    fn from(value: RangeRequest) -> Self {
        Self::new(
            value.first_row,
            value.first_col,
            value.last_row,
            value.last_col,
        )
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RequestedLimits {
    max_rows: Option<u64>,
    max_columns: Option<u64>,
    max_cells: Option<u64>,
    max_conditional_rules: Option<u64>,
    max_conditional_evaluations: Option<u64>,
    max_drawing_objects: Option<u64>,
    max_media_bytes: Option<u64>,
    max_image_dimension: Option<u64>,
    max_image_pixels: Option<u64>,
    max_decoded_media_bytes: Option<u64>,
    max_chart_series: Option<u64>,
    max_chart_points: Option<u64>,
    max_text_bytes: Option<u64>,
    max_glyphs: Option<u64>,
    max_text_runs: Option<u64>,
    max_text_lines: Option<u64>,
    max_path_commands: Option<u64>,
    max_scene_nodes: Option<u64>,
    max_dimension_raw: Option<u64>,
    max_output_bytes: Option<u64>,
    max_logical_pages: Option<u64>,
    max_pages: Option<u64>,
    max_total_scene_nodes: Option<u64>,
    max_backend_commands: Option<u64>,
    max_raster_dimension: Option<u32>,
    max_raster_pixels: Option<u64>,
    max_png_bytes: Option<u64>,
    max_image_bytes: Option<u64>,
    max_images: Option<u64>,
    max_font_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RequestOptions {
    range: Option<RangeRequest>,
    gridlines: Option<bool>,
    include_hidden: Option<bool>,
    omit_sparse_pages: Option<bool>,
    single_page_sheets: Option<bool>,
    limits: RequestedLimits,
}

#[derive(Debug, Clone, Copy)]
struct EffectiveResourceLimits {
    image_bytes: u64,
    images: u64,
    font_bytes: u64,
}

#[derive(Debug)]
struct EffectiveOptions {
    render: RenderOptions,
    print: PrintOptions,
    resources: EffectiveResourceLimits,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkbookInspection<'a> {
    schema_version: u32,
    sheet_count: usize,
    sheets: Vec<SheetInspection<'a>>,
    embedded_images: u64,
    embedded_image_bytes: u64,
    font_pack_sha256: Option<&'a str>,
    font_faces: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SheetInspection<'a> {
    index: usize,
    name: &'a str,
    embedded_images: usize,
}

/// Return the immutable workbook input ceiling.
#[wasm_bindgen(js_name = maxInputBytes)]
pub fn max_input_bytes() -> usize {
    MAX_INPUT_BYTES
}

/// Return deterministic capability and hard-limit metadata as JSON.
#[wasm_bindgen(js_name = capabilitiesJson)]
pub fn capabilities_json() -> String {
    serde_json::json!({
        "schemaVersion": 1,
        "protocol": "rxls.render-worker.v1",
        "outputs": ["sheet-svg", "tile-svg", "page-svg", "page-png"],
        "limits": {
            "maxInputBytes": MAX_INPUT_BYTES,
            "maxImageBytes": MAX_IMAGE_BYTES,
            "maxImages": MAX_IMAGES,
            "maxFontBytes": MAX_FONT_BYTES,
            "maxRows": MAX_ROWS,
            "maxColumns": MAX_COLUMNS,
            "maxCells": MAX_CELLS,
            "maxConditionalRules": MAX_CONDITIONAL_RULES,
            "maxConditionalEvaluations": MAX_CONDITIONAL_EVALUATIONS,
            "maxDrawingObjects": MAX_DRAWING_OBJECTS,
            "maxMediaBytes": MAX_MEDIA_BYTES,
            "maxImageDimension": MAX_IMAGE_DIMENSION,
            "maxImagePixels": MAX_IMAGE_PIXELS,
            "maxDecodedMediaBytes": MAX_DECODED_MEDIA_BYTES,
            "maxChartSeries": MAX_CHART_SERIES,
            "maxChartPoints": MAX_CHART_POINTS,
            "maxTextBytes": MAX_TEXT_BYTES,
            "maxGlyphs": MAX_GLYPHS,
            "maxTextRuns": MAX_TEXT_RUNS,
            "maxTextLines": MAX_TEXT_LINES,
            "maxPathCommands": MAX_PATH_COMMANDS,
            "maxSceneNodes": MAX_SCENE_NODES,
            "maxDimensionRaw": MAX_DIMENSION_RAW,
            "maxOutputBytes": MAX_OUTPUT_BYTES,
            "maxSheets": MAX_SHEETS,
            "maxLogicalPages": MAX_LOGICAL_PAGES,
            "maxPages": MAX_PAGES,
            "maxTotalSceneNodes": MAX_TOTAL_SCENE_NODES,
            "maxBackendCommands": MAX_BACKEND_COMMANDS,
            "maxRasterDimension": MAX_RASTER_DIMENSION,
            "maxRasterPixels": MAX_RASTER_PIXELS,
            "maxPngBytes": MAX_PNG_BYTES,
            "minDpi": MIN_DPI,
            "maxDpi": MAX_DPI,
        },
        "fontUploads": {
            "supported": true,
            "verified": true,
            "maxBytes": MAX_FONT_BYTES,
            "bundleSchema": "rxls.font-bundle.v1",
        },
        "embeddedImages": {"bounded": true, "painted": true},
    })
    .to_string()
}

/// Parsed workbook plus an optional verified, filesystem-free font pack.
///
/// A worker keeps one session per open document, avoiding repeated workbook
/// parsing and font verification for virtual page and tile requests.
#[wasm_bindgen]
pub struct RenderSession {
    workbook: Workbook,
    font_pack: Option<FontPack>,
    font_pack_bytes: u64,
}

#[wasm_bindgen]
impl RenderSession {
    /// Parse one workbook and optional `rxls.font-bundle.v1` byte envelope.
    #[wasm_bindgen(constructor)]
    pub fn new(bytes: &[u8], font_bundle: &[u8]) -> Result<RenderSession, JsValue> {
        RenderSession::new_core(bytes, font_bundle).map_err(js_error)
    }

    /// Return bounded sheet and embedded-resource metadata.
    #[wasm_bindgen(js_name = inspectionJson)]
    pub fn inspection_json(&self) -> Result<String, JsValue> {
        self.inspection_json_core(&RequestOptions::default())
            .map_err(js_error)
    }

    /// Render a whole sheet or options-selected range as SVG.
    #[wasm_bindgen(js_name = renderSheetSvg)]
    pub fn render_sheet_svg(
        &self,
        sheet_index: usize,
        options_json: &str,
    ) -> Result<String, JsValue> {
        self.render_sheet_svg_core(sheet_index, options_json)
            .map_err(js_error)
    }

    /// Render one explicit rectangular tile as SVG.
    #[wasm_bindgen(js_name = renderTileSvg)]
    #[allow(clippy::too_many_arguments)]
    pub fn render_tile_svg(
        &self,
        sheet_index: usize,
        first_row: u32,
        first_col: u16,
        last_row: u32,
        last_col: u16,
        options_json: &str,
    ) -> Result<String, JsValue> {
        self.render_tile_svg_core(
            sheet_index,
            RangeRequest {
                first_row,
                first_col,
                last_row,
                last_col,
            },
            options_json,
        )
        .map_err(js_error)
    }

    /// Build the print/page map without serializing every page.
    #[wasm_bindgen(js_name = printManifestJson)]
    pub fn print_manifest_json(
        &self,
        sheet_index: usize,
        options_json: &str,
    ) -> Result<String, JsValue> {
        self.print_manifest_json_core(sheet_index, options_json)
            .map_err(js_error)
    }

    /// Render exactly one print page as SVG.
    #[wasm_bindgen(js_name = renderPrintPageSvg)]
    pub fn render_print_page_svg(
        &self,
        sheet_index: usize,
        page_index: usize,
        options_json: &str,
    ) -> Result<String, JsValue> {
        self.render_print_page_svg_core(sheet_index, page_index, options_json)
            .map_err(js_error)
    }

    /// Render exactly one print page as PNG.
    #[wasm_bindgen(js_name = renderPrintPagePng)]
    pub fn render_print_page_png(
        &self,
        sheet_index: usize,
        page_index: usize,
        dpi: u32,
        options_json: &str,
    ) -> Result<Vec<u8>, JsValue> {
        self.render_print_page_png_core(sheet_index, page_index, dpi, options_json)
            .map_err(js_error)
    }
}

/// Parse workbook bytes and return bounded sheet metadata as JSON.
#[wasm_bindgen(js_name = inspectWorkbook)]
pub fn inspect_workbook(bytes: &[u8]) -> Result<String, JsValue> {
    RenderSession::new_core(bytes, &[])
        .and_then(|session| session.inspection_json_core(&RequestOptions::default()))
        .map_err(js_error)
}

/// Render a whole sheet or an options-selected range as SVG.
#[wasm_bindgen(js_name = renderSheetSvg)]
pub fn render_sheet_svg_wasm(
    bytes: &[u8],
    sheet_index: usize,
    options_json: &str,
) -> Result<String, JsValue> {
    render_sheet_svg_core(bytes, sheet_index, options_json).map_err(js_error)
}

/// Render one explicit rectangular tile as SVG.
#[wasm_bindgen(js_name = renderTileSvg)]
#[allow(clippy::too_many_arguments)]
pub fn render_tile_svg(
    bytes: &[u8],
    sheet_index: usize,
    first_row: u32,
    first_col: u16,
    last_row: u32,
    last_col: u16,
    options_json: &str,
) -> Result<String, JsValue> {
    let range = RangeRequest {
        first_row,
        first_col,
        last_row,
        last_col,
    };
    render_tile_svg_core(bytes, sheet_index, range, options_json).map_err(js_error)
}

/// Build the path-neutral print/page map as JSON without serializing pages.
#[wasm_bindgen(js_name = printManifestJson)]
pub fn print_manifest_json(
    bytes: &[u8],
    sheet_index: usize,
    options_json: &str,
) -> Result<String, JsValue> {
    print_manifest_json_core(bytes, sheet_index, options_json).map_err(js_error)
}

/// Render exactly one print page as SVG.
#[wasm_bindgen(js_name = renderPrintPageSvg)]
pub fn render_print_page_svg(
    bytes: &[u8],
    sheet_index: usize,
    page_index: usize,
    options_json: &str,
) -> Result<String, JsValue> {
    render_print_page_svg_core(bytes, sheet_index, page_index, options_json).map_err(js_error)
}

/// Rasterize exactly one print page as a PNG byte buffer.
///
/// PNG requires outlined text. This stateless convenience function has no font
/// bundle argument, so text-bearing pages return the renderer's stable
/// `png_requires_outlined_text` error. Worker clients use [`RenderSession`]
/// with a verified in-memory pack for PNG output.
#[wasm_bindgen(js_name = renderPrintPagePng)]
pub fn render_print_page_png_wasm(
    bytes: &[u8],
    sheet_index: usize,
    page_index: usize,
    dpi: u32,
    options_json: &str,
) -> Result<Vec<u8>, JsValue> {
    render_print_page_png_core(bytes, sheet_index, page_index, dpi, options_json).map_err(js_error)
}

#[cfg(test)]
fn inspect_workbook_core(bytes: &[u8], options: &RequestOptions) -> Result<String, FacadeError> {
    RenderSession::new_core(bytes, &[])?.inspection_json_core(options)
}

fn render_sheet_svg_core(
    bytes: &[u8],
    sheet_index: usize,
    options_json: &str,
) -> Result<String, FacadeError> {
    RenderSession::new_core(bytes, &[])?.render_sheet_svg_core(sheet_index, options_json)
}

fn render_tile_svg_core(
    bytes: &[u8],
    sheet_index: usize,
    range: RangeRequest,
    options_json: &str,
) -> Result<String, FacadeError> {
    RenderSession::new_core(bytes, &[])?.render_tile_svg_core(sheet_index, range, options_json)
}

fn print_manifest_json_core(
    bytes: &[u8],
    sheet_index: usize,
    options_json: &str,
) -> Result<String, FacadeError> {
    RenderSession::new_core(bytes, &[])?.print_manifest_json_core(sheet_index, options_json)
}

fn render_print_page_svg_core(
    bytes: &[u8],
    sheet_index: usize,
    page_index: usize,
    options_json: &str,
) -> Result<String, FacadeError> {
    RenderSession::new_core(bytes, &[])?.render_print_page_svg_core(
        sheet_index,
        page_index,
        options_json,
    )
}

fn render_print_page_png_core(
    bytes: &[u8],
    sheet_index: usize,
    page_index: usize,
    dpi: u32,
    options_json: &str,
) -> Result<Vec<u8>, FacadeError> {
    RenderSession::new_core(bytes, &[])?.render_print_page_png_core(
        sheet_index,
        page_index,
        dpi,
        options_json,
    )
}

impl RenderSession {
    fn new_core(bytes: &[u8], font_bundle: &[u8]) -> Result<Self, FacadeError> {
        check_input(bytes)?;
        let (font_pack, font_pack_bytes) = load_font_bundle(font_bundle)?;
        let workbook = parse_workbook(bytes)?;
        if workbook.sheets.len() as u64 > MAX_SHEETS {
            return Err(FacadeError::limit(
                "sheets",
                MAX_SHEETS,
                workbook.sheets.len() as u64,
            ));
        }
        check_embedded_images(
            &workbook,
            EffectiveResourceLimits {
                image_bytes: MAX_IMAGE_BYTES,
                images: MAX_IMAGES,
                font_bytes: MAX_FONT_BYTES,
            },
        )?;
        Ok(Self {
            workbook,
            font_pack,
            font_pack_bytes,
        })
    }

    fn inspection_json_core(&self, options: &RequestOptions) -> Result<String, FacadeError> {
        let effective = effective_options(options, self.font_pack.as_ref())?;
        check_font_bytes(self.font_pack_bytes, effective.resources.font_bytes)?;
        let (image_count, image_bytes) =
            check_embedded_images(&self.workbook, effective.resources)?;
        let inspection = WorkbookInspection {
            schema_version: 1,
            sheet_count: self.workbook.sheets.len(),
            sheets: self
                .workbook
                .sheets
                .iter()
                .enumerate()
                .map(|(index, sheet)| SheetInspection {
                    index,
                    name: &sheet.name,
                    embedded_images: sheet.images().len(),
                })
                .collect(),
            embedded_images: image_count,
            embedded_image_bytes: image_bytes,
            font_pack_sha256: self.font_pack.as_ref().map(FontPack::pack_sha256),
            font_faces: self.font_pack.as_ref().map_or(0, FontPack::font_count),
        };
        let output = serde_json::to_string(&inspection).map_err(|_| {
            FacadeError::simple(
                "serialization_failed",
                "workbook inspection could not be serialized",
                "output",
            )
        })?;
        enforce_output(output.len(), effective.render.limits.max_output_bytes)?;
        Ok(output)
    }

    fn render_sheet_svg_core(
        &self,
        sheet_index: usize,
        options_json: &str,
    ) -> Result<String, FacadeError> {
        let request = parse_options(options_json)?;
        let effective = effective_options(&request, self.font_pack.as_ref())?;
        check_font_bytes(self.font_pack_bytes, effective.resources.font_bytes)?;
        check_embedded_images(&self.workbook, effective.resources)?;
        render_sheet_svg(&self.workbook, sheet_index, &effective.render)
            .map(|output| output.svg)
            .map_err(map_render_error)
    }

    fn render_tile_svg_core(
        &self,
        sheet_index: usize,
        range: RangeRequest,
        options_json: &str,
    ) -> Result<String, FacadeError> {
        let mut request = parse_options(options_json)?;
        if request.range.is_some() {
            return Err(FacadeError::simple(
                "conflicting_range",
                "tile coordinates and options.range cannot both be supplied",
                "options.range",
            ));
        }
        request.range = Some(range);
        let effective = effective_options(&request, self.font_pack.as_ref())?;
        check_font_bytes(self.font_pack_bytes, effective.resources.font_bytes)?;
        check_embedded_images(&self.workbook, effective.resources)?;
        render_sheet_svg(&self.workbook, sheet_index, &effective.render)
            .map(|output| output.svg)
            .map_err(map_render_error)
    }

    fn print_manifest_json_core(
        &self,
        sheet_index: usize,
        options_json: &str,
    ) -> Result<String, FacadeError> {
        let (prepared, output_limit) = self.prepare_document(sheet_index, options_json)?;
        let report = prepared.report.to_json();
        enforce_output(report.len(), output_limit)?;
        Ok(report)
    }

    fn render_print_page_svg_core(
        &self,
        sheet_index: usize,
        page_index: usize,
        options_json: &str,
    ) -> Result<String, FacadeError> {
        let (prepared, output_limit) = self.prepare_document(sheet_index, options_json)?;
        check_page_index(&prepared, page_index)?;
        let page =
            build_print_page(&self.workbook, &prepared, page_index).map_err(map_render_error)?;
        render_scene_svg(&page.scene, output_limit).map_err(map_render_error)
    }

    fn render_print_page_png_core(
        &self,
        sheet_index: usize,
        page_index: usize,
        dpi: u32,
        options_json: &str,
    ) -> Result<Vec<u8>, FacadeError> {
        if !(MIN_DPI..=MAX_DPI).contains(&dpi) {
            return Err(FacadeError::simple(
                "dpi_out_of_range",
                format!("dpi must be between {MIN_DPI} and {MAX_DPI}"),
                "dpi",
            ));
        }
        let (prepared, _) = self.prepare_document(sheet_index, options_json)?;
        check_page_index(&prepared, page_index)?;
        let page =
            build_print_page(&self.workbook, &prepared, page_index).map_err(map_render_error)?;
        let document = PrintDocument {
            pages: vec![page],
            report: prepared.report,
            limits: prepared.limits,
        };
        render_print_page_png(&document.pages[0], dpi, &document).map_err(map_render_error)
    }

    fn prepare_document(
        &self,
        sheet_index: usize,
        options_json: &str,
    ) -> Result<(PreparedPrintDocument, u64), FacadeError> {
        let request = parse_options(options_json)?;
        let effective = effective_options(&request, self.font_pack.as_ref())?;
        check_font_bytes(self.font_pack_bytes, effective.resources.font_bytes)?;
        let output_limit = effective.render.limits.max_output_bytes;
        check_embedded_images(&self.workbook, effective.resources)?;
        prepare_print_document(&self.workbook, sheet_index, &effective.print)
            .map(|prepared| (prepared, output_limit))
            .map_err(map_render_error)
    }
}

fn check_page_index(
    prepared: &PreparedPrintDocument,
    page_index: usize,
) -> Result<(), FacadeError> {
    prepared
        .report
        .pages
        .get(page_index)
        .map(|_| ())
        .ok_or_else(|| {
            FacadeError::simple(
                "page_index_out_of_range",
                format!(
                    "page index {page_index} is out of range for {} pages",
                    prepared.report.pages.len()
                ),
                "pageIndex",
            )
        })
}

fn load_font_bundle(font_bundle: &[u8]) -> Result<(Option<FontPack>, u64), FacadeError> {
    if font_bundle.is_empty() {
        return Ok((None, 0));
    }
    if font_bundle.len() as u64 > MAX_FONT_BUNDLE_BYTES {
        return Err(FacadeError::limit(
            "fontBundleBytes",
            MAX_FONT_BUNDLE_BYTES,
            font_bundle.len() as u64,
        ));
    }
    let mut cursor = BundleCursor::new(font_bundle);
    if cursor.take(FONT_BUNDLE_MAGIC.len())? != FONT_BUNDLE_MAGIC {
        return Err(invalid_font_bundle());
    }
    let manifest_len = cursor.read_u32()? as usize;
    if manifest_len as u64 > MAX_FONT_MANIFEST_BYTES {
        return Err(FacadeError::limit(
            "fontManifestBytes",
            MAX_FONT_MANIFEST_BYTES,
            manifest_len as u64,
        ));
    }
    let manifest = cursor.take(manifest_len)?.to_vec();
    let member_count = cursor.read_u32()? as u64;
    if member_count > MAX_FONT_FILES {
        return Err(FacadeError::limit(
            "fontFiles",
            MAX_FONT_FILES,
            member_count,
        ));
    }
    let mut members = Vec::with_capacity(member_count as usize);
    let mut payload_bytes = manifest_len as u64;
    for _ in 0..member_count {
        let name_len = cursor.read_u32()? as usize;
        if name_len == 0 || name_len > 4_096 {
            return Err(invalid_font_bundle());
        }
        let name = std::str::from_utf8(cursor.take(name_len)?)
            .map_err(|_| invalid_font_bundle())?
            .to_owned();
        let member_len = cursor.read_u32()? as usize;
        if member_len as u64 > MAX_FONT_FILE_BYTES {
            return Err(FacadeError::limit(
                "fontMemberBytes",
                MAX_FONT_FILE_BYTES,
                member_len as u64,
            ));
        }
        payload_bytes = payload_bytes
            .checked_add(member_len as u64)
            .ok_or_else(|| FacadeError::limit("fontBytes", MAX_FONT_BYTES, u64::MAX))?;
        if payload_bytes > MAX_FONT_BYTES {
            return Err(FacadeError::limit(
                "fontBytes",
                MAX_FONT_BYTES,
                payload_bytes,
            ));
        }
        members.push(FontPackMember::new(name, cursor.take(member_len)?.to_vec()));
    }
    if !cursor.is_empty() {
        return Err(invalid_font_bundle());
    }
    let limits = FontPackLimits {
        max_manifest_bytes: MAX_FONT_MANIFEST_BYTES,
        max_fonts: 128,
        max_font_bytes: MAX_FONT_FILE_BYTES,
        max_total_bytes: MAX_FONT_BYTES,
        max_auxiliary_bytes: MAX_FONT_AUXILIARY_BYTES,
        max_files: MAX_FONT_FILES,
        max_directory_depth: 16,
        max_outline_commands_per_glyph: 16_384,
    };
    FontPack::load_memory_with_limits(&manifest, members, limits)
        .map(|pack| (Some(pack), payload_bytes))
        .map_err(map_font_error)
}

#[derive(Debug)]
struct BundleCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> BundleCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], FacadeError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(invalid_font_bundle)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(invalid_font_bundle)?;
        self.offset = end;
        Ok(value)
    }

    fn read_u32(&mut self) -> Result<u32, FacadeError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| invalid_font_bundle())?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

fn invalid_font_bundle() -> FacadeError {
    FacadeError::simple(
        "invalid_font_bundle",
        "font bundle is malformed or truncated",
        "fontPack",
    )
}

fn map_font_error(error: FontPackError) -> FacadeError {
    match error {
        FontPackError::LimitExceeded {
            resource,
            limit,
            actual,
        } => FacadeError::limit(resource, limit, actual),
        FontPackError::UnsafePath => FacadeError::simple(
            "unsafe_font_path",
            "font pack contains an unsafe member name",
            "fontPack",
        ),
        _ => FacadeError::simple(
            "invalid_font_pack",
            "font pack failed manifest, digest, license, or OpenType validation",
            "fontPack",
        ),
    }
}

fn parse_options(options_json: &str) -> Result<RequestOptions, FacadeError> {
    if options_json.trim().is_empty() {
        return Ok(RequestOptions::default());
    }
    if options_json.len() > 64 * 1024 {
        return Err(FacadeError::limit(
            "options_bytes",
            64 * 1024,
            options_json.len() as u64,
        ));
    }
    serde_json::from_str(options_json).map_err(|_| {
        FacadeError::simple(
            "invalid_options",
            "render options JSON is invalid or contains an unknown field",
            "options",
        )
    })
}

fn effective_options(
    request: &RequestOptions,
    font_pack: Option<&FontPack>,
) -> Result<EffectiveOptions, FacadeError> {
    let limits = &request.limits;
    let render_limits = RenderLimits {
        max_rows: requested_u64("maxRows", limits.max_rows, MAX_ROWS)?,
        max_columns: requested_u64("maxColumns", limits.max_columns, MAX_COLUMNS)?,
        max_cells: requested_u64("maxCells", limits.max_cells, MAX_CELLS)?,
        max_conditional_rules: requested_u64(
            "maxConditionalRules",
            limits.max_conditional_rules,
            MAX_CONDITIONAL_RULES,
        )?,
        max_conditional_evaluations: requested_u64(
            "maxConditionalEvaluations",
            limits.max_conditional_evaluations,
            MAX_CONDITIONAL_EVALUATIONS,
        )?,
        max_drawing_objects: requested_u64(
            "maxDrawingObjects",
            limits.max_drawing_objects,
            MAX_DRAWING_OBJECTS,
        )?,
        max_media_bytes: requested_u64("maxMediaBytes", limits.max_media_bytes, MAX_MEDIA_BYTES)?,
        max_image_dimension: requested_u64(
            "maxImageDimension",
            limits.max_image_dimension,
            MAX_IMAGE_DIMENSION,
        )?,
        max_image_pixels: requested_u64(
            "maxImagePixels",
            limits.max_image_pixels,
            MAX_IMAGE_PIXELS,
        )?,
        max_decoded_media_bytes: requested_u64(
            "maxDecodedMediaBytes",
            limits.max_decoded_media_bytes,
            MAX_DECODED_MEDIA_BYTES,
        )?,
        max_chart_series: requested_u64(
            "maxChartSeries",
            limits.max_chart_series,
            MAX_CHART_SERIES,
        )?,
        max_chart_points: requested_u64(
            "maxChartPoints",
            limits.max_chart_points,
            MAX_CHART_POINTS,
        )?,
        max_text_bytes: requested_u64("maxTextBytes", limits.max_text_bytes, MAX_TEXT_BYTES)?,
        max_glyphs: requested_u64("maxGlyphs", limits.max_glyphs, MAX_GLYPHS)?,
        max_text_runs: requested_u64("maxTextRuns", limits.max_text_runs, MAX_TEXT_RUNS)?,
        max_text_lines: requested_u64("maxTextLines", limits.max_text_lines, MAX_TEXT_LINES)?,
        max_path_commands: requested_u64(
            "maxPathCommands",
            limits.max_path_commands,
            MAX_PATH_COMMANDS,
        )?,
        max_scene_nodes: requested_u64("maxSceneNodes", limits.max_scene_nodes, MAX_SCENE_NODES)?,
        max_dimension_raw: requested_u64(
            "maxDimensionRaw",
            limits.max_dimension_raw,
            MAX_DIMENSION_RAW,
        )?,
        max_output_bytes: requested_u64(
            "maxOutputBytes",
            limits.max_output_bytes,
            MAX_OUTPUT_BYTES,
        )?,
    };
    let print_limits = PrintLimits {
        max_logical_pages: requested_u64(
            "maxLogicalPages",
            limits.max_logical_pages,
            MAX_LOGICAL_PAGES,
        )?,
        max_pages: requested_u64("maxPages", limits.max_pages, MAX_PAGES)?,
        max_total_scene_nodes: requested_u64(
            "maxTotalSceneNodes",
            limits.max_total_scene_nodes,
            MAX_TOTAL_SCENE_NODES,
        )?,
        max_backend_commands: requested_u64(
            "maxBackendCommands",
            limits.max_backend_commands,
            MAX_BACKEND_COMMANDS,
        )?,
        max_pdf_bytes: MAX_OUTPUT_BYTES,
        max_raster_dimension: requested_u32(
            "maxRasterDimension",
            limits.max_raster_dimension,
            MAX_RASTER_DIMENSION,
        )?,
        max_raster_pixels: requested_u64(
            "maxRasterPixels",
            limits.max_raster_pixels,
            MAX_RASTER_PIXELS,
        )?,
        max_png_bytes_per_page: requested_u64("maxPngBytes", limits.max_png_bytes, MAX_PNG_BYTES)?,
    };
    let font_bytes =
        requested_allow_zero_u64("maxFontBytes", limits.max_font_bytes, MAX_FONT_BYTES)?;
    let resources = EffectiveResourceLimits {
        image_bytes: requested_allow_zero_u64(
            "maxImageBytes",
            limits.max_image_bytes,
            MAX_IMAGE_BYTES,
        )?,
        images: requested_allow_zero_u64("maxImages", limits.max_images, MAX_IMAGES)?,
        font_bytes,
    };
    let mut render = RenderOptions {
        limits: render_limits,
        ..RenderOptions::default()
    };
    if let Some(range) = request.range {
        render.selection = RenderSelection::Range(range.into());
    }
    if let Some(gridlines) = request.gridlines {
        render.gridlines = gridlines;
    }
    if let Some(include_hidden) = request.include_hidden {
        render.include_hidden = include_hidden;
    }
    // Only a verified in-memory pack can populate this field. Host font
    // discovery is never attempted in WebAssembly.
    render.font_pack = font_pack.cloned();
    let print = PrintOptions {
        render: render.clone(),
        omit_sparse_pages: request.omit_sparse_pages.unwrap_or(true),
        single_page_sheets: request.single_page_sheets.unwrap_or(false),
        limits: print_limits,
    };
    Ok(EffectiveOptions {
        render,
        print,
        resources,
    })
}

fn requested_u64(
    resource: &'static str,
    requested: Option<u64>,
    hard_max: u64,
) -> Result<u64, FacadeError> {
    let value = requested.unwrap_or(hard_max);
    if value == 0 || value > hard_max {
        return Err(FacadeError::limit(resource, hard_max, value));
    }
    Ok(value)
}

fn requested_allow_zero_u64(
    resource: &'static str,
    requested: Option<u64>,
    hard_max: u64,
) -> Result<u64, FacadeError> {
    let value = requested.unwrap_or(hard_max);
    if value > hard_max {
        return Err(FacadeError::limit(resource, hard_max, value));
    }
    Ok(value)
}

fn requested_u32(
    resource: &'static str,
    requested: Option<u32>,
    hard_max: u32,
) -> Result<u32, FacadeError> {
    let value = requested.unwrap_or(hard_max);
    if value == 0 || value > hard_max {
        return Err(FacadeError::limit(
            resource,
            u64::from(hard_max),
            u64::from(value),
        ));
    }
    Ok(value)
}

fn check_input(bytes: &[u8]) -> Result<(), FacadeError> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(FacadeError::limit(
            "inputBytes",
            MAX_INPUT_BYTES as u64,
            bytes.len() as u64,
        ));
    }
    Ok(())
}

fn parse_workbook(bytes: &[u8]) -> Result<Workbook, FacadeError> {
    Workbook::open(bytes).map_err(|_| {
        FacadeError::simple(
            "parse_failed",
            "spreadsheet input is malformed, encrypted, unsupported, or over budget",
            "input",
        )
    })
}

fn check_embedded_images(
    workbook: &Workbook,
    limits: EffectiveResourceLimits,
) -> Result<(u64, u64), FacadeError> {
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    for sheet in &workbook.sheets {
        count = count.saturating_add(sheet.images().len() as u64);
        if count > limits.images {
            return Err(FacadeError::limit("embeddedImages", limits.images, count));
        }
        for image in sheet.images() {
            bytes = bytes.saturating_add(image.data.len() as u64);
            if bytes > limits.image_bytes {
                return Err(FacadeError::limit(
                    "embeddedImageBytes",
                    limits.image_bytes,
                    bytes,
                ));
            }
        }
    }
    Ok((count, bytes))
}

fn check_font_bytes(actual: u64, limit: u64) -> Result<(), FacadeError> {
    if actual > limit {
        return Err(FacadeError::limit("fontBytes", limit, actual));
    }
    Ok(())
}

fn enforce_output(actual: usize, limit: u64) -> Result<(), FacadeError> {
    if actual as u64 > limit {
        return Err(FacadeError::limit("outputBytes", limit, actual as u64));
    }
    Ok(())
}

fn map_render_error(error: RenderError) -> FacadeError {
    match error {
        RenderError::SheetIndexOutOfRange {
            requested,
            sheet_count,
        } => FacadeError::simple(
            "sheet_index_out_of_range",
            format!("sheet index {requested} is out of range for {sheet_count} sheets"),
            "sheetIndex",
        ),
        RenderError::InvalidRange { .. } => {
            FacadeError::simple("invalid_range", "render range is reversed", "range")
        }
        RenderError::RangeOutsideGrid { .. } => FacadeError::simple(
            "range_outside_grid",
            "render range exceeds the spreadsheet grid",
            "range",
        ),
        RenderError::LimitExceeded {
            kind,
            limit,
            actual,
        } => FacadeError::limit(limit_resource(kind), limit, actual),
        RenderError::CoordinateOverflow => FacadeError::simple(
            "coordinate_overflow",
            "render coordinate arithmetic overflowed",
            "layout",
        ),
        RenderError::Typography { reason } => FacadeError::simple(
            "typography_failed",
            format!("typography backend rejected input: {reason}"),
            "typography",
        ),
        RenderError::Backend { reason } => FacadeError::simple(
            "backend_failed",
            format!("output backend rejected input: {reason}"),
            "backend",
        ),
    }
}

fn limit_resource(kind: LimitKind) -> &'static str {
    kind.code()
}

fn js_error(error: FacadeError) -> JsValue {
    let js_error = js_sys::Error::new(&error.message);
    js_error.set_name("RxlsRenderError");
    let object: &JsValue = js_error.as_ref();
    set_property(object, "code", &JsValue::from_str(error.code));
    set_property(object, "location", &JsValue::from_str(error.location));
    set_property(
        object,
        "resource",
        &error.resource.map_or(JsValue::NULL, JsValue::from_str),
    );
    set_property(
        object,
        "limit",
        &error
            .limit
            .map_or(JsValue::NULL, |value| JsValue::from_f64(value as f64)),
    );
    set_property(
        object,
        "actual",
        &error
            .actual
            .map_or(JsValue::NULL, |value| JsValue::from_f64(value as f64)),
    );
    js_error.into()
}

fn set_property(object: &JsValue, name: &str, value: &JsValue) {
    let _ = js_sys::Reflect::set(object, &JsValue::from_str(name), value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authored_workbook() -> Vec<u8> {
        let mut workbook = Workbook::new();
        workbook.add_sheet("한글 Sheet").write(0, 0, "hello");
        workbook.to_xlsx()
    }

    #[test]
    fn capabilities_are_stable_and_enable_verified_memory_fonts() {
        let capabilities: serde_json::Value =
            serde_json::from_str(&capabilities_json()).expect("capabilities JSON");
        assert_eq!(capabilities["schemaVersion"], 1);
        assert_eq!(capabilities["protocol"], "rxls.render-worker.v1");
        assert_eq!(capabilities["limits"]["maxInputBytes"], MAX_INPUT_BYTES);
        assert_eq!(capabilities["limits"]["maxSheets"], MAX_SHEETS);
        assert_eq!(capabilities["fontUploads"]["supported"], true);
        assert_eq!(capabilities["fontUploads"]["maxBytes"], MAX_FONT_BYTES);
        assert_eq!(capabilities["embeddedImages"]["painted"], true);
    }

    #[test]
    fn options_reject_unknown_fields_and_limit_increases() {
        assert_eq!(
            parse_options(r#"{"hostPath":"/tmp/private"}"#)
                .unwrap_err()
                .code,
            "invalid_options"
        );
        let request = parse_options(r#"{"limits":{"maxCells":250001}}"#).unwrap();
        let error = effective_options(&request, None).unwrap_err();
        assert_eq!(error.code, "limit_exceeded");
        assert_eq!(error.resource, Some("maxCells"));
        assert_eq!(error.limit, Some(MAX_CELLS));
        assert_eq!(error.actual, Some(MAX_CELLS + 1));

        let request = parse_options(r#"{"limits":{"maxChartPoints":250001}}"#).unwrap();
        let error = effective_options(&request, None).unwrap_err();
        assert_eq!(error.resource, Some("maxChartPoints"));
    }

    #[test]
    fn options_allow_stricter_resource_policies() {
        let request =
            parse_options(r#"{"limits":{"maxImages":0,"maxImageBytes":0,"maxFontBytes":0}}"#)
                .unwrap();
        let effective = effective_options(&request, None).unwrap();
        assert_eq!(effective.resources.images, 0);
        assert_eq!(effective.resources.image_bytes, 0);

        assert_eq!(effective.resources.font_bytes, 0);

        let request = parse_options(r#"{"limits":{"maxFontBytes":67108865}}"#).unwrap();
        assert_eq!(
            effective_options(&request, None).unwrap_err().resource,
            Some("maxFontBytes")
        );
        assert_eq!(
            check_font_bytes(2, 1).unwrap_err().resource,
            Some("fontBytes")
        );
    }

    #[test]
    fn inspection_obeys_the_requested_output_ceiling() {
        let session = RenderSession::new_core(&authored_workbook(), &[]).unwrap();
        let options = parse_options(r#"{"limits":{"maxOutputBytes":1}}"#).unwrap();
        let error = session.inspection_json_core(&options).unwrap_err();
        assert_eq!(error.code, "limit_exceeded");
        assert_eq!(error.resource, Some("outputBytes"));
    }

    #[test]
    fn font_bundle_envelope_rejects_bad_magic_and_truncation() {
        assert_eq!(
            load_font_bundle(b"not-a-font-bundle").unwrap_err().code,
            "invalid_font_bundle"
        );
        let mut truncated = FONT_BUNDLE_MAGIC.to_vec();
        truncated.extend_from_slice(&8_u32.to_le_bytes());
        assert_eq!(
            load_font_bundle(&truncated).unwrap_err().code,
            "invalid_font_bundle"
        );
    }

    #[test]
    fn native_core_inspects_and_renders_sheet_and_tile() {
        let bytes = authored_workbook();
        let inspection: serde_json::Value = serde_json::from_str(
            &inspect_workbook_core(&bytes, &RequestOptions::default()).unwrap(),
        )
        .unwrap();
        assert_eq!(inspection["sheetCount"], 1);
        assert_eq!(inspection["sheets"][0]["name"], "한글 Sheet");

        let sheet = render_sheet_svg_core(&bytes, 0, "{}").unwrap();
        assert!(sheet.contains("<svg"));
        assert!(sheet.contains("hello"));
        let tile = render_tile_svg_core(
            &bytes,
            0,
            RangeRequest {
                first_row: 0,
                first_col: 0,
                last_row: 0,
                last_col: 0,
            },
            "{}",
        )
        .unwrap();
        assert_eq!(sheet, tile);
    }

    #[test]
    fn native_core_builds_page_manifest_and_one_page_svg() {
        let bytes = authored_workbook();
        let manifest: serde_json::Value =
            serde_json::from_str(&print_manifest_json_core(&bytes, 0, "{}").unwrap()).unwrap();
        assert_eq!(manifest["schema_version"], 2);
        assert_eq!(manifest["pages"].as_array().unwrap().len(), 1);
        let svg = render_print_page_svg_core(&bytes, 0, 0, "{}").unwrap();
        assert!(svg.contains("<svg"));
        assert_eq!(
            render_print_page_svg_core(&bytes, 0, 1, "{}")
                .unwrap_err()
                .code,
            "page_index_out_of_range"
        );
    }

    #[test]
    fn errors_are_path_neutral() {
        let error =
            render_sheet_svg_core(b"not a workbook", 0, r#"{"gridlines":true}"#).unwrap_err();
        assert_eq!(error.code, "parse_failed");
        assert!(!error.message.contains('/'));
        assert!(!error.message.contains('\\'));
        assert!(!error.message.contains("Users"));
    }

    #[test]
    fn browser_caps_are_stricly_below_or_equal_to_native_defaults() {
        let native = RenderLimits::default();
        assert!(MAX_ROWS <= native.max_rows);
        assert!(MAX_COLUMNS <= native.max_columns);
        assert!(MAX_CELLS <= native.max_cells);
        assert!(MAX_CONDITIONAL_RULES <= native.max_conditional_rules);
        assert!(MAX_CONDITIONAL_EVALUATIONS <= native.max_conditional_evaluations);
        assert!(MAX_DRAWING_OBJECTS <= native.max_drawing_objects);
        assert!(MAX_MEDIA_BYTES <= native.max_media_bytes);
        assert!(MAX_IMAGE_DIMENSION <= native.max_image_dimension);
        assert!(MAX_IMAGE_PIXELS <= native.max_image_pixels);
        assert!(MAX_DECODED_MEDIA_BYTES <= native.max_decoded_media_bytes);
        assert!(MAX_CHART_SERIES <= native.max_chart_series);
        assert!(MAX_CHART_POINTS <= native.max_chart_points);
        assert!(MAX_TEXT_BYTES <= native.max_text_bytes);
        assert!(MAX_SCENE_NODES <= native.max_scene_nodes);
        assert!(MAX_OUTPUT_BYTES <= native.max_output_bytes);
        assert!(MAX_DIMENSION_RAW <= native.max_dimension_raw);
        assert_eq!(rxls_render::Fixed::from_pixels(1).raw(), 1_024);
    }
}
