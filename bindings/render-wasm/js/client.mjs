import {
  MAX_DPI,
  MAX_EDIT_HISTORY_BYTES,
  MAX_EDIT_HISTORY_ENTRIES,
  MAX_INPUT_BYTES,
  MAX_MANUAL_PAGE_BREAKS,
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
  limitError,
  parseWorkerMessage,
  preflightRequest,
  validateFontPack,
  validateRange,
  validateSvgOutput
} from "./protocol.mjs";

const NON_CANCELLABLE_DISPATCHED_OPERATIONS = new Set([
  "close",
  "set-cell",
  "set-document-properties",
  "undo-edit",
  "redo-edit"
]);

export class RenderWorkerClient {
  #worker;
  #pending = new Map();
  #nextRequest = 1;
  #nextDocument = 1;
  #closed = false;
  #terminalError = null;
  #ready = false;
  #outbox = [];
  #pendingResourceBytes = 0;

  constructor(workerOrUrl, { WorkerClass = globalThis.Worker } = {}) {
    if (workerOrUrl && typeof workerOrUrl.postMessage === "function") {
      this.#worker = workerOrUrl;
    } else {
      if (typeof WorkerClass !== "function") {
        throw new TypeError("Worker is unavailable; pass a Worker-compatible instance");
      }
      this.#worker = new WorkerClass(workerOrUrl, {
        type: "module",
        name: "rxls-render-worker"
      });
    }
    this.#worker.addEventListener("message", (event) => this.#receive(event.data));
    this.#worker.addEventListener("error", (event) => {
      this.#closeWithError(
        new RenderProtocolError("worker_crashed", event.message || "render worker crashed")
      );
    });
    this.#worker.addEventListener("messageerror", () => {
      this.#closeWithError(
        new RenderProtocolError("worker_message_error", "render worker message was not cloneable")
      );
    });
  }

  capabilities(options = {}) {
    return this.request("capabilities", {}, options);
  }

  open(bytes, { documentId, fontPack, ...requestOptions } = {}) {
    this.#assertAllocationCapacity(0);
    const input = asBytes(bytes, "bytes");
    const id = documentId ?? `document-${this.#nextDocument++}`;
    const resourceBytes = preflightRequest({
      operation: "open",
      payload: { documentId: id, bytes: input, fontPack }
    });
    this.#assertAllocationCapacity(resourceBytes);
    const localOptions = validateClientRequestOptions(requestOptions);
    const copiedFontPack = copyFontPack(fontPack);
    const workbook = input.slice();
    const transfer = [workbook.buffer];
    if (copiedFontPack) {
      transfer.push(copiedFontPack.manifest.buffer);
      for (const member of copiedFontPack.members) {
        transfer.push(member.bytes.buffer);
      }
    }
    return this.#request(
      "open",
      { documentId: id, bytes: workbook, fontPack: copiedFontPack },
      localOptions,
      transfer,
      true
    );
  }

  closeDocument(documentId, options = {}) {
    return this.request("close", { documentId }, options);
  }

  editStatus(documentId, options = {}) {
    return this.request("edit-status", { documentId }, options);
  }

  readCell(documentId, sheetIndex, row, col, options = {}) {
    return this.request("read-cell", { documentId, sheetIndex, row, col }, options);
  }

  setCell(documentId, sheetIndex, row, col, value, options = {}) {
    return this.request(
      "set-cell",
      { documentId, sheetIndex, row, col, value },
      options
    );
  }

  setDocumentProperties(documentId, properties, options = {}) {
    return this.request("set-document-properties", { documentId, properties }, options);
  }

  undoEdit(documentId, options = {}) {
    return this.request("undo-edit", { documentId }, options);
  }

  redoEdit(documentId, options = {}) {
    return this.request("redo-edit", { documentId }, options);
  }

  saveDocument(documentId, options = {}) {
    return this.request("save-document", { documentId }, options);
  }

  preparePages(documentId, sheetIndex, renderOptions = {}, requestOptions = {}) {
    return this.request(
      "prepare-pages",
      { documentId, sheetIndex, options: renderOptions },
      requestOptions
    );
  }

  renderSheet(documentId, sheetIndex, renderOptions = {}, requestOptions = {}) {
    return this.request(
      "render-sheet",
      { documentId, sheetIndex, options: renderOptions },
      requestOptions
    );
  }

  renderTile(documentId, sheetIndex, range, renderOptions = {}, requestOptions = {}) {
    return this.request(
      "render-tile",
      { documentId, sheetIndex, range, options: renderOptions },
      requestOptions
    );
  }

  renderPage(documentId, sheetIndex, pageIndex, renderOptions = {}, requestOptions = {}) {
    return this.request(
      "render-page",
      { documentId, sheetIndex, pageIndex, options: renderOptions },
      requestOptions
    );
  }

  renderPagePng(
    documentId,
    sheetIndex,
    pageIndex,
    dpi = 96,
    renderOptions = {},
    requestOptions = {}
  ) {
    return this.request(
      "render-page-png",
      { documentId, sheetIndex, pageIndex, dpi, options: renderOptions },
      requestOptions
    );
  }

  request(operation, payload, options = {}) {
    let localOptions;
    try {
      localOptions = validateClientRequestOptions(options);
    } catch (error) {
      return Promise.reject(error);
    }
    return this.#request(operation, payload, localOptions);
  }

  #request(
    operation,
    payload,
    { signal, onProgress },
    transfer = [],
    payloadIsolated = false
  ) {
    if (this.#closed) {
      return Promise.reject(this.#closedError());
    }
    if (this.#pending.size >= MAX_PENDING_REQUESTS) {
      return Promise.reject(
        limitError(
          "pendingRequests",
          MAX_PENDING_REQUESTS,
          this.#pending.size + 1,
          "client"
        )
      );
    }
    let message;
    let resourceBytes;
    try {
      const parsed = parseWorkerMessage({
        protocol: PROTOCOL,
        type: "request",
        requestId: `request-${this.#nextRequest}`,
        operation,
        payload
      });
      const preflightResourceBytes = preflightRequest(parsed);
      if (
        this.#pendingResourceBytes + preflightResourceBytes >
        MAX_PENDING_RESOURCE_BYTES
      ) {
        return Promise.reject(
          limitError(
            "pendingResourceBytes",
            MAX_PENDING_RESOURCE_BYTES,
            this.#pendingResourceBytes + preflightResourceBytes,
            "client"
          )
        );
      }
      const isolatedPayload = payloadIsolated
        ? parsed.payload
        : cloneRequestPayload(operation, parsed.payload);
      message = parseWorkerMessage({ ...parsed, payload: isolatedPayload });
      resourceBytes = preflightRequest(message);
    } catch (error) {
      return Promise.reject(error);
    }
    if (this.#pendingResourceBytes + resourceBytes > MAX_PENDING_RESOURCE_BYTES) {
      return Promise.reject(
        limitError(
          "pendingResourceBytes",
          MAX_PENDING_RESOURCE_BYTES,
          this.#pendingResourceBytes + resourceBytes,
          "client"
        )
      );
    }
    const requestId = `request-${this.#nextRequest++}`;
    let abortListener;
    const promise = new Promise((resolve, reject) => {
      if (signal?.aborted) {
        reject(abortError());
        return;
      }
      abortListener = () => {
        const pending = this.#pending.get(requestId);
        if (pending) {
          this.#cancelPending(requestId, pending);
        }
      };
      signal?.addEventListener("abort", abortListener, { once: true });
      this.#pendingResourceBytes += resourceBytes;
      this.#pending.set(requestId, {
        resolve,
        reject,
        onProgress,
        signal,
        abortListener,
        resourceBytes,
        operation,
        responseIdentity: responseIdentityFor(operation, message.payload),
        dispatched: false,
        progressCompleted: -1,
        progressTotal: null
      });
      const row = {
        message: { ...message, requestId },
        transfer
      };
      if (this.#ready) {
        this.#dispatch(row);
      } else {
        this.#outbox.push(row);
      }
    });
    Object.defineProperty(promise, "requestId", { value: requestId });
    return promise;
  }

  cancel(requestId) {
    const pending = this.#pending.get(requestId);
    if (!pending) {
      return false;
    }
    return this.#cancelPending(requestId, pending);
  }

  terminate() {
    this.#closeWithError(
      new RenderProtocolError("client_closed", "render worker was terminated")
    );
  }

  #receive(message) {
    if (this.#closed) {
      return;
    }
    let incoming;
    try {
      incoming = this.#parseIncoming(message);
    } catch (error) {
      this.#closeWithError(asWorkerMessageError(error));
      return;
    }
    if (incoming.type === "ready") {
      this.#ready = true;
      for (const row of this.#outbox.splice(0)) {
        this.#dispatch(row);
      }
      return;
    }
    if (incoming.type === "ignore") {
      return;
    }
    const { pending, message: parsed } = incoming;
    if (incoming.type === "progress") {
      pending.progressCompleted = parsed.completed;
      pending.progressTotal = parsed.total;
      pending.onProgress?.({
        completed: parsed.completed,
        total: parsed.total,
        stage: parsed.stage
      });
      return;
    }
    this.#pending.delete(parsed.requestId);
    this.#pendingResourceBytes -= pending.resourceBytes;
    pending.signal?.removeEventListener("abort", pending.abortListener);
    if (parsed.ok) {
      pending.resolve(parsed.result);
      return;
    }
    const error = new RenderProtocolError(
      parsed.error.code,
      parsed.error.message,
      parsed.error.location,
      parsed.error
    );
    pending.reject(error);
  }

  #parseIncoming(message) {
    assertPlainRecord(message, "worker message");
    if (message.protocol !== PROTOCOL) {
      throw invalidWorkerMessage(`worker protocol must equal ${PROTOCOL}`);
    }
    if (message.type === "ready") {
      assertExactKeys(message, ["protocol", "type", "capabilities"], "ready message");
      if (this.#ready) {
        throw invalidWorkerMessage("worker sent ready more than once");
      }
      validateCapabilities(message.capabilities);
      return { type: "ready" };
    }
    if (!this.#ready) {
      throw invalidWorkerMessage("worker sent a response before declaring readiness");
    }
    if (message.type === "progress") {
      validateProgressMessage(message);
      const pending = this.#pending.get(message.requestId);
      if (!pending) {
        return { type: "ignore" };
      }
      if (!pending.dispatched) {
        throw invalidWorkerMessage("worker responded to a request that was not dispatched");
      }
      if (
        (pending.progressTotal !== null && pending.progressTotal !== message.total) ||
        message.completed < pending.progressCompleted
      ) {
        throw invalidWorkerMessage("worker progress must be monotonic with a stable total");
      }
      return { type: "progress", pending, message };
    }
    if (message.type === "result") {
      validateResultEnvelope(message);
      const pending = this.#pending.get(message.requestId);
      if (!pending) {
        return { type: "ignore" };
      }
      if (!pending.dispatched) {
        throw invalidWorkerMessage("worker responded to a request that was not dispatched");
      }
      if (message.ok) {
        validateOperationResult(
          pending.operation,
          pending.responseIdentity,
          message.result
        );
      }
      return { type: "result", pending, message };
    }
    throw invalidWorkerMessage("worker message type is not supported");
  }

  #failAll(error) {
    for (const pending of this.#pending.values()) {
      pending.signal?.removeEventListener("abort", pending.abortListener);
      pending.reject(error);
    }
    this.#pending.clear();
    this.#outbox = [];
    this.#pendingResourceBytes = 0;
  }

  #closeWithError(error) {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    this.#terminalError = error;
    this.#ready = false;
    this.#worker.terminate?.();
    this.#failAll(error);
  }

  #closedError() {
    return this.#terminalError ?? new RenderProtocolError("client_closed", "client is closed");
  }

  #removeFromOutbox(requestId) {
    this.#outbox = this.#outbox.filter((row) => row.message.requestId !== requestId);
  }

  #dispatch(row) {
    const pending = this.#pending.get(row.message.requestId);
    if (pending) {
      pending.dispatched = true;
    }
    try {
      this.#worker.postMessage(row.message, row.transfer);
    } catch {
      const failed = this.#pending.get(row.message.requestId);
      if (!failed) {
        return;
      }
      this.#pending.delete(row.message.requestId);
      this.#pendingResourceBytes -= failed.resourceBytes;
      failed.signal?.removeEventListener("abort", failed.abortListener);
      failed.reject(
        new RenderProtocolError(
          "worker_message_error",
          "render worker request could not be cloned"
        )
      );
    }
  }

  #sendCancel(requestId) {
    try {
      this.#worker.postMessage({ protocol: PROTOCOL, type: "cancel", requestId });
    } catch {
      // The local promise is still cancelled and all retained resources are released.
    }
  }

  #cancelPending(requestId, pending) {
    if (
      pending.dispatched &&
      NON_CANCELLABLE_DISPATCHED_OPERATIONS.has(pending.operation)
    ) {
      return false;
    }
    if (pending.dispatched) {
      this.#sendCancel(requestId);
    } else {
      this.#removeFromOutbox(requestId);
    }
    this.#pendingResourceBytes -= pending.resourceBytes;
    this.#pending.delete(requestId);
    pending.signal?.removeEventListener("abort", pending.abortListener);
    pending.reject(abortError());
    return true;
  }

  #assertAllocationCapacity(resourceBytes) {
    if (this.#closed) {
      throw this.#closedError();
    }
    if (this.#pending.size >= MAX_PENDING_REQUESTS) {
      throw limitError(
        "pendingRequests",
        MAX_PENDING_REQUESTS,
        this.#pending.size + 1,
        "client"
      );
    }
    if (this.#pendingResourceBytes + resourceBytes > MAX_PENDING_RESOURCE_BYTES) {
      throw limitError(
        "pendingResourceBytes",
        MAX_PENDING_RESOURCE_BYTES,
        this.#pendingResourceBytes + resourceBytes,
        "client"
      );
    }
  }
}

export function getRenderWorkerUrl() {
  return new URL("./worker.mjs", import.meta.url);
}

export function createRenderWorkerClient(workerUrl = getRenderWorkerUrl(), options) {
  return new RenderWorkerClient(workerUrl, options);
}

function cloneRequestPayload(operation, payload) {
  if (operation === "open") {
    return {
      documentId: payload.documentId,
      bytes: asBytes(payload.bytes, "payload.bytes").slice(),
      fontPack: copyFontPack(payload.fontPack)
    };
  }
  try {
    return structuredClone(payload);
  } catch {
    throw new RenderProtocolError(
      "invalid_payload",
      "request payload could not be isolated from caller mutation",
      "payload"
    );
  }
}

function responseIdentityFor(operation, payload) {
  switch (operation) {
    case "capabilities":
      return Object.freeze({});
    case "open":
    case "close":
    case "edit-status":
    case "set-document-properties":
    case "undo-edit":
    case "redo-edit":
    case "save-document":
      return Object.freeze({ documentId: payload.documentId });
    case "read-cell":
    case "set-cell":
      return Object.freeze({
        documentId: payload.documentId,
        sheetIndex: payload.sheetIndex,
        row: payload.row,
        col: payload.col
      });
    case "prepare-pages":
    case "render-sheet":
      return Object.freeze({
        documentId: payload.documentId,
        sheetIndex: payload.sheetIndex
      });
    case "render-tile":
      return Object.freeze({
        documentId: payload.documentId,
        sheetIndex: payload.sheetIndex,
        range: Object.freeze({ ...payload.range })
      });
    case "render-page":
      return Object.freeze({
        documentId: payload.documentId,
        sheetIndex: payload.sheetIndex,
        pageIndex: payload.pageIndex
      });
    case "render-page-png":
      return Object.freeze({
        documentId: payload.documentId,
        sheetIndex: payload.sheetIndex,
        pageIndex: payload.pageIndex,
        dpi: payload.dpi ?? 96
      });
    default:
      throw new RenderProtocolError(
        "unknown_operation",
        "operation is not supported",
        "operation"
      );
  }
}

const CAPABILITY_LIMIT_KEYS = [
  "maxInputBytes",
  "maxImageBytes",
  "maxImages",
  "maxFontBytes",
  "maxRows",
  "maxColumns",
  "maxCells",
  "maxConditionalRules",
  "maxConditionalEvaluations",
  "maxDrawingObjects",
  "maxMediaBytes",
  "maxImageDimension",
  "maxImagePixels",
  "maxDecodedMediaBytes",
  "maxChartSeries",
  "maxChartPoints",
  "maxTextBytes",
  "maxGlyphs",
  "maxTextRuns",
  "maxTextLines",
  "maxPathCommands",
  "maxSceneNodes",
  "maxDimensionRaw",
  "maxOutputBytes",
  "maxSheets",
  "maxLogicalPages",
  "maxPages",
  "maxTotalSceneNodes",
  "maxBackendCommands",
  "maxRasterDimension",
  "maxRasterPixels",
  "maxPngBytes",
  "minDpi",
  "maxDpi"
];

const EDIT_OPERATIONS = [
  "read-cell",
  "set-cell",
  "set-document-properties",
  "undo-edit",
  "redo-edit",
  "save-document"
];

const DOCUMENT_PROPERTY_KEYS = [
  "title",
  "subject",
  "creator",
  "keywords",
  "description",
  "lastModifiedBy",
  "company",
  "created"
];

const EDIT_STATE_KEYS = [
  "schemaVersion",
  "capability",
  "reason",
  "dirty",
  "canUndo",
  "canRedo",
  "undoDepth",
  "redoDepth",
  "historyBytes",
  "editedParts"
];

const EDIT_REASONS = new Set([
  "legacy-biff",
  "binary-package",
  "open-document",
  "package-metadata-loss"
]);

function validateCapabilities(value) {
  assertPlainRecord(value, "worker capabilities");
  assertExactKeys(
    value,
    [
      "schemaVersion",
      "protocol",
      "outputs",
      "limits",
      "fontUploads",
      "embeddedImages",
      "editing"
    ],
    "worker capabilities"
  );
  if (
    value.schemaVersion !== 1 ||
    value.protocol !== PROTOCOL ||
    !exactArray(value.outputs, ["sheet-svg", "tile-svg", "page-svg", "page-png"])
  ) {
    throw invalidWorkerMessage("worker capabilities identity is invalid");
  }

  assertPlainRecord(value.limits, "worker capability limits");
  assertExactKeys(value.limits, CAPABILITY_LIMIT_KEYS, "worker capability limits");
  if (CAPABILITY_LIMIT_KEYS.some((key) => !positiveSafeInteger(value.limits[key]))) {
    throw invalidWorkerMessage("worker capability limits must be positive safe integers");
  }
  if (
    value.limits.maxInputBytes > MAX_INPUT_BYTES ||
    value.limits.maxOutputBytes > MAX_OUTPUT_BYTES ||
    value.limits.maxPngBytes > MAX_PNG_BYTES ||
    value.limits.maxSheets > MAX_SHEETS ||
    value.limits.maxPages > MAX_PAGES ||
    value.limits.minDpi < MIN_DPI ||
    value.limits.maxDpi > MAX_DPI ||
    value.limits.minDpi > value.limits.maxDpi
  ) {
    throw invalidWorkerMessage("worker capabilities exceed the client hard limits");
  }

  assertPlainRecord(value.fontUploads, "worker font capabilities");
  assertExactKeys(
    value.fontUploads,
    ["supported", "verified", "maxBytes", "bundleSchema"],
    "worker font capabilities"
  );
  if (
    value.fontUploads.supported !== true ||
    value.fontUploads.verified !== true ||
    value.fontUploads.bundleSchema !== "rxls.font-bundle.v1" ||
    value.fontUploads.maxBytes !== value.limits.maxFontBytes
  ) {
    throw invalidWorkerMessage("worker font capabilities are invalid");
  }

  assertPlainRecord(value.embeddedImages, "worker image capabilities");
  assertExactKeys(value.embeddedImages, ["bounded", "painted"], "worker image capabilities");
  if (value.embeddedImages.bounded !== true || value.embeddedImages.painted !== true) {
    throw invalidWorkerMessage("worker image capabilities are invalid");
  }

  assertPlainRecord(value.editing, "worker editing capabilities");
  assertExactKeys(
    value.editing,
    [
      "supported",
      "formats",
      "preservation",
      "operations",
      "maxHistoryEntries",
      "maxHistoryBytes"
    ],
    "worker editing capabilities"
  );
  if (
    value.editing.supported !== true ||
    !exactArray(value.editing.formats, ["xlsx", "xlsm"]) ||
    value.editing.preservation !== "untouched-package-parts" ||
    !exactArray(value.editing.operations, EDIT_OPERATIONS) ||
    !positiveSafeInteger(value.editing.maxHistoryEntries) ||
    value.editing.maxHistoryEntries > MAX_EDIT_HISTORY_ENTRIES ||
    !positiveSafeInteger(value.editing.maxHistoryBytes) ||
    value.editing.maxHistoryBytes > MAX_EDIT_HISTORY_BYTES
  ) {
    throw invalidWorkerMessage("worker editing capabilities are invalid");
  }
  return value;
}

function validateProgressMessage(message) {
  assertExactKeys(
    message,
    ["protocol", "type", "requestId", "completed", "total", "stage"],
    "progress message"
  );
  validateResponseRequestId(message.requestId);
  if (
    !nonNegativeSafeInteger(message.completed) ||
    !positiveSafeInteger(message.total) ||
    message.completed > message.total ||
    !safeWorkerText(message.stage, 128)
  ) {
    throw invalidWorkerMessage("worker progress is invalid");
  }
}

function validateResultEnvelope(message) {
  assertExactKeys(
    message,
    ["protocol", "type", "requestId", "ok", "result", "error"],
    "result message"
  );
  validateResponseRequestId(message.requestId);
  if (typeof message.ok !== "boolean") {
    throw invalidWorkerMessage("worker result status must be boolean");
  }
  if (message.ok) {
    if (message.error !== null || message.result === undefined) {
      throw invalidWorkerMessage("successful worker result has an invalid envelope");
    }
    return;
  }
  if (message.result !== null) {
    throw invalidWorkerMessage("failed worker result must contain a null result");
  }
  validateErrorPayload(message.error);
}

function validateErrorPayload(error) {
  assertPlainRecord(error, "worker error");
  assertExactKeys(
    error,
    ["code", "message", "location", "resource", "limit", "actual"],
    "worker error"
  );
  if (
    !safeWorkerText(error.code, 128) ||
    !safeWorkerText(error.message, 4_096) ||
    !safeWorkerText(error.location, 512) ||
    (error.resource !== null && !safeWorkerText(error.resource, 128)) ||
    (error.limit !== null && !nonNegativeSafeInteger(error.limit)) ||
    (error.actual !== null && !nonNegativeSafeInteger(error.actual))
  ) {
    throw invalidWorkerMessage("worker error payload is invalid");
  }
}

function validateOperationResult(operation, payload, result) {
  switch (operation) {
    case "capabilities":
      validateCapabilities(result);
      return;
    case "open":
      assertPlainRecord(result, "open result");
      assertExactKeys(result, ["documentId", "workbook", "editState"], "open result");
      assertIdentity(result.documentId, payload.documentId, "documentId");
      validateWorkbookInspection(result.workbook);
      validateEditState(result.editState);
      return;
    case "close":
      assertPlainRecord(result, "close result");
      assertExactKeys(result, ["documentId", "closed"], "close result");
      assertIdentity(result.documentId, payload.documentId, "documentId");
      if (typeof result.closed !== "boolean") {
        throw invalidWorkerMessage("close result status must be boolean");
      }
      return;
    case "edit-status":
      assertPlainRecord(result, "edit status result");
      assertExactKeys(result, ["documentId", "editState"], "edit status result");
      assertIdentity(result.documentId, payload.documentId, "documentId");
      validateEditState(result.editState);
      return;
    case "read-cell":
      validateReadCellResult(payload, result);
      return;
    case "set-cell":
    case "set-document-properties":
    case "undo-edit":
    case "redo-edit":
      assertPlainRecord(result, "edit result");
      assertExactKeys(result, ["documentId", "workbook", "editState"], "edit result");
      assertIdentity(result.documentId, payload.documentId, "documentId");
      validateWorkbookInspection(result.workbook);
      validateEditState(result.editState);
      return;
    case "save-document":
      validateSaveResult(payload, result);
      return;
    case "prepare-pages":
      validatePreparePagesResult(payload, result);
      return;
    case "render-sheet":
      validateSvgResult(payload, result, ["documentId", "sheetIndex", "mimeType", "svg"]);
      return;
    case "render-tile":
      validateSvgResult(
        payload,
        result,
        ["documentId", "sheetIndex", "range", "mimeType", "svg"]
      );
      validateRangeIdentity(result.range, payload.range);
      return;
    case "render-page":
      validateSvgResult(
        payload,
        result,
        ["documentId", "sheetIndex", "pageIndex", "mimeType", "svg"]
      );
      assertIdentity(result.pageIndex, payload.pageIndex, "pageIndex");
      return;
    case "render-page-png":
      validatePngResult(payload, result);
      return;
    default:
      throw invalidWorkerMessage("worker result operation is not supported");
  }
}

function validateWorkbookInspection(value) {
  assertPlainRecord(value, "workbook inspection");
  assertExactKeys(
    value,
    [
      "schemaVersion",
      "sheetCount",
      "sheets",
      "embeddedImages",
      "embeddedImageBytes",
      "fontPackSha256",
      "fontFaces",
      "properties"
    ],
    "workbook inspection"
  );
  if (
    value.schemaVersion !== 1 ||
    !nonNegativeSafeInteger(value.sheetCount) ||
    value.sheetCount > MAX_SHEETS ||
    !Array.isArray(value.sheets) ||
    value.sheets.length !== value.sheetCount ||
    !nonNegativeSafeInteger(value.embeddedImages) ||
    !nonNegativeSafeInteger(value.embeddedImageBytes) ||
    (value.fontPackSha256 !== null && !validSha256(value.fontPackSha256)) ||
    !nonNegativeSafeInteger(value.fontFaces)
  ) {
    throw invalidWorkerMessage("workbook inspection is invalid");
  }
  validateDocumentProperties(value.properties);
  let embeddedImages = 0;
  for (const [index, sheet] of value.sheets.entries()) {
    assertPlainRecord(sheet, "workbook sheet inspection");
    assertExactKeys(sheet, ["index", "name", "embeddedImages"], "workbook sheet inspection");
    if (
      sheet.index !== index ||
      !boundedWorkerString(sheet.name, MAX_OUTPUT_BYTES) ||
      !nonNegativeSafeInteger(sheet.embeddedImages)
    ) {
      throw invalidWorkerMessage("workbook sheet inspection is invalid");
    }
    embeddedImages += sheet.embeddedImages;
  }
  if (!Number.isSafeInteger(embeddedImages) || embeddedImages !== value.embeddedImages) {
    throw invalidWorkerMessage("workbook image totals are inconsistent");
  }
}

function validateDocumentProperties(value) {
  assertPlainRecord(value, "document properties");
  assertExactKeys(value, DOCUMENT_PROPERTY_KEYS, "document properties");
  if (
    DOCUMENT_PROPERTY_KEYS.some(
      (key) => value[key] !== null && typeof value[key] !== "string"
    )
  ) {
    throw invalidWorkerMessage("document properties are invalid");
  }
}

function validateEditState(value) {
  assertPlainRecord(value, "edit state");
  assertExactKeys(value, EDIT_STATE_KEYS, "edit state");
  if (
    value.schemaVersion !== 1 ||
    !["read-write", "read-only"].includes(value.capability) ||
    (value.reason !== null && !EDIT_REASONS.has(value.reason)) ||
    typeof value.dirty !== "boolean" ||
    typeof value.canUndo !== "boolean" ||
    typeof value.canRedo !== "boolean" ||
    !nonNegativeSafeInteger(value.undoDepth) ||
    !nonNegativeSafeInteger(value.redoDepth) ||
    value.undoDepth + value.redoDepth > MAX_EDIT_HISTORY_ENTRIES ||
    !nonNegativeSafeInteger(value.historyBytes) ||
    value.historyBytes > MAX_EDIT_HISTORY_BYTES ||
    !Array.isArray(value.editedParts) ||
    value.editedParts.length > MAX_SHEETS + 8 ||
    value.editedParts.some((part) => !safeEditedPart(part))
  ) {
    throw invalidWorkerMessage("edit state is invalid");
  }
  if (
    (value.capability === "read-write" && value.reason !== null) ||
    (value.capability === "read-only" && !EDIT_REASONS.has(value.reason)) ||
    value.canUndo !== (value.undoDepth > 0) ||
    value.canRedo !== (value.redoDepth > 0) ||
    (value.undoDepth + value.redoDepth > 0) !== (value.historyBytes > 0) ||
    (!value.dirty && value.editedParts.length > 0) ||
    (value.capability === "read-only" &&
      (value.dirty || value.undoDepth > 0 || value.redoDepth > 0)) ||
    !strictlySortedUnique(value.editedParts)
  ) {
    throw invalidWorkerMessage("edit state contains contradictory metadata");
  }
}

function validateReadCellResult(payload, result) {
  assertPlainRecord(result, "cell result");
  assertExactKeys(
    result,
    ["documentId", "schemaVersion", "sheetIndex", "row", "col", "value", "formatted"],
    "cell result"
  );
  assertIdentity(result.documentId, payload.documentId, "documentId");
  assertIdentity(result.schemaVersion, 1, "schemaVersion");
  assertIdentity(result.sheetIndex, payload.sheetIndex, "sheetIndex");
  assertIdentity(result.row, payload.row, "row");
  assertIdentity(result.col, payload.col, "col");
  if (result.formatted !== null && typeof result.formatted !== "string") {
    throw invalidWorkerMessage("formatted cell value is invalid");
  }
  validateInspectedCell(result.value);
}

function validateInspectedCell(value, depth = 0) {
  assertPlainRecord(value, "inspected cell");
  if (depth > 16 || typeof value.kind !== "string") {
    throw invalidWorkerMessage("inspected cell is invalid");
  }
  switch (value.kind) {
    case "blank":
      assertExactKeys(value, ["kind"], "blank cell");
      return;
    case "text":
    case "error":
      assertExactKeys(value, ["kind", "value"], "text cell");
      if (typeof value.value !== "string") {
        throw invalidWorkerMessage("text cell value is invalid");
      }
      return;
    case "number":
    case "date":
      assertExactKeys(value, ["kind", "value"], "numeric cell");
      if (typeof value.value !== "number" || !Number.isFinite(value.value)) {
        throw invalidWorkerMessage("numeric cell value is invalid");
      }
      return;
    case "boolean":
      assertExactKeys(value, ["kind", "value"], "boolean cell");
      if (typeof value.value !== "boolean") {
        throw invalidWorkerMessage("boolean cell value is invalid");
      }
      return;
    case "formula":
      assertExactKeys(value, ["kind", "formula", "cached"], "formula cell");
      if (typeof value.formula !== "string") {
        throw invalidWorkerMessage("formula cell value is invalid");
      }
      validateInspectedCell(value.cached, depth + 1);
      return;
    default:
      throw invalidWorkerMessage("inspected cell kind is invalid");
  }
}

function validateSaveResult(payload, result) {
  assertPlainRecord(result, "save result");
  assertExactKeys(result, ["documentId", "mimeType", "bytes"], "save result");
  assertIdentity(result.documentId, payload.documentId, "documentId");
  if (
    result.mimeType !== "application/octet-stream" ||
    !(result.bytes instanceof Uint8Array) ||
    result.bytes.byteLength > MAX_INPUT_BYTES ||
    !hasZipLocalHeader(result.bytes)
  ) {
    throw invalidWorkerMessage("saved workbook result is invalid");
  }
}

function validatePreparePagesResult(payload, result) {
  assertPlainRecord(result, "prepare pages result");
  assertExactKeys(result, ["documentId", "sheetIndex", "manifest"], "prepare pages result");
  assertIdentity(result.documentId, payload.documentId, "documentId");
  assertIdentity(result.sheetIndex, payload.sheetIndex, "sheetIndex");
  validatePrintManifest(result.manifest, payload.sheetIndex);
}

function validatePrintManifest(manifest, expectedSheetIndex) {
  assertPlainRecord(manifest, "print manifest");
  assertAllowedKeys(
    manifest,
    [
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
    ],
    ["layout_override", "page_order"],
    "print manifest"
  );
  if (
    manifest.schema_version !== 2 ||
    manifest.sheet_index !== expectedSheetIndex ||
    !boundedWorkerString(manifest.sheet_name, MAX_OUTPUT_BYTES) ||
    (manifest.layout_override !== undefined &&
      manifest.layout_override !== "single_page_sheets") ||
    (manifest.page_order !== undefined &&
      !["down_then_over", "over_then_down", "unknown"].includes(
        manifest.page_order
      )) ||
    !nonNegativeSafeInteger(manifest.scale_permille) ||
    !nonNegativeSafeInteger(manifest.logical_pages) ||
    !nonNegativeSafeInteger(manifest.sparse_pages_omitted)
  ) {
    throw invalidWorkerMessage("print manifest identity or pagination metadata is invalid");
  }

  validateRenderReport(
    manifest.source_report,
    expectedSheetIndex,
    manifest.sheet_name,
    "print manifest source report"
  );
  if (
    !Array.isArray(manifest.source_reports) ||
    manifest.source_reports.length === 0 ||
    manifest.source_reports.length > MAX_PAGES
  ) {
    throw invalidWorkerMessage("print manifest source report list is invalid");
  }
  for (const report of manifest.source_reports) {
    validateRenderReport(
      report,
      expectedSheetIndex,
      manifest.sheet_name,
      "print manifest source report"
    );
  }

  validatePaper(manifest.paper);
  validateContentRect(manifest.content_rect);
  validateSortedIntegerArray(manifest.manual_row_breaks, "manual row breaks");
  validateSortedIntegerArray(manifest.manual_col_breaks, "manual column breaks");

  if (!Array.isArray(manifest.pages) || manifest.pages.length > MAX_PAGES) {
    throw invalidWorkerMessage("print manifest page list is invalid");
  }
  for (const [index, page] of manifest.pages.entries()) {
    validatePrintPage(page, index);
  }
  if (
    manifest.logical_pages !==
    manifest.pages.length + manifest.sparse_pages_omitted
  ) {
    throw invalidWorkerMessage("print manifest logical page totals are inconsistent");
  }
  validateWarningSummaries(manifest.warnings, "print manifest warnings");
}

function validateRenderReport(value, expectedSheetIndex, expectedSheetName, name) {
  assertPlainRecord(value, name);
  assertExactKeys(
    value,
    [
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
    ],
    name
  );
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
  if (
    value.schema_version !== 2 ||
    value.sheet_index !== expectedSheetIndex ||
    value.sheet_name !== expectedSheetName ||
    countKeys.some((key) => !nonNegativeSafeInteger(value[key])) ||
    (value.font_pack_sha256 !== null && !validSha256(value.font_pack_sha256)) ||
    !Array.isArray(value.font_faces) ||
    value.font_faces.length > 512 ||
    !Array.isArray(value.warnings) ||
    value.warnings.length > 1_024
  ) {
    throw invalidWorkerMessage(`${name} metadata is invalid`);
  }
  validateReportRange(value.range, `${name} range`);
  for (const face of value.font_faces) {
    validateRenderedFontFace(face);
  }
  for (const warning of value.warnings) {
    validateRenderWarning(warning);
  }
}

function validateReportRange(value, name) {
  assertPlainRecord(value, name);
  assertExactKeys(
    value,
    ["first_row", "first_col", "last_row", "last_col"],
    name
  );
  if (
    !nonNegativeSafeInteger(value.first_row) ||
    !nonNegativeSafeInteger(value.first_col) ||
    !nonNegativeSafeInteger(value.last_row) ||
    !nonNegativeSafeInteger(value.last_col) ||
    value.last_row < value.first_row ||
    value.last_col < value.first_col
  ) {
    throw invalidWorkerMessage(`${name} is invalid`);
  }
}

function validateRenderedFontFace(value) {
  assertPlainRecord(value, "rendered font face");
  assertExactKeys(
    value,
    [
      "source_pack_sha256",
      "face_sha256",
      "family",
      "weight",
      "italic",
      "substituted"
    ],
    "rendered font face"
  );
  if (
    !validSha256(value.source_pack_sha256) ||
    !validSha256(value.face_sha256) ||
    !boundedWorkerString(value.family, 4_096) ||
    !positiveSafeInteger(value.weight) ||
    value.weight > 1_000 ||
    typeof value.italic !== "boolean" ||
    typeof value.substituted !== "boolean"
  ) {
    throw invalidWorkerMessage("rendered font face is invalid");
  }
}

function validateRenderWarning(value) {
  assertPlainRecord(value, "render warning");
  assertExactKeys(value, ["code", "occurrences", "first_cell"], "render warning");
  if (!safeWorkerText(value.code, 128) || !positiveSafeInteger(value.occurrences)) {
    throw invalidWorkerMessage("render warning is invalid");
  }
  if (value.first_cell === null) {
    return;
  }
  assertPlainRecord(value.first_cell, "render warning first cell");
  assertExactKeys(value.first_cell, ["row", "col"], "render warning first cell");
  if (
    !nonNegativeSafeInteger(value.first_cell.row) ||
    !nonNegativeSafeInteger(value.first_cell.col)
  ) {
    throw invalidWorkerMessage("render warning first cell is invalid");
  }
}

function validatePaper(value) {
  assertPlainRecord(value, "print paper");
  assertExactKeys(value, ["code", "width_raw", "height_raw"], "print paper");
  if (
    !nonNegativeSafeInteger(value.code) ||
    !positiveSafeInteger(value.width_raw) ||
    !positiveSafeInteger(value.height_raw)
  ) {
    throw invalidWorkerMessage("print paper is invalid");
  }
}

function validateContentRect(value) {
  assertPlainRecord(value, "print content rectangle");
  assertExactKeys(
    value,
    ["x_raw", "y_raw", "width_raw", "height_raw"],
    "print content rectangle"
  );
  if (
    !safeInteger(value.x_raw) ||
    !safeInteger(value.y_raw) ||
    !nonNegativeSafeInteger(value.width_raw) ||
    !nonNegativeSafeInteger(value.height_raw)
  ) {
    throw invalidWorkerMessage("print content rectangle is invalid");
  }
}

function validatePrintPage(value, expectedIndex) {
  assertPlainRecord(value, "print page");
  assertExactKeys(
    value,
    [
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
    ],
    "print page"
  );
  if (
    value.output_index !== expectedIndex ||
    !positiveSafeInteger(value.displayed_page_number) ||
    !nonNegativeSafeInteger(value.area_index) ||
    !nonNegativeSafeInteger(value.horizontal_index) ||
    !nonNegativeSafeInteger(value.vertical_index) ||
    typeof value.manual_col_break_before !== "boolean" ||
    typeof value.manual_row_break_before !== "boolean" ||
    !positiveSafeInteger(value.scale_permille)
  ) {
    throw invalidWorkerMessage("print page metadata is invalid");
  }
  validateReportRange(value.body_range, "print page body range");
  validateIndexPair(value.repeat_rows, "print page repeat rows");
  validateIndexPair(value.repeat_cols, "print page repeat columns");
}

function validateIndexPair(value, name) {
  if (value === null) {
    return;
  }
  if (
    !Array.isArray(value) ||
    value.length !== 2 ||
    !nonNegativeSafeInteger(value[0]) ||
    !nonNegativeSafeInteger(value[1]) ||
    value[1] < value[0]
  ) {
    throw invalidWorkerMessage(`${name} is invalid`);
  }
}

function validateSortedIntegerArray(value, name) {
  if (
    !Array.isArray(value) ||
    value.length > MAX_MANUAL_PAGE_BREAKS ||
    value.some((item) => !nonNegativeSafeInteger(item)) ||
    !strictlyIncreasing(value)
  ) {
    throw invalidWorkerMessage(`${name} are invalid`);
  }
}

function validateWarningSummaries(value, name) {
  if (!Array.isArray(value) || value.length > 1_024) {
    throw invalidWorkerMessage(`${name} are invalid`);
  }
  for (const warning of value) {
    assertPlainRecord(warning, name);
    assertExactKeys(warning, ["code", "occurrences"], name);
    if (!safeWorkerText(warning.code, 128) || !positiveSafeInteger(warning.occurrences)) {
      throw invalidWorkerMessage(`${name} are invalid`);
    }
  }
}

function validateSvgResult(payload, result, keys) {
  assertPlainRecord(result, "SVG result");
  assertExactKeys(result, keys, "SVG result");
  assertIdentity(result.documentId, payload.documentId, "documentId");
  assertIdentity(result.sheetIndex, payload.sheetIndex, "sheetIndex");
  if (result.mimeType !== "image/svg+xml") {
    throw invalidWorkerMessage("SVG result MIME type is invalid");
  }
  validateSvgOutput(result.svg, MAX_OUTPUT_BYTES);
}

function validateRangeIdentity(actual, expected) {
  const range = validateRange(actual);
  if (
    range.firstRow !== expected.firstRow ||
    range.firstCol !== expected.firstCol ||
    range.lastRow !== expected.lastRow ||
    range.lastCol !== expected.lastCol
  ) {
    throw invalidWorkerMessage("render range does not match its request");
  }
}

function validatePngResult(payload, result) {
  assertPlainRecord(result, "PNG result");
  assertExactKeys(
    result,
    ["documentId", "sheetIndex", "pageIndex", "dpi", "mimeType", "bytes"],
    "PNG result"
  );
  assertIdentity(result.documentId, payload.documentId, "documentId");
  assertIdentity(result.sheetIndex, payload.sheetIndex, "sheetIndex");
  assertIdentity(result.pageIndex, payload.pageIndex, "pageIndex");
  assertIdentity(result.dpi, payload.dpi ?? 96, "dpi");
  const signature = [137, 80, 78, 71, 13, 10, 26, 10];
  if (
    result.mimeType !== "image/png" ||
    !(result.bytes instanceof Uint8Array) ||
    result.bytes.byteLength > MAX_PNG_BYTES ||
    result.bytes.byteLength < signature.length ||
    !signature.every((byte, index) => result.bytes[index] === byte)
  ) {
    throw invalidWorkerMessage("PNG result is invalid");
  }
}

function assertIdentity(actual, expected, name) {
  if (actual !== expected) {
    throw invalidWorkerMessage(`worker result ${name} does not match its request`);
  }
}

function validateResponseRequestId(value) {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.length > 128 ||
    !/^[A-Za-z0-9._:-]+$/.test(value)
  ) {
    throw invalidWorkerMessage("worker requestId is invalid");
  }
}

function assertPlainRecord(value, name) {
  if (!isPlainRecord(value)) {
    throw invalidWorkerMessage(`${name} must be a plain object`);
  }
}

function isPlainRecord(value) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return false;
  }
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function assertExactKeys(value, expected, name) {
  const keys = Object.keys(value);
  if (keys.length !== expected.length || expected.some((key) => !Object.hasOwn(value, key))) {
    throw invalidWorkerMessage(`${name} has an invalid shape`);
  }
}

function assertAllowedKeys(value, required, optional, name) {
  const keys = Object.keys(value);
  const allowed = new Set([...required, ...optional]);
  if (
    required.some((key) => !Object.hasOwn(value, key)) ||
    keys.some((key) => !allowed.has(key))
  ) {
    throw invalidWorkerMessage(`${name} has an invalid shape`);
  }
}

function exactArray(value, expected) {
  return (
    Array.isArray(value) &&
    value.length === expected.length &&
    expected.every((item, index) => value[index] === item)
  );
}

function positiveSafeInteger(value) {
  return Number.isSafeInteger(value) && value > 0;
}

function safeInteger(value) {
  return Number.isSafeInteger(value);
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

function safeWorkerText(value, maxBytes) {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    new TextEncoder().encode(value).byteLength <= maxBytes &&
    !/[\u0000-\u001f\u007f]/.test(value)
  );
}

function boundedWorkerString(value, maxBytes) {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    new TextEncoder().encode(value).byteLength <= maxBytes
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

function strictlyIncreasing(values) {
  for (let index = 1; index < values.length; index += 1) {
    if (values[index - 1] >= values[index]) {
      return false;
    }
  }
  return true;
}

function invalidWorkerMessage(message) {
  return new RenderProtocolError("worker_message_error", message, "worker");
}

function asWorkerMessageError(error) {
  if (error instanceof RenderProtocolError && error.code === "worker_message_error") {
    return error;
  }
  return invalidWorkerMessage(
    error instanceof Error ? `invalid worker response: ${error.message}` : "invalid worker response"
  );
}

function copyFontPack(fontPack) {
  if (fontPack === undefined || fontPack === null) {
    return undefined;
  }
  const validated = validateFontPack(fontPack);
  return {
    manifest: validated.manifest.slice(),
    members: validated.members.map((member, index) => ({
      name: fontPack.members[index].name,
      bytes: member.bytes.slice()
    }))
  };
}

function abortError() {
  if (typeof DOMException === "function") {
    return new DOMException("render request was cancelled", "AbortError");
  }
  const error = new Error("render request was cancelled");
  error.name = "AbortError";
  return error;
}

function validateClientRequestOptions(options) {
  if (
    options === null ||
    typeof options !== "object" ||
    Array.isArray(options) ||
    (Object.getPrototypeOf(options) !== Object.prototype &&
      Object.getPrototypeOf(options) !== null)
  ) {
    throw new RenderProtocolError(
      "invalid_client_options",
      "client request options must be a plain object",
      "client"
    );
  }
  const allowed = new Set(["signal", "onProgress"]);
  if (Object.keys(options).some((key) => !allowed.has(key))) {
    throw new RenderProtocolError(
      "invalid_client_options",
      "client request options contain an unknown field",
      "client"
    );
  }
  const { signal, onProgress } = options;
  if (
    signal !== undefined &&
    (signal === null ||
      typeof signal !== "object" ||
      typeof signal.addEventListener !== "function" ||
      typeof signal.removeEventListener !== "function" ||
      typeof signal.aborted !== "boolean")
  ) {
    throw new RenderProtocolError(
      "invalid_client_options",
      "signal must implement the AbortSignal contract",
      "client"
    );
  }
  if (onProgress !== undefined && typeof onProgress !== "function") {
    throw new RenderProtocolError(
      "invalid_client_options",
      "onProgress must be a function",
      "client"
    );
  }
  return { signal, onProgress };
}
