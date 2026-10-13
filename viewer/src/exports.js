import { safeBaseName } from "./core.js";
import { downloadBlob, loadImage } from "./browser-files.js";

export const TILED_EXPORT_ERROR = "Full-sheet SVG/PNG export is unavailable in tiled view. Use a bounded page view or save the workbook.";

const MAX_PNG_PIXELS = 16 * 1024 * 1024;

/** Export the displayed sheet/page through browser downloads or the host bridge. */
export function createExportController({
  state,
  elements,
  host = false,
  setBusy,
  showError,
  postHostMessage,
  download = downloadBlob,
  browser = globalThis
}) {
  return { exportSvg, exportPng };

  function exportSvg(requestId = null) {
    if (rejectTiledExport("svg", requestId)) return;
    if (!state.svgText) {
      return;
    }
    const fileName = `${exportBaseName()}.svg`;
    if (host) {
      elements["status-message"].textContent = "SVG export requested";
      postHostMessage({
        type: "export",
        requestId,
        kind: "svg",
        fileName,
        bytes: new TextEncoder().encode(state.svgText)
      });
    } else {
      download(new Blob([state.svgText], { type: "image/svg+xml;charset=utf-8" }), fileName);
      elements["status-message"].textContent = "SVG exported";
    }
    elements["export-menu"].removeAttribute("open");
  }

  async function exportPng(requestId = null) {
    if (rejectTiledExport("png", requestId)) return;
    if (!state.svgElement) {
      return;
    }
    try {
      setBusy(true, "Creating PNG");
      const serialized = new browser.XMLSerializer().serializeToString(state.svgElement);
      const source = new Blob([serialized], { type: "image/svg+xml;charset=utf-8" });
      const sourceUrl = browser.URL.createObjectURL(source);
      try {
        const image = await loadImage(sourceUrl, browser);
        const pixelScale = Math.min(
          2,
          Math.sqrt(MAX_PNG_PIXELS / (state.documentWidth * state.documentHeight))
        );
        const width = Math.max(1, Math.round(state.documentWidth * pixelScale));
        const height = Math.max(1, Math.round(state.documentHeight * pixelScale));
        const canvas = browser.document.createElement("canvas");
        canvas.width = width;
        canvas.height = height;
        const context = canvas.getContext("2d", { alpha: false });
        context.fillStyle = "#ffffff";
        context.fillRect(0, 0, width, height);
        context.drawImage(image, 0, 0, width, height);
        const png = await new Promise((resolve, reject) => {
          canvas.toBlob(
            (blob) => (blob ? resolve(blob) : reject(new Error("PNG encoding failed."))),
            "image/png"
          );
        });
        const fileName = `${exportBaseName()}.png`;
        if (host) {
          const bytes = new Uint8Array(await png.arrayBuffer());
          elements["status-message"].textContent = "PNG export requested";
          postHostMessage({
            type: "export",
            requestId,
            kind: "png",
            fileName,
            bytes
          });
        } else {
          download(png, fileName);
          elements["status-message"].textContent = "PNG exported";
        }
      } finally {
        browser.URL.revokeObjectURL(sourceUrl);
      }
    } catch (error) {
      showError(error);
    } finally {
      setBusy(false);
      elements["export-menu"].removeAttribute("open");
    }
  }

  function rejectTiledExport(kind, requestId) {
    if (state.displayKind !== "tiled") return false;
    const error = new Error(TILED_EXPORT_ERROR);
    error.code = "tiled_export_unavailable";
    showError(error);
    elements["status-message"].textContent = TILED_EXPORT_ERROR;
    // Existing bounded known-kind identity routing rejects this negative
    // envelope before any encoder, Blob, image, canvas or save dialog runs.
    if (host && (requestId === null ||
        (typeof requestId === "string" && requestId.length > 0 && requestId.length <= 64))) {
      postHostMessage({ type: "export", requestId, kind,
        fileName: `export.${kind}`, bytes: new Uint8Array(0) });
    }
    elements["export-menu"].removeAttribute("open");
    return true;
  }

  function exportBaseName() {
    const sheetName = state.workbook?.sheets?.[state.sheetIndex]?.name ?? "sheet";
    const suffix = state.mode === "page" ? `page-${state.pageIndex + 1}` : "sheet";
    return `${safeBaseName(state.file?.name)}-${safeBaseName(sheetName)}-${suffix}`;
  }
}
