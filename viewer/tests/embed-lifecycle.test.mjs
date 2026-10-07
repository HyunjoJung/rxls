// Actual main helper source; explicit worker/DOM/render outcome doubles, not browser/WASM proof.
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import { createLatestRequestGate, extensionOf } from "../src/core.js";
import { EditorEmbedError } from "../src/embed/protocol.mjs";

const source = await readFile(new URL("../src/main.js", import.meta.url), "utf8");
function section(start, end) {
  const first = source.indexOf(start), last = source.indexOf(end, first);
  assert.ok(first >= 0 && last > first, `${start} boundary`);
  return source.slice(first, last);
}
const opening = section("function beginOpenRequest(label)", "async function createWorkerTarget(workerUrl)");
const lifecycle = source.slice(source.indexOf("function embedState() {"));
const flush = () => new Promise((resolve) => setImmediate(resolve));
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function fixture(options = {}) {
  const calls = { constructed: 0, opens: 0, terminations: [], trace: [], errors: [], changes: 0,
    cancelGrid: 0, cancelPaste: 0, render: 0 };
  class Worker { terminate() { calls.trace.push("worker-terminate"); } }
  class Client {
    constructor(target) { this.target = target; this.id = ++calls.constructed; this.closed = false; }
    async open(bytes, identity) {
      calls.opens++;
      return options.open ? options.open(bytes, identity) : {
        workbook: { sheetCount: 1, sheets: [{ name: "Current", index: 0 }] },
        editState: { capability: "read-write", dirty: false, canUndo: false, canRedo: false },
      };
    }
    terminate() {
      if (this.closed) return;
      this.closed = true;
      calls.terminations.push(this.id);
      calls.trace.push(`client-terminate:${this.id}`);
      if (this.target instanceof Worker) this.target.terminate();
    }
  }
  const openRequests = createLatestRequestGate();
  const state = { client: null, hostWorker: null, documentId: null, workbook: null, editState: null,
    file: null, openGeneration: 0, renderEpoch: 0, openRequest: null,
    viewportDisposed: false, busy: false, sheetIndex: 0, pageIndex: 0, mode: "sheet",
    manifests: new Map(), runtime: { RenderWorkerClient: Client }, svgText: "", svgElement: null };
  const controls = new Map();
  const elements = new Proxy({}, { get(_target, name) {
    if (!controls.has(name)) controls.set(name, { value: "", textContent: "", replaceChildren() {} });
    return controls.get(name);
  } });
  const sandbox = {
    state, elements, Worker, URL, AbortController, Uint8Array, EditorEmbedError, extensionOf, openRequests,
    baseUrl: new URL("https://consumer.example/nonroot/editor-v1/"),
    grid: { invalidate() {}, hasDraft: () => Boolean(options.gridDraft),
      cancel() { calls.cancelGrid++; return true; } },
    editing: { hasPendingMutation: () => Boolean(options.mutation), hasDraftChanges: () => Boolean(options.dialogDraft) },
    rangePaste: { hasPending: () => Boolean(options.pasteDraft), cancel() { calls.cancelPaste++; return true; } },
    closeCellEditor() {}, closePropertiesEditor() {}, dismissError() {}, closeSidebar() {},
    updateWorkbookUi() {}, onKeyDown() {},
    window: { removeEventListener(name) { calls.trace.push(`remove-listener:${name}`); } },
    workbench: { setDraft() {} },
    createWorkerTarget: () => options.factory ? options.factory() : new URL("https://consumer.example/worker.mjs"),
    async renderCurrent() {
      calls.render++;
      const current = state.documentId;
      const outcome = options.render ? await options.render(state, calls) : { status: "ready" };
      if (state.documentId === current && !state.viewportDisposed) state.busy = false;
      return outcome;
    },
    resetViewport() { calls.trace.push(`reset:${state.documentId}`); },
    disposeViewport() {
      calls.trace.push("viewport-dispose");
      state.viewportDisposed = true;
      options.onViewportDispose?.();
    },
    setBusy(value) { state.busy = value; },
    showEmpty() { calls.trace.push("empty"); },
    showError(error) { calls.errors.push(error); },
  };
  vm.createContext(sandbox);
  vm.runInContext('let disposed = false; const embedMode = true; const embedHost = { publishState() { publish(); } };',
    vm.createContext(Object.assign(sandbox, { publish: () => { calls.changes++; } })));
  vm.runInContext(opening + lifecycle, sandbox);
  function installExisting(id = "retained") {
    const client = new Client(new Worker());
    state.client = client;
    state.hostWorker = client.target;
    state.documentId = id;
    state.workbook = { sheetCount: 1, sheets: [{ name: id, index: 0 }] };
    state.editState = { capability: "read-write", dirty: false, canUndo: false, canRedo: false };
    state.file = { name: `${id}.xlsx` };
    return client;
  }
  return { sandbox, state, calls, options, openRequests, Client, Worker, installExisting,
    load: (replace = "reject") => sandbox.loadEmbeddedWorkbook(Uint8Array.of(80, 75), { fileName: "new.xlsx", replace }),
    dispose: () => sandbox.disposeEmbeddedViewer() };
}

for (const status of ["ready", "empty"]) {
  test(`current ${status} load waits for the actual helper outcome and retains source`, async () => {
    const gate = deferred();
    const env = fixture({ render: () => gate.promise });
    let settled = false;
    const load = env.load().then(() => { settled = true; });
    await flush();
    assert.equal(settled, false);
    assert.equal(env.state.workbook.sheetCount, 1);
    assert.equal(env.sandbox.embedState().loaded, true);
    assert.equal(env.state.busy, true);
    gate.resolve({ status });
    await load;
    assert.equal(settled, true);
    assert.equal(env.state.file.name, "new.xlsx");
    assert.equal(env.state.busy, false);
    assert.deepEqual(env.calls.terminations, []);
    env.dispose();
  });
}

test("current failed view resets its exact adopted geometry before teardown and remains recoverable", async () => {
  const error = Object.assign(new Error("private native failure text"), { code: "limit_exceeded" });
  let first = true;
  const env = fixture({ render: () => first ? (first = false, { status: "failed", error }) : { status: "ready" } });
  await assert.rejects(env.load(), (actual) => actual === error);
  assert.equal(env.state.workbook, null);
  assert.equal(env.state.viewportDisposed, false);
  const terminate = env.calls.trace.indexOf("client-terminate:1");
  assert.ok(terminate > env.calls.trace.indexOf("reset:" + env.calls.trace.find((x) => x.startsWith("reset:viewer-")).slice(6)));
  assert.deepEqual(env.calls.terminations, [1]);
  // The real render helper already reports returned failed outcomes; no duplicate adapter diagnostic.
  assert.deepEqual(env.calls.errors, []);
  await env.load();
  assert.equal(env.state.file.name, "new.xlsx");
  env.dispose();
});

test("accepted corrupt replacement before adoption clears only the still-matching retained source", async () => {
  const error = Object.assign(new Error("invalid workbook"), { code: "invalid_input" });
  const env = fixture({ open: async () => { throw error; } });
  const old = env.installExisting();
  await assert.rejects(env.load("discard"), (actual) => actual === error);
  assert.equal(old.closed, true);
  assert.equal(env.state.workbook, null);
  assert.equal(env.calls.trace.filter((x) => x === "reset:retained").length, 2);
  const finalReset = env.calls.trace.lastIndexOf("reset:retained");
  assert.ok(finalReset < env.calls.trace.indexOf(`client-terminate:${old.id}`));
  assert.equal(env.state.viewportDisposed, false);
  assert.deepEqual(env.calls.errors, [error]);
  env.dispose();
});

for (const field of ["gridDraft", "dialogDraft", "pasteDraft", "dirty"]) {
  test(`${field} replacement rejects before source or draft ownership is changed`, async () => {
    const env = fixture({ [field]: true });
    const client = env.installExisting();
    if (field === "dirty") env.state.editState.dirty = true;
    const generation = env.state.openGeneration;
    await assert.rejects(env.load(), { code: "dirty_replacement" });
    assert.equal(env.state.client, client);
    assert.equal(env.state.openGeneration, generation);
    assert.equal(client.closed, false);
    assert.equal(env.calls.opens, 0);
    assert.equal(env.calls.cancelGrid, 0);
    assert.equal(env.calls.cancelPaste, 0);
    env.dispose();
  });
}

for (const point of ["factory", "open", "render"]) {
  test(`a stale ${point} await cannot erase or terminate a newer source`, async () => {
    const gate = deferred();
    const options = point === "factory" ? { factory: () => gate.promise }
      : point === "open" ? { open: () => gate.promise } : { render: () => gate.promise };
    const env = fixture(options);
    const pending = env.load();
    const rejection = assert.rejects(pending, { code: "stale_operation" });
    await flush();
    env.openRequests.begin();
    env.state.openGeneration++;
    env.state.openRequest = null;
    const newer = env.installExisting("newer");
    if (point === "factory") gate.resolve(new URL("https://consumer.example/worker.mjs"));
    else if (point === "open") gate.resolve({ workbook: {}, editState: {} });
    else gate.resolve({ status: "ready" });
    await rejection;
    assert.equal(env.state.client, newer);
    assert.equal(env.state.file.name, "newer.xlsx");
    assert.equal(newer.closed, false);
    assert.equal(env.calls.trace.includes("reset:newer"), false);
    env.dispose();
  });
}

test("stale render status alone rejects without clearing an adopted current source", async () => {
  const env = fixture({ render: () => ({ status: "stale" }) });
  await assert.rejects(env.load(), { code: "stale_operation" });
  assert.equal(env.state.file.name, "new.xlsx");
  assert.equal(env.state.workbook.sheetCount, 1);
  assert.deepEqual(env.calls.terminations, []);
  env.dispose();
});

test("same client/document with different worker owner cannot pass failed-load cleanup", async () => {
  const gate = deferred();
  const env = fixture({ render: () => gate.promise });
  const pending = env.load();
  const rejected = assert.rejects(pending, { code: "stale_operation" });
  await flush();
  env.state.hostWorker = new env.Worker();
  gate.resolve({ status: "failed", error: new Error("old view") });
  await rejected;
  assert.equal(env.state.file.name, "new.xlsx");
  assert.deepEqual(env.calls.terminations, []);
  env.dispose();
});

test("final disposal settles viewport before exact worker teardown and rejects late load once", async () => {
  const gate = deferred();
  const env = fixture({ factory: () => new env.Worker(), render: () => gate.promise,
    onViewportDispose: () => gate.resolve({ status: "stale" }) });
  const pending = env.load();
  const rejected = assert.rejects(pending, { code: "disposed" });
  await flush();
  env.dispose();
  env.dispose();
  await rejected;
  assert.equal(env.calls.trace.filter((x) => x === "viewport-dispose").length, 1);
  assert.equal(env.calls.trace.filter((x) => x === "worker-terminate").length, 1);
  assert.ok(env.calls.trace.indexOf("viewport-dispose") < env.calls.trace.indexOf("worker-terminate"));
  assert.equal(env.state.workbook, null);
  assert.equal(env.state.busy, false);
  assert.equal(env.state.viewportDisposed, true);
});

test("disposal of A leaves B's live source and unchanged draft untouched", async () => {
  const a = fixture(), b = fixture({ gridDraft: true });
  const clientB = b.installExisting("B");
  await a.load();
  a.dispose();
  assert.equal(clientB.closed, false);
  assert.equal(b.state.file.name, "B.xlsx");
  assert.equal(b.sandbox.embedState().draft, true);
  assert.equal(b.state.viewportDisposed, false);
  assert.equal(b.calls.cancelGrid, 0);
  b.dispose();
});

test("disposal after an awaited owned worker factory prevents any client construction or open", async () => {
  const gate = deferred();
  const env = fixture({ factory: () => gate.promise });
  const pending = env.load();
  const rejected = assert.rejects(pending, { code: "disposed" });
  env.dispose();
  gate.resolve(new env.Worker());
  await rejected;
  assert.equal(env.calls.constructed, 0);
  assert.equal(env.calls.opens, 0);
  assert.equal(env.calls.trace.filter((x) => x === "worker-terminate").length, 1);
  assert.ok(env.calls.trace.indexOf("viewport-dispose") < env.calls.trace.indexOf("worker-terminate"));
});

test("final disposal releases unpublished opening and retained clients only after viewport teardown", async () => {
  const gate = deferred();
  const env = fixture({ open: () => gate.promise });
  const retained = env.installExisting();
  const pending = env.load("discard");
  const rejected = assert.rejects(pending, { code: "disposed" });
  await flush();
  env.dispose();
  gate.resolve({ workbook: {}, editState: {} });
  await rejected;
  assert.equal(retained.closed, true);
  assert.deepEqual(env.calls.terminations.sort(), [1, 2]);
  for (const id of env.calls.terminations) {
    assert.ok(env.calls.trace.indexOf("viewport-dispose") < env.calls.trace.indexOf(`client-terminate:${id}`));
  }
});
