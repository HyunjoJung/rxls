import assert from "node:assert/strict";
import test from "node:test";
import * as path from "node:path";
import { pathToFileURL } from "node:url";

import {
  MAX_EXPORT_BYTES,
  parentUriPath,
  parseExportIdentity,
  parseWebviewMessage,
  safeExportFileName
} from "../../protocol";

test("accepts bounded SVG and PNG export messages", () => {
  const svg = new TextEncoder().encode('<svg xmlns="http://www.w3.org/2000/svg"></svg>');
  const svgMessage = parseWebviewMessage({
    type: "export",
    requestId: null,
    kind: "svg",
    fileName: "report-sheet.svg",
    bytes: svg
  });
  assert.equal(svgMessage?.type, "export");
  assert.equal(svgMessage?.fileName, "report-sheet.svg");

  const png = Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0]);
  assert.equal(
    parseWebviewMessage({
      type: "export",
      requestId: "request-1",
      kind: "png",
      fileName: "report.png",
      bytes: png
    })?.type,
    "export"
  );
});

test("long names retain the complete required suffix within the original host limits", () => {
  for (const kind of ["svg", "png"] as const) {
    for (const length of [119, 120, 121, 122, 180]) {
      const name = "A".repeat(length - 4) + `.${kind}`;
      assert.equal(safeExportFileName(name, kind), "A".repeat(Math.min(length, 120) - 4) + `.${kind}`);
    }
    assert.equal(safeExportFileName("A".repeat(177) + `.${kind}`, kind), undefined);
    assert.equal(
      safeExportFileName("A".repeat(176) + `.${kind.toUpperCase()}`, kind),
      "A".repeat(116) + `.${kind.toUpperCase()}`
    );
  }
  assert.equal(safeExportFileName("A".repeat(116) + ".svg.png", "svg"), undefined);
  assert.equal(safeExportFileName("A".repeat(116) + ".png.svg", "png"), undefined);
  assert.equal(safeExportFileName(".svg", "svg"), undefined);
  assert.equal(safeExportFileName("report", "svg"), undefined);
  assert.equal(safeExportFileName("..\\..\\report.SVG", "svg"), "report.SVG");
  assert.equal(safeExportFileName("caf\u00e9.svg", "svg"), "cafe-.svg");
});

test("rejection identity never accepts or inspects a filename or binary payload", () => {
  const invalid = { type: "export", requestId: "request-1", kind: "svg", fileName: "wrong.png", bytes: "not bytes" };
  assert.deepEqual(parseExportIdentity(invalid), { requestId: "request-1", kind: "svg" });
  assert.equal(parseWebviewMessage(invalid), undefined);
  assert.deepEqual(parseExportIdentity({ type: "export", requestId: null, kind: "png" }), { requestId: null, kind: "png" });
  for (const requestId of [undefined, "", "A".repeat(65), 1, false]) {
    assert.equal(parseExportIdentity({ type: "export", requestId, kind: "svg" }), undefined);
  }
  assert.equal(parseExportIdentity({ type: "export", requestId: null, kind: "pdf" }), undefined);
  assert.equal(parseExportIdentity({ type: "loaded", requestId: null, kind: "svg" }), undefined);
  assert.equal(parseExportIdentity([]), undefined);
});

test("long filenames do not weaken byte or signature validation", () => {
  const envelope = { type: "export", requestId: "request-1", kind: "svg", fileName: "A".repeat(176) + ".svg" };
  for (const bytes of [new Uint8Array(), new TextEncoder().encode("not an svg"), new Uint8Array(MAX_EXPORT_BYTES + 1), [60, 115, 118, 103, 62]]) {
    assert.equal(parseWebviewMessage({ ...envelope, bytes }), undefined);
  }
  const good = parseWebviewMessage({ ...envelope, bytes: new TextEncoder().encode("<svg></svg>") });
  assert.ok(good?.type === "export");
  assert.equal(good.fileName.length, 120);
  assert.equal(good.requestId, "request-1");
});

test("real viewer sheet/page exports cross the host parser with their bytes and identity intact", async () => {
  // A computed URL avoids importing a second TypeScript root or copying the producer.
  const viewerUrl = pathToFileURL(path.resolve(__dirname, "../../../../../viewer/src/exports.js")).href;
  const { createExportController } = await import(viewerUrl);
  const png = Uint8Array.from([137, 80, 78, 71, 13, 10, 26, 10, 0]);
  for (const mode of ["sheet", "page"]) {
    const state = { file: { name: "W".repeat(80) + ".xlsx" }, workbook: { sheets: [{ name: "S".repeat(31) }] }, sheetIndex: 0, mode, pageIndex: 0, svgText: "<svg></svg>", svgElement: {}, documentWidth: 10, documentHeight: 10 };
    const messages: Record<string, unknown>[] = [];
    const elements = { "status-message": { textContent: "" }, "export-menu": { removeAttribute() {} } };
    const browser = {
      URL: { createObjectURL: () => "blob:producer", revokeObjectURL() {} },
      XMLSerializer: class { serializeToString() { return state.svgText; } },
      Image: class {
        private loaded: (() => void) | undefined;
        addEventListener(type: string, callback: () => void): void { if (type === "load") this.loaded = callback; }
        set src(_value: string) { queueMicrotask(() => this.loaded?.()); }
      },
      document: { createElement: () => ({ getContext: () => ({ fillRect() {}, drawImage() {} }), toBlob(callback: (blob: Blob) => void) { callback(new Blob([png], { type: "image/png" })); } }) }
    };
    const controller = createExportController({ state, elements, host: true, browser, setBusy() {}, showError(error: Error) { throw error; }, postHostMessage(message: Record<string, unknown>) { messages.push(message); } });
    controller.exportSvg("svg-producer");
    await controller.exportPng("png-producer");
    for (const raw of messages) {
      assert.ok(typeof raw.fileName === "string" && raw.fileName.length > 120 && raw.fileName.length <= 180);
      const message = parseWebviewMessage(raw);
      assert.ok(message?.type === "export");
      assert.equal(message.requestId, `${message.kind}-producer`);
      assert.equal(message.fileName.length, 120);
      assert.ok(message.fileName.endsWith(`.${message.kind}`));
      assert.deepEqual(message.bytes, message.kind === "svg" ? new TextEncoder().encode(state.svgText) : png);
    }
    assert.equal(messages.length, 2);
  }
});

test("rejects oversized, mismatched, and path-like exports", () => {
  assert.equal(
    parseWebviewMessage({
      type: "export",
      requestId: null,
      kind: "png",
      fileName: "wrong.svg",
      bytes: new Uint8Array(MAX_EXPORT_BYTES + 1)
    }),
    undefined
  );
  assert.equal(safeExportFileName("../../report.svg", "svg"), "report.svg");
  assert.equal(safeExportFileName("report.png", "svg"), undefined);
  assert.equal(
    parseWebviewMessage({
      type: "export",
      requestId: "",
      kind: "svg",
      fileName: "report.svg",
      bytes: new TextEncoder().encode("<svg></svg>")
    }),
    undefined
  );
});

test("normalizes loaded state without accepting malformed fields", () => {
  const loaded = parseWebviewMessage({
    type: "loaded",
    generation: 2,
    preview: {
      fileName: "book.xlsx",
      format: "xlsx",
      sheetCount: 2,
      sheetIndex: 0,
      mode: "sheet",
      pageIndex: 0,
      rendered: true,
      host: "vscode",
      ignored: "value"
    }
  });
  assert.equal(loaded?.type, "loaded");
  assert.equal(loaded?.preview.sheetCount, 2);
  assert.equal(
    parseWebviewMessage({ type: "loaded", generation: -1, preview: {} }),
    undefined
  );
});

test("builds provider-neutral parent URI paths", () => {
  assert.equal(parentUriPath("/workspace/report.xlsx"), "/workspace");
  assert.equal(parentUriPath("/report.xlsx"), "/");
  const windowsPath = ["C:", "work", "report.xlsx"].join("\\");
  const windowsParent = ["C:", "work"].join("/");
  assert.equal(parentUriPath(windowsPath), windowsParent);
});
