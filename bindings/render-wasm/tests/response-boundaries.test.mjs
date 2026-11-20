import test from "node:test";
import assert from "node:assert/strict";
import { RenderWorkerClient } from "../js/client.mjs";
import { RenderWorkerRuntime } from "../js/worker-runtime.mjs";
import { PROTOCOL } from "../js/protocol.mjs";
import { EXPECTED_CAPABILITIES } from "./browser/contract.mjs";

function workbook() {
  return {
    schemaVersion: 1, sheetCount: 1,
    sheets: [{ index: 0, name: "Sheet1", embeddedImages: 0 }],
    embeddedImages: 0, embeddedImageBytes: 0,
    fontPackSha256: null, fontFaces: 0,
    properties: {
      title: null, subject: null, creator: null, keywords: null,
      description: null, lastModifiedBy: null, company: null, created: null
    }
  };
}

function editState(overrides = {}) {
  return {
    schemaVersion: 2, calculation: { status: "unknown", summary: null }, capability: "read-write", reason: null,
    dirty: false, canUndo: false, canRedo: false,
    undoDepth: 0, redoDepth: 0, historyBytes: 0, editedParts: [],
    ...overrides
  };
}

/** Real client/runtime/protocol; only browser transport and Rust execution are fake. */
function harness(t, configure = () => {}, tamper = (message) => message) {
  const state = {
    workbook: workbook(), editState: editState(), cell: { kind: "blank" },
    capabilities: structuredClone(EXPECTED_CAPABILITIES), freed: 0
  };
  configure(state);
  const wasm = {
    capabilitiesJson: () => JSON.stringify(state.capabilities),
    RenderSession: class {
      inspectionJson() { return JSON.stringify(state.workbook); }
      editStateJson() { return JSON.stringify(state.editState); }
      readCellJson(sheetIndex, row, col) {
        return JSON.stringify({ schemaVersion: 1, sheetIndex, row, col, value: state.cell, formatted: null });
      }
      free() { state.freed++; }
    }
  };
  const listeners = new Map();
  let closed = false;
  const emit = (message) => {
    if (!closed) for (const listener of listeners.get("message") ?? []) listener({ data: message });
  };
  const runtime = new RenderWorkerRuntime({
    wasm,
    send: (message) => queueMicrotask(() => emit(tamper(structuredClone(message))))
  });
  const worker = {
    addEventListener(type, listener) {
      if (!listeners.has(type)) listeners.set(type, []);
      listeners.get(type).push(listener);
    },
    postMessage(message) { runtime.receive(structuredClone(message)); },
    terminate() { closed = true; runtime.closeAll(); }
  };
  const client = new RenderWorkerClient(worker);
  t.after(() => client.terminate());
  emit({ protocol: PROTOCOL, type: "ready", capabilities: runtime.capabilities() });
  return { client, runtime, state, isClosed: () => closed };
}

test("shared response rules accept a complete client/runtime open and cell read", async (t) => {
  const { client, state } = harness(t);
  const opened = await client.open(Uint8Array.of(1));
  assert.deepEqual(opened.workbook, state.workbook);
  assert.deepEqual(opened.editState, state.editState);
  state.cell = { kind: "formula", formula: "A1*2", cached: { kind: "number", value: 4 } };
  const read = await client.readCell(opened.documentId, 0, 0, 0);
  assert.deepEqual(read.value, state.cell);
});

for (const [name, configure] of [
  ["extra edit-state field", (state) => { state.editState.extra = true; }],
  ["contradictory history", (state) => { state.editState.canUndo = true; }],
  ["image total mismatch", (state) => { state.workbook.embeddedImages = 1; }],
  ["extra document property", (state) => { state.workbook.properties.extra = null; }]
]) {
  test(`WASM boundary rejects ${name} and releases the provisional session`, async (t) => {
    const { client, state, isClosed } = harness(t, configure);
    await assert.rejects(client.open(Uint8Array.of(1)), { code: "wasm_api_mismatch", location: "wasm" });
    assert.equal(state.freed, 1);
    assert.equal(isClosed(), false);
  });
}

test("WASM boundary retains lower advertised history limits", async (t) => {
  const { client, state } = harness(t, (state) => {
    state.capabilities.editing.maxHistoryEntries = 2;
    state.capabilities.editing.maxHistoryBytes = 100;
    state.editState = editState({ dirty: true, canUndo: true, undoDepth: 2, historyBytes: 100 });
  });
  const opened = await client.open(Uint8Array.of(1));
  assert.equal(opened.editState.historyBytes, 100);
  state.editState.historyBytes = 101;
  await assert.rejects(client.editStatus(opened.documentId), { code: "wasm_api_mismatch", location: "wasm" });
  state.editState.historyBytes = 100;
  state.editState.undoDepth = 3;
  await assert.rejects(client.editStatus(opened.documentId), { code: "wasm_api_mismatch", location: "wasm" });
});

test("client still independently rejects a corrupted worker response", async (t) => {
  const { client, isClosed } = harness(t, undefined, (message) => {
    if (message.type === "result" && message.ok) message.result.editState.extra = true;
    return message;
  });
  await assert.rejects(client.open(Uint8Array.of(1)), { code: "worker_message_error", location: "worker" });
  assert.equal(isClosed(), true);
});

test("client transport identity validation remains outside shared data rules", async (t) => {
  const { client, isClosed } = harness(t, undefined, (message) => {
    if (message.type === "result" && message.ok) message.result.documentId = "wrong-document";
    return message;
  });
  await assert.rejects(client.open(Uint8Array.of(1)), { code: "worker_message_error", location: "worker" });
  assert.equal(isClosed(), true);
});
