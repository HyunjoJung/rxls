//! Bounded WebAssembly facade for the standalone `rxls-render` engine.
//!
//! The facade is intentionally synchronous. The JavaScript package runs it in
//! a dedicated module worker, so parsing and layout never block the browser's
//! main thread. Every request is capped below the native renderer defaults.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use rxls::{Cell, DocProperties, EditCapability, EditReadOnlyReason, Spreadsheet, Workbook};
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
const MAX_EDIT_HISTORY_ENTRIES: usize = 20;
const MAX_EDIT_HISTORY_BYTES: usize = MAX_INPUT_BYTES;

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
    properties: DocumentPropertiesInspection<'a>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SheetInspection<'a> {
    index: usize,
    name: &'a str,
    embedded_images: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentPropertiesInspection<'a> {
    title: Option<&'a str>,
    subject: Option<&'a str>,
    creator: Option<&'a str>,
    keywords: Option<&'a str>,
    description: Option<&'a str>,
    last_modified_by: Option<&'a str>,
    company: Option<&'a str>,
    created: Option<&'a str>,
}

impl<'a> From<&'a DocProperties> for DocumentPropertiesInspection<'a> {
    fn from(properties: &'a DocProperties) -> Self {
        Self {
            title: properties.title.as_deref(),
            subject: properties.subject.as_deref(),
            creator: properties.creator.as_deref(),
            keywords: properties.keywords.as_deref(),
            description: properties.description.as_deref(),
            last_modified_by: properties.last_modified_by.as_deref(),
            company: properties.company.as_deref(),
            created: properties.created.as_deref(),
        }
    }
}

#[derive(Debug, Clone)]
struct EditSnapshot {
    bytes: Vec<u8>,
    dirty: bool,
    edited_parts: Vec<String>,
}

struct EditStateView<'a> {
    dirty: bool,
    undo_depth: usize,
    redo_depth: usize,
    history_bytes: usize,
    edited_parts: &'a [String],
}

#[derive(Debug, Clone, Copy)]
enum HistorySide {
    Undo,
    Redo,
}

#[derive(Debug, PartialEq, Eq)]
struct HistoryProjection {
    undo_drop: usize,
    redo_drop: usize,
    undo_depth: usize,
    redo_depth: usize,
    bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CellEditRequest {
    sheet_index: usize,
    row: u32,
    col: u16,
    value: EditableCell,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DocumentPropertiesEditRequest {
    title: RequiredNullableString,
    subject: RequiredNullableString,
    creator: RequiredNullableString,
    keywords: RequiredNullableString,
    description: RequiredNullableString,
    last_modified_by: RequiredNullableString,
    company: RequiredNullableString,
    created: RequiredNullableString,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RequiredNullableString {
    String(String),
    Null,
}

impl RequiredNullableString {
    fn into_option(self) -> Option<String> {
        match self {
            Self::String(value) => Some(value),
            Self::Null => None,
        }
    }
}

impl From<DocumentPropertiesEditRequest> for DocProperties {
    fn from(request: DocumentPropertiesEditRequest) -> Self {
        let mut properties = Self::new();
        properties.title = request.title.into_option();
        properties.subject = request.subject.into_option();
        properties.creator = request.creator.into_option();
        properties.keywords = request.keywords.into_option();
        properties.description = request.description.into_option();
        properties.last_modified_by = request.last_modified_by.into_option();
        properties.company = request.company.into_option();
        properties.created = request.created.into_option();
        properties
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields, rename_all = "kebab-case")]
enum EditableCell {
    Blank,
    Text {
        value: String,
    },
    Number {
        value: f64,
    },
    Date {
        value: f64,
    },
    Boolean {
        value: bool,
    },
    Error {
        value: String,
    },
    Formula {
        formula: String,
        cached: EditableCachedCell,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields, rename_all = "kebab-case")]
enum EditableCachedCell {
    Text { value: String },
    Number { value: f64 },
    Date { value: f64 },
    Boolean { value: bool },
    Error { value: String },
}

impl EditableCachedCell {
    fn into_cell(self) -> Cell {
        match self {
            Self::Text { value } => Cell::Text(value),
            Self::Number { value } => Cell::Number(value),
            Self::Date { value } => Cell::Date(value),
            Self::Boolean { value } => Cell::Bool(value),
            Self::Error { value } => Cell::Error(value),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum InspectedCell<'a> {
    Blank,
    Text {
        value: &'a str,
    },
    Number {
        value: f64,
    },
    Date {
        value: f64,
    },
    Boolean {
        value: bool,
    },
    Error {
        value: &'a str,
    },
    Formula {
        formula: &'a str,
        cached: Box<InspectedCell<'a>>,
    },
}

impl<'a> From<Option<&'a Cell>> for InspectedCell<'a> {
    fn from(cell: Option<&'a Cell>) -> Self {
        match cell {
            None => Self::Blank,
            Some(Cell::Text(value)) => Self::Text { value },
            Some(Cell::Number(value)) => Self::Number { value: *value },
            Some(Cell::Date(value)) => Self::Date { value: *value },
            Some(Cell::Bool(value)) => Self::Boolean { value: *value },
            Some(Cell::Error(value)) => Self::Error { value },
            Some(Cell::Formula { formula, cached }) => Self::Formula {
                formula,
                cached: Box::new(Self::from(Some(cached.as_ref()))),
            },
        }
    }
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
        "protocol": "rxls.render-worker.v2",
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
        "editing": {
            "supported": true,
            "formats": ["xlsx", "xlsm"],
            "preservation": "untouched-package-parts",
            "operations": [
                "read-cell",
                "set-cell",
                "set-document-properties",
                "undo-edit",
                "redo-edit",
                "save-document",
            ],
            "maxHistoryEntries": MAX_EDIT_HISTORY_ENTRIES,
            "maxHistoryBytes": MAX_EDIT_HISTORY_BYTES,
        },
    })
    .to_string()
}

/// Parsed workbook plus an optional verified, filesystem-free font pack.
///
/// A worker keeps one session per open document, avoiding repeated workbook
/// parsing and font verification for virtual page and tile requests.
#[wasm_bindgen]
pub struct RenderSession {
    spreadsheet: Spreadsheet,
    workbook: Workbook,
    font_pack: Option<FontPack>,
    font_pack_bytes: u64,
    undo: Vec<EditSnapshot>,
    redo: Vec<EditSnapshot>,
    dirty: bool,
    edited_parts: Vec<String>,
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

    /// Return typed edit capability, history, dirty state, and touched parts.
    #[wasm_bindgen(js_name = editStateJson)]
    pub fn edit_state_json(&self) -> String {
        self.edit_state_value().to_string()
    }

    /// Read one typed cell for an edit form without exposing the workbook object.
    #[wasm_bindgen(js_name = readCellJson)]
    pub fn read_cell_json(
        &self,
        sheet_index: usize,
        row: u32,
        col: u16,
    ) -> Result<String, JsValue> {
        self.read_cell_json_core(sheet_index, row, col)
            .map_err(js_error)
    }

    /// Apply one package-preserving cell value, formula, or clear operation.
    #[wasm_bindgen(js_name = setCellJson)]
    pub fn set_cell_json(&mut self, request_json: &str) -> Result<String, JsValue> {
        self.set_cell_json_core(request_json).map_err(js_error)
    }

    /// Replace the bounded workbook document-property set atomically.
    #[wasm_bindgen(js_name = setDocumentPropertiesJson)]
    pub fn set_document_properties_json(&mut self, request_json: &str) -> Result<String, JsValue> {
        self.set_document_properties_json_core(request_json)
            .map_err(js_error)
    }

    /// Restore the preceding browser edit snapshot.
    #[wasm_bindgen(js_name = undoEditJson)]
    pub fn undo_edit_json(&mut self) -> Result<String, JsValue> {
        self.undo_edit_core().map_err(js_error)
    }

    /// Reapply the next browser edit snapshot.
    #[wasm_bindgen(js_name = redoEditJson)]
    pub fn redo_edit_json(&mut self) -> Result<String, JsValue> {
        self.redo_edit_core().map_err(js_error)
    }

    /// Serialize the current retained XLSX/XLSM package for local download.
    #[wasm_bindgen(js_name = saveDocumentBytes)]
    pub fn save_document_bytes(&self) -> Result<Vec<u8>, JsValue> {
        self.save_document_bytes_core().map_err(js_error)
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
        let spreadsheet = parse_spreadsheet(bytes)?;
        let workbook = spreadsheet.workbook().clone();
        validate_session_workbook(&workbook)?;
        Ok(Self {
            spreadsheet,
            workbook,
            font_pack,
            font_pack_bytes,
            undo: Vec::new(),
            redo: Vec::new(),
            dirty: false,
            edited_parts: Vec::new(),
        })
    }

    fn inspection_json_core(&self, options: &RequestOptions) -> Result<String, FacadeError> {
        self.inspection_json_for(&self.workbook, options)
    }

    fn inspection_json_for(
        &self,
        workbook: &Workbook,
        options: &RequestOptions,
    ) -> Result<String, FacadeError> {
        let effective = effective_options(options, self.font_pack.as_ref())?;
        check_font_bytes(self.font_pack_bytes, effective.resources.font_bytes)?;
        let (image_count, image_bytes) = check_embedded_images(workbook, effective.resources)?;
        let inspection = WorkbookInspection {
            schema_version: 1,
            sheet_count: workbook.sheets.len(),
            sheets: workbook
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
            properties: DocumentPropertiesInspection::from(&workbook.properties),
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

    fn edit_state_value(&self) -> serde_json::Value {
        Self::edit_state_value_for(
            &self.spreadsheet,
            EditStateView {
                dirty: self.dirty,
                undo_depth: self.undo.len(),
                redo_depth: self.redo.len(),
                history_bytes: history_bytes(&self.undo) + history_bytes(&self.redo),
                edited_parts: &self.edited_parts,
            },
        )
    }

    fn edit_state_value_for(
        spreadsheet: &Spreadsheet,
        state: EditStateView<'_>,
    ) -> serde_json::Value {
        let (capability, reason) = match spreadsheet.edit_capability() {
            EditCapability::ReadWrite => ("read-write", None),
            EditCapability::ReadOnly(reason) => ("read-only", Some(edit_reason(reason))),
        };
        serde_json::json!({
            "schemaVersion": 1,
            "capability": capability,
            "reason": reason,
            "dirty": state.dirty,
            "canUndo": state.undo_depth > 0,
            "canRedo": state.redo_depth > 0,
            "undoDepth": state.undo_depth,
            "redoDepth": state.redo_depth,
            "historyBytes": state.history_bytes,
            "editedParts": state.edited_parts,
        })
    }

    fn mutation_result_json_for(
        &self,
        spreadsheet: &Spreadsheet,
        workbook: &Workbook,
        edit_state: EditStateView<'_>,
    ) -> Result<String, FacadeError> {
        let workbook: serde_json::Value =
            serde_json::from_str(&self.inspection_json_for(workbook, &RequestOptions::default())?)
                .map_err(|_| {
                    FacadeError::simple(
                        "serialization_failed",
                        "workbook inspection could not be assembled",
                        "output",
                    )
                })?;
        let output = serde_json::json!({
            "workbook": workbook,
            "editState": Self::edit_state_value_for(spreadsheet, edit_state),
        })
        .to_string();
        enforce_output(output.len(), MAX_OUTPUT_BYTES)?;
        Ok(output)
    }

    fn read_cell_json_core(
        &self,
        sheet_index: usize,
        row: u32,
        col: u16,
    ) -> Result<String, FacadeError> {
        check_cell_coordinate(row, col)?;
        let sheet = self.workbook.sheets.get(sheet_index).ok_or_else(|| {
            FacadeError::simple(
                "sheet_index_out_of_range",
                format!(
                    "sheet index {sheet_index} is out of range for {} sheets",
                    self.workbook.sheets.len()
                ),
                "sheetIndex",
            )
        })?;
        let value = InspectedCell::from(sheet.cell(row, col));
        let output = serde_json::json!({
            "schemaVersion": 1,
            "sheetIndex": sheet_index,
            "row": row,
            "col": col,
            "value": value,
            "formatted": sheet.formatted(row, col),
        })
        .to_string();
        enforce_output(output.len(), MAX_OUTPUT_BYTES)?;
        Ok(output)
    }

    fn set_cell_json_core(&mut self, request_json: &str) -> Result<String, FacadeError> {
        let request: CellEditRequest = parse_edit_request(request_json, "cell edit")?;
        check_cell_coordinate(request.row, request.col)?;
        if let EditableCell::Formula { formula, .. } = &request.value {
            let body = formula.trim().trim_start_matches('=').trim();
            if body.is_empty() {
                return Err(FacadeError::simple(
                    "invalid_edit",
                    "formula cannot be empty after removing leading '='",
                    "edit.value.formula",
                ));
            }
        }
        let sheet_name = self
            .workbook
            .sheets
            .get(request.sheet_index)
            .map(|sheet| sheet.name.clone())
            .ok_or_else(|| {
                FacadeError::simple(
                    "sheet_index_out_of_range",
                    format!(
                        "sheet index {} is out of range for {} sheets",
                        request.sheet_index,
                        self.workbook.sheets.len()
                    ),
                    "sheetIndex",
                )
            })?;
        let row = request.row;
        let col = request.col;
        self.apply_edit(move |spreadsheet| match request.value {
            EditableCell::Blank => spreadsheet.clear_cell_value(&sheet_name, row, col),
            EditableCell::Text { value } => {
                spreadsheet.set_cell_value(&sheet_name, row, col, Cell::Text(value))
            }
            EditableCell::Number { value } => {
                spreadsheet.set_cell_value(&sheet_name, row, col, Cell::Number(value))
            }
            EditableCell::Date { value } => {
                spreadsheet.set_cell_value(&sheet_name, row, col, Cell::Date(value))
            }
            EditableCell::Boolean { value } => {
                spreadsheet.set_cell_value(&sheet_name, row, col, Cell::Bool(value))
            }
            EditableCell::Error { value } => {
                spreadsheet.set_cell_value(&sheet_name, row, col, Cell::Error(value))
            }
            EditableCell::Formula { formula, cached } => {
                spreadsheet.set_cell_formula(&sheet_name, row, col, formula, cached.into_cell())
            }
        })
    }

    fn set_document_properties_json_core(
        &mut self,
        request_json: &str,
    ) -> Result<String, FacadeError> {
        let request: DocumentPropertiesEditRequest =
            parse_edit_request(request_json, "document properties")?;
        let properties = DocProperties::from(request);
        self.apply_edit(move |spreadsheet| spreadsheet.set_document_properties(properties))
    }

    fn apply_edit(
        &mut self,
        edit: impl FnOnce(&mut Spreadsheet) -> rxls::Result<()>,
    ) -> Result<String, FacadeError> {
        ensure_editable(&self.spreadsheet)?;
        let previous = self.snapshot()?;
        let mut candidate = self.spreadsheet.clone();
        edit(&mut candidate).map_err(map_edit_error)?;
        let bytes = candidate.save().map_err(map_edit_error)?;
        check_saved_workbook(&bytes)?;
        let workbook = parse_workbook(&bytes)?;
        validate_session_workbook(&workbook)?;

        let mut edited_parts = self.edited_parts.clone();
        for part in candidate.edited_parts() {
            if !edited_parts.iter().any(|existing| existing == part) {
                edited_parts.push(part.clone());
            }
        }
        edited_parts.sort();

        let projection = project_history(
            self.undo
                .iter()
                .map(|snapshot| snapshot.bytes.len())
                .chain(std::iter::once(previous.bytes.len()))
                .collect(),
            Vec::new(),
            HistorySide::Undo,
        );
        let output = self.mutation_result_json_for(
            &candidate,
            &workbook,
            EditStateView {
                dirty: true,
                undo_depth: projection.undo_depth,
                redo_depth: projection.redo_depth,
                history_bytes: projection.bytes,
                edited_parts: &edited_parts,
            },
        )?;
        self.undo.push(previous);
        self.redo.clear();
        apply_history_projection(&mut self.undo, &mut self.redo, &projection);
        self.spreadsheet = candidate;
        self.workbook = workbook;
        self.dirty = true;
        self.edited_parts = edited_parts;
        Ok(output)
    }

    fn undo_edit_core(&mut self) -> Result<String, FacadeError> {
        ensure_editable(&self.spreadsheet)?;
        let target = self.undo.last().ok_or_else(|| {
            FacadeError::simple("history_empty", "there is no edit to undo", "history")
        })?;
        let current = self.snapshot()?;
        let spreadsheet = parse_spreadsheet(&target.bytes)?;
        let workbook = spreadsheet.workbook().clone();
        validate_session_workbook(&workbook)?;
        let dirty = target.dirty;
        let edited_parts = target.edited_parts.clone();
        let projection = project_history(
            self.undo[..self.undo.len() - 1]
                .iter()
                .map(|snapshot| snapshot.bytes.len())
                .collect(),
            self.redo
                .iter()
                .map(|snapshot| snapshot.bytes.len())
                .chain(std::iter::once(current.bytes.len()))
                .collect(),
            HistorySide::Undo,
        );
        let output = self.mutation_result_json_for(
            &spreadsheet,
            &workbook,
            EditStateView {
                dirty,
                undo_depth: projection.undo_depth,
                redo_depth: projection.redo_depth,
                history_bytes: projection.bytes,
                edited_parts: &edited_parts,
            },
        )?;
        self.undo.pop();
        self.redo.push(current);
        apply_history_projection(&mut self.undo, &mut self.redo, &projection);
        self.spreadsheet = spreadsheet;
        self.workbook = workbook;
        self.dirty = dirty;
        self.edited_parts = edited_parts;
        Ok(output)
    }

    fn redo_edit_core(&mut self) -> Result<String, FacadeError> {
        ensure_editable(&self.spreadsheet)?;
        let target = self.redo.last().ok_or_else(|| {
            FacadeError::simple("history_empty", "there is no edit to redo", "history")
        })?;
        let current = self.snapshot()?;
        let spreadsheet = parse_spreadsheet(&target.bytes)?;
        let workbook = spreadsheet.workbook().clone();
        validate_session_workbook(&workbook)?;
        let dirty = target.dirty;
        let edited_parts = target.edited_parts.clone();
        let projection = project_history(
            self.undo
                .iter()
                .map(|snapshot| snapshot.bytes.len())
                .chain(std::iter::once(current.bytes.len()))
                .collect(),
            self.redo[..self.redo.len() - 1]
                .iter()
                .map(|snapshot| snapshot.bytes.len())
                .collect(),
            HistorySide::Redo,
        );
        let output = self.mutation_result_json_for(
            &spreadsheet,
            &workbook,
            EditStateView {
                dirty,
                undo_depth: projection.undo_depth,
                redo_depth: projection.redo_depth,
                history_bytes: projection.bytes,
                edited_parts: &edited_parts,
            },
        )?;
        self.redo.pop();
        self.undo.push(current);
        apply_history_projection(&mut self.undo, &mut self.redo, &projection);
        self.spreadsheet = spreadsheet;
        self.workbook = workbook;
        self.dirty = dirty;
        self.edited_parts = edited_parts;
        Ok(output)
    }

    fn snapshot(&self) -> Result<EditSnapshot, FacadeError> {
        let bytes = self.spreadsheet.save().map_err(map_edit_error)?;
        check_saved_workbook(&bytes)?;
        Ok(EditSnapshot {
            bytes,
            dirty: self.dirty,
            edited_parts: self.edited_parts.clone(),
        })
    }

    fn save_document_bytes_core(&self) -> Result<Vec<u8>, FacadeError> {
        ensure_editable(&self.spreadsheet)?;
        let bytes = self.spreadsheet.save().map_err(map_edit_error)?;
        check_saved_workbook(&bytes)?;
        Ok(bytes)
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

fn check_saved_workbook(bytes: &[u8]) -> Result<(), FacadeError> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(FacadeError::limit(
            "savedWorkbookBytes",
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

fn parse_spreadsheet(bytes: &[u8]) -> Result<Spreadsheet, FacadeError> {
    Spreadsheet::open(bytes).map_err(|_| {
        FacadeError::simple(
            "parse_failed",
            "spreadsheet input is malformed, encrypted, unsupported, or over budget",
            "input",
        )
    })
}

fn validate_session_workbook(workbook: &Workbook) -> Result<(), FacadeError> {
    if workbook.sheets.len() as u64 > MAX_SHEETS {
        return Err(FacadeError::limit(
            "sheets",
            MAX_SHEETS,
            workbook.sheets.len() as u64,
        ));
    }
    check_embedded_images(
        workbook,
        EffectiveResourceLimits {
            image_bytes: MAX_IMAGE_BYTES,
            images: MAX_IMAGES,
            font_bytes: MAX_FONT_BYTES,
        },
    )?;
    Ok(())
}

fn parse_edit_request<T: for<'de> Deserialize<'de>>(
    request_json: &str,
    label: &'static str,
) -> Result<T, FacadeError> {
    if request_json.len() > 128 * 1024 {
        return Err(FacadeError::limit(
            "editRequestBytes",
            128 * 1024,
            request_json.len() as u64,
        ));
    }
    serde_json::from_str(request_json).map_err(|_| {
        FacadeError::simple(
            "invalid_edit",
            format!("{label} request is invalid or contains an unknown field"),
            "edit",
        )
    })
}

fn check_cell_coordinate(row: u32, col: u16) -> Result<(), FacadeError> {
    if row > 1_048_575 || col > 16_383 {
        return Err(FacadeError::simple(
            "cell_out_of_range",
            "cell is outside the Excel grid",
            "cell",
        ));
    }
    Ok(())
}

fn ensure_editable(spreadsheet: &Spreadsheet) -> Result<(), FacadeError> {
    match spreadsheet.edit_capability() {
        EditCapability::ReadWrite => Ok(()),
        EditCapability::ReadOnly(reason) => Err(FacadeError::simple(
            "edit_read_only",
            format!(
                "spreadsheet is read-only for package-preserving edit: {}",
                edit_reason(reason)
            ),
            "editCapability",
        )),
    }
}

fn edit_reason(reason: &EditReadOnlyReason) -> &'static str {
    match reason {
        EditReadOnlyReason::LegacyBiff => "legacy-biff",
        EditReadOnlyReason::BinaryPackage => "binary-package",
        EditReadOnlyReason::OpenDocument => "open-document",
        EditReadOnlyReason::PackageMetadataLoss => "package-metadata-loss",
    }
}

fn map_edit_error(_error: rxls::Error) -> FacadeError {
    FacadeError::simple(
        "edit_failed",
        "the package-preserving edit was rejected and no change was committed",
        "edit",
    )
}

fn history_bytes(history: &[EditSnapshot]) -> usize {
    history.iter().fold(0usize, |total, snapshot| {
        total.saturating_add(snapshot.bytes.len())
    })
}

fn project_history(
    mut undo: Vec<usize>,
    mut redo: Vec<usize>,
    tie_evict: HistorySide,
) -> HistoryProjection {
    let initial_undo = undo.len();
    let initial_redo = redo.len();
    let mut bytes = undo
        .iter()
        .chain(&redo)
        .fold(0usize, |total, size| total.saturating_add(*size));
    while undo.len().saturating_add(redo.len()) > MAX_EDIT_HISTORY_ENTRIES
        || bytes > MAX_EDIT_HISTORY_BYTES
    {
        let side = match (undo.is_empty(), redo.is_empty()) {
            (false, true) => HistorySide::Undo,
            (true, false) => HistorySide::Redo,
            (false, false) if undo.len() > redo.len() => HistorySide::Undo,
            (false, false) if redo.len() > undo.len() => HistorySide::Redo,
            (false, false) => tie_evict,
            (true, true) => break,
        };
        let removed = match side {
            HistorySide::Undo => undo.remove(0),
            HistorySide::Redo => redo.remove(0),
        };
        bytes = bytes.saturating_sub(removed);
    }
    HistoryProjection {
        undo_drop: initial_undo - undo.len(),
        redo_drop: initial_redo - redo.len(),
        undo_depth: undo.len(),
        redo_depth: redo.len(),
        bytes,
    }
}

fn apply_history_projection(
    undo: &mut Vec<EditSnapshot>,
    redo: &mut Vec<EditSnapshot>,
    projection: &HistoryProjection,
) {
    undo.drain(..projection.undo_drop);
    redo.drain(..projection.redo_drop);
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

    fn styled_authored_workbook() -> Vec<u8> {
        let mut workbook = Workbook::new();
        workbook.add_sheet("Styled").write_with_format(
            0,
            0,
            "styled",
            &rxls::Format::new().set_bold(),
        );
        workbook.to_xlsx()
    }

    #[test]
    fn capabilities_are_stable_and_enable_verified_memory_fonts() {
        let capabilities: serde_json::Value =
            serde_json::from_str(&capabilities_json()).expect("capabilities JSON");
        assert_eq!(capabilities["schemaVersion"], 1);
        assert_eq!(capabilities["protocol"], "rxls.render-worker.v2");
        assert_eq!(capabilities["limits"]["maxInputBytes"], MAX_INPUT_BYTES);
        assert_eq!(capabilities["limits"]["maxSheets"], MAX_SHEETS);
        assert_eq!(capabilities["fontUploads"]["supported"], true);
        assert_eq!(capabilities["fontUploads"]["maxBytes"], MAX_FONT_BYTES);
        assert_eq!(capabilities["embeddedImages"]["painted"], true);
        assert_eq!(
            capabilities["editing"]["formats"],
            serde_json::json!(["xlsx", "xlsm"])
        );
        assert_eq!(
            capabilities["editing"]["maxHistoryEntries"],
            MAX_EDIT_HISTORY_ENTRIES
        );
        assert_eq!(
            capabilities["editing"]["maxHistoryBytes"],
            MAX_EDIT_HISTORY_BYTES
        );
    }

    #[test]
    fn history_projection_enforces_one_shared_entry_and_byte_budget() {
        let half_plus_one = MAX_EDIT_HISTORY_BYTES / 2 + 1;
        let after_undo =
            project_history(vec![half_plus_one], vec![half_plus_one], HistorySide::Undo);
        assert_eq!(
            after_undo,
            HistoryProjection {
                undo_drop: 1,
                redo_drop: 0,
                undo_depth: 0,
                redo_depth: 1,
                bytes: half_plus_one,
            }
        );

        let after_redo =
            project_history(vec![half_plus_one], vec![half_plus_one], HistorySide::Redo);
        assert_eq!(after_redo.undo_depth, 1);
        assert_eq!(after_redo.redo_depth, 0);
        assert_eq!(after_redo.undo_drop, 0);
        assert_eq!(after_redo.redo_drop, 1);
        assert!(after_redo.bytes <= MAX_EDIT_HISTORY_BYTES);

        let entry_bound = project_history(vec![1; 10], vec![1; 11], HistorySide::Undo);
        assert_eq!(entry_bound.undo_depth + entry_bound.redo_depth, 20);
        assert_eq!(entry_bound.redo_drop, 1);
        assert!(entry_bound.bytes <= MAX_EDIT_HISTORY_BYTES);
    }

    #[test]
    fn editable_session_reparses_cells_and_round_trips_history() {
        let mut session = RenderSession::new_core(&authored_workbook(), &[]).expect("open session");
        let initial: serde_json::Value =
            serde_json::from_str(&session.edit_state_json()).expect("initial edit state");
        assert_eq!(initial["capability"], "read-write");
        assert_eq!(initial["dirty"], false);

        let mutation: serde_json::Value = serde_json::from_str(
            &session
                .set_cell_json_core(
                    r#"{"sheetIndex":0,"row":0,"col":0,"value":{"kind":"text","value":"updated"}}"#,
                )
                .expect("edit cell"),
        )
        .expect("mutation JSON");
        let edited: serde_json::Value = serde_json::from_str(
            &session
                .read_cell_json_core(0, 0, 0)
                .expect("read edited cell"),
        )
        .expect("edited cell JSON");
        assert_eq!(edited["value"]["kind"], "text");
        assert_eq!(edited["value"]["value"], "updated");
        let state = session.edit_state_value();
        assert_eq!(state["dirty"], true);
        assert_eq!(state["canUndo"], true);
        assert_eq!(state["canRedo"], false);
        assert_eq!(mutation["editState"], state);

        let saved = session
            .save_document_bytes_core()
            .expect("save edited package");
        let reopened = Workbook::open(&saved).expect("reopen edited package");
        assert_eq!(
            reopened.sheets[0].cell(0, 0),
            Some(&Cell::Text("updated".to_string()))
        );

        let undo: serde_json::Value =
            serde_json::from_str(&session.undo_edit_core().expect("undo edit")).expect("undo JSON");
        let undone: serde_json::Value = serde_json::from_str(
            &session
                .read_cell_json_core(0, 0, 0)
                .expect("read undone cell"),
        )
        .expect("undone cell JSON");
        assert_eq!(undone["value"]["value"], "hello");
        assert_eq!(session.edit_state_value()["dirty"], false);
        assert_eq!(session.edit_state_value()["canRedo"], true);
        assert_eq!(
            session.edit_state_value()["editedParts"],
            serde_json::json!([])
        );
        assert_eq!(undo["editState"], session.edit_state_value());

        let redo: serde_json::Value =
            serde_json::from_str(&session.redo_edit_core().expect("redo edit")).expect("redo JSON");
        let redone: serde_json::Value = serde_json::from_str(
            &session
                .read_cell_json_core(0, 0, 0)
                .expect("read redone cell"),
        )
        .expect("redone cell JSON");
        assert_eq!(redone["value"]["value"], "updated");
        assert_eq!(session.edit_state_value()["dirty"], true);
        assert_eq!(
            session.edit_state_value()["editedParts"],
            serde_json::json!(["xl/worksheets/sheet1.xml"])
        );
        assert_eq!(redo["editState"], session.edit_state_value());
    }

    #[test]
    fn blank_cell_edit_preserves_style_across_save_undo_and_redo() {
        let mut session =
            RenderSession::new_core(&styled_authored_workbook(), &[]).expect("open session");
        let original_style = session.workbook.sheets[0]
            .cell_style(0, 0)
            .cloned()
            .expect("authored style");

        session
            .set_cell_json_core(r#"{"sheetIndex":0,"row":0,"col":0,"value":{"kind":"blank"}}"#)
            .expect("clear styled cell value");

        assert_eq!(session.workbook.sheets[0].cell(0, 0), None);
        assert_eq!(
            session.edit_state_value()["editedParts"],
            serde_json::json!(["xl/worksheets/sheet1.xml"])
        );
        let saved = session
            .save_document_bytes_core()
            .expect("save blank styled cell");
        let reopened = Workbook::open(&saved).expect("reopen saved workbook");
        assert_eq!(reopened.sheets[0].cell(0, 0), None);

        let mut restored = RenderSession::new_core(&saved, &[]).expect("open cleared workbook");
        restored
            .set_cell_json_core(
                r#"{"sheetIndex":0,"row":0,"col":0,"value":{"kind":"text","value":"restored"}}"#,
            )
            .expect("restore cell value");
        assert_eq!(
            restored.workbook.sheets[0].cell_style(0, 0),
            Some(&original_style)
        );

        session.undo_edit_core().expect("undo blank edit");
        assert_eq!(
            session.workbook.sheets[0].cell(0, 0),
            Some(&Cell::Text("styled".to_string()))
        );
        assert_eq!(
            session.workbook.sheets[0].cell_style(0, 0),
            Some(&original_style)
        );
        assert_eq!(session.edit_state_value()["dirty"], false);
        assert_eq!(
            session.edit_state_value()["editedParts"],
            serde_json::json!([])
        );

        session.redo_edit_core().expect("redo blank edit");
        assert_eq!(session.workbook.sheets[0].cell(0, 0), None);
        assert_eq!(session.edit_state_value()["dirty"], true);
    }

    #[test]
    fn editable_session_rejects_empty_formula_without_mutating_state() {
        let mut session = RenderSession::new_core(&authored_workbook(), &[]).expect("open session");
        let before_bytes = session
            .save_document_bytes_core()
            .expect("save before rejected formula");
        let before_state = session.edit_state_value();

        let error = session
            .set_cell_json_core(
                r#"{"sheetIndex":0,"row":0,"col":0,"value":{"kind":"formula","formula":"=","cached":{"kind":"number","value":0}}}"#,
            )
            .expect_err("formula without a body must fail");

        assert_eq!(error.code, "invalid_edit");
        assert_eq!(error.location, "edit.value.formula");
        assert_eq!(session.edit_state_value(), before_state);
        assert_eq!(
            session
                .save_document_bytes_core()
                .expect("save after rejected formula"),
            before_bytes
        );
    }

    #[test]
    fn editable_session_history_evicts_the_oldest_snapshot_at_the_shared_cap() {
        let mut session = RenderSession::new_core(&authored_workbook(), &[]).expect("open session");
        for index in 0..=MAX_EDIT_HISTORY_ENTRIES {
            session
                .set_cell_json_core(&format!(
                    r#"{{"sheetIndex":0,"row":0,"col":0,"value":{{"kind":"text","value":"edit-{index}"}}}}"#
                ))
                .expect("apply edit");
        }

        let full = session.edit_state_value();
        assert_eq!(full["undoDepth"], MAX_EDIT_HISTORY_ENTRIES);
        assert_eq!(full["redoDepth"], 0);
        assert!(full["historyBytes"].as_u64().unwrap() <= MAX_EDIT_HISTORY_BYTES as u64);

        for _ in 0..MAX_EDIT_HISTORY_ENTRIES {
            session.undo_edit_core().expect("undo retained edit");
        }
        assert_eq!(
            session.workbook.sheets[0].cell(0, 0),
            Some(&Cell::Text("edit-0".to_string()))
        );
        assert_eq!(session.edit_state_value()["dirty"], true);
        assert_eq!(session.edit_state_value()["undoDepth"], 0);
        assert_eq!(
            session.edit_state_value()["redoDepth"],
            MAX_EDIT_HISTORY_ENTRIES
        );
        assert_eq!(
            session
                .undo_edit_core()
                .expect_err("oldest snapshot was evicted")
                .code,
            "history_empty"
        );

        session.redo_edit_core().expect("redo retained edit");
        session
            .set_cell_json_core(
                r#"{"sheetIndex":0,"row":0,"col":0,"value":{"kind":"text","value":"branched"}}"#,
            )
            .expect("branch from history");
        assert_eq!(session.edit_state_value()["canRedo"], false);
        assert_eq!(session.edit_state_value()["redoDepth"], 0);
    }

    #[test]
    fn editable_session_updates_properties_and_rejects_partial_failures() {
        let mut session = RenderSession::new_core(&authored_workbook(), &[]).expect("open session");
        session
            .set_document_properties_json_core(
                r#"{"title":"Browser report","subject":null,"creator":"rxls","keywords":null,"description":null,"lastModifiedBy":null,"company":"Open source","created":null}"#,
            )
            .expect("edit properties");
        assert_eq!(
            session.workbook.properties.title.as_deref(),
            Some("Browser report")
        );
        assert_eq!(
            session.workbook.properties.company.as_deref(),
            Some("Open source")
        );

        let before = session
            .save_document_bytes_core()
            .expect("save before failure");
        let before_state = session.edit_state_value();
        let error = session
            .set_document_properties_json_core(r#"{"title":"incomplete replacement"}"#)
            .expect_err("missing property fields must fail");
        assert_eq!(error.code, "invalid_edit");
        assert_eq!(
            session.workbook.properties.title.as_deref(),
            Some("Browser report")
        );
        assert_eq!(session.edit_state_value(), before_state);
        assert_eq!(
            session
                .save_document_bytes_core()
                .expect("save after rejected properties"),
            before
        );

        let error = session
            .set_cell_json_core(
                "{\"sheetIndex\":0,\"row\":0,\"col\":0,\"value\":{\"kind\":\"text\",\"value\":\"bad\\u0001text\"}}",
            )
            .expect_err("invalid XML text must fail");
        assert_eq!(error.code, "edit_failed");
        assert_eq!(session.edit_state_value(), before_state);
        assert_eq!(
            session
                .save_document_bytes_core()
                .expect("save after failure"),
            before
        );
    }

    #[test]
    fn legacy_workbook_reports_a_typed_read_only_edit_reason() {
        let bytes = include_bytes!("../../../tests/fixtures/xls/korean-cp949-biff5.xls");
        let session = RenderSession::new_core(bytes, &[]).expect("open legacy workbook");
        let state = session.edit_state_value();
        assert_eq!(state["capability"], "read-only");
        assert_eq!(state["reason"], "legacy-biff");
        assert_eq!(
            session.save_document_bytes_core().unwrap_err().code,
            "edit_read_only"
        );
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

    /// [`check_input`] is the Rust-side ceiling on raw workbook bytes: the
    /// last line of defense for any caller that reaches the compiled wasm
    /// module without going through the JavaScript worker's duplicate
    /// preflight check (for example a native host embedding this crate, or
    /// a hand-rolled caller of the stateless `render_sheet_svg_wasm`-style
    /// exports). It must fail closed on its own, not merely as a side effect
    /// of the JS-layer check.
    #[test]
    fn input_byte_ceiling_fails_closed_at_the_rust_boundary_independent_of_the_js_layer() {
        let oversized = vec![0_u8; MAX_INPUT_BYTES + 1];
        let error = check_input(&oversized).unwrap_err();
        assert_eq!(error.code, "limit_exceeded");
        assert_eq!(error.resource, Some("inputBytes"));
        assert_eq!(error.limit, Some(MAX_INPUT_BYTES as u64));
        assert_eq!(error.actual, Some((MAX_INPUT_BYTES + 1) as u64));

        let saved_error = check_saved_workbook(&oversized).unwrap_err();
        assert_eq!(saved_error.code, "limit_exceeded");
        assert_eq!(saved_error.resource, Some("savedWorkbookBytes"));
        assert_eq!(saved_error.limit, Some(MAX_INPUT_BYTES as u64));
        assert_eq!(saved_error.actual, Some((MAX_INPUT_BYTES + 1) as u64));

        // The boundary itself must stay usable: exactly at the ceiling is fine.
        assert!(check_input(&vec![0_u8; MAX_INPUT_BYTES]).is_ok());
        assert!(check_saved_workbook(&vec![0_u8; MAX_INPUT_BYTES]).is_ok());
    }

    /// Request-time validation (`options_reject_unknown_fields_and_limit_increases`
    /// above) only proves that an over-hard-max override is rejected before any
    /// workbook content is inspected. It does not prove that real, in-range
    /// content which exceeds an accepted stricter limit is caught while
    /// rendering. This test exercises that second, previously untested path:
    /// a legally requested `maxRows` override that the workbook's actual used
    /// range violates must still fail closed with the renderer's own typed
    /// `LimitExceeded` error, mapped through [`map_render_error`].
    #[test]
    fn render_time_limit_breach_on_real_content_maps_to_a_typed_facade_error() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Sheet1");
        for row in 0..11 {
            sheet.write(row, 0, format!("row {row}"));
        }
        let bytes = workbook.to_xlsx();

        // maxRows: 1 is a legal, stricter-than-default request (it is not
        // rejected by `effective_options`), but the workbook actually uses
        // eleven rows, so the renderer itself must reject it while building
        // the scene rather than silently clipping the extra rows.
        let error = render_sheet_svg_core(&bytes, 0, r#"{"limits":{"maxRows":1}}"#).unwrap_err();
        assert_eq!(error.code, "limit_exceeded");
        assert_eq!(error.resource, Some("rows"));
        assert_eq!(error.limit, Some(1));
        assert!(error.actual.unwrap() > 1, "actual was {:?}", error.actual);
    }

    /// Print pages must be independently constructible from one open,
    /// reused [`RenderSession`] in any order, with no hidden dependency on
    /// having built earlier pages first. This is the facade-level half of
    /// virtualized output: the underlying `render` crate already proves the
    /// memory distinction (see `render/tests/printing.rs`,
    /// `prepared_page_map_builds_exactly_the_requested_original_page`), and
    /// this test proves the wasm facade exposes that same random-access
    /// contract rather than accidentally reintroducing an ordering
    /// requirement or a per-page cache.
    #[test]
    fn print_pages_are_built_out_of_order_from_one_reused_session() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Print");
        for row in 0..120 {
            sheet.write(row, 0, format!("row {row}"));
        }
        let bytes = workbook.to_xlsx();

        let session = RenderSession::new_core(&bytes, &[]).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_str(&session.print_manifest_json_core(0, "{}").unwrap()).unwrap();
        let page_count = manifest["pages"].as_array().unwrap().len();
        assert!(
            page_count >= 2,
            "fixture must paginate into at least two pages, got {page_count}"
        );

        // Request the last page before the first page on the same session.
        // A cache or ordering bug would surface here as a wrong page, a
        // panic, or a dependency on having rendered page 0 first.
        let last = session
            .render_print_page_svg_core(0, page_count - 1, "{}")
            .unwrap();
        let first = session.render_print_page_svg_core(0, 0, "{}").unwrap();
        assert!(last.contains("<svg"));
        assert!(first.contains("<svg"));
        assert_ne!(
            first, last,
            "distinct pages of a multi-page sheet must render distinct content"
        );

        // The same page requested twice, out of order, is still deterministic.
        let last_again = session
            .render_print_page_svg_core(0, page_count - 1, "{}")
            .unwrap();
        assert_eq!(last, last_again);
    }
}
