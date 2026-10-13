/** Internal response contracts shared by the client and WASM boundary. */
import { validateRecalculationSummary } from "./protocol.mjs";

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
  "editedParts",
  "calculation"
];

const EDIT_REASONS = new Set([
  "legacy-biff",
  "binary-package",
  "open-document",
  "package-metadata-loss"
]);

/**
 * Keep data rules independent of transport identity and error classification.
 * Limits must come from the calling boundary: the client uses its hard limits,
 * while the worker uses its already-validated, possibly lower WASM limits.
 * This factory is internal; it does not add a package export or wire field.
 */
export function createResponseValidators({
  invalid,
  maxSheets,
  maxOutputBytes,
  maxHistoryEntries,
  maxHistoryBytes
}) {
  if (typeof invalid !== "function") {
    throw new TypeError("response validators require an error factory");
  }
  for (const [name, value] of Object.entries({
    maxSheets, maxOutputBytes, maxHistoryEntries, maxHistoryBytes
  })) {
    if (!Number.isSafeInteger(value) || value <= 0) {
      throw new TypeError(`${name} must be a positive safe integer`);
    }
  }

  return Object.freeze({
    validateWorkbookInspection,
    validateDocumentProperties,
    validateEditState,
    validateRecalculationMatch,
    validateInspectedCell
  });

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
      value.sheetCount > maxSheets ||
      !Array.isArray(value.sheets) ||
      value.sheets.length !== value.sheetCount ||
      !nonNegativeSafeInteger(value.embeddedImages) ||
      !nonNegativeSafeInteger(value.embeddedImageBytes) ||
      (value.fontPackSha256 !== null && !validSha256(value.fontPackSha256)) ||
      !nonNegativeSafeInteger(value.fontFaces)
    ) {
      throw invalid("workbook inspection is invalid");
    }
    validateDocumentProperties(value.properties);
    let embeddedImages = 0;
    for (const [index, sheet] of value.sheets.entries()) {
      assertPlainRecord(sheet, "workbook sheet inspection");
      assertExactKeys(sheet, ["index", "name", "embeddedImages"], "workbook sheet inspection");
      if (
        sheet.index !== index ||
        !boundedWorkerString(sheet.name, maxOutputBytes) ||
        !nonNegativeSafeInteger(sheet.embeddedImages)
      ) {
        throw invalid("workbook sheet inspection is invalid");
      }
      embeddedImages += sheet.embeddedImages;
    }
    if (!Number.isSafeInteger(embeddedImages) || embeddedImages !== value.embeddedImages) {
      throw invalid("workbook image totals are inconsistent");
    }
    return value;
  }

  function validateDocumentProperties(value) {
    assertPlainRecord(value, "document properties");
    assertExactKeys(value, DOCUMENT_PROPERTY_KEYS, "document properties");
    if (
      DOCUMENT_PROPERTY_KEYS.some(
        (key) => value[key] !== null && typeof value[key] !== "string"
      )
    ) {
      throw invalid("document properties are invalid");
    }
    return value;
  }

  // Called after both independently bounded contracts have been validated.
  function validateRecalculationMatch(editState, summary) {
    assertPlainRecord(editState, "edit state");
    assertPlainRecord(summary, "recalculation summary");
    assertPlainRecord(editState.calculation, "calculation observation");
    const observation = editState.calculation;
    const actual = observation.summary;
    if (observation.status !== "observed" ||
        actual.computedCells !== summary.computedCells ||
        actual.unchangedCells !== summary.unchangedCells ||
        actual.unsupportedCells !== summary.unsupportedCells ||
        actual.reasons.length !== summary.reasons.length ||
        actual.reasons.some((reason, index) => reason !== summary.reasons[index])) {
      throw invalid("recalculating edit must report its current calculation observation");
    }
  }

  function validateCalculationObservation(value) {
    assertPlainRecord(value, "calculation observation");
    assertExactKeys(value, ["status", "summary"], "calculation observation");
    if (value.status === "unknown") {
      if (value.summary !== null) throw invalid("unknown calculation state must not claim an observation");
    } else if (value.status === "observed" || value.status === "stale") {
      try { validateRecalculationSummary(value.summary); }
      catch { throw invalid("calculation observation summary is invalid"); }
    } else {
      throw invalid("calculation observation status is invalid");
    }
  }

  function validateEditState(value) {
    assertPlainRecord(value, "edit state");
    assertExactKeys(value, EDIT_STATE_KEYS, "edit state");
    if (
      value.schemaVersion !== 2 ||
      !["read-write", "read-only"].includes(value.capability) ||
      (value.reason !== null && !EDIT_REASONS.has(value.reason)) ||
      typeof value.dirty !== "boolean" ||
      typeof value.canUndo !== "boolean" ||
      typeof value.canRedo !== "boolean" ||
      !nonNegativeSafeInteger(value.undoDepth) ||
      !nonNegativeSafeInteger(value.redoDepth) ||
      value.undoDepth + value.redoDepth > maxHistoryEntries ||
      !nonNegativeSafeInteger(value.historyBytes) ||
      value.historyBytes > maxHistoryBytes ||
      !Array.isArray(value.editedParts) ||
      value.editedParts.length > maxSheets + 8 ||
      value.editedParts.some((part) => !safeEditedPart(part))
    ) {
      throw invalid("edit state is invalid");
    }
    validateCalculationObservation(value.calculation);
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
      throw invalid("edit state contains contradictory metadata");
    }
    return value;
  }

  function validateInspectedCell(value, depth = 0) {
    assertPlainRecord(value, "inspected cell");
    if (depth > 16 || typeof value.kind !== "string") {
      throw invalid("inspected cell is invalid");
    }
    switch (value.kind) {
      case "blank":
        assertExactKeys(value, ["kind"], "blank cell");
        return value;
      case "text":
      case "error":
        assertExactKeys(value, ["kind", "value"], "text cell");
        if (typeof value.value !== "string") {
          throw invalid("text cell value is invalid");
        }
        return value;
      case "number":
      case "date":
        assertExactKeys(value, ["kind", "value"], "numeric cell");
        if (typeof value.value !== "number" || !Number.isFinite(value.value)) {
          throw invalid("numeric cell value is invalid");
        }
        return value;
      case "boolean":
        assertExactKeys(value, ["kind", "value"], "boolean cell");
        if (typeof value.value !== "boolean") {
          throw invalid("boolean cell value is invalid");
        }
        return value;
      case "formula":
        assertExactKeys(value, ["kind", "formula", "cached"], "formula cell");
        if (typeof value.formula !== "string") {
          throw invalid("formula cell value is invalid");
        }
        validateInspectedCell(value.cached, depth + 1);
        return value;
      default:
        throw invalid("inspected cell kind is invalid");
    }
  }

  function assertPlainRecord(value, name) {
    if (!isPlainRecord(value)) {
      throw invalid(`${name} must be a plain object`);
    }
  }

  function assertExactKeys(value, expected, name) {
    const keys = Object.keys(value);
    if (keys.length !== expected.length || expected.some((key) => !Object.hasOwn(value, key))) {
      throw invalid(`${name} has an invalid shape`);
    }
  }
}

function isPlainRecord(value) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return false;
  }
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function nonNegativeSafeInteger(value) {
  return Number.isSafeInteger(value) && value >= 0;
}

function validSha256(value) {
  return typeof value === "string" && /^[0-9a-f]{64}$/.test(value);
}

function boundedWorkerString(value, maxBytes) {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    value.length <= maxBytes &&
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
