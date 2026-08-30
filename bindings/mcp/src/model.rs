//! JSON Schema-backed MCP request and response types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct OpenWorkbookParams {
    /// Existing XLS, XLSX, XLSM, XLSB, or ODS path below an allowed root.
    pub(crate) path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct SessionParams {
    /// Opaque ID returned by `workbook_open`.
    pub(crate) session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ReadRangeParams {
    /// Opaque ID returned by `workbook_open`.
    pub(crate) session_id: String,
    /// Case-sensitive worksheet name.
    pub(crate) sheet: String,
    /// Inclusive A1 range, for example `A1:D20`.
    pub(crate) range: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct CompareWorkbooksParams {
    /// Opaque ID for the left workbook returned by `workbook_open`.
    pub(crate) left_session_id: String,
    /// Case-sensitive worksheet name in the left workbook.
    pub(crate) left_sheet: String,
    /// Opaque ID for the right workbook returned by `workbook_open`.
    pub(crate) right_session_id: String,
    /// Case-sensitive worksheet name in the right workbook.
    pub(crate) right_sheet: String,
    /// Inclusive A1 range compared at matching coordinates, for example `A1:D20`.
    pub(crate) range: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ExportSheetParams {
    /// Opaque ID returned by `workbook_open`.
    pub(crate) session_id: String,
    /// Case-sensitive worksheet name.
    pub(crate) sheet: String,
    /// Deterministic text export format.
    pub(crate) format: ExportFormat,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExportFormat {
    Csv,
    Markdown,
    Html,
}

impl ExportFormat {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Markdown => "markdown",
            Self::Html => "html",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct SetCellsParams {
    /// Opaque ID returned by `workbook_open`.
    pub(crate) session_id: String,
    /// Case-sensitive worksheet name.
    pub(crate) sheet: String,
    /// Atomic cell changes. The whole call rolls back if any edit fails.
    pub(crate) edits: Vec<CellEdit>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum CellEdit {
    /// Replace a value while retaining the cell's existing style.
    Set {
        /// A1 cell address.
        cell: String,
        /// New scalar value.
        value: InputValue,
    },
    /// Set formula source and its cached scalar value.
    Formula {
        /// A1 cell address.
        cell: String,
        /// Formula source, with or without a leading equals sign.
        formula: String,
        /// Cached value returned before recalculation.
        cached: InputValue,
    },
}

impl CellEdit {
    pub(crate) fn address(&self) -> &str {
        match self {
            Self::Set { cell, .. } | Self::Formula { cell, .. } => cell,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub(crate) enum InputValue {
    /// UTF-8 cell text.
    Text(String),
    /// Finite numeric value.
    Number(f64),
    /// Raw Excel date serial.
    Date(f64),
    /// Boolean value.
    Boolean(bool),
    /// Standard Excel error display string such as `#N/A`.
    Error(String),
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct SaveCopyParams {
    /// Opaque ID returned by `workbook_open`.
    pub(crate) session_id: String,
    /// New XLSX/XLSM path below an allowed root. Existing files are rejected.
    pub(crate) path: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct SheetSummary {
    pub(crate) name: String,
    pub(crate) sheet_type: String,
    pub(crate) visibility: String,
    pub(crate) used_range: Option<String>,
    pub(crate) populated_cells: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct SessionSummary {
    pub(crate) session_id: String,
    pub(crate) path: String,
    pub(crate) format: String,
    pub(crate) edit_capability: String,
    pub(crate) current_bytes: usize,
    pub(crate) source_sha256: String,
    pub(crate) current_sha256: String,
    pub(crate) sheet_count: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct OpenWorkbookResult {
    #[serde(flatten)]
    pub(crate) session: SessionSummary,
    pub(crate) sheets: Vec<SheetSummary>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ListSessionsResult {
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) retained_bytes: usize,
    pub(crate) max_sessions: usize,
    pub(crate) max_retained_bytes: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ParseProvenanceResult {
    pub(crate) container: String,
    pub(crate) recovered: bool,
    pub(crate) recovery_codes: Vec<String>,
    pub(crate) recoveries_truncated: bool,
    pub(crate) partial: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct InspectWorkbookResult {
    #[serde(flatten)]
    pub(crate) session: SessionSummary,
    pub(crate) date_1904: bool,
    pub(crate) text_truncated: bool,
    pub(crate) active_sheet: Option<String>,
    pub(crate) defined_name_count: usize,
    pub(crate) sheets: Vec<SheetSummary>,
    pub(crate) edited_parts: Vec<String>,
    pub(crate) provenance: ParseProvenanceResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum TypedCell {
    Text {
        value: String,
    },
    Number {
        value: f64,
    },
    Date {
        serial: f64,
    },
    Boolean {
        value: bool,
    },
    Error {
        value: String,
    },
    Formula {
        formula: String,
        cached: Box<TypedCell>,
    },
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct CellResult {
    pub(crate) address: String,
    pub(crate) row: u32,
    pub(crate) column: u16,
    pub(crate) value: Option<TypedCell>,
    pub(crate) display: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ReadRangeResult {
    pub(crate) session_id: String,
    pub(crate) sheet: String,
    pub(crate) range: String,
    pub(crate) cell_count: usize,
    pub(crate) rows: Vec<Vec<CellResult>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub(crate) struct ComparedCell {
    pub(crate) value: Option<TypedCell>,
    pub(crate) display: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct CellDifference {
    pub(crate) address: String,
    pub(crate) row: u32,
    pub(crate) column: u16,
    pub(crate) left: ComparedCell,
    pub(crate) right: ComparedCell,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct CompareWorkbooksResult {
    pub(crate) left_session_id: String,
    pub(crate) left_sha256: String,
    pub(crate) left_sheet: String,
    pub(crate) right_session_id: String,
    pub(crate) right_sha256: String,
    pub(crate) right_sheet: String,
    pub(crate) range: String,
    pub(crate) compared_cells: usize,
    pub(crate) identical: bool,
    pub(crate) difference_count: usize,
    pub(crate) returned_differences: usize,
    pub(crate) max_returned_differences: usize,
    pub(crate) returned_detail_bytes: usize,
    pub(crate) max_detail_bytes: usize,
    pub(crate) differences_truncated: bool,
    pub(crate) truncated_by_count: bool,
    pub(crate) truncated_by_size: bool,
    pub(crate) differences: Vec<CellDifference>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ExportSheetResult {
    pub(crate) session_id: String,
    pub(crate) sheet: String,
    pub(crate) format: String,
    pub(crate) bytes: usize,
    pub(crate) content: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SetCellsResult {
    pub(crate) session_id: String,
    pub(crate) applied_edits: usize,
    pub(crate) current_bytes: usize,
    pub(crate) current_sha256: String,
    pub(crate) edited_parts: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SaveCopyResult {
    pub(crate) session_id: String,
    pub(crate) path: String,
    pub(crate) bytes: usize,
    pub(crate) sha256: String,
    pub(crate) edited_parts: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct CloseSessionResult {
    pub(crate) session_id: String,
    pub(crate) released_bytes: usize,
    pub(crate) remaining_sessions: usize,
}
