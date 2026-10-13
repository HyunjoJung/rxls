import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import * as viewport from "../src/viewport.js";
import { clampZoom, fitZoom } from "../src/core.js";

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

test("a stale async worker factory terminates only its returned owned Worker before any client or workbook open", async () => {
  for (const ownedWorker of [true, false]) {
    const gate = deferred();
    let constructed = 0, opened = 0, terminated = 0;
    class Worker { terminate() { terminated += 1; } }
    const workerTarget = ownedWorker ? new Worker() : new URL("https://example.test/worker.mjs");
    const request = { token: 1, client: null };
    const state = { openRequest: request, runtime: {
      RenderWorkerClient: class { constructor() { constructed += 1; } open() { opened += 1; } },
    } };
    let currentToken = 1;
    const sandbox = { state, Worker, URL,
      baseUrl: new URL("https://example.test/nested/"),
      createWorkerTarget: () => gate.promise,
      openRequests: { isCurrent: (token) => token === currentToken },
      failOpenRequest() { throw new Error("stale factory invoked the error UI"); },
    };
    vm.createContext(sandbox);
    vm.runInContext(opening, sandbox);
    const pending = sandbox.openWorkbook(Uint8Array.of(1), { name: "book.xlsx" }, request);
    currentToken = 2;
    state.openRequest = { token: 2, client: null };
    gate.resolve(workerTarget);
    assert.equal(await pending, false);
    assert.equal(constructed, 0);
    assert.equal(opened, 0);
    assert.equal(terminated, ownedWorker ? 1 : 0);
    assert.equal(request.client, null);
    assert.equal(state.openRequest.token, 2);
  }
});

test("a current async worker factory retains the original one-client one-open path for Worker and URL targets", async () => {
  for (const ownedWorker of [true, false]) {
    const gate = deferred();
    const calls = { targets: [], opens: [], terminated: 0, rendered: 0, updates: 0 };
    class Worker { terminate() { calls.terminated += 1; } }
    const workerTarget = ownedWorker ? new Worker() : new URL("https://example.test/worker.mjs");
    const request = { token: 1, client: null };
    const workbook = { sheets: [{ name: "Small" }], sheetCount: 1 };
    const state = { openRequest: request, client: null, manifests: new Map(), runtime: {
      RenderWorkerClient: class {
        constructor(target) { calls.targets.push(target); }
        async open(bytes, options) { calls.opens.push({ bytes, options }); return { workbook, editState: { capability: "read-write" } }; }
        terminate() { calls.terminated += 1; }
      },
    } };
    const sandbox = { state, Worker, URL,
      baseUrl: new URL("https://example.test/nested/"),
      createWorkerTarget: () => gate.promise,
      openRequests: { isCurrent: (token) => token === 1 },
      grid: { invalidate() {} },
      updateWorkbookUi() { calls.updates += 1; },
      async renderCurrent() { calls.rendered += 1; return { status: "ready" }; },
      closeSidebar() {}, failOpenRequest() { throw new Error("current open failed"); },
    };
    vm.createContext(sandbox);
    vm.runInContext(opening, sandbox);
    const pending = sandbox.openWorkbook(Uint8Array.of(1, 2), { name: "book.xlsx" }, request);
    assert.equal(calls.targets.length, 0);
    gate.resolve(workerTarget);
    assert.equal(await pending, true);
    assert.deepEqual(calls.targets, [workerTarget]);
    assert.equal(calls.opens.length, 1);
    assert.equal(calls.rendered, 1);
    assert.equal(calls.updates, 1);
    assert.equal(calls.terminated, 0);
    assert.equal(state.hostWorker, ownedWorker ? workerTarget : null);
    assert.equal(state.workbook, workbook);
    assert.equal(state.openRequest, null);
  }
});

test("first tiled readiness awaits a real covering tile, clears full export state, and sends no AbortSignal", async () => {
  const gate = deferred();
  const env = fixture({ render: () => gate.promise });
  let settled = false;
  const opening = env.sandbox.renderCurrent({ fit: true }).then((result) => { settled = true; return result; });
  await env.runFrames();
  assert.equal(settled, false);
  assert.equal(env.state.busy, true);
  assert.equal(env.state.svgText, "");
  assert.equal(env.state.svgElement, null);
  assert.equal(env.calls.tiles.length, 1);
  assert.equal(env.calls.tiles[0].length, 6);
  assert.equal(env.calls.tiles[0][3], "9007199254740993");
  gate.resolve(tileFor(env.calls.tiles[0]));
  await env.runFrames();
  assert.equal((await opening).status, "ready");
  assert.equal(env.state.viewportReady, true);
  assert.equal(env.surface.children.length, 1);
  assert.equal(env.state.svgText, "");
  env.dispose();
});

test("a tile that misses the actual visible rectangle fails the first readiness barrier", async () => {
  const env = fixture({ render: (...args) => Promise.resolve({ ...tileFor(args),
    logicalRect: { xRaw: 0, yRaw: 0, widthRaw: 1, heightRaw: 1 } }) });
  const opening = env.sandbox.renderCurrent({ fit: true });
  await env.runFrames();
  assert.equal((await opening).status, "failed");
  assert.equal(env.state.viewportReady, false);
  assert.equal(env.surface.children.length, 0);
  assert.match(env.calls.errors[0].message, /does not cover/);
  env.dispose();
});

test("collapsed positive geometry waits for actual bounds; empty geometry never fabricates a tile", async () => {
  const env = fixture({ width: 0, height: 0 });
  const opening = env.sandbox.renderCurrent({ fit: true });
  await env.runFrames();
  assert.equal(env.calls.tiles.length, 0);
  assert.equal(env.state.renderOutcome, "pending");
  assert.equal(env.state.busy, true);
  env.port.clientWidth = 400;
  env.port.clientHeight = 200;
  env.sandbox.scheduleViewport();
  await env.runFrames();
  assert.equal((await opening).status, "ready");
  env.dispose();
  const empty = fixture({ descriptor: descriptor({ sourceRange: null, widthRaw: 0, heightRaw: 0, preparationReport: null }) });
  assert.equal((await empty.sandbox.renderCurrent({ fit: true })).status, "empty");
  assert.equal(empty.calls.tiles.length, 0);
  assert.equal(empty.state.viewportContext.preparationReport, null);
  empty.dispose();
});

test("a stale preparation is released by its exact old handle and cannot replace a newer full view", async () => {
  const gate = deferred();
  const env = fixture({ prepare: () => gate.promise });
  const opening = env.sandbox.renderCurrent({ fit: true });
  await flush();
  env.state.openGeneration += 1;
  env.state.client = { renderSheetInteractive: async () => ({ svg: "new full" }) };
  const newer = await env.sandbox.renderCurrent({ fit: false });
  gate.resolve(descriptor());
  assert.equal((await opening).status, "stale");
  assert.equal(newer.status, "ready");
  assert.equal(env.state.svgText, "new full");
  assert.equal(env.state.viewportContext, null);
  assert.deepEqual(env.calls.releases[0], ["one", 0, descriptor().geometryId, descriptor().revision]);
  env.dispose();
});

test("reset retains old CPU ownership and stale output cannot overwrite the newer geometry", async () => {
  const gate = deferred();
  let first = true;
  const env = fixture({ prepare: (_id, sheetIndex) => descriptor({ sheetIndex,
    geometryId: `vp-00000000-0000-4000-8000-000000000001-${sheetIndex + 1}` }),
    render: (...args) => { if (first) { first = false; return gate.promise; } return Promise.resolve(tileFor(args)); } });
  const old = env.sandbox.renderCurrent({ fit: true });
  await env.runFrames();
  env.state.sheetIndex = 1;
  const current = env.sandbox.renderCurrent({ fit: true });
  await env.runFrames();
  assert.equal((await old).status, "stale");
  assert.equal(env.calls.tiles.length, 1);
  assert.equal(env.sandbox.schedulerForTest.inspect().active, 1);
  gate.resolve(tileFor(env.calls.tiles[0]));
  await env.runFrames();
  assert.equal((await current).status, "ready");
  assert.equal(env.calls.tiles.length, 2);
  assert.equal(env.state.viewportContext.geometryId.endsWith("-2"), true);
  assert.equal(env.sandbox.schedulerForTest.inspect().peakActive, 1);
  env.dispose();
});

test("initial fit includes the CSS surface ceiling; later excessive zoom preserves the last valid view", async () => {
  const env = fixture({ descriptor: descriptor({ widthRaw: 100 * 1024, heightRaw: 16_000_000 * 1024 }) });
  const opening = env.sandbox.renderCurrent({ fit: true });
  await env.runFrames();
  assert.equal((await opening).status, "ready");
  assert.equal(env.state.zoom, 0.5);
  assert.match(env.sandbox.elements["render-detail"].textContent, /fitted to viewport/);
  const originalSvg = env.state.viewportSvg;
  env.sandbox.setZoom(1);
  assert.equal(env.state.zoom, 0.5);
  assert.equal(env.state.viewportSvg, originalSvg);
  assert.match(env.calls.errors.at(-1).message, /surface limit/);
  env.dispose();
});

test("fractional padded DOM bounds and raw coordinates above 2^32 reach the real client transport unchanged", async () => {
  const env = fixture({ descriptor: descriptor({ widthRaw: 12_000_000 * 1024 }) });
  const opening = env.sandbox.renderCurrent({ fit: true });
  await env.runFrames();
  await opening;
  env.port.scrollLeft = 1_250_000;
  env.sandbox.scheduleViewport();
  await env.runFrames();
  assert.ok(env.calls.tiles.at(-1)[4].xRaw > 2 ** 32);
  assert.equal(Number.isSafeInteger(env.calls.tiles.at(-1)[4].xRaw), true);
  assert.equal(env.calls.tiles.at(-1)[3], "9007199254740993");
  const cover = env.sandbox.currentViewportCover(env.state.viewportContext);
  assert.equal(viewport.viewportContains(env.state.viewportSvg.rect, cover.visible), true);
  env.dispose();
});

test("actual DOM clamp and untyped full errors fail rather than silently switching or becoming ready", async () => {
  const env = fixture();
  env.surface.getBoundingClientRect = () => ({ left: 22, top: 22, right: 100, bottom: 100, width: 78, height: 78 });
  assert.equal((await env.sandbox.renderCurrent({ fit: true })).status, "failed");
  assert.equal(env.state.viewportReady, false);
  assert.match(env.calls.errors[0].message, /clamped/);
  env.dispose();
  const unsupported = fixture({ fullError: Object.assign(new Error("drawing"), { code: "unsupported" }) });
  assert.equal((await unsupported.sandbox.renderCurrent({ fit: true })).status, "failed");
  assert.equal(unsupported.calls.prepare, 0);
  unsupported.dispose();
});

test("unapplied dialogs, paste and cell drafts block before mode assignment, including after an await", async () => {
  for (const guard of ["dialog", "paste", "draft", "after-await"]) {
    const env = fixture({ fullError: null });
    const gate = deferred();
    if (guard === "dialog") env.editing.readyForViewChange = () => false;
    if (guard === "paste") env.sandbox.rangePaste.hasPending = () => true;
    if (guard === "draft") env.grid.hasDraft = () => true;
    if (guard === "after-await") env.sandbox.commitGridDraft = () => gate.promise;
    const change = env.sandbox.setMode("page");
    if (guard === "after-await") {
      env.grid.hasDraft = () => true;
      gate.resolve(true);
    }
    await change;
    assert.equal(env.state.mode, "sheet", guard);
    assert.equal(env.calls.full, 0);
    env.dispose();
  }
});

test("final disposal settles a pending barrier but retains the tile reservation until original settlement", async () => {
  const gate = deferred();
  const env = fixture({ render: () => gate.promise });
  const opening = env.sandbox.renderCurrent({ fit: true });
  await env.runFrames();
  env.dispose();
  assert.equal((await opening).status, "stale");
  assert.equal(env.sandbox.schedulerForTest.inspect().cache.incomingBytes, 6 * 1024 * 1024);
  gate.resolve(tileFor(env.calls.tiles[0]));
  await flush();
  assert.equal(env.sandbox.schedulerForTest.inspect().cache.incomingBytes, 0);
  assert.equal(env.surface.children.length, 0);
});

test("first-readiness deadline cannot be resurrected by late original delivery and retains CPU ownership", async () => {
  const gate = deferred();
  const env = fixture({ render: () => gate.promise });
  const opening = env.sandbox.renderCurrent({ fit: true });
  await env.runFrames();
  const firstTimer = [...env.timers.values()][0];
  firstTimer();
  assert.equal((await opening).status, "failed");
  assert.equal(env.sandbox.schedulerForTest.inspect().active, 1);
  gate.resolve(tileFor(env.calls.tiles[0]));
  await flush();
  assert.equal(env.state.viewportReady, false);
  assert.equal(env.state.renderOutcome, "failed");
  assert.equal(env.surface.children.length, 0);
  assert.equal(env.sandbox.schedulerForTest.inspect().cache.incomingBytes, 0);
  env.dispose();
});

test("bounded diagnostic snapshot records actual covering times and report counters without source text or context refs", async () => {
  const env = fixture();
  const opening = env.sandbox.renderCurrent({ fit: true });
  await env.runFrames();
  await opening;
  const first = env.sandbox.viewerStateForTest();
  assert.equal(first.rendered, true);
  assert.equal(first.viewport.coverage.cacheKind, "rendered");
  assert.ok(first.viewport.coverage.coveredAtMs >= first.viewport.coverage.acceptedAtMs);
  assert.ok(first.viewport.tile.mountedAtMs >= first.viewport.coverage.acceptedAtMs);
  assert.ok(first.viewport.preparation.finishedAtMs >= first.viewport.preparation.startedAtMs);
  assert.equal(first.viewport.tile.svgBytes, 6);
  assert.equal(first.viewport.tile.sceneNodes, 1);
  assert.equal(first.viewport.coverage.revision, "9007199254740993");
  assert.equal(Object.isFrozen(first.viewport.coverage.visible), true);
  const text = JSON.stringify(first);
  assert.doesNotMatch(text, /<svg|old full|"client"|"context"|"history"/);
  env.sandbox.scheduleViewport();
  await env.runFrames();
  const next = env.sandbox.viewerStateForTest();
  assert.ok(next.viewport.coverage.intent > first.viewport.coverage.intent);
  assert.equal(next.viewport.coverage.cacheKind, "covered");
  assert.equal(next.viewport.firstCoverage.coveredAtMs, first.viewport.firstCoverage.coveredAtMs);
  assert.equal(first.viewport.coverage.cacheKind, "rendered"); // Immutable prior snapshot.
  env.dispose();
});

test("diagnostic zoom accepts only four exact numbers and rejects surface overflow without changing DOM or logical identity", async () => {
  const env = fixture({ descriptor: descriptor({ widthRaw: 100 * 1024, heightRaw: 16_000_000 * 1024 }) });
  const opening = env.sandbox.renderCurrent({ fit: true });
  await env.runFrames();
  await opening;
  const prior = env.state.viewportSvg;
  const style = { ...env.surface.style };
  for (const value of ["1", 0, 0.5, NaN, Infinity, 4, 1]) {
    const result = env.sandbox.viewerSetZoomForTest(value);
    assert.equal(result.accepted, false);
    assert.equal(env.state.zoom, 0.5);
    assert.equal(env.state.viewportSvg, prior);
    assert.deepEqual(env.surface.style, style);
  }
  assert.equal(env.sandbox.viewerSetZoomForTest(0.25).accepted, true);
  await env.runFrames();
  assert.equal(env.sandbox.viewerStateForTest().rendered, true);
  assert.equal(env.state.viewportContext.widthRaw, 100 * 1024);
  env.dispose();
});

test("cache reuse records the original attached namespace without another worker render", async () => {
  const env = fixture();
  const opening = env.sandbox.renderCurrent({ fit: true });
  await env.runFrames();
  await opening;
  const initial = env.sandbox.viewerStateForTest().viewport.coverage;
  env.port.scrollTop = 500;
  env.sandbox.scheduleViewport();
  await env.runFrames();
  assert.equal(env.calls.tiles.length, 2);
  env.port.scrollTop = 0;
  env.sandbox.scheduleViewport();
  await env.runFrames();
  const cached = env.sandbox.viewerStateForTest().viewport.coverage;
  assert.equal(env.calls.tiles.length, 2);
  assert.equal(cached.cacheKind, "cached");
  assert.equal(cached.namespace, initial.namespace);
  assert.ok(cached.coveredAtMs >= cached.acceptedAtMs);
  assert.equal(cached.geometryId, initial.geometryId);
  env.dispose();
});

test("all four bounded diagnostic zoom values route through actual zoom while preserving raw source geometry", async () => {
  const env = fixture();
  const opening = env.sandbox.renderCurrent({ fit: true });
  await env.runFrames();
  await opening;
  const widthRaw = env.state.viewportContext.widthRaw;
  for (const value of [0.25, 1, 2, 3]) {
    const result = env.sandbox.viewerSetZoomForTest(value);
    assert.equal(result.accepted, true);
    assert.equal(result.zoom, value);
    await env.runFrames();
    assert.equal(env.sandbox.viewerStateForTest().rendered, true);
    assert.equal(env.state.viewportContext.widthRaw, widthRaw);
    assert.equal(env.calls.prepare, 1);
  }
  env.dispose();
});

test("reset clears the exact mounted old tile before a failed subsequent render and final disposal", async () => {
  for (const action of ["failed-render", "dispose"]) {
    const env = fixture();
    const opening = env.sandbox.renderCurrent({ fit: true });
    await env.runFrames();
    assert.equal((await opening).status, "ready");
    const oldWrapper = env.state.viewportSvg.wrapper;
    assert.equal(oldWrapper.parentElement, env.surface);
    if (action === "failed-render") {
      env.state.client.renderSheetInteractive = async () => { throw new Error("next render failed"); };
      assert.equal((await env.sandbox.renderCurrent({ fit: false })).status, "failed");
    } else env.dispose();
    assert.equal(env.surface.children.length, 0);
    assert.equal(oldWrapper.parentElement, null);
    assert.equal(env.state.viewportSvg, null);
    assert.equal(env.state.viewportContext, null);
    assert.equal(env.state.displayKind, "none");
    assert.equal(env.calls.releases.length, 1);
    env.dispose();
  }
});
