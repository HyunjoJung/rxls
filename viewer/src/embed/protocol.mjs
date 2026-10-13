export const EMBED_PROTOCOL = "rxls.editor-embed.v1";
export const MAX_WORKBOOK_BYTES = 32 * 1024 * 1024;
export const READY_TIMEOUT_MS = 30_000;
export const OPERATION_TIMEOUT_MS = 60_000;
export const DISPOSE_TIMEOUT_MS = 1_000;
const ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const DIGEST = /^[0-9a-f]{64}$/;
const FORMATS = new Set(["xls", "xlsx", "xlsm", "xlsb", "ods"]);
const CODE = /^[a-z][a-z0-9_]{0,63}$/;

export class EditorEmbedError extends Error {
  constructor(code, message) {
    super(message);
    this.name = "EditorEmbedError";
    this.code = code;
  }
}

export function requireCondition(condition, code, message) {
  if (!condition) throw new EditorEmbedError(code, message);
}

function record(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

export function validIdentity(value) {
  return record(value) && ID.test(value.instanceId) && ID.test(value.nonce);
}

export function sameIdentity(value, identity) {
  return validIdentity(value) && value.instanceId === identity.instanceId &&
    value.nonce === identity.nonce;
}

export function envelope(identity, type, extra = {}) {
  return { ...extra, protocol: EMBED_PROTOCOL, ...identity, type };
}

export function requireEnvelope(value, identity) {
  requireCondition(record(value) && sameIdentity(value, identity),
    "invalid_message", "The editor response identity is invalid.");
  requireCondition(value.protocol === EMBED_PROTOCOL, "protocol_mismatch",
    "The editor protocol version does not match this client.");
}

export function requireRequestId(value) {
  requireCondition(Number.isSafeInteger(value) && value > 0,
    "invalid_message", "The editor request identity is invalid.");
}

export function requireWorkbookName(value) {
  requireCondition(typeof value === "string" && value.length > 0 &&
    value.length <= 255 && !/[\u0000-\u001f\u007f\\/]/.test(value) &&
    value.lastIndexOf(".") > 0 && FORMATS.has(value.split(".").at(-1)?.toLowerCase()),
  "invalid_filename", "Use a workbook filename with a supported extension.");
  return value;
}

export function requireBuffer(value) {
  requireCondition(value instanceof ArrayBuffer && value.byteLength > 0 &&
    value.byteLength <= MAX_WORKBOOK_BYTES, "invalid_bytes",
  "The workbook must contain between 1 byte and 32 MiB.");
  return value;
}

export function copyWorkbook(value) {
  requireCondition(value instanceof Uint8Array, "invalid_bytes",
    "Pass workbook bytes as a Uint8Array.");
  requireCondition(value.byteLength > 0 && value.byteLength <= MAX_WORKBOOK_BYTES,
    "invalid_bytes", "The workbook must contain between 1 byte and 32 MiB.");
  return new Uint8Array(value).buffer;
}

export function requireState(value) {
  requireCondition(record(value), "invalid_message", "The editor state is invalid.");
  for (const key of ["loaded", "busy", "dirty", "draft", "pendingMutation", "canUndo", "canRedo"]) {
    requireCondition(typeof value[key] === "boolean", "invalid_message", "The editor state is invalid.");
  }
  requireCondition(Number.isInteger(value.sheetCount) && value.sheetCount >= 0 &&
    value.sheetCount <= 255 && Number.isInteger(value.sheetIndex) &&
    value.sheetIndex >= 0 && value.sheetIndex < Math.max(1, value.sheetCount),
  "invalid_message", "The editor sheet identity is invalid.");
  requireCondition(value.capability === null || ["read-only", "read-write"].includes(value.capability),
    "invalid_message", "The editor capability is invalid.");
  requireCondition(value.fileName === null || (typeof value.fileName === "string" && value.fileName.length <= 255),
    "invalid_message", "The editor filename is invalid.");
  requireCondition(value.format === null || FORMATS.has(value.format),
    "invalid_message", "The editor format is invalid.");
  requireCondition(value.reason === null || (typeof value.reason === "string" && value.reason.length <= 128),
    "invalid_message", "The editor reason is invalid.");
  return Object.freeze(Object.fromEntries([
    "loaded", "busy", "dirty", "draft", "pendingMutation", "canUndo", "canRedo", "fileName",
    "format", "sheetCount", "sheetIndex", "capability", "reason",
  ].map((key) => [key, value[key]])));
}

export function requireRuntimeIdentity(value) {
  requireCondition(record(value) && DIGEST.test(value.bundleId) &&
    typeof value.packageVersion === "string" && value.packageVersion.length > 0 && value.packageVersion.length <= 64 &&
    value.workerProtocol === "rxls.render-worker.v2", "runtime_mismatch",
  "The packaged editor runtime identity is invalid.");
  return Object.freeze({ bundleId: value.bundleId, packageVersion: value.packageVersion, workerProtocol: value.workerProtocol });
}

export function publicError(error) {
  const code = typeof error?.code === "string" && CODE.test(error.code)
    ? error.code : "operation_failed";
  // Do not expose workbook text or arbitrary native/worker exception strings.
  const messages = {
    busy: "Wait for the current editor operation to finish.",
    dirty_replacement: "Discard or save workbook changes before replacing it.",
    draft_pending: "Apply or cancel unapplied dialog and paste changes before saving.",
    draft_not_committed: "Finish the current cell edit before saving.",
    read_only: "This workbook is available for viewing only.",
    no_workbook: "Load a workbook before saving.",
    disposed: "The editor instance has been disposed.",
    stale_operation: "The workbook changed during the editor operation.",
    operation_failed: "The editor operation failed.",
  };
  return { code, message: messages[code] ?? "The editor operation failed." };
}

export function parseBootstrap(location) {
  const params = new URLSearchParams(location.hash.slice(1));
  const identity = { instanceId: params.get("instance"), nonce: params.get("nonce") };
  const parentOrigin = params.get("parentOrigin");
  requireCondition(validIdentity(identity), "invalid_bootstrap", "The editor instance identity is invalid.");
  let parent;
  try { parent = new URL(parentOrigin); } catch { /* validated below */ }
  requireCondition(parent && ["http:", "https:"].includes(parent.protocol) &&
    parent.origin === parentOrigin, "invalid_bootstrap", "The editor parent origin is invalid.");
  return { identity, parentOrigin };
}
