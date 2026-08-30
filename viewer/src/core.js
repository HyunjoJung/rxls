export const ACCEPTED_EXTENSIONS = new Set(["xls", "xlsx", "xlsm", "xlsb", "ods"]);

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
