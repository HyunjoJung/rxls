export const MAX_WORKBOOK_BYTES = 32 * 1024 * 1024;
export const MAX_SESSION_BYTES = 128 * 1024 * 1024;
export const MAX_OPEN_PREVIEWS = 4;
export const MAX_EXPORT_BYTES = 16 * 1024 * 1024;

export type ExportKind = "svg" | "png";

export interface LoadedMessage {
  type: "loaded";
  generation: number;
  preview: {
    fileName: string | null;
    format: string | null;
    sheetCount: number;
    sheetIndex: number;
    mode: string;
    pageIndex: number;
    rendered: boolean;
    host: string;
  };
}

export interface PreviewErrorMessage {
  type: "preview-error";
  generation: number;
  code: string;
  message: string;
}

export interface ExportMessage {
  type: "export";
  requestId: string | null;
  kind: ExportKind;
  fileName: string;
  bytes: Uint8Array;
}

export type WebviewMessage =
  | { type: "ready" }
  | { type: "reload" }
  | LoadedMessage
  | PreviewErrorMessage
  | ExportMessage;

export function parseWebviewMessage(value: unknown): WebviewMessage | undefined {
  if (!isRecord(value) || typeof value.type !== "string") {
    return undefined;
  }
  if (value.type === "ready" || value.type === "reload") {
    return { type: value.type };
  }
  if (value.type === "loaded") {
    if (!positiveSafeInteger(value.generation) || !isRecord(value.preview)) {
      return undefined;
    }
    const preview = value.preview;
    if (
      !nullableString(preview.fileName, 255) ||
      !nullableString(preview.format, 16) ||
      !nonNegativeSafeInteger(preview.sheetCount) ||
      !nonNegativeSafeInteger(preview.sheetIndex) ||
      !nonNegativeSafeInteger(preview.pageIndex) ||
      typeof preview.mode !== "string" ||
      preview.mode.length > 16 ||
      typeof preview.rendered !== "boolean" ||
      typeof preview.host !== "string" ||
      preview.host.length > 16
    ) {
      return undefined;
    }
    return {
      type: "loaded",
      generation: value.generation,
      preview: {
        fileName: preview.fileName,
        format: preview.format,
        sheetCount: preview.sheetCount,
        sheetIndex: preview.sheetIndex,
        mode: preview.mode,
        pageIndex: preview.pageIndex,
        rendered: preview.rendered,
        host: preview.host
      }
    };
  }
  if (value.type === "preview-error") {
    if (
      !nonNegativeSafeInteger(value.generation) ||
      typeof value.code !== "string" ||
      value.code.length === 0 ||
      value.code.length > 64 ||
      typeof value.message !== "string" ||
      value.message.length === 0 ||
      value.message.length > 512
    ) {
      return undefined;
    }
    return {
      type: "preview-error",
      generation: value.generation,
      code: value.code,
      message: value.message
    };
  }
  if (value.type === "export") {
    if (
      (value.kind !== "svg" && value.kind !== "png") ||
      (value.requestId !== null &&
        (typeof value.requestId !== "string" ||
          value.requestId.length === 0 ||
          value.requestId.length > 64))
    ) {
      return undefined;
    }
    const bytes = binaryBytes(value.bytes);
    if (!bytes || bytes.byteLength === 0 || bytes.byteLength > MAX_EXPORT_BYTES) {
      return undefined;
    }
    const fileName = safeExportFileName(value.fileName, value.kind);
    if (!fileName || !validExportSignature(bytes, value.kind)) {
      return undefined;
    }
    return {
      type: "export",
      requestId: value.requestId,
      kind: value.kind,
      fileName,
      bytes
    };
  }
  return undefined;
}

export function safeExportFileName(value: unknown, kind: ExportKind): string | undefined {
  if (typeof value !== "string" || value.length === 0 || value.length > 180) {
    return undefined;
  }
  const leaf = value.replaceAll("\\", "/").split("/").at(-1) ?? "";
  const safe = leaf
    .normalize("NFKD")
    .replace(/[^A-Za-z0-9._-]+/g, "-")
    .replace(/^[._-]+|[._-]+$/g, "")
    .slice(0, 120);
  const extension = `.${kind}`;
  if (!safe.toLowerCase().endsWith(extension)) {
    return undefined;
  }
  return safe;
}

export function parentUriPath(path: string): string {
  const normalized = path.replaceAll("\\", "/");
  const index = normalized.lastIndexOf("/");
  return index <= 0 ? "/" : normalized.slice(0, index);
}

function validExportSignature(bytes: Uint8Array, kind: ExportKind): boolean {
  if (kind === "png") {
    return (
      bytes.byteLength >= 8 &&
      bytes[0] === 0x89 &&
      bytes[1] === 0x50 &&
      bytes[2] === 0x4e &&
      bytes[3] === 0x47 &&
      bytes[4] === 0x0d &&
      bytes[5] === 0x0a &&
      bytes[6] === 0x1a &&
      bytes[7] === 0x0a
    );
  }
  const prefix = new TextDecoder().decode(bytes.subarray(0, Math.min(bytes.byteLength, 512)));
  return /^\s*(?:<\?xml[^>]*>\s*)?<svg(?:\s|>)/i.test(prefix);
}

function binaryBytes(value: unknown): Uint8Array | undefined {
  if (value instanceof Uint8Array) {
    return value;
  }
  if (value instanceof ArrayBuffer) {
    return new Uint8Array(value);
  }
  if (ArrayBuffer.isView(value)) {
    return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  }
  return undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nullableString(value: unknown, maximum: number): value is string | null {
  return value === null || (typeof value === "string" && value.length <= maximum);
}

function positiveSafeInteger(value: unknown): value is number {
  return Number.isSafeInteger(value) && Number(value) > 0;
}

function nonNegativeSafeInteger(value: unknown): value is number {
  return Number.isSafeInteger(value) && Number(value) >= 0;
}
