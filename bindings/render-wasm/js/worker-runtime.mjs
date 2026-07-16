import {
  MAX_DPI,
  MAX_INPUT_BYTES,
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
  encodeFontBundle,
  fontPackByteLength,
  limitError,
  normalizeError,
  optionsJson,
  parseWorkerMessage,
  validateDocumentId,
  validateRequestId,
  validateSvgOutput
} from "./protocol.mjs";

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
  #draining = false;
  #capabilities;
  #maxOutputBytes;
  #maxPngBytes;

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
        await this.#run(message);
        this.#requestIds.delete(message.requestId);
        this.#activeRequestId = null;
        this.#activeResourceBytes = 0;
      }
    } finally {
      this.#activeRequestId = null;
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
      this.#cancelled.add(requestId);
    }
  }

  async #run(message) {
    const { requestId, operation, payload } = message;
    try {
      this.#throwIfCancelled(requestId);
      this.#progress(requestId, 0, 3, "accepted");
      await yieldToWorkerMessages();
      this.#throwIfCancelled(requestId);
      this.#progress(requestId, 1, 3, operationStage(operation));
      const result = await this.#execute(operation, payload);
      this.#throwIfCancelled(requestId);
      this.#progress(requestId, 2, 3, "finalizing");
      this.#progress(requestId, 3, 3, "complete");
      const transfer = result?.transfer ?? [];
      this.#sendResult(requestId, true, result?.value ?? result, null, transfer);
    } catch (error) {
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
    const resourceBytes = bytes.byteLength + fontBundle.byteLength;
    const total = this.#resourceBytes + resourceBytes;
    if (total > MAX_OPEN_RESOURCE_BYTES) {
      throw limitError("openResourceBytes", MAX_OPEN_RESOURCE_BYTES, total, "documents");
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
      const workbook = parseBoundedJson(
        await session.inspectionJson(),
        "inspection",
        this.#maxOutputBytes
      );
      if (
        !Number.isSafeInteger(workbook?.sheetCount) ||
        workbook.sheetCount < 0 ||
        workbook.sheetCount > MAX_SHEETS
      ) {
        throw new RenderProtocolError(
          "wasm_api_mismatch",
          "inspection contains an invalid sheet count",
          "wasm"
        );
      }
      this.#documents.set(documentId, { session, resourceBytes });
      this.#resourceBytes = total;
      return { documentId, workbook };
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

  async #preparePages(payload) {
    const { documentId, session } = this.#document(payload);
    const sheetIndex = boundedIndex(
      payload.sheetIndex,
      "payload.sheetIndex",
      MAX_SHEETS,
      "sheets"
    );
    const manifest = parseBoundedJson(
      await session.printManifestJson(sheetIndex, optionsJson(payload.options)),
      "print manifest",
      this.#maxOutputBytes
    );
    if (!Array.isArray(manifest?.pages)) {
      throw new RenderProtocolError(
        "wasm_api_mismatch",
        "print manifest does not contain a page array",
        "wasm"
      );
    }
    if (manifest.pages.length > MAX_PAGES) {
      throw limitError("pages", MAX_PAGES, manifest.pages.length, "output");
    }
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

function validateRange(value) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new RenderProtocolError("invalid_range", "range must be an object", "payload.range");
  }
  const range = {
    firstRow: nonNegativeInteger(value.firstRow, "payload.range.firstRow"),
    firstCol: nonNegativeInteger(value.firstCol, "payload.range.firstCol"),
    lastRow: nonNegativeInteger(value.lastRow, "payload.range.lastRow"),
    lastCol: nonNegativeInteger(value.lastCol, "payload.range.lastCol")
  };
  if (range.firstRow > range.lastRow || range.firstCol > range.lastCol) {
    throw new RenderProtocolError("invalid_range", "range is reversed", "payload.range");
  }
  if (range.lastRow > 1_048_575 || range.lastCol > 16_383) {
    throw new RenderProtocolError(
      "range_outside_grid",
      "range exceeds the spreadsheet grid",
      "payload.range"
    );
  }
  return range;
}

function nonNegativeInteger(value, location) {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new RenderProtocolError(
      "invalid_integer",
      `${location} must be a non-negative safe integer`,
      location
    );
  }
  return value;
}

function boundedIndex(value, location, countLimit, resource) {
  const index = nonNegativeInteger(value, location);
  if (index >= countLimit) {
    throw limitError(resource, countLimit, index + 1, location);
  }
  return index;
}

function positiveInteger(value, location) {
  const number = nonNegativeInteger(value, location);
  if (number === 0) {
    throw new RenderProtocolError(
      "invalid_integer",
      `${location} must be positive`,
      location
    );
  }
  return number;
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

function preflightRequest({ operation, payload }) {
  switch (operation) {
    case "capabilities":
      assertExactKeys(payload, [], "payload");
      return 0;
    case "open": {
      assertExactKeys(payload, ["documentId", "bytes", "fontPack"], "payload");
      validateDocumentId(payload.documentId);
      const bytes = asBytes(payload.bytes, "payload.bytes");
      if (bytes.byteLength > MAX_INPUT_BYTES) {
        throw limitError("inputBytes", MAX_INPUT_BYTES, bytes.byteLength, "payload.bytes");
      }
      return bytes.byteLength + fontPackByteLength(payload.fontPack);
    }
    case "close":
      assertExactKeys(payload, ["documentId"], "payload");
      validateDocumentId(payload.documentId);
      return 0;
    case "prepare-pages":
    case "render-sheet":
      assertExactKeys(payload, ["documentId", "sheetIndex", "options"], "payload");
      validateDocumentId(payload.documentId);
      boundedIndex(payload.sheetIndex, "payload.sheetIndex", MAX_SHEETS, "sheets");
      optionsJson(payload.options);
      return 0;
    case "render-tile":
      assertExactKeys(
        payload,
        ["documentId", "sheetIndex", "range", "options"],
        "payload"
      );
      validateDocumentId(payload.documentId);
      boundedIndex(payload.sheetIndex, "payload.sheetIndex", MAX_SHEETS, "sheets");
      validateRange(payload.range);
      optionsJson(payload.options);
      return 0;
    case "render-page":
      assertExactKeys(
        payload,
        ["documentId", "sheetIndex", "pageIndex", "options"],
        "payload"
      );
      validateDocumentId(payload.documentId);
      boundedIndex(payload.sheetIndex, "payload.sheetIndex", MAX_SHEETS, "sheets");
      boundedIndex(payload.pageIndex, "payload.pageIndex", MAX_PAGES, "pages");
      optionsJson(payload.options);
      return 0;
    case "render-page-png": {
      assertExactKeys(
        payload,
        ["documentId", "sheetIndex", "pageIndex", "dpi", "options"],
        "payload"
      );
      validateDocumentId(payload.documentId);
      boundedIndex(payload.sheetIndex, "payload.sheetIndex", MAX_SHEETS, "sheets");
      boundedIndex(payload.pageIndex, "payload.pageIndex", MAX_PAGES, "pages");
      const dpi = positiveInteger(payload.dpi ?? 96, "payload.dpi");
      if (dpi < MIN_DPI || dpi > MAX_DPI) {
        throw new RenderProtocolError(
          "dpi_out_of_range",
          `dpi must be between ${MIN_DPI} and ${MAX_DPI}`,
          "payload.dpi"
        );
      }
      optionsJson(payload.options);
      return 0;
    }
    default:
      throw new RenderProtocolError("unknown_operation", "operation is not supported");
  }
}

function assertExactKeys(value, allowed, location) {
  const allow = new Set(allowed);
  for (const key of Object.keys(value)) {
    if (!allow.has(key)) {
      throw new RenderProtocolError(
        "invalid_payload",
        `${location} contains an unknown field`,
        location
      );
    }
  }
}
