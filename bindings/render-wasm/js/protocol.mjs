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
export const MAX_EDIT_HISTORY_ENTRIES = 20;
export const MAX_EDIT_HISTORY_BYTES = MAX_INPUT_BYTES;
export const MAX_PENDING_REQUESTS = 32;
export const MAX_PENDING_RESOURCE_BYTES = 128 * 1024 * 1024;
export const MAX_OUTPUT_BYTES = 16 * 1024 * 1024;
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
  "capabilities",
  "open",
  "close",
  "prepare-pages",
  "render-sheet",
  "render-tile",
  "render-page",
  "render-page-png",
  "edit-status",
  "read-cell",
  "set-cell",
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
    case "set-cell": {
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
    case "set-document-properties": {
      assertExactKeys(payload, ["documentId", "properties"], "payload");
      validateDocumentId(payload.documentId);
      validateDocumentProperties(payload.properties);
      return editJson(payload.properties).byteLength;
    }
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
      if (!allowBlank) {
        throw new RenderProtocolError(
          "invalid_edit",
          "a formula cached value cannot contain a formula",
          location
        );
      }
      assertExactKeys(value, ["kind", "formula", "cached"], location);
      validateEditString(value.formula, `${location}.formula`);
      if (value.formula.trim().replace(/^=+/, "").trim().length === 0) {
        throw new RenderProtocolError(
          "invalid_edit",
          `${location}.formula cannot be empty after removing leading '='`,
          `${location}.formula`
        );
      }
      validateEditableCell(value.cached, `${location}.cached`, false);
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
