import { safeBaseName } from "./core.js";
import { downloadBlob, loadImage } from "./browser-files.js";

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
    if (!state.svgText) {
      return;
    }
    const fileName = `${exportBaseName()}.svg`;
    if (host) {
      postHostMessage({
        type: "export",
        requestId,
        kind: "svg",
        fileName,
        bytes: new TextEncoder().encode(state.svgText)
      });
    } else {
      download(new Blob([state.svgText], { type: "image/svg+xml;charset=utf-8" }), fileName);
    }
    elements["status-message"].textContent = "SVG exported";
    elements["export-menu"].removeAttribute("open");
  }

  async function exportPng(requestId = null) {
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
          postHostMessage({
            type: "export",
            requestId,
            kind: "png",
            fileName,
            bytes: new Uint8Array(await png.arrayBuffer())
          });
        } else {
          download(png, fileName);
        }
        elements["status-message"].textContent = "PNG exported";
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

  function exportBaseName() {
    const sheetName = state.workbook?.sheets?.[state.sheetIndex]?.name ?? "sheet";
    const suffix = state.mode === "page" ? `page-${state.pageIndex + 1}` : "sheet";
    return `${safeBaseName(state.file?.name)}-${safeBaseName(sheetName)}-${suffix}`;
  }
}
