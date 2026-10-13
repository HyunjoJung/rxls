import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import * as viewport from "../src/viewport.js";
import { clampZoom, fitZoom, createLatestRequestGate, extensionOf } from "../src/core.js";
import { EditorEmbedError } from "../src/embed/protocol.mjs";

// Exercise the actual main integration helpers with the actual scheduler.
// The DOM/worker below are explicit model doubles, not browser/WASM proof.
const source = await readFile(new URL("../src/main.js", import.meta.url), "utf8");
function section(start, end) {
  const first = source.indexOf(start);
  const last = source.indexOf(end, first);
  assert.ok(first >= 0 && last > first, `${start} boundary`);
  return source.slice(first, last).replaceAll("export function", "function");
}
const helpers = section("function readyForViewChange()", "function parsedSvg(svgText)") +
  section("function setZoom(value)", "async function exportWithDraft(kind)") +
  section("async function selectSheet(index)", "function readyForViewChange()") +
  section("export function viewerStateForTest()", "globalThis.__rxlsViewerState =");
const scheduler = section("const viewportScheduler =", "const editing =");
const opening = section("function isCurrentOpenRequest(request)", "function failOpenRequest(request, error)") +
  section("async function openWorkbook(bytes, file, request)", "async function createWorkerTarget(workerUrl)");
const flush = () => new Promise((resolve) => setImmediate(resolve));
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const FULL_LIMIT = Object.assign(new Error("bounded full sheet"), {
  code: "limit_exceeded", resource: "cells", limit: 250000, actual: 250001,
});
function descriptor(overrides = {}) {
  return { schemaVersion: 1, documentId: "one", sheetIndex: 0,
    geometryId: "vp-00000000-0000-4000-8000-000000000001-1", revision: "9007199254740993",
    widthRaw: 3000 * 1024, heightRaw: 5000 * 1024, sheetVisibility: "visible",
    sourceRange: { startRow: 0, startCol: 0, endRow: 100, endCol: 30 },
    preparationReport: { coordinateVisits: 101 }, ...overrides };
}
function element() {
  return { style: {}, hidden: false, textContent: "", disabled: false, children: [],
    classList: { add() {}, remove() {} }, setAttribute() {}, removeEventListener() {},
    append(child) { this.children.push(child); child.parentElement = this; },
    remove() { if (this.parentElement) this.parentElement.children = this.parentElement.children.filter((child) => child !== this); this.parentElement = null; },
    replaceChildren(...children) {
      for (const child of this.children) child.parentElement = null;
      this.children = children;
      for (const child of children) child.parentElement = this;
    } };
}
function fixture({ prepare, render, fullError = FULL_LIMIT, width = 400, height = 200, descriptor: data = descriptor() } = {}) {
  const calls = { full: 0, capabilities: 0, prepare: 0, tiles: [], releases: [], errors: [], busy: [], refresh: 0 };
  const client = {
    async renderSheetInteractive() { calls.full += 1; if (fullError) throw fullError; return { svg: "full" }; },
    async viewportCapabilities() { calls.capabilities += 1; return { schemaVersion: 1 }; },
    async prepareViewport(...args) { calls.prepare += 1; return prepare ? prepare(...args) : data; },
    renderViewportTile(...args) {
      calls.tiles.push(args);
      return render ? render(...args) : Promise.resolve(tileFor(args));
    },
    async releaseViewport(...args) { calls.releases.push(args); return { released: true }; },
  };
  const state = { client, documentId: "one", workbook: { sheets: [{ name: "Small" }, { name: "Large" }] },
    editState: { capability: "read-write", dirty: true, canUndo: true }, sheetIndex: 0, mode: "sheet",
    manifests: new Map(), pageIndex: 0, zoom: 1, svgText: "old full", svgElement: element(),
    openGeneration: 1, renderEpoch: 0, busy: false, displayKind: "full", renderOutcome: "ready",
    viewportContext: null, viewportSvg: null, viewportReady: false, viewportBarrier: null,
    viewportFrame: null, viewportInitialFit: false, viewportDisposed: false };
  const controls = new Map();
  const elements = new Proxy({}, { get(_target, name) {
    if (!controls.has(name)) controls.set(name, element());
    return controls.get(name);
  } });
  const port = elements["viewer-viewport"];
  Object.assign(port, { clientWidth: width, clientHeight: height, clientLeft: 1, clientTop: 2,
    scrollLeft: 0, scrollTop: 0, getBoundingClientRect: () => ({ left: 0, top: 0, right: width + 2, bottom: height + 4 }) });
  const surface = elements["document-surface"];
  surface.getBoundingClientRect = () => {
    const width = port.clientWidth === 0 ? 0 : Number.parseFloat(surface.style.width || "0");
    const height = port.clientHeight === 0 ? 0 : Number.parseFloat(surface.style.height || "0");
    const left = 22.25 - port.scrollLeft;
    const top = 22.5 - port.scrollTop;
    return { left, top, right: left + width, bottom: top + height, width, height };
  };
  Object.defineProperties(port, {
    scrollWidth: { get: () => Math.ceil(Number.parseFloat(surface.style.width || "0")) + 45 },
    scrollHeight: { get: () => Math.ceil(Number.parseFloat(surface.style.height || "0")) + 45 },
  });
  const frames = new Map(), timers = new Map();
  let nextId = 0;
  const grid = { hasDraft: () => false, invalidate() {}, mount() {}, reposition() {} };
  const editing = { readyForViewChange: () => true };
  const sandbox = { state, elements, grid, editing,
    rangePaste: { hasPending: () => false }, commitGridDraft: async () => true,
    updateSheetSelection() {}, updateModeUi() {}, closeSidebar() {}, updatePageUi() {},
    viewportObserver: { disconnect() {} }, vscodeHost: null,
    performance, hostKind: "browser",
    workbench: { refresh: async () => { calls.refresh += 1; } },
    ...viewport, clampZoom, fitZoom,
    createViewportScheduler: (options) => viewport.createViewportScheduler({ ...options,
      setTimer: (fn) => { timers.set(++nextId, fn); return nextId; }, clearTimer: (id) => timers.delete(id) }),
    requestAnimationFrame: (fn) => { frames.set(++nextId, fn); return nextId; },
    cancelAnimationFrame: (id) => frames.delete(id),
    setTimeout: (fn) => { timers.set(++nextId, fn); return nextId; }, clearTimeout: (id) => timers.delete(id),
    setBusy(value) { state.busy = value; calls.busy.push(value); },
    showError: (error) => calls.errors.push(error),
    parsedSvg: () => element(), document: { createElement: () => element() },
    showSvg(value) { state.svgText = value; state.svgElement = element(); },
  };
  vm.createContext(sandbox);
  vm.runInContext(helpers + scheduler + "globalThis.schedulerForTest = viewportScheduler;", sandbox);
  async function runFrames() {
    for (let round = 0; round < 6; round += 1) {
      await flush();
      const pending = [...frames.values()]; frames.clear();
      for (const fn of pending) fn();
    }
  }
  return { sandbox, state, calls, frames, timers, port, surface, grid, editing, runFrames,
    dispose: () => sandbox.disposeViewport() };
}
function tileFor(args) {
  const [documentId, sheetIndex, geometryId, revision, requestedRect, namespace] = args;
  return { documentId, sheetIndex, geometryId, revision, namespace,
    logicalRect: { ...requestedRect }, svg: "<svg/>", report: { svg_bytes: 6, scene_nodes: 1 },
    metrics: { coordinateVisits: 1, geometryBytes: 16, haloRows: 0, haloColumns: 0, haloCells: 0 } };
}

function embeddedFixture(options) {
  const env = fixture(options);
  const client = env.state.client;
  const workbook = { sheets: [{ name: "Large", index: 0 }], sheetCount: 1 };
  env.state.client = null;
  env.state.documentId = null;
  env.state.workbook = null;
  env.state.editState = null;
  env.state.file = null;
  client.open = async () => ({ workbook, editState: { capability: "read-write", dirty: false } });
  client.terminate = () => { env.calls.terminated = (env.calls.terminated ?? 0) + 1; };
  Object.assign(env.sandbox, {
    Worker: class {}, URL, AbortController, Uint8Array, EditorEmbedError, extensionOf,
    baseUrl: new URL("https://consumer.example/nonroot/editor-v1/"),
    openRequests: createLatestRequestGate(),
    createWorkerTarget: () => new URL("https://consumer.example/worker.mjs"),
    closeCellEditor() {}, closePropertiesEditor() {}, dismissError() {}, updateWorkbookUi() {}, updateEditUi() {},
    publish() {},
  });
  env.state.runtime = { RenderWorkerClient: class { constructor() { return client; } } };
  env.grid.cancel = () => true;
  env.editing.hasPendingMutation = () => false;
  env.editing.hasDraftChanges = () => false;
  env.sandbox.rangePaste.cancel = () => true;
  vm.runInContext('let disposed = false; const embedMode = true; const embedHost = { publishState() { publish(); } };' +
    section("function beginOpenRequest(label)", "async function createWorkerTarget(workerUrl)") +
    section("function showEmpty()", "function showError(error,") +
    source.slice(source.indexOf("function embedState() {")), env.sandbox);
  env.load = () => env.sandbox.loadEmbeddedWorkbook(Uint8Array.of(80, 75), { fileName: "large.xlsx", replace: "reject" });
  return env;
}

test("embed load awaits actual renderCurrent and the real scheduler's current DOM-covering tile barrier", async () => {
  const gate = deferred();
  const env = embeddedFixture({ render: () => gate.promise });
  let done = false;
  const load = env.load().then(() => { done = true; });
  await env.runFrames();
  assert.equal(env.calls.prepare, 1);
  assert.equal(env.calls.tiles.length, 1);
  assert.equal(done, false);
  assert.equal(env.state.busy, true);
  gate.resolve(tileFor(env.calls.tiles[0]));
  await env.runFrames();
  await load;
  assert.equal(env.state.renderOutcome, "ready");
  assert.equal(env.state.viewportReady, true);
  assert.equal(env.surface.children.length, 1);
  env.dispose();
});

test("embed positive geometry stays pending while collapsed and requires current expanded DOM coverage", async () => {
  const gate = deferred();
  const env = embeddedFixture({ width: 0, render: () => gate.promise });
  let done = false;
  const load = env.load().then(() => { done = true; });
  await env.runFrames();
  assert.equal(env.calls.tiles.length, 0);
  assert.equal(done, false);
  env.port.clientWidth = 700;
  env.sandbox.scheduleViewport();
  await env.runFrames();
  assert.equal(env.calls.tiles.length, 1);
  gate.resolve(tileFor(env.calls.tiles[0]));
  await env.runFrames();
  await load;
  assert.equal(env.state.viewportReady, true);
  assert.equal(env.state.renderOutcome, "ready");
  env.dispose();
});

test("embed first-tile failure uses actual failed outcome and releases the exact handle before clearing source", async () => {
  const gate = deferred();
  const error = new Error("owned first tile failed");
  const env = embeddedFixture({ render: () => gate.promise });
  const load = env.load();
  const rejected = assert.rejects(load, (actual) => actual === error);
  void rejected.catch(() => {});
  await env.runFrames();
  gate.reject(error);
  await env.runFrames();
  await rejected;
  assert.equal(env.state.workbook, null);
  assert.equal(env.state.viewportContext, null);
  assert.equal(env.state.viewportDisposed, false);
  assert.equal(env.state.viewportReady, false);
  assert.equal(env.calls.terminated, 1);
  assert.equal(env.calls.releases.length, 1);
  env.dispose();
});

test("embed actual zero-geometry preparation resolves empty without scheduling a synthetic visible tile", async () => {
  const env = embeddedFixture({ descriptor: descriptor({ widthRaw: 0, heightRaw: 0 }) });
  const load = env.load();
  await env.runFrames();
  await load;
  assert.equal(env.state.renderOutcome, "empty");
  assert.equal(env.calls.prepare, 1);
  assert.equal(env.calls.tiles.length, 0);
  assert.equal(env.state.workbook.sheetCount, 1);
  env.dispose();
});
