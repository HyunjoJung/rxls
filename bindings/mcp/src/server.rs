//! Stateful local MCP tools and filesystem policy.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use rxls::{
    export_csv, export_html, export_markdown, Cell, CellErrorType, CsvOptions, EditCapability,
    EditReadOnlyReason, Sheet, SheetType, SheetVisible, Spreadsheet,
};
use schemars::JsonSchema;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::a1::{format_cell, parse_cell, CellAddress, CellRange};
use crate::model::{
    CellDifference, CellEdit, CellResult, CloseSessionResult, CompareWorkbooksParams,
    CompareWorkbooksResult, ComparedCell, ExportFormat, ExportSheetParams, ExportSheetResult,
    InputValue, InspectWorkbookResult, ListSessionsResult, OpenWorkbookParams, OpenWorkbookResult,
    ParseProvenanceResult, ReadRangeParams, ReadRangeResult, SaveCopyParams, SaveCopyResult,
    SessionParams, SessionSummary, SetCellsParams, SetCellsResult, SheetSummary, TypedCell,
};
use crate::{
    MAX_BATCH_EDITS, MAX_COMPARE_DETAIL_BYTES, MAX_COMPARE_DIFFERENCES, MAX_OUTPUT_BYTES,
    MAX_SESSIONS, MAX_SESSION_BYTES, MAX_WORKBOOK_BYTES,
};

static SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Canonical filesystem roots and relative-path base used by the MCP server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    roots: Vec<PathBuf>,
    base_dir: PathBuf,
}

impl ServerConfig {
    /// Canonicalize and validate one or more allowed directory roots.
    ///
    /// Relative workbook paths are resolved from the process current directory,
    /// then checked against these roots. Existing paths are canonicalized before
    /// the check so a symlink cannot escape the configured boundary.
    pub fn new(roots: Vec<PathBuf>) -> Result<Self, String> {
        if roots.is_empty() {
            return Err(error(
                "RXLS_MCP_INVALID_ROOT",
                "at least one allowed root is required",
            ));
        }
        let base_dir = fs::canonicalize(std::env::current_dir().map_err(|_| {
            error(
                "RXLS_MCP_CURRENT_DIR_FAILED",
                "could not read the process current directory",
            )
        })?)
        .map_err(|_| {
            error(
                "RXLS_MCP_CURRENT_DIR_FAILED",
                "could not canonicalize the process current directory",
            )
        })?;

        let mut canonical_roots = Vec::with_capacity(roots.len());
        for root in roots {
            let canonical = fs::canonicalize(&root).map_err(|_| {
                error(
                    "RXLS_MCP_INVALID_ROOT",
                    format!("root does not exist: {}", root.display()),
                )
            })?;
            if !canonical.is_dir() {
                return Err(error(
                    "RXLS_MCP_INVALID_ROOT",
                    format!("root is not a directory: {}", canonical.display()),
                ));
            }
            if !canonical_roots.contains(&canonical) {
                canonical_roots.push(canonical);
            }
        }
        canonical_roots.sort();
        Ok(Self {
            roots: canonical_roots,
            base_dir,
        })
    }

    /// Canonical allowed roots in deterministic order.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }
}

/// Stateful tools-only MCP server for reading and preserving spreadsheet files.
#[derive(Debug, Clone)]
pub struct RxlsMcpServer {
    config: Arc<ServerConfig>,
    state: Arc<Mutex<ServerState>>,
}

#[derive(Debug, Default)]
struct ServerState {
    sessions: BTreeMap<String, Session>,
    retained_bytes: usize,
    next_session: u64,
}

#[derive(Debug)]
struct Session {
    id: String,
    path: PathBuf,
    format: WorkbookFormat,
    spreadsheet: Spreadsheet,
    source_sha256: String,
    current_sha256: String,
    current_bytes: usize,
    edited_parts: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkbookFormat {
    Xls,
    Xlsx,
    Xlsm,
    Xlsb,
    Ods,
}

impl WorkbookFormat {
    fn from_path(path: &Path) -> Result<Self, String> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match extension.as_str() {
            "xls" => Ok(Self::Xls),
            "xlsx" => Ok(Self::Xlsx),
            "xlsm" => Ok(Self::Xlsm),
            "xlsb" => Ok(Self::Xlsb),
            "ods" => Ok(Self::Ods),
            _ => Err(error(
                "RXLS_MCP_UNSUPPORTED_FORMAT",
                "supported extensions are .xls, .xlsx, .xlsm, .xlsb, and .ods",
            )),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Xls => "xls",
            Self::Xlsx => "xlsx",
            Self::Xlsm => "xlsm",
            Self::Xlsb => "xlsb",
            Self::Ods => "ods",
        }
    }

    const fn is_editable_ooxml(self) -> bool {
        matches!(self, Self::Xlsx | Self::Xlsm)
    }
}

impl RxlsMcpServer {
    /// Create an empty MCP server with validated filesystem policy.
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config: Arc::new(config),
            state: Arc::new(Mutex::new(ServerState {
                next_session: 1,
                ..ServerState::default()
            })),
        }
    }

    fn state(&self) -> Result<MutexGuard<'_, ServerState>, String> {
        self.state.lock().map_err(|_| {
            error(
                "RXLS_MCP_STATE_POISONED",
                "server state is unavailable after an internal panic",
            )
        })
    }

    fn resolve_existing(&self, input: &str) -> Result<PathBuf, String> {
        let requested = self.resolve_input(input)?;
        let canonical = fs::canonicalize(&requested).map_err(|_| {
            error(
                "RXLS_MCP_PATH_NOT_FOUND",
                format!("path does not exist: {}", requested.display()),
            )
        })?;
        if !canonical.is_file() {
            return Err(error(
                "RXLS_MCP_NOT_A_FILE",
                format!("path is not a file: {}", canonical.display()),
            ));
        }
        self.ensure_allowed(&canonical)?;
        Ok(canonical)
    }

    fn resolve_new(&self, input: &str) -> Result<PathBuf, String> {
        let requested = self.resolve_input(input)?;
        if requested.symlink_metadata().is_ok() {
            return Err(error(
                "RXLS_MCP_DESTINATION_EXISTS",
                format!("destination already exists: {}", requested.display()),
            ));
        }
        let file_name = requested.file_name().ok_or_else(|| {
            error(
                "RXLS_MCP_INVALID_DESTINATION",
                "destination must include a file name",
            )
        })?;
        let parent = requested.parent().unwrap_or(Path::new("."));
        let canonical_parent = fs::canonicalize(parent).map_err(|_| {
            error(
                "RXLS_MCP_INVALID_DESTINATION",
                format!("destination parent does not exist: {}", parent.display()),
            )
        })?;
        if !canonical_parent.is_dir() {
            return Err(error(
                "RXLS_MCP_INVALID_DESTINATION",
                "destination parent is not a directory",
            ));
        }
        self.ensure_allowed(&canonical_parent)?;
        Ok(canonical_parent.join(file_name))
    }

    fn resolve_input(&self, input: &str) -> Result<PathBuf, String> {
        if input.is_empty() || input.contains('\0') {
            return Err(error(
                "RXLS_MCP_INVALID_PATH",
                "path must be a non-empty string without NUL bytes",
            ));
        }
        let path = PathBuf::from(input);
        Ok(if path.is_absolute() {
            path
        } else {
            self.config.base_dir.join(path)
        })
    }

    fn ensure_allowed(&self, canonical: &Path) -> Result<(), String> {
        if self
            .config
            .roots
            .iter()
            .any(|root| canonical.starts_with(root))
        {
            Ok(())
        } else {
            Err(error(
                "RXLS_MCP_PATH_OUTSIDE_ROOT",
                format!(
                    "path is outside every allowed root: {}",
                    canonical.display()
                ),
            ))
        }
    }
}

#[tool_router]
impl RxlsMcpServer {
    /// Open one local workbook and return an opaque session ID.
    #[tool(
        description = "Open a local XLS, XLSX, XLSM, XLSB, or ODS workbook below an allowed root",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workbook_open(
        &self,
        Parameters(params): Parameters<OpenWorkbookParams>,
    ) -> Result<Json<OpenWorkbookResult>, String> {
        let path = self.resolve_existing(&params.path)?;
        let format = WorkbookFormat::from_path(&path)?;
        let bytes = read_bounded_file(&path)?;
        let spreadsheet = Spreadsheet::open(&bytes).map_err(|source| {
            error(
                "RXLS_MCP_OPEN_FAILED",
                format!("rxls could not parse the workbook: {source}"),
            )
        })?;
        validate_detected_format(format, spreadsheet.edit_capability())?;

        let source_sha256 = sha256(&bytes);
        let sheets = sheet_summaries(spreadsheet.workbook());
        let mut state = self.state()?;
        if state.sessions.len() >= MAX_SESSIONS {
            return Err(error(
                "RXLS_MCP_SESSION_LIMIT",
                format!("at most {MAX_SESSIONS} workbook sessions may be open"),
            ));
        }
        if state.retained_bytes.saturating_add(bytes.len()) > MAX_SESSION_BYTES {
            return Err(error(
                "RXLS_MCP_MEMORY_LIMIT",
                format!("open sessions may retain at most {MAX_SESSION_BYTES} source bytes"),
            ));
        }

        let id = format!("wb-{:08}", state.next_session);
        let session = Session {
            id: id.clone(),
            path,
            format,
            spreadsheet,
            source_sha256: source_sha256.clone(),
            current_sha256: source_sha256,
            current_bytes: bytes.len(),
            edited_parts: BTreeSet::new(),
        };
        let result = OpenWorkbookResult {
            session: session.summary(),
            sheets,
        };
        let checked = checked_json(result)?;
        state.next_session = state.next_session.saturating_add(1);
        state.retained_bytes += bytes.len();
        state.sessions.insert(id, session);
        Ok(checked)
    }

    /// List active local workbook sessions without returning workbook contents.
    #[tool(
        description = "List active rxls workbook sessions and their bounded memory use",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workbook_list_sessions(&self) -> Result<Json<ListSessionsResult>, String> {
        let state = self.state()?;
        checked_json(ListSessionsResult {
            sessions: state.sessions.values().map(Session::summary).collect(),
            retained_bytes: state.retained_bytes,
            max_sessions: MAX_SESSIONS,
            max_retained_bytes: MAX_SESSION_BYTES,
        })
    }

    /// Inspect workbook structure, parse provenance, and edit capability.
    #[tool(
        description = "Inspect workbook metadata, sheets, parse provenance, and preservation edit status",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workbook_inspect(
        &self,
        Parameters(params): Parameters<SessionParams>,
    ) -> Result<Json<InspectWorkbookResult>, String> {
        let state = self.state()?;
        let session = find_session(&state, &params.session_id)?;
        let workbook = session.spreadsheet.workbook();
        let provenance = workbook.parse_provenance();
        checked_json(InspectWorkbookResult {
            session: session.summary(),
            date_1904: workbook.date1904,
            text_truncated: workbook.text_truncated,
            active_sheet: workbook.active_sheet_name().map(str::to_string),
            defined_name_count: workbook.defined_names.len() + workbook.local_defined_names.len(),
            sheets: sheet_summaries(workbook),
            edited_parts: session.edited_parts.iter().cloned().collect(),
            provenance: ParseProvenanceResult {
                container: provenance.container.code().to_string(),
                recovered: provenance.is_recovered(),
                recovery_codes: provenance
                    .recoveries()
                    .iter()
                    .map(|code| code.code().to_string())
                    .collect(),
                recoveries_truncated: provenance.recoveries_truncated(),
                partial: provenance.partial,
            },
        })
    }

    /// Read a bounded dense A1 range with typed and display values.
    #[tool(
        description = "Read at most 10,000 cells from an inclusive A1 worksheet range",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workbook_read_range(
        &self,
        Parameters(params): Parameters<ReadRangeParams>,
    ) -> Result<Json<ReadRangeResult>, String> {
        let range = CellRange::parse(&params.range)?;
        let state = self.state()?;
        let session = find_session(&state, &params.session_id)?;
        let sheet = find_worksheet(session.spreadsheet.workbook(), &params.sheet)?;
        let mut rows = Vec::with_capacity(
            usize::try_from(range.end.row - range.start.row + 1).unwrap_or_default(),
        );
        for row in range.start.row..=range.end.row {
            let mut cells = Vec::with_capacity(usize::from(range.end.col - range.start.col + 1));
            for col in range.start.col..=range.end.col {
                let address = CellAddress { row, col };
                cells.push(CellResult {
                    address: format_cell(address),
                    row: row + 1,
                    column: col + 1,
                    value: sheet.cell(row, col).map(typed_cell),
                    display: sheet.formatted(row, col).map(str::to_string),
                });
            }
            rows.push(cells);
        }
        checked_json(ReadRangeResult {
            session_id: session.id.clone(),
            sheet: sheet.name.clone(),
            range: params.range,
            cell_count: range.cell_count(),
            rows,
        })
    }

    /// Compare a bounded A1 range across two open workbook sessions.
    #[tool(
        description = "Compare typed and displayed values for at most 10,000 matching worksheet cells",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workbook_compare(
        &self,
        Parameters(params): Parameters<CompareWorkbooksParams>,
    ) -> Result<Json<CompareWorkbooksResult>, String> {
        let range = CellRange::parse(&params.range)?;
        let state = self.state()?;
        let left_session = find_session(&state, &params.left_session_id)?;
        let right_session = find_session(&state, &params.right_session_id)?;
        let left_sheet = find_worksheet(left_session.spreadsheet.workbook(), &params.left_sheet)?;
        let right_sheet =
            find_worksheet(right_session.spreadsheet.workbook(), &params.right_sheet)?;

        let mut difference_count = 0usize;
        let mut returned_detail_bytes = 0usize;
        let mut truncated_by_count = false;
        let mut truncated_by_size = false;
        let mut differences = Vec::new();
        for row in range.start.row..=range.end.row {
            for col in range.start.col..=range.end.col {
                if left_sheet.cell(row, col) == right_sheet.cell(row, col)
                    && left_sheet.formatted(row, col) == right_sheet.formatted(row, col)
                {
                    continue;
                }
                difference_count = difference_count.saturating_add(1);
                if differences.len() >= MAX_COMPARE_DIFFERENCES {
                    truncated_by_count = true;
                    continue;
                }
                let difference = CellDifference {
                    address: format_cell(CellAddress { row, col }),
                    row: row + 1,
                    column: col + 1,
                    left: compared_cell(left_sheet, row, col),
                    right: compared_cell(right_sheet, row, col),
                };
                let detail_bytes = serde_json::to_vec(&difference)
                    .map_err(|_| {
                        error(
                            "RXLS_MCP_SERIALIZE_FAILED",
                            "comparison detail could not be serialized",
                        )
                    })?
                    .len();
                if returned_detail_bytes.saturating_add(detail_bytes) > MAX_COMPARE_DETAIL_BYTES {
                    truncated_by_size = true;
                    continue;
                }
                returned_detail_bytes += detail_bytes;
                differences.push(difference);
            }
        }
        let returned_differences = differences.len();
        checked_json(CompareWorkbooksResult {
            left_session_id: left_session.id.clone(),
            left_sha256: left_session.current_sha256.clone(),
            left_sheet: left_sheet.name.clone(),
            right_session_id: right_session.id.clone(),
            right_sha256: right_session.current_sha256.clone(),
            right_sheet: right_sheet.name.clone(),
            range: params.range,
            compared_cells: range.cell_count(),
            identical: difference_count == 0,
            difference_count,
            returned_differences,
            max_returned_differences: MAX_COMPARE_DIFFERENCES,
            returned_detail_bytes,
            max_detail_bytes: MAX_COMPARE_DETAIL_BYTES,
            differences_truncated: returned_differences < difference_count,
            truncated_by_count,
            truncated_by_size,
            differences,
        })
    }

    /// Export one worksheet as bounded deterministic text.
    #[tool(
        description = "Export one worksheet as bounded CSV, Markdown, or HTML",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workbook_export_sheet(
        &self,
        Parameters(params): Parameters<ExportSheetParams>,
    ) -> Result<Json<ExportSheetResult>, String> {
        let state = self.state()?;
        let session = find_session(&state, &params.session_id)?;
        let sheet = find_worksheet(session.spreadsheet.workbook(), &params.sheet)?;
        let export_limit = MAX_OUTPUT_BYTES / 2;
        let content = match params.format {
            ExportFormat::Csv => export_csv(
                sheet,
                CsvOptions {
                    max_output_bytes: export_limit,
                    ..CsvOptions::default()
                },
            )
            .map_err(|source| error("RXLS_MCP_EXPORT_FAILED", source))?,
            ExportFormat::Markdown => export_markdown(sheet, export_limit)
                .map_err(|source| error("RXLS_MCP_EXPORT_FAILED", source))?,
            ExportFormat::Html => export_html(sheet, export_limit)
                .map_err(|source| error("RXLS_MCP_EXPORT_FAILED", source))?,
        };
        checked_json(ExportSheetResult {
            session_id: session.id.clone(),
            sheet: sheet.name.clone(),
            format: params.format.as_str().to_string(),
            bytes: content.len(),
            content,
        })
    }

    /// Atomically apply a bounded batch of package-preserving XLSX/XLSM edits.
    #[tool(
        description = "Atomically set values or write formulas to up to 100 XLSX/XLSM cells while preserving untouched package parts",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workbook_set_cells(
        &self,
        Parameters(params): Parameters<SetCellsParams>,
    ) -> Result<Json<SetCellsResult>, String> {
        let parsed = validate_edits(&params.edits)?;
        let mut state = self.state()?;
        let previous_bytes = find_session(&state, &params.session_id)?.current_bytes;
        let other_bytes = state.retained_bytes.saturating_sub(previous_bytes);
        let session = find_session_mut(&mut state, &params.session_id)?;
        if !session.format.is_editable_ooxml()
            || session.spreadsheet.edit_capability() != &EditCapability::ReadWrite
        {
            return Err(error(
                "RXLS_MCP_READ_ONLY",
                "only losslessly retained XLSX/XLSM sessions can be edited",
            ));
        }
        find_worksheet(session.spreadsheet.workbook(), &params.sheet)?;

        let mut candidate = session.spreadsheet.clone();
        for (edit, address) in params.edits.iter().zip(parsed) {
            apply_edit(&mut candidate, &params.sheet, edit, address)?;
        }
        let candidate_parts = candidate.edited_parts().to_vec();
        let bytes = candidate.save().map_err(|source| {
            error(
                "RXLS_MCP_EDIT_FAILED",
                format!("edited package could not be serialized: {source}"),
            )
        })?;
        enforce_workbook_size(bytes.len())?;
        if other_bytes.saturating_add(bytes.len()) > MAX_SESSION_BYTES {
            return Err(error(
                "RXLS_MCP_MEMORY_LIMIT",
                format!("open sessions may retain at most {MAX_SESSION_BYTES} current bytes"),
            ));
        }
        let reopened = Spreadsheet::open(&bytes).map_err(|source| {
            error(
                "RXLS_MCP_EDIT_FAILED",
                format!("edited package did not reopen cleanly: {source}"),
            )
        })?;
        if reopened.edit_capability() != &EditCapability::ReadWrite {
            return Err(error(
                "RXLS_MCP_EDIT_FAILED",
                "edited package lost preservation edit capability",
            ));
        }
        let mut edited_parts = session.edited_parts.clone();
        edited_parts.extend(candidate_parts);
        let current_sha256 = sha256(&bytes);
        let checked = checked_json(SetCellsResult {
            session_id: session.id.clone(),
            applied_edits: params.edits.len(),
            current_bytes: bytes.len(),
            current_sha256: current_sha256.clone(),
            edited_parts: edited_parts.iter().cloned().collect(),
        })?;
        session.edited_parts = edited_parts;
        session.spreadsheet = reopened;
        session.current_bytes = bytes.len();
        session.current_sha256 = current_sha256;
        state.retained_bytes = other_bytes + bytes.len();
        Ok(checked)
    }

    /// Save current retained package bytes to a new path without overwriting.
    #[tool(
        description = "Save an edited XLSX/XLSM session to a new same-format path without overwriting any file",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workbook_save_copy(
        &self,
        Parameters(params): Parameters<SaveCopyParams>,
    ) -> Result<Json<SaveCopyResult>, String> {
        let destination = self.resolve_new(&params.path)?;
        let destination_format = WorkbookFormat::from_path(&destination)?;
        let (id, source_format, bytes, edited_parts) = {
            let state = self.state()?;
            let session = find_session(&state, &params.session_id)?;
            if !session.format.is_editable_ooxml()
                || session.spreadsheet.edit_capability() != &EditCapability::ReadWrite
            {
                return Err(error(
                    "RXLS_MCP_READ_ONLY",
                    "only losslessly retained XLSX/XLSM sessions can be saved",
                ));
            }
            let bytes = session.spreadsheet.save().map_err(|source| {
                error(
                    "RXLS_MCP_SAVE_FAILED",
                    format!("retained package could not be serialized: {source}"),
                )
            })?;
            (
                session.id.clone(),
                session.format,
                bytes,
                session.edited_parts.iter().cloned().collect::<Vec<_>>(),
            )
        };
        if destination_format != source_format {
            return Err(error(
                "RXLS_MCP_FORMAT_MISMATCH",
                format!(
                    "save-copy extension must remain .{}",
                    source_format.as_str()
                ),
            ));
        }
        let checked = checked_json(SaveCopyResult {
            session_id: id,
            path: destination.to_string_lossy().into_owned(),
            bytes: bytes.len(),
            sha256: sha256(&bytes),
            edited_parts,
        })?;
        publish_new_file(&destination, &bytes)?;
        Ok(checked)
    }

    /// Close a workbook session and release its retained bytes.
    #[tool(
        description = "Close an rxls workbook session and release its retained memory",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workbook_close(
        &self,
        Parameters(params): Parameters<SessionParams>,
    ) -> Result<Json<CloseSessionResult>, String> {
        let mut state = self.state()?;
        let session = state.sessions.remove(&params.session_id).ok_or_else(|| {
            error(
                "RXLS_MCP_SESSION_NOT_FOUND",
                format!("unknown session: {}", params.session_id),
            )
        })?;
        state.retained_bytes = state.retained_bytes.saturating_sub(session.current_bytes);
        checked_json(CloseSessionResult {
            session_id: session.id,
            released_bytes: session.current_bytes,
            remaining_sessions: state.sessions.len(),
        })
    }
}

#[tool_handler(
    instructions = "Open a workbook first. Use returned session IDs for bounded reads, comparisons, exports, preservation-aware XLSX/XLSM edits, save-copy, and close. All paths remain below server-configured roots."
)]
impl ServerHandler for RxlsMcpServer {}

impl Session {
    fn summary(&self) -> SessionSummary {
        SessionSummary {
            session_id: self.id.clone(),
            path: self.path.to_string_lossy().into_owned(),
            format: self.format.as_str().to_string(),
            edit_capability: capability_code(self.spreadsheet.edit_capability()).to_string(),
            current_bytes: self.current_bytes,
            source_sha256: self.source_sha256.clone(),
            current_sha256: self.current_sha256.clone(),
            sheet_count: self.spreadsheet.workbook().sheets.len(),
        }
    }
}

fn read_bounded_file(path: &Path) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|_| {
        error(
            "RXLS_MCP_READ_FAILED",
            format!("could not open workbook: {}", path.display()),
        )
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| error("RXLS_MCP_READ_FAILED", "could not inspect workbook size"))?;
    if metadata.len() > MAX_WORKBOOK_BYTES as u64 {
        return Err(error(
            "RXLS_MCP_FILE_TOO_LARGE",
            format!("workbook exceeds the {MAX_WORKBOOK_BYTES}-byte limit"),
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    file.take((MAX_WORKBOOK_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| error("RXLS_MCP_READ_FAILED", "could not read workbook bytes"))?;
    enforce_workbook_size(bytes.len())?;
    Ok(bytes)
}

fn enforce_workbook_size(bytes: usize) -> Result<(), String> {
    if bytes > MAX_WORKBOOK_BYTES {
        Err(error(
            "RXLS_MCP_FILE_TOO_LARGE",
            format!("workbook exceeds the {MAX_WORKBOOK_BYTES}-byte limit"),
        ))
    } else {
        Ok(())
    }
}

fn validate_detected_format(
    format: WorkbookFormat,
    capability: &EditCapability,
) -> Result<(), String> {
    let matches = matches!(
        (format, capability),
        (
            WorkbookFormat::Xls,
            EditCapability::ReadOnly(EditReadOnlyReason::LegacyBiff)
        ) | (
            WorkbookFormat::Xlsb,
            EditCapability::ReadOnly(EditReadOnlyReason::BinaryPackage)
        ) | (
            WorkbookFormat::Ods,
            EditCapability::ReadOnly(EditReadOnlyReason::OpenDocument)
        ) | (
            WorkbookFormat::Xlsx | WorkbookFormat::Xlsm,
            EditCapability::ReadWrite
        ) | (
            WorkbookFormat::Xlsx | WorkbookFormat::Xlsm,
            EditCapability::ReadOnly(EditReadOnlyReason::PackageMetadataLoss),
        )
    );
    if matches {
        Ok(())
    } else {
        Err(error(
            "RXLS_MCP_FORMAT_MISMATCH",
            "file contents do not match the workbook extension",
        ))
    }
}

fn capability_code(capability: &EditCapability) -> &'static str {
    match capability {
        EditCapability::ReadWrite => "read_write_preserving",
        EditCapability::ReadOnly(EditReadOnlyReason::LegacyBiff) => "read_only_legacy_biff",
        EditCapability::ReadOnly(EditReadOnlyReason::BinaryPackage) => "read_only_binary_package",
        EditCapability::ReadOnly(EditReadOnlyReason::OpenDocument) => "read_only_open_document",
        EditCapability::ReadOnly(EditReadOnlyReason::PackageMetadataLoss) => {
            "read_only_package_metadata_loss"
        }
    }
}

fn find_session<'a>(state: &'a ServerState, id: &str) -> Result<&'a Session, String> {
    state.sessions.get(id).ok_or_else(|| {
        error(
            "RXLS_MCP_SESSION_NOT_FOUND",
            format!("unknown session: {id}"),
        )
    })
}

fn find_session_mut<'a>(state: &'a mut ServerState, id: &str) -> Result<&'a mut Session, String> {
    state.sessions.get_mut(id).ok_or_else(|| {
        error(
            "RXLS_MCP_SESSION_NOT_FOUND",
            format!("unknown session: {id}"),
        )
    })
}

fn find_worksheet<'a>(workbook: &'a rxls::Workbook, name: &str) -> Result<&'a Sheet, String> {
    let sheet = workbook
        .sheet_by_name(name)
        .ok_or_else(|| error("RXLS_MCP_SHEET_NOT_FOUND", format!("unknown sheet: {name}")))?;
    if !sheet.is_worksheet {
        return Err(error(
            "RXLS_MCP_NOT_A_WORKSHEET",
            format!("sheet does not contain a worksheet grid: {name}"),
        ));
    }
    Ok(sheet)
}

fn sheet_summaries(workbook: &rxls::Workbook) -> Vec<SheetSummary> {
    workbook.sheets.iter().map(sheet_summary).collect()
}

fn sheet_summary(sheet: &Sheet) -> SheetSummary {
    SheetSummary {
        name: sheet.name.clone(),
        sheet_type: match sheet.sheet_type() {
            SheetType::WorkSheet => "worksheet",
            SheetType::DialogSheet => "dialog_sheet",
            SheetType::MacroSheet => "macro_sheet",
            SheetType::ChartSheet => "chart_sheet",
            SheetType::Vba => "vba",
        }
        .to_string(),
        visibility: match sheet.visible() {
            SheetVisible::Visible => "visible",
            SheetVisible::Hidden => "hidden",
            SheetVisible::VeryHidden => "very_hidden",
        }
        .to_string(),
        used_range: sheet.dimensions().map(|(row0, col0, row1, col1)| {
            format!(
                "{}:{}",
                format_cell(CellAddress {
                    row: row0,
                    col: col0
                }),
                format_cell(CellAddress {
                    row: row1,
                    col: col1
                })
            )
        }),
        populated_cells: sheet.display_cells().count(),
    }
}

fn typed_cell(cell: &Cell) -> TypedCell {
    match cell {
        Cell::Text(value) => TypedCell::Text {
            value: value.clone(),
        },
        Cell::Number(value) => TypedCell::Number { value: *value },
        Cell::Date(serial) => TypedCell::Date { serial: *serial },
        Cell::Bool(value) => TypedCell::Boolean { value: *value },
        Cell::Error(value) => TypedCell::Error {
            value: value.clone(),
        },
        Cell::Formula { formula, cached } => TypedCell::Formula {
            formula: formula.clone(),
            cached: Box::new(typed_cell(cached)),
        },
    }
}

fn compared_cell(sheet: &Sheet, row: u32, col: u16) -> ComparedCell {
    ComparedCell {
        value: sheet.cell(row, col).map(typed_cell),
        display: sheet.formatted(row, col).map(str::to_string),
    }
}

fn validate_edits(edits: &[CellEdit]) -> Result<Vec<CellAddress>, String> {
    if edits.is_empty() {
        return Err(error(
            "RXLS_MCP_INVALID_EDIT",
            "at least one cell edit is required",
        ));
    }
    if edits.len() > MAX_BATCH_EDITS {
        return Err(error(
            "RXLS_MCP_EDIT_LIMIT",
            format!("at most {MAX_BATCH_EDITS} cell edits may be applied at once"),
        ));
    }
    let mut addresses = Vec::with_capacity(edits.len());
    let mut seen = BTreeSet::new();
    for edit in edits {
        let address = parse_cell(edit.address())?;
        if !seen.insert((address.row, address.col)) {
            return Err(error(
                "RXLS_MCP_DUPLICATE_EDIT",
                format!("cell appears more than once: {}", edit.address()),
            ));
        }
        match edit {
            CellEdit::Set { value, .. } => validate_input_value(value)?,
            CellEdit::Formula {
                formula, cached, ..
            } => {
                let source = formula.strip_prefix('=').unwrap_or(formula).trim();
                if source.is_empty() {
                    return Err(error(
                        "RXLS_MCP_INVALID_FORMULA",
                        "formula source must not be empty",
                    ));
                }
                validate_input_value(cached)?;
            }
        }
        addresses.push(address);
    }
    Ok(addresses)
}

fn validate_input_value(value: &InputValue) -> Result<(), String> {
    match value {
        InputValue::Number(value) | InputValue::Date(value) if !value.is_finite() => Err(error(
            "RXLS_MCP_INVALID_NUMBER",
            "numeric and date values must be finite",
        )),
        InputValue::Error(value) if CellErrorType::from_excel_error(value).is_none() => Err(error(
            "RXLS_MCP_INVALID_ERROR",
            "error value must be a standard Excel error such as #N/A or #DIV/0!",
        )),
        _ => Ok(()),
    }
}

fn input_cell(value: &InputValue) -> Cell {
    match value {
        InputValue::Text(value) => Cell::Text(value.clone()),
        InputValue::Number(value) => Cell::Number(*value),
        InputValue::Date(serial) => Cell::Date(*serial),
        InputValue::Boolean(value) => Cell::Bool(*value),
        InputValue::Error(value) => Cell::Error(value.clone()),
    }
}

fn apply_edit(
    spreadsheet: &mut Spreadsheet,
    sheet: &str,
    edit: &CellEdit,
    address: CellAddress,
) -> Result<(), String> {
    let result = match edit {
        CellEdit::Set { value, .. } => {
            spreadsheet.set_cell_value(sheet, address.row, address.col, input_cell(value))
        }
        CellEdit::Formula {
            formula, cached, ..
        } => spreadsheet.set_cell_formula(
            sheet,
            address.row,
            address.col,
            formula,
            input_cell(cached),
        ),
    };
    result.map_err(|source| {
        error(
            "RXLS_MCP_EDIT_FAILED",
            format!("{}: {source}", format_cell(address)),
        )
    })
}

fn publish_new_file(destination: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = destination.parent().unwrap_or(Path::new("."));
    let file_name = destination.file_name().ok_or_else(|| {
        error(
            "RXLS_MCP_INVALID_DESTINATION",
            "destination must include a file name",
        )
    })?;
    let mut temporary = None;
    for _ in 0..128 {
        let ordinal = SAVE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{}.rxls-mcp-{}-{ordinal}.tmp",
            file_name.to_string_lossy(),
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => {
                temporary = Some((temp_path, file));
                break;
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                return Err(error(
                    "RXLS_MCP_SAVE_FAILED",
                    "could not create a sibling temporary file",
                ));
            }
        }
    }
    let (temp_path, mut file) = temporary.ok_or_else(|| {
        error(
            "RXLS_MCP_SAVE_FAILED",
            "could not allocate a unique sibling temporary file",
        )
    })?;
    if file.write_all(bytes).and_then(|_| file.sync_all()).is_err() {
        drop(file);
        let _ = fs::remove_file(&temp_path);
        return Err(error(
            "RXLS_MCP_SAVE_FAILED",
            "could not write and sync the complete workbook",
        ));
    }
    drop(file);
    if let Err(source) = fs::hard_link(&temp_path, destination) {
        let _ = fs::remove_file(&temp_path);
        return if source.kind() == std::io::ErrorKind::AlreadyExists {
            Err(error(
                "RXLS_MCP_DESTINATION_EXISTS",
                format!("destination already exists: {}", destination.display()),
            ))
        } else {
            Err(error(
                "RXLS_MCP_SAVE_FAILED",
                "filesystem could not atomically publish the no-overwrite save copy",
            ))
        };
    }
    let _ = fs::remove_file(&temp_path);
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| {
            error(
                "RXLS_MCP_SAVE_FAILED",
                "could not sync the destination directory",
            )
        })?;
    Ok(())
}

fn checked_json<T>(value: T) -> Result<Json<T>, String>
where
    T: Serialize + JsonSchema,
{
    let bytes = serde_json::to_vec(&value).map_err(|_| {
        error(
            "RXLS_MCP_SERIALIZE_FAILED",
            "tool result could not be serialized",
        )
    })?;
    if bytes.len() > MAX_OUTPUT_BYTES {
        return Err(error(
            "RXLS_MCP_OUTPUT_TOO_LARGE",
            format!("tool result exceeds the {MAX_OUTPUT_BYTES}-byte limit"),
        ));
    }
    Ok(Json(value))
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn error(code: &str, message: impl std::fmt::Display) -> String {
    format!("{code}: {message}")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rmcp::model::CallToolRequestParams;
    use rmcp::ServiceExt;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn write_sample_xlsx(path: &Path) {
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write_string(0, 0, "name");
        sheet.write_string(0, 1, "amount");
        sheet.write_string(1, 0, "alpha");
        sheet.write_number(1, 1, 10.0);
        let bytes = workbook.to_xlsx_checked().expect("author sample workbook");
        fs::write(path, bytes).expect("write sample workbook");
    }

    fn write_number_column_xlsx(path: &Path, offset: f64, cells: usize) {
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("Data");
        for row in 0..cells {
            sheet.write_number(
                u32::try_from(row).expect("test row fits in u32"),
                0,
                row as f64 + offset,
            );
        }
        let bytes = workbook
            .to_xlsx_checked()
            .expect("author comparison workbook");
        fs::write(path, bytes).expect("write comparison workbook");
    }

    fn write_text_column_xlsx(path: &Path, prefix: &str, cells: usize) {
        let mut workbook = rxls::Workbook::new();
        let sheet = workbook.add_sheet("Data");
        let suffix = "x".repeat(20_000);
        for row in 0..cells {
            sheet.write_string(
                u32::try_from(row).expect("test row fits in u32"),
                0,
                format!("{prefix}-{row:04}-{suffix}"),
            );
        }
        let bytes = workbook
            .to_xlsx_checked()
            .expect("author long-text workbook");
        fs::write(path, bytes).expect("write long-text workbook");
    }

    fn server_for(root: &TempDir) -> RxlsMcpServer {
        RxlsMcpServer::new(
            ServerConfig::new(vec![root.path().to_path_buf()]).expect("configure server root"),
        )
    }

    fn open_sample(server: &RxlsMcpServer, path: &Path) -> OpenWorkbookResult {
        server
            .workbook_open(Parameters(OpenWorkbookParams {
                path: path.to_string_lossy().into_owned(),
            }))
            .expect("open sample")
            .0
    }

    #[test]
    fn edits_reopen_in_session_and_save_without_overwrite() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("source.xlsx");
        write_sample_xlsx(&source);
        let server = server_for(&root);
        let opened = open_sample(&server, &source);
        assert_eq!(opened.session.edit_capability, "read_write_preserving");

        let edited = server
            .workbook_set_cells(Parameters(SetCellsParams {
                session_id: opened.session.session_id.clone(),
                sheet: "Data".to_string(),
                edits: vec![
                    CellEdit::Set {
                        cell: "B2".to_string(),
                        value: InputValue::Number(42.0),
                    },
                    CellEdit::Formula {
                        cell: "C2".to_string(),
                        formula: "=B2*2".to_string(),
                        cached: InputValue::Number(84.0),
                    },
                ],
            }))
            .expect("edit cells")
            .0;
        assert_eq!(edited.applied_edits, 2);
        assert_ne!(opened.session.source_sha256, edited.current_sha256);
        assert!(!edited.edited_parts.is_empty());

        let read = server
            .workbook_read_range(Parameters(ReadRangeParams {
                session_id: opened.session.session_id.clone(),
                sheet: "Data".to_string(),
                range: "A1:C2".to_string(),
            }))
            .expect("read edited range")
            .0;
        assert!(matches!(
            read.rows[1][1].value,
            Some(TypedCell::Number { value: 42.0 })
        ));
        assert!(matches!(
            &read.rows[1][2].value,
            Some(TypedCell::Formula { formula, cached })
                if formula == "B2*2" && matches!(**cached, TypedCell::Number { value: 84.0 })
        ));

        let destination = root.path().join("edited.xlsx");
        let saved = server
            .workbook_save_copy(Parameters(SaveCopyParams {
                session_id: opened.session.session_id.clone(),
                path: destination.to_string_lossy().into_owned(),
            }))
            .expect("save copy")
            .0;
        assert_eq!(saved.sha256, edited.current_sha256);
        let reopened = Spreadsheet::open(&fs::read(&destination).unwrap()).unwrap();
        assert!(matches!(
            reopened.workbook().sheet_by_name("Data").unwrap().cell(1, 1),
            Some(Cell::Number(value)) if *value == 42.0
        ));

        let error = server
            .workbook_save_copy(Parameters(SaveCopyParams {
                session_id: opened.session.session_id,
                path: destination.to_string_lossy().into_owned(),
            }))
            .err()
            .expect("existing destination must fail");
        assert!(error.starts_with("RXLS_MCP_DESTINATION_EXISTS:"));
    }

    #[test]
    fn enforces_root_and_format_capability() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("outside.xlsx");
        write_sample_xlsx(&outside_file);
        let server = server_for(&root);
        let error = server
            .workbook_open(Parameters(OpenWorkbookParams {
                path: outside_file.to_string_lossy().into_owned(),
            }))
            .err()
            .expect("outside-root open must fail");
        assert!(error.starts_with("RXLS_MCP_PATH_OUTSIDE_ROOT:"));

        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/xls/korean-unicode-biff8.xls");
        let local_xls = root.path().join("legacy.xls");
        fs::copy(fixture, &local_xls).unwrap();
        let opened = open_sample(&server, &local_xls);
        assert_eq!(opened.session.edit_capability, "read_only_legacy_biff");
        let error = server
            .workbook_set_cells(Parameters(SetCellsParams {
                session_id: opened.session.session_id,
                sheet: opened.sheets[0].name.clone(),
                edits: vec![CellEdit::Set {
                    cell: "A1".to_string(),
                    value: InputValue::Text("blocked".to_string()),
                }],
            }))
            .err()
            .expect("legacy edit must fail");
        assert!(error.starts_with("RXLS_MCP_READ_ONLY:"));
    }

    #[test]
    fn rejects_duplicate_and_oversized_edit_batches() {
        let duplicate = vec![
            CellEdit::Set {
                cell: "A1".to_string(),
                value: InputValue::Text("first".to_string()),
            },
            CellEdit::Set {
                cell: "$A$1".to_string(),
                value: InputValue::Boolean(true),
            },
        ];
        assert!(validate_edits(&duplicate)
            .unwrap_err()
            .starts_with("RXLS_MCP_DUPLICATE_EDIT:"));
        let too_many = (0..=MAX_BATCH_EDITS)
            .map(|index| CellEdit::Set {
                cell: format!("A{}", index + 1),
                value: InputValue::Boolean(true),
            })
            .collect::<Vec<_>>();
        assert!(validate_edits(&too_many)
            .unwrap_err()
            .starts_with("RXLS_MCP_EDIT_LIMIT:"));
    }

    #[test]
    fn compares_bounded_ranges_and_reports_truncation() {
        let root = TempDir::new().unwrap();
        let left_path = root.path().join("left.xlsx");
        let right_path = root.path().join("right.xlsx");
        write_number_column_xlsx(&left_path, 0.0, 101);
        write_number_column_xlsx(&right_path, 1.0, 101);
        let server = server_for(&root);
        let left = open_sample(&server, &left_path);
        let right = open_sample(&server, &right_path);

        let compared = server
            .workbook_compare(Parameters(CompareWorkbooksParams {
                left_session_id: left.session.session_id.clone(),
                left_sheet: "Data".to_string(),
                right_session_id: right.session.session_id,
                right_sheet: "Data".to_string(),
                range: "A1:A101".to_string(),
            }))
            .expect("compare workbooks")
            .0;
        assert!(!compared.identical);
        assert_eq!(compared.compared_cells, 101);
        assert_eq!(compared.difference_count, 101);
        assert_eq!(compared.returned_differences, MAX_COMPARE_DIFFERENCES);
        assert_eq!(compared.max_returned_differences, MAX_COMPARE_DIFFERENCES);
        assert!(compared.returned_detail_bytes <= compared.max_detail_bytes);
        assert!(compared.differences_truncated);
        assert!(compared.truncated_by_count);
        assert!(!compared.truncated_by_size);
        assert_eq!(compared.differences.first().unwrap().address, "A1");
        assert_eq!(compared.differences.last().unwrap().address, "A100");
        assert!(matches!(
            compared.differences[0].left.value,
            Some(TypedCell::Number { value: 0.0 })
        ));
        assert!(matches!(
            compared.differences[0].right.value,
            Some(TypedCell::Number { value: 1.0 })
        ));

        let identical = server
            .workbook_compare(Parameters(CompareWorkbooksParams {
                left_session_id: left.session.session_id.clone(),
                left_sheet: "Data".to_string(),
                right_session_id: left.session.session_id,
                right_sheet: "Data".to_string(),
                range: "A1:A101".to_string(),
            }))
            .expect("compare identical session")
            .0;
        assert!(identical.identical);
        assert_eq!(identical.difference_count, 0);
        assert!(identical.differences.is_empty());
        assert!(!identical.differences_truncated);
        assert!(!identical.truncated_by_count);
        assert!(!identical.truncated_by_size);
    }

    #[test]
    fn comparison_detail_bytes_remain_bounded_for_long_cells() {
        let root = TempDir::new().unwrap();
        let left_path = root.path().join("long-left.xlsx");
        let right_path = root.path().join("long-right.xlsx");
        write_text_column_xlsx(&left_path, "left", 20);
        write_text_column_xlsx(&right_path, "right", 20);
        let server = server_for(&root);
        let left = open_sample(&server, &left_path);
        let right = open_sample(&server, &right_path);

        let compared = server
            .workbook_compare(Parameters(CompareWorkbooksParams {
                left_session_id: left.session.session_id,
                left_sheet: "Data".to_string(),
                right_session_id: right.session.session_id,
                right_sheet: "Data".to_string(),
                range: "A1:A20".to_string(),
            }))
            .expect("compare long cells")
            .0;
        assert_eq!(compared.difference_count, 20);
        assert!(compared.returned_differences < compared.difference_count);
        assert!(compared.returned_detail_bytes <= MAX_COMPARE_DETAIL_BYTES);
        assert!(compared.differences_truncated);
        assert!(!compared.truncated_by_count);
        assert!(compared.truncated_by_size);
    }

    #[tokio::test]
    async fn exposes_nine_tools_and_structured_results_over_mcp() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("protocol.xlsx");
        write_sample_xlsx(&source);
        let server = server_for(&root);
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            server
                .serve(server_transport)
                .await
                .expect("start server")
                .waiting()
                .await
                .expect("wait for server");
        });
        let client = ().serve(client_transport).await.expect("start client");

        let tools = client.list_tools(None).await.expect("list tools");
        let names = tools
            .tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            names,
            BTreeSet::from([
                "workbook_close",
                "workbook_compare",
                "workbook_export_sheet",
                "workbook_inspect",
                "workbook_list_sessions",
                "workbook_open",
                "workbook_read_range",
                "workbook_save_copy",
                "workbook_set_cells",
            ])
        );
        assert!(tools.tools.iter().all(|tool| tool.output_schema.is_some()));
        assert!(tools.tools.iter().all(|tool| {
            tool.annotations
                .as_ref()
                .is_some_and(|annotations| annotations.open_world_hint == Some(false))
        }));
        for name in [
            "workbook_compare",
            "workbook_export_sheet",
            "workbook_inspect",
            "workbook_list_sessions",
            "workbook_read_range",
        ] {
            let tool = tools
                .tools
                .iter()
                .find(|tool| tool.name == name)
                .expect("read-only tool");
            assert_eq!(
                tool.annotations
                    .as_ref()
                    .and_then(|annotations| annotations.read_only_hint),
                Some(true)
            );
        }

        let arguments = json!({ "path": source.to_string_lossy() })
            .as_object()
            .unwrap()
            .clone();
        let result = client
            .call_tool(CallToolRequestParams::new("workbook_open").with_arguments(arguments))
            .await
            .expect("call workbook_open");
        assert_ne!(result.is_error, Some(true));
        assert_eq!(
            result
                .structured_content
                .as_ref()
                .and_then(|value| value.get("format"))
                .and_then(serde_json::Value::as_str),
            Some("xlsx")
        );
        let session_id = result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("session_id"))
            .and_then(serde_json::Value::as_str)
            .expect("open result session ID");
        let arguments = json!({
            "left_session_id": session_id,
            "left_sheet": "Data",
            "right_session_id": session_id,
            "right_sheet": "Data",
            "range": "A1:B2"
        })
        .as_object()
        .unwrap()
        .clone();
        let compared = client
            .call_tool(CallToolRequestParams::new("workbook_compare").with_arguments(arguments))
            .await
            .expect("call workbook_compare");
        assert_ne!(compared.is_error, Some(true));
        assert_eq!(
            compared
                .structured_content
                .as_ref()
                .and_then(|value| value.get("identical"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );

        client.cancel().await.expect("cancel client");
        server_task.await.expect("join server");
    }
}
