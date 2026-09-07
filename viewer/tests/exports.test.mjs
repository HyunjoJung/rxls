import test from "node:test";
import assert from "node:assert/strict";
import { createExportController } from "../src/exports.js";
import { downloadBlob } from "../src/browser-files.js";

function setup({ host = false, imageFails = false, encodingFails = false } = {}) {
  const calls = { downloads: [], messages: [], busy: [], errors: [], revoked: [], draws: [] };
  const state = {
    file: { name: "Quarter report.xlsx" },
    workbook: { sheets: [{ name: "Results" }] },
    sheetIndex: 0,
    mode: "page",
    pageIndex: 2,
    svgText: '<svg xmlns="http://www.w3.org/2000/svg"/>',
    svgElement: {},
    documentWidth: 8192,
    documentHeight: 8192
  };
  const elements = {
    "status-message": { textContent: "" },
    "export-menu": { removeAttribute: () => { calls.menuClosed = true; } }
  };
  const canvas = {
    getContext: () => ({
      fillRect() {},
      drawImage: (...args) => calls.draws.push(args)
    }),
    toBlob: (callback) => callback(encodingFails ? null : new Blob(["png"], { type: "image/png" }))
  };
  const browser = {
    URL: {
      createObjectURL: () => "blob:export-source",
      revokeObjectURL: (url) => calls.revoked.push(url)
    },
    XMLSerializer: class { serializeToString() { return state.svgText; } },
    Image: class {
      listeners = new Map();
      addEventListener(type, callback) { this.listeners.set(type, callback); }
      set src(_value) { queueMicrotask(() => this.listeners.get(imageFails ? "error" : "load")()); }
    },
    document: { createElement: () => canvas }
  };
  const controller = createExportController({
    state, elements, host, browser,
    setBusy: (busy) => calls.busy.push(busy),
    showError: (error) => calls.errors.push(error),
    postHostMessage: (message) => calls.messages.push(message),
    download: (blob, name) => calls.downloads.push({ blob, name })
  });
  return { controller, calls, canvas, state, elements };
}

test("browser SVG export keeps the current sheet/page name and uses a local download", async () => {
  const { controller, calls, state } = setup();
  controller.exportSvg();
  assert.equal(calls.downloads[0].name, "Quarter-report-Results-page-3.svg");
  assert.equal(await calls.downloads[0].blob.text(), state.svgText);
  assert.deepEqual(calls.messages, []);
  assert.equal(calls.menuClosed, true);
});

test("VS Code SVG export forwards bytes and request identity instead of downloading", () => {
  const { controller, calls, state } = setup({ host: true });
  controller.exportSvg("request-7");
  assert.deepEqual(calls.downloads, []);
  assert.deepEqual(calls.messages, [{
    type: "export", requestId: "request-7", kind: "svg",
    fileName: "Quarter-report-Results-page-3.svg",
    bytes: new TextEncoder().encode(state.svgText)
  }]);
});

test("PNG export retains its pixel budget, releases the source URL, and restores idle state", async () => {
  const { controller, calls, canvas } = setup();
  await controller.exportPng();
  assert.equal(canvas.width, 4096);
  assert.equal(canvas.height, 4096);
  assert.equal(canvas.width * canvas.height, 16 * 1024 * 1024);
  assert.equal(calls.downloads[0].name, "Quarter-report-Results-page-3.png");
  assert.deepEqual(calls.revoked, ["blob:export-source"]);
  assert.deepEqual(calls.busy, [true, false]);
  assert.deepEqual(calls.errors, []);
});

test("VS Code PNG export forwards binary output to the host", async () => {
  const { controller, calls } = setup({ host: true });
  await controller.exportPng("request-8");
  assert.deepEqual(calls.downloads, []);
  assert.equal(calls.messages[0].requestId, "request-8");
  assert.equal(calls.messages[0].kind, "png");
  assert.deepEqual(calls.messages[0].bytes, new TextEncoder().encode("png"));
});

for (const mode of ["imageFails", "encodingFails"]) {
  test(`PNG ${mode} releases resources and reports failure without downloading`, async () => {
    const { controller, calls } = setup({ [mode]: true });
    await controller.exportPng();
    assert.deepEqual(calls.downloads, []);
    assert.equal(calls.errors.length, 1);
    assert.deepEqual(calls.revoked, ["blob:export-source"]);
    assert.deepEqual(calls.busy, [true, false]);
    assert.equal(calls.menuClosed, true);
  });
}

test("export commands do nothing before a sheet has rendered", async () => {
  const { controller, state, calls } = setup();
  state.svgText = "";
  state.svgElement = null;
  controller.exportSvg();
  await controller.exportPng();
  assert.deepEqual(calls.downloads, []);
  assert.deepEqual(calls.busy, []);
});

test("browser downloads defer object URL cleanup until after the anchor click", () => {
  const calls = [];
  const anchor = { click: () => calls.push("click") };
  let cleanup;
  downloadBlob(new Blob(["data"]), "copy.xlsx", {
    URL: {
      createObjectURL: () => "blob:download",
      revokeObjectURL: (url) => calls.push(url)
    },
    document: { createElement: () => anchor },
    setTimeout(callback, delay) { cleanup = callback; assert.equal(delay, 1000); }
  });
  assert.equal(anchor.download, "copy.xlsx");
  assert.equal(anchor.href, "blob:download");
  assert.deepEqual(calls, ["click"]);
  cleanup();
  assert.deepEqual(calls, ["click", "blob:download"]);
});
