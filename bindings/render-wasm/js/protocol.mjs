export const PROTOCOL = "rxls.render-worker.v2";
export const MAX_INPUT_BYTES = 32 * 1024 * 1024;
export const MAX_FONT_BYTES = 64 * 1024 * 1024;
export const MAX_FONT_MANIFEST_BYTES = 4 * 1024 * 1024;
export const MAX_FONT_FILES = 512;
export const MAX_FONT_FILE_BYTES = 32 * 1024 * 1024;
export const MAX_OPEN_DOCUMENTS = 4;
export const MAX_OPEN_RESOURCE_BYTES = 128 * 1024 * 1024;
export const MAX_OPTIONS_BYTES = 64 * 1024;
export const MAX_EDIT_REQUEST_BYTES = 128 * 1024;
export const MAX_RANGE_EDIT_REQUEST_BYTES = 1024 * 1024;
export const MAX_RANGE_EDIT_CELLS = 10_000;
export const MAX_EDIT_HISTORY_ENTRIES = 20;
export const MAX_EDIT_HISTORY_BYTES = MAX_INPUT_BYTES;
export const MAX_PENDING_REQUESTS = 32;
export const MAX_PENDING_RESOURCE_BYTES = 128 * 1024 * 1024;
export const MAX_OUTPUT_BYTES = 16 * 1024 * 1024;
export const MAX_INTERACTION_CELLS = 250_000;
export const MAX_INTERACTION_DIMENSION_RAW = 2_000_000 * 1024;
export const MAX_PNG_BYTES = 16 * 1024 * 1024;
export const MAX_SHEETS = 255;
export const MAX_PAGES = 512;
export const MAX_MANUAL_PAGE_BREAKS = 1_026;
export const MIN_DPI = 36;
export const MAX_DPI = 300;

const FONT_BUNDLE_MAGIC = new TextEncoder().encode("RXLSFPK1");
const MAX_FONT_NAME_BYTES = 4_096;
const MAX_REQUEST_ID_BYTES = 128;
const MAX_OPTION_NODES = 8_192;
const MAX_OPTION_ARRAY_ITEMS = 4_096;
const SAFE_SVG_ELEMENTS = new Set([
  "svg",
  "title",
  "defs",
  "clippath",
  "rect",
  "line",
  "path",
  "g",
  "image",
  "a",
  "text"
]);
const OPERATIONS = new Set([
  "viewport-capabilities",
  "prepare-viewport",
  "render-viewport-tile",
  "release-viewport",
  "capabilities",
  "open",
  "close",
  "prepare-pages",
  "render-sheet",
  "render-sheet-interactive",
  "render-tile",
  "render-page",
  "render-page-png",
  "edit-status",
  "read-cell",
  "set-cell",
  "set-cell-recalculate",
  "set-range-recalculate",
  "set-document-properties",
  "undo-edit",
  "redo-edit",
  "save-document"
]);

export class RenderProtocolError extends Error {
  constructor(code, message, location = "protocol", details = {}) {
    super(message);
    this.name = "RenderProtocolError";
    this.code = code;
    this.location = location;
    this.resource = details.resource ?? null;
    this.limit = details.limit ?? null;
    this.actual = details.actual ?? null;
  }
}

export function asBytes(value, location = "bytes") {
  if (value instanceof Uint8Array) {
    return value;
  }
  if (value instanceof ArrayBuffer) {
    return new Uint8Array(value);
  }
  if (ArrayBuffer.isView(value)) {
    return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  }
  throw new RenderProtocolError(
    "invalid_bytes",
    `${location} must be an ArrayBuffer or typed-array view`,
    location
  );
}

export function validateRequestId(value) {
  if (typeof value !== "string" || value.length === 0) {
    throw new RenderProtocolError(
      "invalid_request_id",
      "requestId must be a non-empty string",
      "requestId"
    );
  }
  const length = new TextEncoder().encode(value).byteLength;
  if (length > MAX_REQUEST_ID_BYTES || !/^[A-Za-z0-9._:-]+$/.test(value)) {
    throw new RenderProtocolError(
      "invalid_request_id",
      "requestId must be path-neutral ASCII and at most 128 bytes",
      "requestId"
    );
  }
  return value;
}

export function validateDocumentId(value) {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.length > 128 ||
    !/^[A-Za-z0-9._:-]+$/.test(value)
  ) {
    throw new RenderProtocolError(
      "invalid_document_id",
      "documentId must be path-neutral ASCII and at most 128 characters",
      "documentId"
    );
  }
  return value;
}

export function parseWorkerMessage(message) {
  assertPlainObject(message, "message");
  if (message.protocol !== PROTOCOL) {
    throw new RenderProtocolError(
      "protocol_mismatch",
      `protocol must equal ${PROTOCOL}`,
      "protocol"
    );
  }
  if (message.type === "cancel") {
    assertExactKeys(message, ["protocol", "type", "requestId"], "message");
    return {
      protocol: PROTOCOL,
      type: "cancel",
      requestId: validateRequestId(message.requestId)
    };
  }
  if (message.type !== "request") {
    throw new RenderProtocolError(
      "invalid_message_type",
      "message type must be request or cancel",
      "type"
    );
  }
  assertExactKeys(
    message,
    ["protocol", "type", "requestId", "operation", "payload"],
    "message"
  );
  const requestId = validateRequestId(message.requestId);
  if (!OPERATIONS.has(message.operation)) {
    throw new RenderProtocolError(
      "unknown_operation",
      "operation is not supported",
      "operation"
    );
  }
  const payload = message.payload ?? {};
  assertPlainObject(payload, "payload");
  return {
    protocol: PROTOCOL,
    type: "request",
    requestId,
    operation: message.operation,
    payload
  };
}

export function preflightRequest({ operation, payload }) {
  switch (operation) {
    case "viewport-capabilities":
    case "prepare-viewport":
    case "render-viewport-tile":
    case "release-viewport":
      return validateViewportRequest(operation, payload);
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
    case "edit-status":
    case "undo-edit":
    case "redo-edit":
    case "save-document":
      assertExactKeys(payload, ["documentId"], "payload");
      validateDocumentId(payload.documentId);
      return 0;
    case "read-cell":
      assertExactKeys(
        payload,
        ["documentId", "sheetIndex", "row", "col"],
        "payload"
      );
      validateDocumentId(payload.documentId);
      boundedIndex(payload.sheetIndex, "payload.sheetIndex", MAX_SHEETS, "sheets");
      validateCellCoordinate(payload.row, payload.col);
      return 0;
    case "set-cell":
    case "set-cell-recalculate": {
      assertExactKeys(
        payload,
        ["documentId", "sheetIndex", "row", "col", "value"],
        "payload"
      );
      validateDocumentId(payload.documentId);
      boundedIndex(payload.sheetIndex, "payload.sheetIndex", MAX_SHEETS, "sheets");
      validateCellCoordinate(payload.row, payload.col);
      validateEditableCell(payload.value, "payload.value", true);
      return editJson({
        sheetIndex: payload.sheetIndex,
        row: payload.row,
        col: payload.col,
        value: payload.value
      }).byteLength;
    }
    case "set-range-recalculate": {
      rangeCellData(payload, "payload");
      assertExactKeys(payload, ["documentId", "sheetIndex", "startRow", "startCol", "values"], "payload");
      validateDocumentId(payload.documentId);
      boundedIndex(payload.sheetIndex, "payload.sheetIndex", MAX_SHEETS, "sheets");
      validateCellCoordinate(payload.startRow, payload.startCol);
      return rangeEditBytes(payload);
    }
    case "set-document-properties": {
      assertExactKeys(payload, ["documentId", "properties"], "payload");
      validateDocumentId(payload.documentId);
      validateDocumentProperties(payload.properties);
      return editJson(payload.properties).byteLength;
    }
    case "prepare-pages":
    case "render-sheet":
    case "render-sheet-interactive":
      assertExactKeys(payload, ["documentId", "sheetIndex", "options"], "payload");
      validateDocumentId(payload.documentId);
      boundedIndex(payload.sheetIndex, "payload.sheetIndex", MAX_SHEETS, "sheets");
      optionsJson(payload.options);
      if (operation === "render-sheet-interactive") interactiveSheetLimits(payload.options);
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

export function validateRange(value) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new RenderProtocolError("invalid_range", "range must be an object", "payload.range");
  }
  assertExactKeys(
    value,
    ["firstRow", "firstCol", "lastRow", "lastCol"],
    "payload.range"
  );
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

export function validateCellCoordinate(rowValue, colValue) {
  const row = nonNegativeInteger(rowValue, "payload.row");
  const col = nonNegativeInteger(colValue, "payload.col");
  if (row > 1_048_575 || col > 16_383) {
    throw new RenderProtocolError(
      "cell_out_of_range",
      "cell is outside the Excel grid",
      "payload"
    );
  }
  return { row, col };
}

export function editJson(value) {
  const json = JSON.stringify(value);
  const encoded = new TextEncoder().encode(json);
  if (encoded.byteLength > MAX_EDIT_REQUEST_BYTES) {
    throw limitError(
      "editRequestBytes",
      MAX_EDIT_REQUEST_BYTES,
      encoded.byteLength,
      "payload"
    );
  }
  return encoded;
}

// Count each bounded cell before cloning or serializing the complete matrix.
function rangeEditBytes({ sheetIndex, startRow, startCol, values }) {
  const location = "payload.values";
  if (!Array.isArray(values) || values.length === 0 || values.length > MAX_RANGE_EDIT_CELLS) {
    throw new RenderProtocolError("invalid_edit", "values must be a nonempty bounded matrix", location);
  }
  rangeArray(values, values.length, location);
  if (!Array.isArray(values[0])) {
    throw new RenderProtocolError("invalid_edit", "each range row must be an array", location);
  }
  const width = values[0].length;
  if (!Number.isSafeInteger(width) || width <= 0 || width * values.length > MAX_RANGE_EDIT_CELLS) {
    throw new RenderProtocolError("invalid_edit", "range must contain at most 10000 cells", location);
  }
  validateCellCoordinate(startRow + values.length - 1, startCol + width - 1);
  let bytes = JSON.stringify({ sheetIndex, startRow, startCol, values: [] }).length;
  const add = (count) => {
    bytes += count;
    if (bytes > MAX_RANGE_EDIT_REQUEST_BYTES) {
      throw limitError("editRequestBytes", MAX_RANGE_EDIT_REQUEST_BYTES, bytes, location);
    }
  };
  for (let row = 0; row < values.length; row += 1) {
    rangeArray(values[row], width, `${location}[${row}]`);
    add(2 + (row === 0 ? 0 : 1));
    for (let col = 0; col < width; col += 1) {
      const cell = values[row][col];
      const cellLocation = `${location}[${row}][${col}]`;
      rangeCellData(cell, cellLocation);
      validateEditableCell(cell, cellLocation, true);
      // Per-cell strings remain capped by the original 128KiB input contract.
      const cellBytes = new TextEncoder().encode(JSON.stringify(cell)).byteLength;
      if (cellBytes > MAX_EDIT_REQUEST_BYTES) {
        throw limitError("editRequestBytes", MAX_EDIT_REQUEST_BYTES, cellBytes, cellLocation);
      }
      add(cellBytes + (col === 0 ? 0 : 1));
    }
  }
  return bytes;
}

function rangeArray(value, length, location) {
  if (!Array.isArray(value) || Object.getPrototypeOf(value) !== Array.prototype || value.length !== length || Reflect.ownKeys(value).length !== length + 1) {
    throw new RenderProtocolError("invalid_edit", "range rows must be dense and rectangular", location);
  }
  for (let index = 0; index < length; index += 1) {
    const descriptor = Object.getOwnPropertyDescriptor(value, String(index));
    if (!descriptor?.enumerable || !("value" in descriptor)) {
      throw new RenderProtocolError("invalid_edit", "range entries must be own data values", location);
    }
  }
}

function rangeCellData(value, location) {
  assertPlainObject(value, location);
  for (const key of Reflect.ownKeys(value)) {
    const descriptor = Object.getOwnPropertyDescriptor(value, key);
    if (typeof key !== "string" || !descriptor.enumerable || !("value" in descriptor)) {
      throw new RenderProtocolError("invalid_edit", "cell fields must be own data values", location);
    }
  }
  // Formula caches contain scalars, so inspect at most this one extra level.
  if (value.kind === "formula") {
    assertPlainObject(value.cached, `${location}.cached`);
    for (const key of Reflect.ownKeys(value.cached)) {
      const descriptor = Object.getOwnPropertyDescriptor(value.cached, key);
      if (typeof key !== "string" || !descriptor.enumerable || !("value" in descriptor)) {
        throw new RenderProtocolError("invalid_edit", "cached fields must be own data values", location);
      }
    }
  }
}

export function boundedIndex(value, location, countLimit, resource) {
  const index = nonNegativeInteger(value, location);
  if (index >= countLimit) {
    throw limitError(resource, countLimit, index + 1, location);
  }
  return index;
}

export function positiveInteger(value, location) {
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

export function optionsJson(options) {
  if (options === undefined || options === null) {
    return "{}";
  }
  assertJsonValue(options, "options", 0, { nodes: 0 });
  const json = JSON.stringify(options);
  const bytes = new TextEncoder().encode(json).byteLength;
  if (bytes > MAX_OPTIONS_BYTES) {
    throw limitError("optionsBytes", MAX_OPTIONS_BYTES, bytes, "options");
  }
  return json;
}

export function encodeFontBundle(fontPack) {
  if (fontPack === undefined || fontPack === null) {
    return new Uint8Array();
  }
  const { manifest, members } = validateFontPack(fontPack);
  const envelopeBytes = fontEnvelopeBytes(manifest, members);
  const output = new Uint8Array(envelopeBytes);
  const view = new DataView(output.buffer);
  let offset = 0;
  output.set(FONT_BUNDLE_MAGIC, offset);
  offset += FONT_BUNDLE_MAGIC.byteLength;
  view.setUint32(offset, manifest.byteLength, true);
  offset += 4;
  output.set(manifest, offset);
  offset += manifest.byteLength;
  view.setUint32(offset, members.length, true);
  offset += 4;
  for (const member of members) {
    view.setUint32(offset, member.nameBytes.byteLength, true);
    offset += 4;
    output.set(member.nameBytes, offset);
    offset += member.nameBytes.byteLength;
    view.setUint32(offset, member.bytes.byteLength, true);
    offset += 4;
    output.set(member.bytes, offset);
    offset += member.bytes.byteLength;
  }
  return output;
}

export function fontPackByteLength(fontPack) {
  if (fontPack === undefined || fontPack === null) {
    return 0;
  }
  const { manifest, members } = validateFontPack(fontPack);
  return fontEnvelopeBytes(manifest, members);
}

export function validateFontPack(fontPack) {
  assertPlainObject(fontPack, "fontPack");
  assertExactKeys(fontPack, ["manifest", "members"], "fontPack");
  const manifest = asBytes(fontPack.manifest, "fontPack.manifest");
  if (manifest.byteLength > MAX_FONT_MANIFEST_BYTES) {
    throw limitError(
      "fontManifestBytes",
      MAX_FONT_MANIFEST_BYTES,
      manifest.byteLength,
      "fontPack.manifest"
    );
  }
  if (!Array.isArray(fontPack.members)) {
    throw new RenderProtocolError(
      "invalid_font_pack",
      "fontPack.members must be an array",
      "fontPack.members"
    );
  }
  if (fontPack.members.length > MAX_FONT_FILES) {
    throw limitError(
      "fontFiles",
      MAX_FONT_FILES,
      fontPack.members.length,
      "fontPack.members"
    );
  }
  const names = new Set();
  const members = [];
  let payloadBytes = manifest.byteLength;
  for (let index = 0; index < fontPack.members.length; index += 1) {
    const member = fontPack.members[index];
    assertPlainObject(member, `fontPack.members[${index}]`);
    assertExactKeys(member, ["name", "bytes"], `fontPack.members[${index}]`);
    const name = validateFontMemberName(member.name, index);
    if (names.has(name)) {
      throw new RenderProtocolError(
        "duplicate_font_member",
        "font pack member names must be unique",
        `fontPack.members[${index}].name`
      );
    }
    names.add(name);
    const nameBytes = new TextEncoder().encode(name);
    const bytes = asBytes(member.bytes, `fontPack.members[${index}].bytes`);
    if (bytes.byteLength > MAX_FONT_FILE_BYTES) {
      throw limitError(
        "fontMemberBytes",
        MAX_FONT_FILE_BYTES,
        bytes.byteLength,
        `fontPack.members[${index}].bytes`
      );
    }
    payloadBytes = checkedAdd(payloadBytes, bytes.byteLength, "fontBytes");
    if (payloadBytes > MAX_FONT_BYTES) {
      throw limitError("fontBytes", MAX_FONT_BYTES, payloadBytes, "fontPack");
    }
    members.push({ nameBytes, bytes });
  }
  return { manifest, members };
}

export function validateSvgOutput(svg, maxBytes = MAX_OUTPUT_BYTES) {
  if (typeof svg !== "string") {
    throw new RenderProtocolError("invalid_svg", "renderer returned non-text SVG", "output");
  }
  const bytes = new TextEncoder().encode(svg).byteLength;
  const limit = Math.min(validatedOutputLimit(maxBytes), MAX_OUTPUT_BYTES);
  if (bytes > limit) {
    throw limitError("outputBytes", limit, bytes, "output");
  }
  const trimmed = svg.trim();
  if (
    !/^(?:<\?xml\s[^>]*\?>\s*)?<svg\b/i.test(trimmed) ||
    !/<\/svg>\s*$/i.test(trimmed)
  ) {
    throw new RenderProtocolError("invalid_svg", "renderer returned invalid SVG", "output");
  }
  if (
    /<!DOCTYPE|<!ENTITY|<!\[CDATA|<!--|<\?xml-stylesheet/i.test(
      trimmed
    ) ||
    /\s(?:on[a-z][a-z0-9_-]*|style|src|xml:base)\s*=/i.test(trimmed)
  ) {
    throw new RenderProtocolError(
      "unsafe_svg",
      "SVG contains active content or an external-resource surface",
      "output"
    );
  }
  for (const tag of trimmed.matchAll(/<\/?\s*([A-Za-z][A-Za-z0-9:-]*)\b/g)) {
    if (!SAFE_SVG_ELEMENTS.has(tag[1].toLowerCase())) {
      throw new RenderProtocolError(
        "unsafe_svg",
        "SVG contains an element outside the renderer allowlist",
        "output"
      );
    }
  }

  const hrefAssignments = [...trimmed.matchAll(/\b(?:href|xlink:href)\s*=/gi)].length;
  const hrefs = [...trimmed.matchAll(/\b(?:href|xlink:href)\s*=\s*(["'])(.*?)\1/gis)];
  if (hrefAssignments !== hrefs.length) {
    throw new RenderProtocolError("unsafe_svg", "SVG href values must be quoted", "output");
  }
  for (const href of hrefs) {
    const tagStart = trimmed.lastIndexOf("<", href.index);
    const tag = /^<\s*([A-Za-z][A-Za-z0-9:-]*)/.exec(trimmed.slice(tagStart, href.index));
    const tagName = tag?.[1]?.toLowerCase();
    const value = href[2];
    if (tagName === "image") {
      if (!isEmbeddedPng(value)) {
        throw new RenderProtocolError(
          "external_svg_resource",
          "SVG images must be embedded PNG data",
          "output"
        );
      }
      continue;
    }
    if (tagName !== "a" || !isSafeHyperlink(value)) {
      throw new RenderProtocolError(
        "external_svg_resource",
        "SVG resource references are not allowed",
        "output"
      );
    }
  }

  for (const attribute of trimmed.matchAll(
    /\s(?:clip-path|fill|stroke|filter|mask|marker-start|marker-mid|marker-end|cursor)\s*=\s*(["'])(.*?)\1/gis
  )) {
    for (const match of attribute[2].matchAll(/url\s*\(\s*(["']?)(.*?)\1\s*\)/gis)) {
      if (!/^#[A-Za-z_][A-Za-z0-9_.:-]*$/.test(match[2])) {
        throw new RenderProtocolError(
          "external_svg_resource",
          "SVG URL references must target an in-document fragment",
          "output"
        );
      }
    }
  }
  return bytes;
}

/** Resolve lower request budgets without coupling them to non-interactive paths. */
export function interactiveSheetLimits(options = {}, capabilities = {}) {
  const limits = options?.limits ?? {};
  assertPlainObject(limits, "options.limits");
  const result = {};
  for (const [key, hard] of Object.entries({
    maxCells: MAX_INTERACTION_CELLS,
    maxDimensionRaw: MAX_INTERACTION_DIMENSION_RAW,
    maxOutputBytes: MAX_OUTPUT_BYTES
  })) {
    const requested = limits[key] ?? hard;
    if (!Number.isSafeInteger(requested) || requested <= 0 || requested > hard) {
      throw limitError(key, hard, Number.isSafeInteger(requested) ? requested : 0, "options.limits");
    }
    const capability = capabilities[key] ?? hard;
    if (!Number.isSafeInteger(capability) || capability <= 0 || capability > hard) {
      throw new RenderProtocolError("wasm_api_mismatch", `invalid ${key} capability`, "wasm");
    }
    result[key] = Math.min(requested, capability);
  }
  return result;
}

/** Validate geometry and count the complete JSON envelope without allocating it. */
export function validateInteractiveSheetOutput(value, limits = {}) {
  const budget = interactiveSheetLimits({ limits });
  interactionRecord(value, ["svg", "interaction"]);
  interactionRecord(value.interaction, ["schemaVersion", "width", "height", "cells"]);
  const { schemaVersion, width, height, cells } = value.interaction;
  const maximum = budget.maxDimensionRaw / 1024;
  if (schemaVersion !== 1 || !finitePositive(width) || !finitePositive(height) ||
      width > maximum || height > maximum || !Array.isArray(cells)) {
    throw invalidInteraction("invalid interaction schema or canvas");
  }
  if (cells.length > budget.maxCells) {
    throw limitError("maxCells", budget.maxCells, cells.length, "interaction");
  }
  interactionArray(cells, cells.length);
  if (typeof value.svg !== "string") throw invalidInteraction("SVG must be text");
  let bytes = '{"svg":'.length;
  const add = (count) => {
    bytes += count;
    if (bytes > budget.maxOutputBytes) {
      throw limitError("outputBytes", budget.maxOutputBytes, bytes, "output");
    }
  };
  add(2); // JSON string quotes.
  for (let index = 0; index < value.svg.length; index += 1) {
    const code = value.svg.charCodeAt(index);
    if (code === 34 || code === 92) add(2);
    else if (code < 32) add([8, 9, 10, 12, 13].includes(code) ? 2 : 6);
    else if (code < 128) add(1);
    else if (code < 2048) add(2);
    else if (code >= 0xd800 && code <= 0xdbff &&
             value.svg.charCodeAt(index + 1) >= 0xdc00 && value.svg.charCodeAt(index + 1) <= 0xdfff) {
      add(4);
      index += 1;
    } else add(code >= 0xd800 && code <= 0xdfff ? 6 : 3);
  }
  add(',"interaction":{"schemaVersion":1,"width":'.length + String(width).length);
  add(',"height":'.length + String(height).length + ',"cells":['.length);
  const anchors = new Set();
  for (let index = 0; index < cells.length; index += 1) {
    const cell = cells[index];
    interactionArray(cell, 6);
    const [row, col, x, y, cellWidth, cellHeight] = cell;
    if (!Number.isSafeInteger(row) || row < 0 || row > 1_048_575 ||
        !Number.isSafeInteger(col) || col < 0 || col > 16_383 ||
        !Number.isFinite(x) || x < 0 || !Number.isFinite(y) || y < 0 ||
        !finitePositive(cellWidth) || !finitePositive(cellHeight) ||
        x + cellWidth > width || y + cellHeight > height) {
      throw invalidInteraction("cell rectangle or source coordinate is invalid");
    }
    const anchor = row * 16_384 + col;
    if (anchors.has(anchor)) throw invalidInteraction("duplicate cell anchor");
    anchors.add(anchor);
    add((index === 0 ? 0 : 1) + 2 + cell.map((number) => String(number)).join(",").length);
  }
  add("]}}".length);
  validateSvgOutput(value.svg, budget.maxOutputBytes);
  return bytes;
}

function invalidInteraction(message) {
  return new RenderProtocolError("invalid_interaction", message, "interaction");
}

function finitePositive(value) {
  return typeof value === "number" && Number.isFinite(value) && value > 0;
}

function interactionRecord(value, keys) {
  if (value === null || typeof value !== "object" || Array.isArray(value) ||
      ![Object.prototype, null].includes(Object.getPrototypeOf(value)) ||
      Reflect.ownKeys(value).length !== keys.length) {
    throw invalidInteraction("interaction record has an invalid shape");
  }
  for (const key of keys) {
    const property = Object.getOwnPropertyDescriptor(value, key);
    if (!property?.enumerable || !Object.hasOwn(property, "value")) {
      throw invalidInteraction("interaction fields must be enumerable data properties");
    }
  }
}

function interactionArray(value, length) {
  if (!Array.isArray(value) || Object.getPrototypeOf(value) !== Array.prototype ||
      value.length !== length || Reflect.ownKeys(value).length !== length + 1) {
    throw invalidInteraction("interaction arrays must have an exact dense shape");
  }
  for (let index = 0; index < length; index += 1) {
    const property = Object.getOwnPropertyDescriptor(value, index);
    if (!property?.enumerable || !Object.hasOwn(property, "value")) {
      throw invalidInteraction("interaction arrays must contain only data elements");
    }
  }
}

export function validateRecalculationSummary(value) {
  const fail = () => new RenderProtocolError("invalid_recalculation", "recalculation summary is invalid", "recalculation");
  try {
    interactionRecord(value, ["computedCells", "unchangedCells", "unsupportedCells", "reasons"]);
  } catch { throw fail(); }
  const { computedCells, unchangedCells, unsupportedCells, reasons } = value;
  const allowed = new Set(["unsupported_function", "volatile", "external_reference",
    "circular_reference", "unresolved_name", "unparsable_expression", "array_semantics", "sheet_not_found"]);
  if (![computedCells, unchangedCells, unsupportedCells].every((count) => Number.isSafeInteger(count) && count >= 0) ||
      computedCells + unsupportedCells > 10_000 || unchangedCells > computedCells ||
      !Array.isArray(reasons) || reasons.length > allowed.size || reasons.length > unsupportedCells ||
      (unsupportedCells > 0) !== (reasons.length > 0)) throw fail();
  try { interactionArray(reasons, reasons.length); } catch { throw fail(); }
  for (let index = 0; index < reasons.length; index += 1) {
    if (!allowed.has(reasons[index]) || (index > 0 && reasons[index - 1] >= reasons[index])) throw fail();
  }
  return value;
}

export function normalizeError(error) {
  const code = safeToken(error?.code) ?? "worker_failed";
  const location = safeLocation(error?.location) ?? "worker";
  const message = sanitizeMessage(
    typeof error?.message === "string" ? error.message : "render worker request failed"
  );
  return {
    code,
    message,
    location,
    resource: safeToken(error?.resource),
    limit: safeInteger(error?.limit),
    actual: safeInteger(error?.actual)
  };
}

export function limitError(resource, limit, actual, location = "limits") {
  return new RenderProtocolError(
    "limit_exceeded",
    `${resource} limit exceeded: limit ${limit}, required ${actual}`,
    location,
    { resource, limit, actual }
  );
}

function validateEditableCell(value, location, allowBlank) {
  assertPlainObject(value, location);
  if (typeof value.kind !== "string") {
    throw new RenderProtocolError(
      "invalid_edit",
      `${location}.kind must identify a supported cell value`,
      `${location}.kind`
    );
  }
  switch (value.kind) {
    case "blank":
      if (!allowBlank) {
        throw new RenderProtocolError(
          "invalid_edit",
          "a formula cached value cannot be blank",
          location
        );
      }
      assertExactKeys(value, ["kind"], location);
      return;
    case "text":
    case "error":
      assertExactKeys(value, ["kind", "value"], location);
      validateEditString(value.value, `${location}.value`);
      return;
    case "number":
    case "date":
      assertExactKeys(value, ["kind", "value"], location);
      if (typeof value.value !== "number" || !Number.isFinite(value.value)) {
        throw new RenderProtocolError(
          "invalid_edit",
          `${location}.value must be a finite number`,
          `${location}.value`
        );
      }
      return;
    case "boolean":
      assertExactKeys(value, ["kind", "value"], location);
      if (typeof value.value !== "boolean") {
        throw new RenderProtocolError(
          "invalid_edit",
          `${location}.value must be a boolean`,
          `${location}.value`
        );
      }
      return;
    case "formula":
    case "formula-auto":
      if (!allowBlank) {
        throw new RenderProtocolError(
          "invalid_edit",
          "a formula cached value cannot contain a formula",
          location
        );
      }
      assertExactKeys(value, value.kind === "formula-auto" ? ["kind", "formula"] : ["kind", "formula", "cached"], location);
      validateEditString(value.formula, `${location}.formula`);
      if (value.formula.trim().replace(/^=+/, "").trim().length === 0) {
        throw new RenderProtocolError(
          "invalid_edit",
          `${location}.formula cannot be empty after removing leading '='`,
          `${location}.formula`
        );
      }
      if (value.kind === "formula") validateEditableCell(value.cached, `${location}.cached`, false);
      return;
    default:
      throw new RenderProtocolError(
        "invalid_edit",
        `${location}.kind is not supported`,
        `${location}.kind`
      );
  }
}

function validateDocumentProperties(value) {
  const location = "payload.properties";
  assertPlainObject(value, location);
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
  assertExactKeys(value, fields, location);
  for (const field of fields) {
    if (!Object.prototype.hasOwnProperty.call(value, field)) {
      throw new RenderProtocolError(
        "invalid_edit",
        `${location}.${field} is required; use null to remove it`,
        `${location}.${field}`
      );
    }
    if (value[field] !== null) {
      validateEditString(value[field], `${location}.${field}`);
    }
  }
}

function validateEditString(value, location) {
  if (typeof value !== "string") {
    throw new RenderProtocolError(
      "invalid_edit",
      `${location} must be a string`,
      location
    );
  }
  if (value.length > MAX_EDIT_REQUEST_BYTES) {
    throw limitError("editRequestBytes", MAX_EDIT_REQUEST_BYTES, value.length, location);
  }
  const bytes = new TextEncoder().encode(value).byteLength;
  if (bytes > MAX_EDIT_REQUEST_BYTES) {
    throw limitError("editRequestBytes", MAX_EDIT_REQUEST_BYTES, bytes, location);
  }
}

function validateFontMemberName(value, index) {
  const location = `fontPack.members[${index}].name`;
  if (typeof value !== "string" || value.length === 0 || value.includes("\\")) {
    throw new RenderProtocolError(
      "unsafe_font_path",
      "font member name must be a canonical relative forward-slash path",
      location
    );
  }
  const segments = value.split("/");
  if (
    value.startsWith("/") ||
    segments.some((segment) => segment === "" || segment === "." || segment === "..")
  ) {
    throw new RenderProtocolError(
      "unsafe_font_path",
      "font member name must be a canonical relative forward-slash path",
      location
    );
  }
  const encoded = new TextEncoder().encode(value);
  if (encoded.byteLength > MAX_FONT_NAME_BYTES) {
    throw limitError("fontMemberNameBytes", MAX_FONT_NAME_BYTES, encoded.byteLength, location);
  }
  return value;
}

function assertPlainObject(value, location) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new RenderProtocolError(
      "invalid_object",
      `${location} must be an object`,
      location
    );
  }
  const prototype = Object.getPrototypeOf(value);
  if (prototype !== Object.prototype && prototype !== null) {
    throw new RenderProtocolError(
      "invalid_object",
      `${location} must have a plain prototype`,
      location
    );
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

function assertJsonValue(value, location, depth, budget) {
  budget.nodes += 1;
  if (budget.nodes > MAX_OPTION_NODES) {
    throw limitError("optionNodes", MAX_OPTION_NODES, budget.nodes, location);
  }
  if (depth > 16) {
    throw new RenderProtocolError(
      "options_too_deep",
      "options nesting exceeds 16 levels",
      location
    );
  }
  if (
    value === null ||
    typeof value === "string" ||
    typeof value === "boolean" ||
    (typeof value === "number" && Number.isFinite(value))
  ) {
    return;
  }
  if (Array.isArray(value)) {
    if (value.length > MAX_OPTION_ARRAY_ITEMS) {
      throw limitError("optionArrayItems", MAX_OPTION_ARRAY_ITEMS, value.length, location);
    }
    for (let index = 0; index < value.length; index += 1) {
      assertJsonValue(value[index], `${location}[${index}]`, depth + 1, budget);
    }
    return;
  }
  assertPlainObject(value, location);
  const keys = Object.keys(value);
  if (keys.length > 256) {
    throw limitError("optionKeys", 256, keys.length, location);
  }
  for (const key of keys) {
    assertJsonValue(value[key], `${location}.${key}`, depth + 1, budget);
  }
}

function fontEnvelopeBytes(manifest, members) {
  let envelopeBytes = FONT_BUNDLE_MAGIC.byteLength + 4 + manifest.byteLength + 4;
  for (const member of members) {
    envelopeBytes = checkedAdd(
      envelopeBytes,
      4 + member.nameBytes.byteLength + 4 + member.bytes.byteLength,
      "fontBundleBytes"
    );
  }
  return envelopeBytes;
}

function checkedAdd(left, right, resource) {
  const sum = left + right;
  if (!Number.isSafeInteger(sum)) {
    throw limitError(resource, Number.MAX_SAFE_INTEGER, sum, "fontPack");
  }
  return sum;
}

function validatedOutputLimit(value) {
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new RenderProtocolError(
      "invalid_output_limit",
      "output byte limit must be a positive safe integer",
      "output"
    );
  }
  return value;
}

function isEmbeddedPng(value) {
  const prefix = "data:image/png;base64,";
  if (!value.startsWith(prefix)) {
    return false;
  }
  const encoded = value.slice(prefix.length);
  if (encoded.length === 0 || encoded.length % 4 !== 0) {
    return false;
  }
  const padding = encoded.endsWith("==") ? 2 : encoded.endsWith("=") ? 1 : 0;
  for (let index = 0; index < encoded.length - padding; index += 1) {
    const code = encoded.charCodeAt(index);
    const valid =
      (code >= 65 && code <= 90) ||
      (code >= 97 && code <= 122) ||
      (code >= 48 && code <= 57) ||
      code === 43 ||
      code === 47;
    if (!valid) {
      return false;
    }
  }
  for (let index = encoded.length - padding; index < encoded.length; index += 1) {
    if (encoded.charCodeAt(index) !== 61) {
      return false;
    }
  }
  return true;
}

function isSafeHyperlink(value) {
  if (/[\u0000-\u001f\u007f]/.test(value) || /&#/.test(value)) {
    return false;
  }
  const unknownEntities = value.replace(/&(amp|quot|apos|lt|gt);/g, "");
  if (/&[A-Za-z][A-Za-z0-9]+;/.test(unknownEntities)) {
    return false;
  }
  const decoded = value
    .replaceAll("&amp;", "&")
    .replaceAll("&quot;", '"')
    .replaceAll("&apos;", "'")
    .replaceAll("&lt;", "<")
    .replaceAll("&gt;", ">");
  const target = decoded.trim();
  if (target === "" || target.startsWith("//")) {
    return false;
  }
  const scheme = /^([A-Za-z][A-Za-z0-9+.-]*):/.exec(target)?.[1]?.toLowerCase();
  return scheme === undefined || scheme === "http" || scheme === "https" || scheme === "mailto";
}

function safeToken(value) {
  return typeof value === "string" && /^[a-zA-Z0-9_.:-]{1,128}$/.test(value)
    ? value
    : null;
}

function safeLocation(value) {
  return typeof value === "string" && /^[a-zA-Z0-9_.:[\]-]{1,160}$/.test(value)
    ? value
    : "worker";
}

function safeInteger(value) {
  return Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function sanitizeMessage(value) {
  let message = value.replace(/[\r\n\t]+/g, " ").slice(0, 512);
  message = message.replace(/file:\/\/\S+/gi, "[path]");
  message = message.replace(/(?:[A-Za-z]:\\|\/(?:Users|home|tmp|private|var)\/)[^\s,;)]*/g, "[path]");
  return message || "render worker request failed";
}

// Additive viewport contracts; legacy worker schemas and caps remain unchanged.
export const VIEWPORT_LIMITS = Object.freeze({
  maxOptionsBytes: 65_536,
  maxCoordinateVisits: 2_000_000,
  maxAxisRuns: 65_536,
  maxGeometryBytes: 8_388_608,
  maxLogicalDimensionRaw: 16_384_000_000,
  maxSourceRecords: 250_000,
  maxTileDimensionRaw: 8_388_608,
  maxTileSvgBytes: 2_097_152,
  maxTileSceneNodes: 100_000,
  maxMetadataBytes: 65_536
});
export const VIEWPORT_RESOURCE_POLICY = Object.freeze({
  maxPreparedPerDocument: 1,
  maxPreparedTotal: 4,
  maxProvisionalTotal: 1,
  geometryReservationBytes: 8_388_608,
  sourceIndexReservationBytes: 8_388_608,
  maxOpenResourceBytes: 134_217_728
});

/** Validate canonical decimal transport without rounding through Number. */
export function validateViewportU64(value, location = "viewport.u64") {
  if (typeof value !== "string" || !/^(?:0|[1-9][0-9]{0,19})$/.test(value) ||
      (value.length === 20 && value > "18446744073709551615")) {
    throw invalidViewport(`invalid canonical u64 at ${location}`);
  }
  return value;
}

/** Worker-owned UUID plus globally monotonic checked preparation attempt. */
export function validateViewportGeometryId(value) {
  if (typeof value !== "string" || value.length > 60 ||
      !/^vp-[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}-[1-9][0-9]{0,19}$/.test(value)) {
    throw invalidViewport("invalid viewport geometry identity");
  }
  validateViewportU64(value.slice(40), "geometryId.counter");
  return value;
}

/** Fixed-point sheet-space request; a browser pixel rectangle is not accepted. */
export function validateViewportRect(value) {
  viewportRecord(value, ["xRaw", "yRaw", "widthRaw", "heightRaw"]);
  const { xRaw, yRaw, widthRaw, heightRaw } = value;
  viewportCount(xRaw, VIEWPORT_LIMITS.maxLogicalDimensionRaw);
  viewportCount(yRaw, VIEWPORT_LIMITS.maxLogicalDimensionRaw);
  viewportCount(widthRaw, VIEWPORT_LIMITS.maxTileDimensionRaw, true);
  viewportCount(heightRaw, VIEWPORT_LIMITS.maxTileDimensionRaw, true);
  viewportCount(xRaw + widthRaw, VIEWPORT_LIMITS.maxLogicalDimensionRaw);
  viewportCount(yRaw + heightRaw, VIEWPORT_LIMITS.maxLogicalDimensionRaw);
  return value;
}

/** A Used-only viewport request reuses existing bounded render options. */
export function viewportOptionsJson(value = {}) {
  if (value === undefined) value = {};
  viewportOptionalRecord(value, [], ["gridlines", "includeHidden", "limits"]);
  for (const key of ["gridlines", "includeHidden"]) {
    if (Object.hasOwn(value, key) && typeof value[key] !== "boolean") throw invalidViewport("invalid viewport boolean option");
  }
  if (Object.hasOwn(value, "limits")) {
    const allowed = ["maxRows", "maxColumns", "maxCells", "maxConditionalRules", "maxConditionalEvaluations",
      "maxDrawingObjects", "maxMediaBytes", "maxImageDimension", "maxImagePixels", "maxDecodedMediaBytes",
      "maxChartSeries", "maxChartPoints", "maxTextBytes", "maxGlyphs", "maxTextRuns", "maxTextLines",
      "maxPathCommands", "maxSceneNodes", "maxDimensionRaw", "maxOutputBytes", "maxLogicalPages", "maxPages",
      "maxTotalSceneNodes", "maxBackendCommands", "maxRasterDimension", "maxRasterPixels", "maxPngBytes",
      "maxImageBytes", "maxImages", "maxFontBytes"];
    viewportOptionalRecord(value.limits, [], allowed);
    for (const key of Object.keys(value.limits)) viewportCount(value.limits[key], Number.MAX_SAFE_INTEGER);
  }
  return optionsJson(value);
}

/** Validate new payload data before property reads, JSON, or structured cloning. */
export function validateViewportRequest(operation, payload) {
  if (operation === "viewport-capabilities") {
    viewportRecord(payload, []);
    return 0;
  }
  const identity = ["documentId", "sheetIndex"];
  if (operation === "prepare-viewport") {
    viewportOptionalRecord(payload, identity, ["options"]);
  } else if (operation === "render-viewport-tile") {
    viewportRecord(payload, [...identity, "geometryId", "revision", "rect", "namespace"]);
  } else if (operation === "release-viewport") {
    viewportRecord(payload, [...identity, "geometryId", "revision"]);
  } else throw invalidViewport("unknown viewport operation");
  validateDocumentId(payload.documentId);
  boundedIndex(payload.sheetIndex, "payload.sheetIndex", MAX_SHEETS, "sheets");
  if (operation === "prepare-viewport") viewportOptionsJson(payload.options);
  else {
    validateViewportGeometryId(payload.geometryId);
    validateViewportU64(payload.revision, "revision");
    if (operation === "render-viewport-tile") {
      validateViewportRect(payload.rect);
      validateViewportU64(payload.namespace, "namespace");
    }
  }
  // Options have their own 64 KiB ceiling; count the bounded envelope too.
  return new TextEncoder().encode(JSON.stringify(payload)).byteLength;
}

/** Reject wrong protocol generations and raised new-path capabilities. */
export function validateViewportCapabilities(value, workerResult = false) {
  viewportRecord(value, workerResult ? ["schemaVersion", "unitsPerPixel", "limits", "resourcePolicy"] :
    ["schemaVersion", "unitsPerPixel", "limits"]);
  if (value.schemaVersion !== 1 || value.unitsPerPixel !== 1024) throw invalidViewport("invalid viewport capabilities");
  viewportRecord(value.limits, Object.keys(VIEWPORT_LIMITS));
  for (const key of Object.keys(VIEWPORT_LIMITS)) {
    if (value.limits[key] !== VIEWPORT_LIMITS[key]) throw invalidViewport("viewport capabilities differ from the fixed contract");
  }
  if (workerResult) {
    viewportRecord(value.resourcePolicy, Object.keys(VIEWPORT_RESOURCE_POLICY));
    for (const key of Object.keys(VIEWPORT_RESOURCE_POLICY)) {
      if (value.resourcePolicy[key] !== VIEWPORT_RESOURCE_POLICY[key]) throw invalidViewport("invalid viewport resource policy");
    }
  }
  viewportMetadataBytes(value);
  return value;
}

const VIEWPORT_DESCRIPTOR_KEYS = ["schemaVersion", "sheetIndex", "geometryId", "revision", "sourceRange",
  "widthRaw", "heightRaw", "sheetVisibility", "preparationReport"];

/** Validate a provisional native descriptor before it can become published. */
export function validateViewportDescriptor(value, expected) {
  viewportRecord(value, VIEWPORT_DESCRIPTOR_KEYS);
  if (value.schemaVersion !== 1 || value.sheetIndex !== expected.sheetIndex ||
      (expected.geometryId !== undefined && value.geometryId !== expected.geometryId)) throw invalidViewport("viewport descriptor identity mismatch");
  validateViewportGeometryId(value.geometryId);
  validateViewportU64(value.revision, "revision");
  viewportCount(value.widthRaw, VIEWPORT_LIMITS.maxLogicalDimensionRaw);
  viewportCount(value.heightRaw, VIEWPORT_LIMITS.maxLogicalDimensionRaw);
  if (!["visible", "hidden", "veryHidden"].includes(value.sheetVisibility)) throw invalidViewport("invalid worksheet visibility");
  if (value.sourceRange === null) {
    if (value.widthRaw !== 0 || value.heightRaw !== 0 || value.preparationReport !== null) throw invalidViewport("empty source must have zero geometry and no fabricated preparation report");
  } else {
    viewportRange(value.sourceRange);
    viewportPreparationReport(value.preparationReport);
  }
  viewportMetadataBytes(value);
  return value;
}

/** Worker descriptors add document identity and conservative reserved resources. */
export function validateViewportPrepareResult(value, expected) {
  viewportRecord(value, ["documentId", ...VIEWPORT_DESCRIPTOR_KEYS, "resources"]);
  if (value.documentId !== expected.documentId) throw invalidViewport("viewport document identity mismatch");
  validateViewportDescriptor(Object.fromEntries(VIEWPORT_DESCRIPTOR_KEYS.map((key) => [key, value[key]])), expected);
  viewportRecord(value.resources, ["geometryReservationBytes", "sourceIndexReservationBytes"]);
  for (const key of ["geometryReservationBytes", "sourceIndexReservationBytes"]) {
    if (value.resources[key] !== VIEWPORT_RESOURCE_POLICY[key]) throw invalidViewport("viewport reservation mismatch");
  }
  viewportMetadataBytes(value);
  return value;
}

/** Tile responses carry logical geometry even when outside/empty paint is null. */
export function validateViewportTileResult(value, expected, workerResult = true) {
  const keys = ["schemaVersion", "sheetIndex", "geometryId", "revision", "namespace", "requestedRect",
    "mimeType", "logicalRect", "sourceRange", "svg", "report", "metrics"];
  viewportRecord(value, workerResult ? ["documentId", ...keys] : keys);
  if (value.schemaVersion !== 1 || value.sheetIndex !== expected.sheetIndex ||
      value.geometryId !== expected.geometryId || value.revision !== expected.revision ||
      value.namespace !== expected.namespace || value.mimeType !== "image/svg+xml" ||
      (workerResult && value.documentId !== expected.documentId)) throw invalidViewport("viewport tile identity mismatch");
  validateViewportGeometryId(value.geometryId);
  validateViewportU64(value.revision, "revision");
  validateViewportU64(value.namespace, "namespace");
  validateViewportRect(value.requestedRect);
  for (const key of ["xRaw", "yRaw", "widthRaw", "heightRaw"]) {
    if (value.requestedRect[key] !== expected.rect[key]) throw invalidViewport("viewport requested rectangle mismatch");
  }
  if (value.logicalRect === null) {
    if ([value.sourceRange, value.svg, value.report, value.metrics].some((entry) => entry !== null)) throw invalidViewport("mixed empty viewport output");
  } else {
    validateViewportRect(value.logicalRect);
    if (value.logicalRect.xRaw !== expected.rect.xRaw || value.logicalRect.yRaw !== expected.rect.yRaw ||
        value.logicalRect.widthRaw > expected.rect.widthRaw || value.logicalRect.heightRaw > expected.rect.heightRaw) throw invalidViewport("invalid clipped viewport output");
    viewportRange(value.sourceRange);
    if (typeof value.svg !== "string" || value.svg.length > VIEWPORT_LIMITS.maxTileSvgBytes) throw invalidViewport("invalid viewport SVG size");
    const svgBytes = validateSvgOutput(value.svg, VIEWPORT_LIMITS.maxTileSvgBytes);
    viewportTileReport(value.report, expected.sheetIndex, value.sourceRange, svgBytes);
    viewportRecord(value.metrics, ["coordinateVisits", "geometryBytes", "haloRows", "haloColumns", "haloCells"]);
    for (const [key, limit] of Object.entries({ coordinateVisits: 2_000_000, geometryBytes: 8_388_608,
      haloRows: 4096, haloColumns: 512, haloCells: 250_000 })) viewportCount(value.metrics[key], limit);
    const totalRows = value.report.visible_rows + value.metrics.haloRows;
    const totalColumns = value.report.visible_columns + value.metrics.haloColumns;
    viewportCount(totalRows, 4096);
    viewportCount(totalColumns, 512);
    const totalGrid = totalRows * totalColumns;
    viewportCount(totalGrid, 250_000);
    const visibleGrid = value.report.visible_rows * value.report.visible_columns;
    viewportCount(visibleGrid, 250_000);
    if (value.metrics.haloCells !== totalGrid - visibleGrid) throw invalidViewport("invalid viewport halo count");
  }
  const metadata = Object.fromEntries((workerResult ? ["documentId", ...keys] : keys).filter((key) => key !== "svg").map((key) => [key, value[key]]));
  viewportMetadataBytes(metadata);
  return value;
}

/** Release acknowledgements cannot acknowledge another source revision. */
export function validateViewportReleaseResult(value, expected) {
  viewportRecord(value, ["schemaVersion", "documentId", "sheetIndex", "geometryId", "revision", "released"]);
  if (value.schemaVersion !== 1 || typeof value.released !== "boolean") throw invalidViewport("invalid viewport release acknowledgement");
  for (const key of ["documentId", "sheetIndex", "geometryId", "revision"]) {
    if (value[key] !== expected[key]) throw invalidViewport("viewport release identity mismatch");
  }
  validateViewportGeometryId(value.geometryId);
  validateViewportU64(value.revision);
  return value;
}

function viewportPreparationReport(value) {
  const bounds = { coordinateVisits: 2_000_000, sourceRawCells: 250_000, sourceHyperlinks: 250_000,
    sourceIndexBuildPeakBytes: 8_388_608, geometryBytes: 8_388_608, textBytes: 8_388_608,
    shapedGlyphs: 1_000_000, textWork: Number.MAX_SAFE_INTEGER, shapedRuns: 500_000,
    textLines: 250_000, pathCommands: 4_000_000, conditionalEvaluations: 500_000 };
  viewportRecord(value, [...Object.keys(bounds), "fontPackSha256", "fontFaces", "warnings"]);
  for (const key of Object.keys(bounds)) viewportCount(value[key], bounds[key]);
  if (value.sourceRawCells + value.sourceHyperlinks > 250_000) throw invalidViewport("source-index entry ceiling exceeded");
  viewportFontWarnings(value.fontPackSha256, value.fontFaces, value.warnings);
}

function viewportTileReport(value, sheetIndex, range, svgBytes) {
  const countKeys = ["rows_considered", "columns_considered", "cells_considered", "visible_rows", "visible_columns",
    "rendered_regions", "hidden_rows_skipped", "hidden_columns_skipped", "merged_regions", "text_bytes", "glyphs", "scene_nodes", "svg_bytes"];
  viewportRecord(value, ["schema_version", "sheet_index", "sheet_name", "range", ...countKeys, "font_pack_sha256", "font_faces", "warnings"]);
  if (value.schema_version !== 2 || value.sheet_index !== sheetIndex) throw invalidViewport("tile report identity mismatch");
  viewportText(value.sheet_name, 4096);
  for (const key of countKeys) viewportCount(value[key], Number.MAX_SAFE_INTEGER);
  viewportCount(value.visible_rows, 4096);
  viewportCount(value.visible_columns, 512);
  viewportCount(value.cells_considered, 250_000);
  viewportCount(value.text_bytes, 8_388_608);
  viewportCount(value.glyphs, 1_000_000);
  viewportCount(value.scene_nodes, 100_000);
  if (value.svg_bytes !== svgBytes) throw invalidViewport("tile SVG byte count mismatch");
  viewportRecord(value.range, ["first_row", "first_col", "last_row", "last_col"]);
  const actualRange = { firstRow: value.range.first_row, firstCol: value.range.first_col,
    lastRow: value.range.last_row, lastCol: value.range.last_col };
  viewportRange(actualRange);
  for (const key of Object.keys(actualRange)) if (actualRange[key] !== range[key]) throw invalidViewport("tile source range mismatch");
  viewportFontWarnings(value.font_pack_sha256, value.font_faces, value.warnings);
}

function viewportFontWarnings(hash, faces, warnings) {
  const digest = (value) => typeof value === "string" && /^[0-9a-f]{64}$/.test(value);
  if (hash !== null && !digest(hash)) throw invalidViewport("invalid viewport font digest");
  viewportArray(faces, 512);
  for (const face of faces) {
    viewportRecord(face, ["source_pack_sha256", "face_sha256", "family", "weight", "italic", "substituted"]);
    if (!digest(face.source_pack_sha256) || !digest(face.face_sha256) ||
        typeof face.italic !== "boolean" || typeof face.substituted !== "boolean") throw invalidViewport("invalid viewport font face");
    viewportText(face.family, 4096);
    viewportCount(face.weight, 1000, true);
  }
  viewportArray(warnings, 512);
  for (const warning of warnings) {
    viewportRecord(warning, ["code", "occurrences", "first_cell"]);
    viewportText(warning.code, 128);
    viewportCount(warning.occurrences, Number.MAX_SAFE_INTEGER, true);
    if (warning.first_cell !== null) {
      viewportRecord(warning.first_cell, ["row", "col"]);
      viewportCount(warning.first_cell.row, 1_048_575);
      viewportCount(warning.first_cell.col, 16_383);
    }
  }
}

function viewportRange(value) {
  viewportRecord(value, ["firstRow", "firstCol", "lastRow", "lastCol"]);
  validateRange(value);
  viewportCount(value.lastRow, 1_048_575);
  viewportCount(value.lastCol, 16_383);
}

function viewportRecord(value, keys) {
  try { interactionRecord(value, keys); } catch { throw invalidViewport("viewport record must contain exact data properties"); }
}

function viewportOptionalRecord(value, required, optional) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw invalidViewport("invalid viewport record");
  const present = Reflect.ownKeys(value);
  if (present.some((key) => ![...required, ...optional].includes(key)) ||
      required.some((key) => !present.includes(key))) throw invalidViewport("invalid viewport fields");
  viewportRecord(value, present);
}

function viewportArray(value, max) {
  if (!Array.isArray(value) || value.length > max) throw invalidViewport("viewport array exceeds its bound");
  try { interactionArray(value, value.length); } catch { throw invalidViewport("viewport array must be dense data elements"); }
}

function viewportText(value, max) {
  if (typeof value !== "string" || value.length > max) throw invalidViewport("invalid viewport text");
}

function viewportCount(value, max, positive = false) {
  if (!Number.isSafeInteger(value) || value < (positive ? 1 : 0) || value > max) throw invalidViewport("viewport count exceeds its bound");
}

function viewportMetadataBytes(value) {
  const json = JSON.stringify(value);
  if (json.length > VIEWPORT_LIMITS.maxMetadataBytes) throw limitError("viewportMetadataBytes", VIEWPORT_LIMITS.maxMetadataBytes, json.length, "viewport");
  const bytes = new TextEncoder().encode(json).byteLength;
  if (bytes > VIEWPORT_LIMITS.maxMetadataBytes) throw limitError("viewportMetadataBytes", VIEWPORT_LIMITS.maxMetadataBytes, bytes, "viewport");
  return bytes;
}

function invalidViewport(message) {
  return new RenderProtocolError("invalid_viewport", message, "viewport");
}
