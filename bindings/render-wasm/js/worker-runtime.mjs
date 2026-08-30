import {
  MAX_DPI,
  MAX_EDIT_HISTORY_BYTES,
  MAX_EDIT_HISTORY_ENTRIES,
  MAX_INPUT_BYTES,
  MAX_MANUAL_PAGE_BREAKS,
  MAX_OPEN_DOCUMENTS,
  MAX_OPEN_RESOURCE_BYTES,
  MAX_OPTIONS_BYTES,
  MAX_OUTPUT_BYTES,
  MAX_PAGES,
  MAX_PENDING_REQUESTS,
  MAX_PENDING_RESOURCE_BYTES,
  MAX_PNG_BYTES,
  MAX_SHEETS,
  MIN_DPI,
  PROTOCOL,
  RenderProtocolError,
  asBytes,
  boundedIndex,
  encodeFontBundle,
  limitError,
  normalizeError,
  optionsJson,
  parseWorkerMessage,
  positiveInteger,
  preflightRequest,
  validateDocumentId,
  validateRange,
  validateRequestId,
  validateSvgOutput
} from "./protocol.mjs";

const NON_CANCELLABLE_ACTIVE_OPERATIONS = new Set([
  "close",
  "set-cell",
  "set-document-properties",
  "undo-edit",
  "redo-edit"
]);

export class RenderWorkerRuntime {
  #wasm;
  #send;
  #documents = new Map();
  #resourceBytes = 0;
  #cancelled = new Set();
  #queue = [];
  #queuedResourceBytes = 0;
  #activeResourceBytes = 0;
  #requestIds = new Set();
  #activeRequestId = null;
  #activeOperation = null;
  #draining = false;
  #capabilities;
  #maxOutputBytes;
  #maxPngBytes;
  #maxEditHistoryEntries;
  #maxEditHistoryBytes;

  constructor({ wasm, send }) {
    if (wasm === null || typeof wasm !== "object") {
      throw new TypeError("wasm must be an initialized rxls-render-wasm module");
    }
    if (typeof send !== "function") {
      throw new TypeError("send must be a function");
    }
    this.#wasm = wasm;
    this.#send = send;
    this.#capabilities = parseBoundedJson(
      wasm.capabilitiesJson?.(),
      "capabilities",
      MAX_OPTIONS_BYTES
    );
    this.#maxOutputBytes = boundedCapability(
      this.#capabilities?.limits?.maxOutputBytes,
      MAX_OUTPUT_BYTES,
      "maxOutputBytes"
    );
    this.#maxPngBytes = boundedCapability(
      this.#capabilities?.limits?.maxPngBytes ?? MAX_PNG_BYTES,
      MAX_PNG_BYTES,
      "maxPngBytes"
    );
    this.#maxEditHistoryEntries = boundedCapability(
      this.#capabilities?.editing?.maxHistoryEntries,
      MAX_EDIT_HISTORY_ENTRIES,
      "maxHistoryEntries"
    );
    this.#maxEditHistoryBytes = boundedCapability(
      this.#capabilities?.editing?.maxHistoryBytes,
      MAX_EDIT_HISTORY_BYTES,
      "maxHistoryBytes"
    );
  }

  receive(rawMessage) {
    let message;
    let resourceBytes = 0;
    try {
      message = parseWorkerMessage(rawMessage);
      if (message.type === "request") {
        resourceBytes = preflightRequest(message);
      }
    } catch (error) {
      const requestId = responseRequestId(rawMessage);
      this.#sendResult(requestId, false, null, normalizeError(error));
      return;
    }
    if (message.type === "cancel") {
      this.#cancel(message.requestId);
      return;
    }
    if (this.#requestIds.has(message.requestId)) {
      this.#sendResult(
        message.requestId,
        false,
        null,
        normalizeError(
          new RenderProtocolError(
            "duplicate_request_id",
            "requestId is already pending",
            "requestId"
          )
        )
      );
      return;
    }
    const pending = this.#queue.length + (this.#activeRequestId === null ? 0 : 1);
    if (pending >= MAX_PENDING_REQUESTS) {
      this.#sendResult(
        message.requestId,
        false,
        null,
        normalizeError(
          limitError("pendingRequests", MAX_PENDING_REQUESTS, pending + 1, "worker")
        )
      );
      return;
    }
    const pendingResourceBytes =
      this.#resourceBytes +
      this.#queuedResourceBytes +
      this.#activeResourceBytes +
      resourceBytes;
    if (pendingResourceBytes > MAX_PENDING_RESOURCE_BYTES) {
      this.#sendResult(
        message.requestId,
        false,
        null,
        normalizeError(
          limitError(
            "pendingResourceBytes",
            MAX_PENDING_RESOURCE_BYTES,
            pendingResourceBytes,
            "worker"
          )
        )
      );
      return;
    }
    this.#requestIds.add(message.requestId);
    this.#queuedResourceBytes += resourceBytes;
    this.#queue.push({ message, resourceBytes });
    void this.#drain();
  }

  closeAll() {
    for (const row of this.#queue.splice(0)) {
      this.#queuedResourceBytes -= row.resourceBytes;
      this.#requestIds.delete(row.message.requestId);
      this.#sendResult(
        row.message.requestId,
        false,
        null,
        normalizeError(cancelledError())
      );
    }
    if (this.#activeRequestId !== null) {
      this.#cancelled.add(this.#activeRequestId);
    }
    for (const document of this.#documents.values()) {
      document.session.free?.();
    }
    this.#documents.clear();
    this.#resourceBytes = 0;
  }

  capabilities() {
    return this.#capabilities;
  }

  async #drain() {
    if (this.#draining) {
      return;
    }
    this.#draining = true;
    try {
      while (this.#queue.length > 0) {
        const row = this.#queue.shift();
        const { message, resourceBytes } = row;
        this.#queuedResourceBytes -= resourceBytes;
        this.#activeResourceBytes = resourceBytes;
        this.#activeRequestId = message.requestId;
        this.#activeOperation = message.operation;
        await this.#run(message);
        this.#requestIds.delete(message.requestId);
        this.#activeRequestId = null;
        this.#activeOperation = null;
        this.#activeResourceBytes = 0;
      }
    } finally {
      this.#activeRequestId = null;
      this.#activeOperation = null;
      this.#activeResourceBytes = 0;
      this.#draining = false;
    }
  }

  #cancel(requestId) {
    const queued = this.#queue.findIndex(({ message }) => message.requestId === requestId);
    if (queued !== -1) {
      const [row] = this.#queue.splice(queued, 1);
      this.#queuedResourceBytes -= row.resourceBytes;
      this.#requestIds.delete(requestId);
      this.#sendResult(requestId, false, null, normalizeError(cancelledError()));
      return;
    }
    if (this.#activeRequestId === requestId) {
      if (NON_CANCELLABLE_ACTIVE_OPERATIONS.has(this.#activeOperation)) {
        return;
      }
      this.#cancelled.add(requestId);
    }
  }

  async #run(message) {
    const { requestId, operation, payload } = message;
    let openTransaction = null;
    try {
      this.#throwIfCancelled(requestId);
      this.#progress(requestId, 0, 3, "accepted");
      await yieldToWorkerMessages();
      this.#throwIfCancelled(requestId);
      this.#progress(requestId, 1, 3, operationStage(operation));
      const result = await this.#execute(operation, payload);
      openTransaction = result?.openTransaction ?? null;
      if (openTransaction) {
        // Synchronous WASM work cannot receive its already-posted cancellation
        // until control returns to the worker event loop. Keep the session
        // provisional across one bounded message turn.
        await yieldToWorkerMessages();
      }
      this.#throwIfCancelled(requestId);
      this.#progress(requestId, 2, 3, "finalizing");
      this.#progress(requestId, 3, 3, "complete");
      if (openTransaction) {
        this.#commitOpen(openTransaction);
      }
      const transfer = result?.transfer ?? [];
      this.#sendResult(requestId, true, result?.value ?? result, null, transfer);
    } catch (error) {
      if (openTransaction) {
        this.#rollbackOpen(openTransaction);
      }
      this.#sendResult(requestId, false, null, normalizeError(error));
    } finally {
      this.#cancelled.delete(requestId);
    }
  }

  async #execute(operation, payload) {
    switch (operation) {
      case "capabilities":
        return this.#capabilities;
      case "open":
        return this.#open(payload);
      case "close":
        return this.#close(payload);
      case "prepare-pages":
        return this.#preparePages(payload);
      case "render-sheet":
        return this.#renderSheet(payload);
      case "render-tile":
        return this.#renderTile(payload);
      case "render-page":
        return this.#renderPage(payload);
      case "render-page-png":
        return this.#renderPagePng(payload);
      case "edit-status":
        return this.#editStatus(payload);
      case "read-cell":
        return this.#readCell(payload);
      case "set-cell":
        return this.#setCell(payload);
      case "set-document-properties":
        return this.#setDocumentProperties(payload);
      case "undo-edit":
        return this.#historyEdit(payload, "undo");
      case "redo-edit":
        return this.#historyEdit(payload, "redo");
      case "save-document":
        return this.#saveDocument(payload);
      default:
        throw new RenderProtocolError("unknown_operation", "operation is not supported");
    }
  }

  async #open(payload) {
    const documentId = validateDocumentId(payload.documentId);
    if (this.#documents.has(documentId)) {
      throw new RenderProtocolError(
        "document_exists",
        "documentId is already open",
        "documentId"
      );
    }
    if (this.#documents.size >= MAX_OPEN_DOCUMENTS) {
      throw limitError(
        "openDocuments",
        MAX_OPEN_DOCUMENTS,
        this.#documents.size + 1,
        "documents"
      );
    }
    const bytes = asBytes(payload.bytes, "payload.bytes");
    if (bytes.byteLength > MAX_INPUT_BYTES) {
      throw limitError("inputBytes", MAX_INPUT_BYTES, bytes.byteLength, "payload.bytes");
    }
    const fontBundle = encodeFontBundle(payload.fontPack);
    const sourceResourceBytes = bytes.byteLength + fontBundle.byteLength;
    const sourceTotal = this.#resourceBytes + sourceResourceBytes;
    if (sourceTotal > MAX_OPEN_RESOURCE_BYTES) {
      throw limitError(
        "openResourceBytes",
        MAX_OPEN_RESOURCE_BYTES,
        sourceTotal,
        "documents"
      );
    }
    const Session = this.#wasm.RenderSession;
    if (typeof Session !== "function") {
      throw new RenderProtocolError(
        "wasm_api_mismatch",
        "initialized wasm module does not export RenderSession",
        "wasm"
      );
    }
    const session = new Session(bytes, fontBundle);
    try {
      const workbook = validateWorkbookInspection(
        parseBoundedJson(
          await session.inspectionJson(),
          "inspection",
          this.#maxOutputBytes
        )
      );
      const editState = validateEditState(
        parseBoundedJson(await session.editStateJson(), "edit state", this.#maxOutputBytes),
        this.#maxEditHistoryEntries,
        this.#maxEditHistoryBytes
      );
      const historyReservationBytes =
        editState.capability === "read-write" ? this.#maxEditHistoryBytes : 0;
      const resourceBytes = sourceResourceBytes + historyReservationBytes;
      const total = this.#resourceBytes + resourceBytes;
      if (total > MAX_OPEN_RESOURCE_BYTES) {
        throw limitError(
          "openResourceBytes",
          MAX_OPEN_RESOURCE_BYTES,
          total,
          "documents"
        );
      }
      return {
        value: { documentId, workbook, editState },
        openTransaction: {
          documentId,
          document: { session, resourceBytes },
          total,
          committed: false
        }
      };
    } catch (error) {
      session.free?.();
      throw error;
    }
  }

  #close(payload) {
    const documentId = validateDocumentId(payload.documentId);
    const document = this.#documents.get(documentId);
    if (!document) {
      return { documentId, closed: false };
    }
    document.session.free?.();
    this.#documents.delete(documentId);
    this.#resourceBytes -= document.resourceBytes;
    return { documentId, closed: true };
  }

  #commitOpen(transaction) {
    this.#documents.set(transaction.documentId, transaction.document);
    this.#resourceBytes = transaction.total;
    transaction.committed = true;
  }

  #rollbackOpen(transaction) {
    if (!transaction.committed) {
      transaction.document.session.free?.();
      return;
    }
    if (this.#documents.get(transaction.documentId) === transaction.document) {
      transaction.document.session.free?.();
      this.#documents.delete(transaction.documentId);
      this.#resourceBytes -= transaction.document.resourceBytes;
    }
  }

  async #preparePages(payload) {
    const { documentId, session } = this.#document(payload);
    const sheetIndex = boundedIndex(
      payload.sheetIndex,
      "payload.sheetIndex",
      MAX_SHEETS,
      "sheets"
    );
    const manifest = validatePrintManifest(
      parseBoundedJson(
        await session.printManifestJson(sheetIndex, optionsJson(payload.options)),
        "print manifest",
        this.#maxOutputBytes
      ),
      sheetIndex
    );
    return { documentId, sheetIndex, manifest };
  }

  async #renderSheet(payload) {
    const { documentId, session } = this.#document(payload);
    const sheetIndex = boundedIndex(
      payload.sheetIndex,
      "payload.sheetIndex",
      MAX_SHEETS,
      "sheets"
    );
    const svg = await session.renderSheetSvg(sheetIndex, optionsJson(payload.options));
    this.#checkSvg(svg);
    return { documentId, sheetIndex, mimeType: "image/svg+xml", svg };
  }

  async #renderTile(payload) {
    const { documentId, session } = this.#document(payload);
    const sheetIndex = boundedIndex(
      payload.sheetIndex,
      "payload.sheetIndex",
      MAX_SHEETS,
      "sheets"
    );
    const range = validateRange(payload.range);
    const svg = await session.renderTileSvg(
      sheetIndex,
      range.firstRow,
      range.firstCol,
      range.lastRow,
      range.lastCol,
      optionsJson(payload.options)
    );
    this.#checkSvg(svg);
    return { documentId, sheetIndex, range, mimeType: "image/svg+xml", svg };
  }

  async #renderPage(payload) {
    const { documentId, session } = this.#document(payload);
    const sheetIndex = boundedIndex(
      payload.sheetIndex,
      "payload.sheetIndex",
      MAX_SHEETS,
      "sheets"
    );
    const pageIndex = boundedIndex(
      payload.pageIndex,
      "payload.pageIndex",
      MAX_PAGES,
      "pages"
    );
    const svg = await session.renderPrintPageSvg(
      sheetIndex,
      pageIndex,
      optionsJson(payload.options)
    );
    this.#checkSvg(svg);
    return { documentId, sheetIndex, pageIndex, mimeType: "image/svg+xml", svg };
  }

  async #renderPagePng(payload) {
    const { documentId, session } = this.#document(payload);
    const sheetIndex = boundedIndex(
      payload.sheetIndex,
      "payload.sheetIndex",
      MAX_SHEETS,
      "sheets"
    );
    const pageIndex = boundedIndex(
      payload.pageIndex,
      "payload.pageIndex",
      MAX_PAGES,
      "pages"
    );
    const dpi = positiveInteger(payload.dpi ?? 96, "payload.dpi");
    if (dpi < MIN_DPI || dpi > MAX_DPI) {
      throw new RenderProtocolError(
        "dpi_out_of_range",
        `dpi must be between ${MIN_DPI} and ${MAX_DPI}`,
        "payload.dpi"
      );
    }
    const value = asBytes(
      await session.renderPrintPagePng(
        sheetIndex,
        pageIndex,
        dpi,
        optionsJson(payload.options)
      ),
      "png"
    );
    this.#checkPng(value);
    const png = value.slice();
    return {
      value: { documentId, sheetIndex, pageIndex, dpi, mimeType: "image/png", bytes: png },
      transfer: [png.buffer]
    };
  }

  async #editStatus(payload) {
    const { documentId, session } = this.#document(payload);
    const editState = validateEditState(
      parseBoundedJson(await session.editStateJson(), "edit state", this.#maxOutputBytes),
      this.#maxEditHistoryEntries,
      this.#maxEditHistoryBytes
    );
    return { documentId, editState };
  }

  async #readCell(payload) {
    const { documentId, session } = this.#document(payload);
    const cell = parseBoundedJson(
      await session.readCellJson(payload.sheetIndex, payload.row, payload.col),
      "cell inspection",
      this.#maxOutputBytes
    );
    if (
      !plainRecord(cell) ||
      !hasExactKeys(cell, ["schemaVersion", "sheetIndex", "row", "col", "value", "formatted"]) ||
      cell.schemaVersion !== 1 ||
      cell.sheetIndex !== payload.sheetIndex ||
      cell.row !== payload.row ||
      cell.col !== payload.col ||
      (cell.formatted !== null && typeof cell.formatted !== "string")
    ) {
      throw invalidCellInspection();
    }
    validateInspectedCell(cell.value);
    return { value: { documentId, ...cell } };
  }

  async #setCell(payload) {
    const { documentId, session } = this.#document(payload);
    const result = await session.setCellJson(
      JSON.stringify({
        sheetIndex: payload.sheetIndex,
        row: payload.row,
        col: payload.col,
        value: payload.value
      })
    );
    return { documentId, ...this.#mutationResult(result) };
  }

  async #setDocumentProperties(payload) {
    const { documentId, session } = this.#document(payload);
    const result = await session.setDocumentPropertiesJson(JSON.stringify(payload.properties));
    return { documentId, ...this.#mutationResult(result) };
  }

  async #historyEdit(payload, direction) {
    const { documentId, session } = this.#document(payload);
    const result =
      direction === "undo" ? await session.undoEditJson() : await session.redoEditJson();
    return { documentId, ...this.#mutationResult(result) };
  }

  async #saveDocument(payload) {
    const { documentId, session } = this.#document(payload);
    const value = asBytes(await session.saveDocumentBytes(), "saved workbook");
    if (value.byteLength > MAX_INPUT_BYTES) {
      throw limitError(
        "savedWorkbookBytes",
        MAX_INPUT_BYTES,
        value.byteLength,
        "output"
      );
    }
    if (!hasZipLocalHeader(value)) {
      throw new RenderProtocolError(
        "wasm_api_mismatch",
        "saved workbook is not an OOXML ZIP package",
        "wasm"
      );
    }
    const bytes = value.slice();
    return {
      value: {
        documentId,
        mimeType: "application/octet-stream",
        bytes
      },
      transfer: [bytes.buffer]
    };
  }

  #mutationResult(json) {
    const result = parseBoundedJson(json, "edit result", this.#maxOutputBytes);
    if (!plainRecord(result) || !hasExactKeys(result, ["workbook", "editState"])) {
      throw new RenderProtocolError(
        "wasm_api_mismatch",
        "edit result does not satisfy the worker contract",
        "wasm"
      );
    }
    const workbook = validateWorkbookInspection(result.workbook);
    const editState = validateEditState(
      result.editState,
      this.#maxEditHistoryEntries,
      this.#maxEditHistoryBytes
    );
    return { workbook, editState };
  }

  #document(payload) {
    const documentId = validateDocumentId(payload.documentId);
    const document = this.#documents.get(documentId);
    if (!document) {
      throw new RenderProtocolError(
        "document_not_open",
        "documentId is not open",
        "documentId"
      );
    }
    return { documentId, session: document.session };
  }

  #checkSvg(svg) {
    validateSvgOutput(svg, this.#maxOutputBytes);
  }

  #checkPng(png) {
    if (png.byteLength > this.#maxPngBytes) {
      throw limitError("pngBytes", this.#maxPngBytes, png.byteLength, "output");
    }
    const signature = [137, 80, 78, 71, 13, 10, 26, 10];
    if (png.byteLength < signature.length || !signature.every((byte, index) => png[index] === byte)) {
      throw new RenderProtocolError("invalid_png", "renderer returned invalid PNG", "output");
    }
  }

  #throwIfCancelled(requestId) {
    if (this.#cancelled.has(requestId)) {
      throw new RenderProtocolError("cancelled", "render request was cancelled", "request");
    }
  }

  #progress(requestId, completed, total, stage) {
    this.#send({ protocol: PROTOCOL, type: "progress", requestId, completed, total, stage });
  }

  #sendResult(requestId, ok, result, error, transfer = []) {
    this.#send(
      { protocol: PROTOCOL, type: "result", requestId, ok, result, error },
      transfer
    );
  }
}

export function installRenderWorker({ wasm, scope = globalThis }) {
  const runtime = new RenderWorkerRuntime({
    wasm,
    send: (message, transfer = []) => scope.postMessage(message, transfer)
  });
  scope.addEventListener("message", (event) => runtime.receive(event.data));
  scope.postMessage({
    protocol: PROTOCOL,
    type: "ready",
    capabilities: runtime.capabilities()
  });
  return runtime;
}

function parseBoundedJson(json, description, maxBytes) {
  if (typeof json !== "string") {
    throw new RenderProtocolError(
      "wasm_api_mismatch",
      `${description} was not JSON text`,
      "wasm"
    );
  }
  const bytes = new TextEncoder().encode(json).byteLength;
  if (bytes > maxBytes) {
    throw limitError("outputBytes", maxBytes, bytes, "output");
  }
  try {
    return JSON.parse(json);
  } catch {
    throw new RenderProtocolError(
      "wasm_api_mismatch",
      `${description} was not valid JSON`,
      "wasm"
    );
  }
}

function boundedCapability(value, hardMax, name) {
  if (!Number.isSafeInteger(value) || value <= 0 || value > hardMax) {
    throw new RenderProtocolError(
      "wasm_api_mismatch",
      `capabilities ${name} is outside the worker hard limit`,
      "wasm"
    );
  }
  return value;
}

function validatePrintManifest(value, expectedSheetIndex) {
  if (!validPrintManifest(value, expectedSheetIndex)) {
    throw new RenderProtocolError(
      "wasm_api_mismatch",
      "print manifest does not satisfy the worker contract",
      "wasm"
    );
  }
  return value;
}

function validPrintManifest(value, expectedSheetIndex) {
  const required = [
    "schema_version",
    "sheet_index",
    "sheet_name",
    "source_report",
    "source_reports",
    "paper",
    "content_rect",
    "manual_row_breaks",
    "manual_col_breaks",
    "scale_permille",
    "logical_pages",
    "sparse_pages_omitted",
    "pages",
    "warnings"
  ];
  if (
    !plainRecord(value) ||
    !hasAllowedKeys(value, required, ["layout_override", "page_order"]) ||
    value.schema_version !== 2 ||
    value.sheet_index !== expectedSheetIndex ||
    !boundedText(value.sheet_name, MAX_OUTPUT_BYTES) ||
    (value.layout_override !== undefined &&
      value.layout_override !== "single_page_sheets") ||
    (value.page_order !== undefined &&
      !["down_then_over", "over_then_down", "unknown"].includes(value.page_order)) ||
    !nonNegativeSafeInteger(value.scale_permille) ||
    !nonNegativeSafeInteger(value.logical_pages) ||
    !nonNegativeSafeInteger(value.sparse_pages_omitted) ||
    !validRenderReport(value.source_report, expectedSheetIndex, value.sheet_name) ||
    !Array.isArray(value.source_reports) ||
    value.source_reports.length === 0 ||
    value.source_reports.length > MAX_PAGES ||
    value.source_reports.some(
      (report) => !validRenderReport(report, expectedSheetIndex, value.sheet_name)
    ) ||
    !validPaper(value.paper) ||
    !validContentRect(value.content_rect) ||
    !validSortedIntegerArray(value.manual_row_breaks) ||
    !validSortedIntegerArray(value.manual_col_breaks) ||
    !Array.isArray(value.pages) ||
    value.pages.length > MAX_PAGES ||
    value.pages.some((page, index) => !validPrintPage(page, index)) ||
    value.logical_pages !== value.pages.length + value.sparse_pages_omitted ||
    !validWarningSummaries(value.warnings)
  ) {
    return false;
  }
  return true;
}

function validRenderReport(value, expectedSheetIndex, expectedSheetName) {
  const keys = [
    "schema_version",
    "sheet_index",
    "sheet_name",
    "range",
    "rows_considered",
    "columns_considered",
    "cells_considered",
    "visible_rows",
    "visible_columns",
    "rendered_regions",
    "hidden_rows_skipped",
    "hidden_columns_skipped",
    "merged_regions",
    "text_bytes",
    "glyphs",
    "scene_nodes",
    "svg_bytes",
    "font_pack_sha256",
    "font_faces",
    "warnings"
  ];
  const countKeys = [
    "rows_considered",
    "columns_considered",
    "cells_considered",
    "visible_rows",
    "visible_columns",
    "rendered_regions",
    "hidden_rows_skipped",
    "hidden_columns_skipped",
    "merged_regions",
    "text_bytes",
    "glyphs",
    "scene_nodes",
    "svg_bytes"
  ];
  return (
    plainRecord(value) &&
    hasExactKeys(value, keys) &&
    value.schema_version === 2 &&
    value.sheet_index === expectedSheetIndex &&
    value.sheet_name === expectedSheetName &&
    validReportRange(value.range) &&
    countKeys.every((key) => nonNegativeSafeInteger(value[key])) &&
    (value.font_pack_sha256 === null || validSha256(value.font_pack_sha256)) &&
    Array.isArray(value.font_faces) &&
    value.font_faces.length <= 512 &&
    value.font_faces.every(validRenderedFontFace) &&
    Array.isArray(value.warnings) &&
    value.warnings.length <= 1_024 &&
    value.warnings.every(validRenderWarning)
  );
}

function validReportRange(value) {
  return (
    plainRecord(value) &&
    hasExactKeys(value, ["first_row", "first_col", "last_row", "last_col"]) &&
    nonNegativeSafeInteger(value.first_row) &&
    nonNegativeSafeInteger(value.first_col) &&
    nonNegativeSafeInteger(value.last_row) &&
    nonNegativeSafeInteger(value.last_col) &&
    value.last_row >= value.first_row &&
    value.last_col >= value.first_col
  );
}

function validRenderedFontFace(value) {
  return (
    plainRecord(value) &&
    hasExactKeys(value, [
      "source_pack_sha256",
      "face_sha256",
      "family",
      "weight",
      "italic",
      "substituted"
    ]) &&
    validSha256(value.source_pack_sha256) &&
    validSha256(value.face_sha256) &&
    boundedText(value.family, 4_096) &&
    Number.isSafeInteger(value.weight) &&
    value.weight > 0 &&
    value.weight <= 1_000 &&
    typeof value.italic === "boolean" &&
    typeof value.substituted === "boolean"
  );
}

function validRenderWarning(value) {
  return (
    plainRecord(value) &&
    hasExactKeys(value, ["code", "occurrences", "first_cell"]) &&
    safeContractText(value.code, 128) &&
    Number.isSafeInteger(value.occurrences) &&
    value.occurrences > 0 &&
    (value.first_cell === null ||
      (plainRecord(value.first_cell) &&
        hasExactKeys(value.first_cell, ["row", "col"]) &&
        nonNegativeSafeInteger(value.first_cell.row) &&
        nonNegativeSafeInteger(value.first_cell.col)))
  );
}

function validPaper(value) {
  return (
    plainRecord(value) &&
    hasExactKeys(value, ["code", "width_raw", "height_raw"]) &&
    nonNegativeSafeInteger(value.code) &&
    Number.isSafeInteger(value.width_raw) &&
    value.width_raw > 0 &&
    Number.isSafeInteger(value.height_raw) &&
    value.height_raw > 0
  );
}

function validContentRect(value) {
  return (
    plainRecord(value) &&
    hasExactKeys(value, ["x_raw", "y_raw", "width_raw", "height_raw"]) &&
    Number.isSafeInteger(value.x_raw) &&
    Number.isSafeInteger(value.y_raw) &&
    nonNegativeSafeInteger(value.width_raw) &&
    nonNegativeSafeInteger(value.height_raw)
  );
}

function validPrintPage(value, expectedIndex) {
  return (
    plainRecord(value) &&
    hasExactKeys(value, [
      "output_index",
      "displayed_page_number",
      "area_index",
      "horizontal_index",
      "vertical_index",
      "manual_col_break_before",
      "manual_row_break_before",
      "body_range",
      "repeat_rows",
      "repeat_cols",
      "scale_permille"
    ]) &&
    value.output_index === expectedIndex &&
    Number.isSafeInteger(value.displayed_page_number) &&
    value.displayed_page_number > 0 &&
    nonNegativeSafeInteger(value.area_index) &&
    nonNegativeSafeInteger(value.horizontal_index) &&
    nonNegativeSafeInteger(value.vertical_index) &&
    typeof value.manual_col_break_before === "boolean" &&
    typeof value.manual_row_break_before === "boolean" &&
    validReportRange(value.body_range) &&
    validIndexPair(value.repeat_rows) &&
    validIndexPair(value.repeat_cols) &&
    Number.isSafeInteger(value.scale_permille) &&
    value.scale_permille > 0
  );
}

function validIndexPair(value) {
  return (
    value === null ||
    (Array.isArray(value) &&
      value.length === 2 &&
      nonNegativeSafeInteger(value[0]) &&
      nonNegativeSafeInteger(value[1]) &&
      value[1] >= value[0])
  );
}

function validSortedIntegerArray(value) {
  return (
    Array.isArray(value) &&
    value.length <= MAX_MANUAL_PAGE_BREAKS &&
    value.every(nonNegativeSafeInteger) &&
    strictlySortedUnique(value)
  );
}

function validWarningSummaries(value) {
  return (
    Array.isArray(value) &&
    value.length <= 1_024 &&
    value.every(
      (warning) =>
        plainRecord(warning) &&
        hasExactKeys(warning, ["code", "occurrences"]) &&
        safeContractText(warning.code, 128) &&
        Number.isSafeInteger(warning.occurrences) &&
        warning.occurrences > 0
    )
  );
}

function hasAllowedKeys(value, required, optional) {
  const keys = Object.keys(value);
  const allowed = new Set([...required, ...optional]);
  return (
    required.every((key) => Object.hasOwn(value, key)) &&
    keys.every((key) => allowed.has(key))
  );
}

function boundedText(value, maxBytes) {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    new TextEncoder().encode(value).byteLength <= maxBytes
  );
}

function safeContractText(value, maxBytes) {
  return boundedText(value, maxBytes) && !/[\u0000-\u001f\u007f]/.test(value);
}

function validateWorkbookInspection(value) {
  if (
    !plainRecord(value) ||
    !hasExactKeys(value, [
      "schemaVersion",
      "sheetCount",
      "sheets",
      "embeddedImages",
      "embeddedImageBytes",
      "fontPackSha256",
      "fontFaces",
      "properties"
    ]) ||
    value.schemaVersion !== 1 ||
    !Number.isSafeInteger(value.sheetCount) ||
    value.sheetCount < 0 ||
    value.sheetCount > MAX_SHEETS ||
    !Array.isArray(value.sheets) ||
    value.sheets.length !== value.sheetCount ||
    !nonNegativeSafeInteger(value.embeddedImages) ||
    !nonNegativeSafeInteger(value.embeddedImageBytes) ||
    (value.fontPackSha256 !== null && !validSha256(value.fontPackSha256)) ||
    !nonNegativeSafeInteger(value.fontFaces) ||
    !validDocumentProperties(value.properties)
  ) {
    throw new RenderProtocolError(
      "wasm_api_mismatch",
      "workbook inspection does not satisfy the worker contract",
      "wasm"
    );
  }
  let embeddedImages = 0;
  for (const [index, sheet] of value.sheets.entries()) {
    if (
      !plainRecord(sheet) ||
      !hasExactKeys(sheet, ["index", "name", "embeddedImages"]) ||
      sheet.index !== index ||
      typeof sheet.name !== "string" ||
      sheet.name.length === 0 ||
      !nonNegativeSafeInteger(sheet.embeddedImages)
    ) {
      throw new RenderProtocolError(
        "wasm_api_mismatch",
        "workbook sheet inspection does not satisfy the worker contract",
        "wasm"
      );
    }
    embeddedImages += sheet.embeddedImages;
  }
  if (!Number.isSafeInteger(embeddedImages) || embeddedImages !== value.embeddedImages) {
    throw new RenderProtocolError(
      "wasm_api_mismatch",
      "workbook image totals do not match its sheets",
      "wasm"
    );
  }
  return value;
}

function validDocumentProperties(value) {
  const fields = [
    "title",
    "subject",
    "creator",
    "keywords",
    "description",
    "lastModifiedBy",
    "company",
    "created"
  ];
  return (
    plainRecord(value) &&
    hasExactKeys(value, fields) &&
    fields.every((field) => value[field] === null || typeof value[field] === "string")
  );
}

function nonNegativeSafeInteger(value) {
  return Number.isSafeInteger(value) && value >= 0;
}

function validSha256(value) {
  return typeof value === "string" && /^[0-9a-f]{64}$/.test(value);
}

function hasZipLocalHeader(value) {
  return (
    value.byteLength >= 4 &&
    value[0] === 0x50 &&
    value[1] === 0x4b &&
    value[2] === 0x03 &&
    value[3] === 0x04
  );
}

function validateEditState(value, maxHistoryEntries, maxHistoryBytes) {
  const reasons = new Set([
    "legacy-biff",
    "binary-package",
    "open-document",
    "package-metadata-loss"
  ]);
  if (
    value === null ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    value.schemaVersion !== 1 ||
    !["read-write", "read-only"].includes(value.capability) ||
    (value.reason !== null && !reasons.has(value.reason)) ||
    typeof value.dirty !== "boolean" ||
    typeof value.canUndo !== "boolean" ||
    typeof value.canRedo !== "boolean" ||
    !Number.isSafeInteger(value.undoDepth) ||
    value.undoDepth < 0 ||
    value.undoDepth > maxHistoryEntries ||
    !Number.isSafeInteger(value.redoDepth) ||
    value.redoDepth < 0 ||
    value.redoDepth > maxHistoryEntries ||
    value.undoDepth + value.redoDepth > maxHistoryEntries ||
    !Number.isSafeInteger(value.historyBytes) ||
    value.historyBytes < 0 ||
    value.historyBytes > maxHistoryBytes ||
    !Array.isArray(value.editedParts) ||
    value.editedParts.length > MAX_SHEETS + 8 ||
    value.editedParts.some((part) => !safeEditedPart(part))
  ) {
    throw new RenderProtocolError(
      "wasm_api_mismatch",
      "edit state does not satisfy the worker contract",
      "wasm"
    );
  }
  if (
    (value.capability === "read-write" && value.reason !== null) ||
    (value.capability === "read-only" && !reasons.has(value.reason)) ||
    value.canUndo !== (value.undoDepth > 0) ||
    value.canRedo !== (value.redoDepth > 0) ||
    (value.undoDepth + value.redoDepth > 0) !== (value.historyBytes > 0) ||
    (!value.dirty && value.editedParts.length > 0) ||
    (value.capability === "read-only" &&
      (value.dirty || value.undoDepth > 0 || value.redoDepth > 0)) ||
    !strictlySortedUnique(value.editedParts)
  ) {
    throw new RenderProtocolError(
      "wasm_api_mismatch",
      "edit state contains contradictory capability or history metadata",
      "wasm"
    );
  }
  return value;
}

function validateInspectedCell(value, depth = 0) {
  if (!plainRecord(value) || depth > 16 || typeof value.kind !== "string") {
    throw invalidCellInspection();
  }
  switch (value.kind) {
    case "blank":
      if (!hasExactKeys(value, ["kind"])) {
        throw invalidCellInspection();
      }
      return;
    case "text":
    case "error":
      if (!hasExactKeys(value, ["kind", "value"]) || typeof value.value !== "string") {
        throw invalidCellInspection();
      }
      return;
    case "number":
    case "date":
      if (
        !hasExactKeys(value, ["kind", "value"]) ||
        typeof value.value !== "number" ||
        !Number.isFinite(value.value)
      ) {
        throw invalidCellInspection();
      }
      return;
    case "boolean":
      if (!hasExactKeys(value, ["kind", "value"]) || typeof value.value !== "boolean") {
        throw invalidCellInspection();
      }
      return;
    case "formula":
      if (
        !hasExactKeys(value, ["kind", "formula", "cached"]) ||
        typeof value.formula !== "string"
      ) {
        throw invalidCellInspection();
      }
      validateInspectedCell(value.cached, depth + 1);
      return;
    default:
      throw invalidCellInspection();
  }
}

function plainRecord(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function hasExactKeys(value, expected) {
  const keys = Object.keys(value);
  return keys.length === expected.length && expected.every((key) => Object.hasOwn(value, key));
}

function invalidCellInspection() {
  return new RenderProtocolError(
    "wasm_api_mismatch",
    "cell inspection does not match the worker contract",
    "wasm"
  );
}

function safeEditedPart(value) {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    value.length <= 512 &&
    !value.startsWith("/") &&
    !value.includes("\\") &&
    !value.split("/").some((segment) => segment === "" || segment === "." || segment === "..") &&
    !/[\u0000-\u001f\u007f]/.test(value)
  );
}

function strictlySortedUnique(values) {
  for (let index = 1; index < values.length; index += 1) {
    if (values[index - 1] >= values[index]) {
      return false;
    }
  }
  return true;
}

function cancelledError() {
  return new RenderProtocolError("cancelled", "render request was cancelled", "request");
}

function yieldToWorkerMessages() {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

function operationStage(operation) {
  switch (operation) {
    case "open":
      return "parsing";
    case "prepare-pages":
      return "paginating";
    case "close":
      return "closing";
    case "save-document":
      return "serializing";
    case "edit-status":
    case "read-cell":
      return "inspecting";
    case "set-cell":
    case "set-document-properties":
    case "undo-edit":
    case "redo-edit":
      return "editing";
    default:
      return "rendering";
  }
}

function responseRequestId(rawMessage) {
  try {
    return validateRequestId(rawMessage?.requestId);
  } catch {
    return "invalid";
  }
}
