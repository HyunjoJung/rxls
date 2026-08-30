export const ACCEPTED_EXTENSIONS = new Set(["xls", "xlsx", "xlsm", "xlsb", "ods"]);
export const MAX_WORKSHEET_ROWS = 1_048_576;
export const MAX_WORKSHEET_COLUMNS = 16_384;

export function extensionOf(name) {
  const match = /\.([^.]+)$/.exec(String(name ?? ""));
  return match ? match[1].toLowerCase() : "";
}

export function acceptsWorkbook(name) {
  return ACCEPTED_EXTENSIONS.has(extensionOf(name));
}

export function formatLabel(name) {
  const extension = extensionOf(name);
  return extension ? extension.toUpperCase() : "Spreadsheet";
}

export function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes < 0) {
    return "-";
  }
  if (bytes < 1024) {
    return `${Math.round(bytes)} B`;
  }
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = units[0];
  for (let index = 1; value >= 1024 && index < units.length; index += 1) {
    value /= 1024;
    unit = units[index];
  }
  return `${value >= 10 ? value.toFixed(1) : value.toFixed(2)} ${unit}`;
}

export function safeBaseName(name) {
  const withoutExtension = String(name ?? "workbook").replace(/\.[^.]+$/, "");
  const safe = withoutExtension
    .normalize("NFKD")
    .replace(/[^A-Za-z0-9._-]+/g, "-")
    .replace(/^[._-]+|[._-]+$/g, "")
    .slice(0, 80);
  return safe || "workbook";
}

export function createLatestRequestGate() {
  let current = 0;
  return Object.freeze({
    begin() {
      current += 1;
      return current;
    },
    invalidate() {
      current += 1;
    },
    isCurrent(token) {
      return token === current;
    }
  });
}

export function sameCellTarget(left, right) {
  return Boolean(
    left &&
      right &&
      left.client === right.client &&
      left.documentId === right.documentId &&
      left.sheetIndex === right.sheetIndex &&
      left.row === right.row &&
      left.col === right.col
  );
}

export function parseCellReference(reference) {
  const normalized = String(reference ?? "")
    .trim()
    .replaceAll("$", "")
    .toUpperCase();
  const match = /^([A-Z]{1,3})([1-9][0-9]{0,6})$/.exec(normalized);
  if (!match) {
    throw new RangeError("Enter a cell reference such as A1 or XFD1048576.");
  }

  let columnNumber = 0;
  for (const character of match[1]) {
    columnNumber = columnNumber * 26 + character.charCodeAt(0) - 64;
  }
  const rowNumber = Number(match[2]);
  if (columnNumber > MAX_WORKSHEET_COLUMNS || rowNumber > MAX_WORKSHEET_ROWS) {
    throw new RangeError("The cell reference is outside the XLSX worksheet grid.");
  }
  return {
    row: rowNumber - 1,
    col: columnNumber - 1,
    normalized: `${match[1]}${rowNumber}`
  };
}

export function savedWorkbookName(name) {
  const extension = extensionOf(name) === "xlsm" ? "xlsm" : "xlsx";
  return `${safeBaseName(name)}-edited.${extension}`;
}

export function editReasonLabel(reason) {
  const labels = {
    "legacy-biff": "XLS is read-only in the browser",
    "binary-package": "XLSB is read-only in the browser",
    "open-document": "ODS is read-only in the browser",
    "package-metadata-loss": "This OOXML package cannot be edited without metadata loss"
  };
  return labels[reason] ?? "This workbook is read-only in the browser";
}

export function editableCell(
  kind,
  value,
  { formula = "", cachedKind = "text", cachedValue = "" } = {}
) {
  if (kind === "blank") {
    return { kind: "blank" };
  }
  if (kind === "formula") {
    const expression = String(formula || value).trim().replace(/^=/, "");
    if (!expression) {
      throw new TypeError("Enter a formula.");
    }
    return {
      kind: "formula",
      formula: expression,
      cached: editableScalar(cachedKind, cachedValue)
    };
  }
  return editableScalar(kind, value);
}

export function clampZoom(value) {
  if (!Number.isFinite(value)) {
    return 1;
  }
  return Math.min(3, Math.max(0.25, value));
}

export function fitZoom(viewportWidth, documentWidth, padding = 48) {
  if (
    !Number.isFinite(viewportWidth) ||
    !Number.isFinite(documentWidth) ||
    documentWidth <= 0
  ) {
    return 1;
  }
  return clampZoom((Math.max(1, viewportWidth - padding) / documentWidth) * 0.98);
}

export function svgDimensions(svg) {
  const viewBox = svg?.viewBox?.baseVal;
  if (viewBox && viewBox.width > 0 && viewBox.height > 0) {
    return { width: viewBox.width, height: viewBox.height };
  }
  const width = parseDimension(svg?.getAttribute?.("width"));
  const height = parseDimension(svg?.getAttribute?.("height"));
  if (width > 0 && height > 0) {
    return { width, height };
  }
  return { width: 1024, height: 768 };
}

export function describeError(error) {
  const code = error?.code ?? error?.name ?? "error";
  const messages = {
    parse_failed: "This file could not be read as a supported spreadsheet.",
    limit_exceeded: "This workbook exceeds a browser safety limit.",
    output_limit: "The rendered output exceeds the browser safety limit.",
    worker_crashed: "The isolated renderer stopped unexpectedly.",
    client_closed: "The previous workbook session was closed.",
    AbortError: "The render operation was cancelled."
  };
  return messages[code] ?? error?.message ?? "The workbook could not be rendered.";
}

function parseDimension(value) {
  const parsed = Number.parseFloat(String(value ?? ""));
  return Number.isFinite(parsed) ? parsed : 0;
}

function editableScalar(kind, value) {
  switch (kind) {
    case "text":
    case "error":
      return { kind, value: String(value ?? "") };
    case "number":
    case "date": {
      const number = Number(value);
      if (!Number.isFinite(number)) {
        throw new TypeError(
          kind === "date" ? "Enter a finite Excel date serial." : "Enter a finite number."
        );
      }
      return { kind, value: number };
    }
    case "boolean":
      if (value === true || value === "true") {
        return { kind, value: true };
      }
      if (value === false || value === "false") {
        return { kind, value: false };
      }
      throw new TypeError("Choose true or false.");
    default:
      throw new TypeError(`Unsupported cell type: ${kind}`);
  }
}
