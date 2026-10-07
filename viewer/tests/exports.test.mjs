import test from "node:test";
import assert from "node:assert/strict";
import { createExportController, TILED_EXPORT_ERROR } from "../src/exports.js";
import { downloadBlob } from "../src/browser-files.js";

test("tiled SVG and PNG fail before encoders or browser output and retain no tile bytes", async () => {
  for (const kind of ["svg", "png"]) {
    const { state, elements, calls } = setup();
    state.displayKind = "tiled";
    state.svgText = "private previous full sheet";
    const browser = new Proxy({}, { get() { throw new Error("Browser encoder was used"); } });
    const controller = createExportController({ state, elements, browser,
      setBusy: () => { throw new Error("PNG busy path was entered"); },
      showError: (error) => calls.errors.push(error),
      postHostMessage: (message) => calls.messages.push(message),
      download: () => { throw new Error("Download was used"); } });
    await (kind === "svg" ? controller.exportSvg() : controller.exportPng());
    assert.equal(calls.errors.length, 1);
    assert.equal(calls.errors[0].message, TILED_EXPORT_ERROR);
    assert.equal(calls.errors[0].code, "tiled_export_unavailable");
    assert.equal(elements["status-message"].textContent, TILED_EXPORT_ERROR);
    assert.deepEqual(calls.messages, []);
  }
});

test("host tiled requests send only bounded negative envelopes and preserve synchronous failure status", async () => {
  for (const kind of ["svg", "png"]) {
    for (const requestId of [null, "pending-identity", undefined, "", "A".repeat(65), 1]) {
      const { state, elements, calls } = setup({ host: true });
      state.displayKind = "tiled";
      const controller = createExportController({ state, elements, host: true,
        browser: new Proxy({}, { get() { throw new Error("PNG path reached"); } }),
        showError: (error) => calls.errors.push(error), setBusy() { throw new Error("Busy path reached"); },
        postHostMessage(message) {
          calls.messages.push(message);
          elements["status-message"].textContent = `${kind.toUpperCase()} export failed`;
        } });
      // Explicit undefined invokes the public default null toolbar identity.
      await (kind === "svg" ? controller.exportSvg(requestId) : controller.exportPng(requestId));
      const bounded = requestId == null || requestId === "pending-identity";
      assert.equal(calls.messages.length, bounded ? 1 : 0);
      if (bounded) {
        assert.equal(calls.messages[0].bytes.byteLength, 0);
        assert.equal(calls.messages[0].fileName, `export.${kind}`);
        assert.equal(calls.messages[0].requestId, requestId ?? null);
        assert.equal(elements["status-message"].textContent, `${kind.toUpperCase()} export failed`);
      }
      assert.equal(calls.errors[0].message, TILED_EXPORT_ERROR);
    }
  }
});

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
    toBlob: (callback) => callback(encodingFails ? null : new Blob([Uint8Array.from([137, 80, 78, 71, 13, 10, 26, 10, 0])], { type: "image/png" }))
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
  const { controller, calls, state, elements } = setup();
  controller.exportSvg();
  assert.equal(calls.downloads[0].name, "Quarter-report-Results-page-3.svg");
  assert.equal(await calls.downloads[0].blob.text(), state.svgText);
  assert.deepEqual(calls.messages, []);
  assert.equal(calls.menuClosed, true);
  assert.equal(elements["status-message"].textContent, "SVG exported");
});

test("VS Code SVG export forwards bytes and request identity instead of downloading", () => {
  const { controller, calls, state, elements } = setup({ host: true });
  controller.exportSvg("request-7");
  assert.deepEqual(calls.downloads, []);
  assert.deepEqual(calls.messages, [{
    type: "export", requestId: "request-7", kind: "svg",
    fileName: "Quarter-report-Results-page-3.svg",
    bytes: new TextEncoder().encode(state.svgText)
  }]);
  assert.equal(elements["status-message"].textContent, "SVG export requested");
});

test("PNG export retains its pixel budget, releases the source URL, and restores idle state", async () => {
  const { controller, calls, canvas, elements } = setup();
  await controller.exportPng();
  assert.equal(canvas.width, 4096);
  assert.equal(canvas.height, 4096);
  assert.equal(canvas.width * canvas.height, 16 * 1024 * 1024);
  assert.equal(calls.downloads[0].name, "Quarter-report-Results-page-3.png");
  assert.deepEqual(calls.revoked, ["blob:export-source"]);
  assert.deepEqual(calls.busy, [true, false]);
  assert.deepEqual(calls.errors, []);
  assert.equal(elements["status-message"].textContent, "PNG exported");
});

test("VS Code PNG export forwards binary output to the host", async () => {
  const { controller, calls, elements } = setup({ host: true });
  await controller.exportPng("request-8");
  assert.deepEqual(calls.downloads, []);
  assert.equal(calls.messages[0].requestId, "request-8");
  assert.equal(calls.messages[0].kind, "png");
  assert.deepEqual(calls.messages[0].bytes, Uint8Array.from([137, 80, 78, 71, 13, 10, 26, 10, 0]));
  assert.equal(elements["status-message"].textContent, "PNG export requested");
});

for (const kind of ["svg", "png"]) {
  test(`${kind} host result received during posting is not replaced by a false success`, async () => {
    const { state, elements, calls } = setup({ host: true });
    const browser = {
      URL: { createObjectURL: () => "blob:sync-result", revokeObjectURL() {} },
      XMLSerializer: class { serializeToString() { return state.svgText; } },
      Image: class { addEventListener(type, callback) { if (type === "load") this.loaded = callback; } set src(_value) { queueMicrotask(() => this.loaded()); } },
      document: { createElement: () => ({ getContext: () => ({ fillRect() {}, drawImage() {} }), toBlob(callback) { callback(new Blob([Uint8Array.from([137, 80, 78, 71, 13, 10, 26, 10, 0])], { type: "image/png" })); } }) }
    };
    const controller = createExportController({ state, elements, host: true, browser, setBusy() {}, showError(error) { throw error; }, postHostMessage(message) { calls.messages.push(message); elements["status-message"].textContent = `${kind.toUpperCase()} export failed`; } });
    if (kind === "svg") controller.exportSvg();
    else await controller.exportPng();
    assert.equal(calls.messages.length, 1);
    assert.equal(elements["status-message"].textContent, `${kind.toUpperCase()} export failed`);
    assert.equal(calls.menuClosed, true);
  });
}

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
