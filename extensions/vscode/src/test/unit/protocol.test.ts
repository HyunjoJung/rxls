import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_EXPORT_BYTES,
  parentUriPath,
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
