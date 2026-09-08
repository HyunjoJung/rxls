import test from "node:test";
import assert from "node:assert/strict";

import * as protocol from "../js/protocol.mjs";
import { RenderWorkerClient } from "../js/client.mjs";
import { RenderWorkerRuntime } from "../js/worker-runtime.mjs";
import { EXPECTED_CAPABILITIES } from "./browser/contract.mjs";

const SVG = '<svg width="100" height="40" viewBox="0 0 100 40"></svg>';
function output() {
  return { svg: SVG, interaction: {
    schemaVersion: 1, width: 100, height: 40,
    cells: [[0, 0, 0, 0, 50, 20], [0, 1, 50, 0, 50, 20], [1, 0, 0, 20, 100, 20]]
  } };
}

class FakeWorker {
  listeners = new Map();
  sent = [];
  terminated = false;
  addEventListener(type, callback) { this.listeners.set(type, callback); }
  postMessage(message) { this.sent.push(message); }
  terminate() { this.terminated = true; }
  emit(message) { this.listeners.get("message")?.({ data: message }); }
}

function clientHarness(limits = {}) {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  const capabilities = structuredClone(EXPECTED_CAPABILITIES);
  Object.assign(capabilities.limits, limits);
  worker.emit({ protocol: protocol.PROTOCOL, type: "ready", capabilities });
  return { client, worker };
}

function response(requestId, value = output(), identity = {}) {
  return { protocol: protocol.PROTOCOL, type: "result", requestId, ok: true, error: null,
    result: { documentId: "doc", sheetIndex: 0, mimeType: "image/svg+xml", ...value, ...identity } };
}

test("interactive sheet client has an additive operation and preserves its typed tuple result", async () => {
  const { client, worker } = clientHarness();
  const options = { limits: { maxCells: 3 } };
  const pending = client.renderSheetInteractive("doc", 0, options);
  assert.equal(worker.sent[0].operation, "render-sheet-interactive");
  options.limits.maxCells = 1;
  worker.emit(response(pending.requestId));
  assert.deepEqual((await pending).interaction.cells, output().interaction.cells);
  client.terminate();
});

test("plain render-sheet still rejects an interactive result's additional keys", async () => {
  const { client, worker } = clientHarness();
  const pending = client.renderSheet("doc", 0);
  worker.emit(response(pending.requestId));
  await assert.rejects(pending, { code: "worker_message_error" });
});

test("interactive cancellation ignores stale result without closing the client", async () => {
  const { client, worker } = clientHarness();
  const controller = new AbortController();
  const pending = client.renderSheetInteractive("doc", 0, {}, { signal: controller.signal });
  controller.abort();
  await assert.rejects(pending, { name: "AbortError" });
  worker.emit(response(pending.requestId, { svg: "stale", interaction: null }));
  const next = client.renderSheetInteractive("doc", 0);
  worker.emit(response(next.requestId));
  assert.equal((await next).sheetIndex, 0);
  assert.equal(worker.terminated, false);
  client.terminate();
});

const malformed = [
  ["sidecar version", (v) => { v.interaction.schemaVersion = 2; }],
  ["text payload", (v) => { v.interaction.text = "not allowed"; }],
  ["missing dimensions", (v) => { delete v.interaction.width; }],
  ["zero canvas", (v) => { v.interaction.width = 0; }],
  ["nonfinite canvas", (v) => { v.interaction.height = Infinity; }],
  ["oversized canvas", (v) => { v.interaction.width = 2_000_001; }],
  ["short tuple", (v) => { v.interaction.cells[0].pop(); }],
  ["long tuple", (v) => { v.interaction.cells[0].push(7); }],
  ["fractional source row", (v) => { v.interaction.cells[0][0] = 0.5; }],
  ["source column beyond grid", (v) => { v.interaction.cells[0][1] = 16_384; }],
  ["negative position", (v) => { v.interaction.cells[0][2] = -1; }],
  ["zero rectangle", (v) => { v.interaction.cells[0][4] = 0; }],
  ["nonfinite rectangle", (v) => { v.interaction.cells[0][4] = NaN; }],
  ["rectangle outside canvas", (v) => { v.interaction.cells[0][4] = 101; }],
  ["duplicate cell anchor", (v) => { v.interaction.cells.push([...v.interaction.cells[0]]); }],
  ["sparse tuple", (v) => { delete v.interaction.cells[0][2]; }],
  ["decorated tuple", (v) => { v.interaction.cells[0].text = "hidden"; }],
  ["unsafe SVG", (v) => { v.svg = '<svg><script>alert(1)</script></svg>'; }]
];

for (const [name, mutate] of malformed) {
  test(`client rejects interactive ${name}`, async () => {
    const { client, worker } = clientHarness();
    const pending = client.renderSheetInteractive("doc", 0);
    const value = output();
    mutate(value);
    worker.emit(response(pending.requestId, value));
    await assert.rejects(pending, { code: "worker_message_error" });
    assert.equal(worker.terminated, true);
  });
}

for (const identity of [{ documentId: "other" }, { sheetIndex: 1 }]) {
  test(`client rejects mismatched interactive identity ${JSON.stringify(identity)}`, async () => {
    const { client, worker } = clientHarness();
    const pending = client.renderSheetInteractive("doc", 0);
    worker.emit(response(pending.requestId, output(), identity));
    await assert.rejects(pending, { code: "worker_message_error" });
  });
}

test("client applies request and advertised interactive cell/dimension/output limits", async () => {
  for (const [options, capabilities] of [
    [{ limits: { maxCells: 2 } }, {}],
    [{ limits: { maxDimensionRaw: 50 * 1024 } }, {}],
    [{ limits: { maxOutputBytes: SVG.length + 10 } }, {}],
    [{}, { maxCells: 2 }],
    [{}, { maxDimensionRaw: 50 * 1024 }],
    [{}, { maxOutputBytes: SVG.length + 10 }]
  ]) {
    const { client, worker } = clientHarness(capabilities);
    const pending = client.renderSheetInteractive("doc", 0, options);
    worker.emit(response(pending.requestId));
    await assert.rejects(pending, { code: "worker_message_error" });
  }
});

test("interactive output byte count matches the full serialized UTF-8 JSON envelope", () => {
  const value = output();
  value.svg = '<svg width="100" height="40" viewBox="0 0 100 40"><text>한글😀\\\"\n</text></svg>';
  const size = new TextEncoder().encode(JSON.stringify(value)).byteLength;
  assert.equal(protocol.validateInteractiveSheetOutput(value, { maxOutputBytes: size }), size);
  assert.throws(() => protocol.validateInteractiveSheetOutput(value, { maxOutputBytes: size - 1 }),
    { code: "limit_exceeded" });
});

test("interactive output rejects getters without invoking them", () => {
  const value = output();
  let invoked = false;
  Object.defineProperty(value.interaction, "cells", { enumerable: true, get() { invoked = true; return []; } });
  assert.throws(() => protocol.validateInteractiveSheetOutput(value), { code: "invalid_interaction" });
  assert.equal(invoked, false);
});

test("interactive preflight validates request identity, fields and lower budgets", () => {
  const request = { operation: "render-sheet-interactive", payload: { documentId: "doc", sheetIndex: 0 } };
  assert.equal(protocol.preflightRequest(request), 0);
  for (const payload of [
    { documentId: "../doc", sheetIndex: 0 },
    { documentId: "doc", sheetIndex: -1 },
    { documentId: "doc", sheetIndex: 0, interaction: {} },
    { documentId: "doc", sheetIndex: 0, options: { limits: { maxCells: 0 } } },
    { documentId: "doc", sheetIndex: 0, options: { limits: { maxCells: 250_001 } } }
  ]) assert.throws(() => protocol.preflightRequest({ ...request, payload }));
});

test("formula-auto accepts a bounded nonempty expression only as a set-cell input", () => {
  const request = (value) => ({ operation: "set-cell", payload: { documentId: "doc", sheetIndex: 0, row: 0, col: 0, value } });
  assert.ok(protocol.preflightRequest(request({ kind: "formula-auto", formula: "=SUM(A1:A2)" })) > 0);
  for (const value of [
    { kind: "formula-auto", formula: " = " },
    { kind: "formula-auto", formula: "1", cached: { kind: "number", value: 1 } },
    { kind: "formula-auto", formula: 1 },
    { kind: "formula-auto", formula: "x".repeat(protocol.MAX_EDIT_REQUEST_BYTES + 1) },
    { kind: "formula", formula: "1", cached: { kind: "formula-auto", formula: "1" } }
  ]) assert.throws(() => protocol.preflightRequest(request(value)));
});

async function runtimeHarness({ render = () => JSON.stringify(output()), limits = {}, legacy = false,
  recalculation = () => ({ computedCells: 2, unchangedCells: 1, unsupportedCells: 1, reasons: ["volatile"] }) } = {}) {
  const messages = [];
  const calls = [];
  const workbook = {
    schemaVersion: 1, sheetCount: 1, sheets: [{ index: 0, name: "S", embeddedImages: 0 }],
    embeddedImages: 0, embeddedImageBytes: 0, fontPackSha256: null, fontFaces: 0,
    properties: Object.fromEntries(["title", "subject", "creator", "keywords", "description",
      "lastModifiedBy", "company", "created"].map((key) => [key, null]))
  };
  const editState = {
    schemaVersion: 1, capability: "read-write", reason: null, dirty: false,
    canUndo: false, canRedo: false, undoDepth: 0, redoDepth: 0, historyBytes: 0, editedParts: []
  };
  class Session {
    inspectionJson() { return JSON.stringify(workbook); }
    editStateJson() { return JSON.stringify(editState); }
    renderSheetSvg() { return SVG; }
    setCellJson(json) {
      calls.push(["set-cell", JSON.parse(json)]);
      return JSON.stringify({ workbook, editState });
    }
    free() { calls.push(["free"]); }
  }
  if (!legacy) Session.prototype.renderSheetInteractiveJson = function (sheet, options) {
    calls.push(["interactive", sheet, JSON.parse(options)]);
    return render();
  };
  if (!legacy) Session.prototype.setCellRecalculateJson = function (json) {
    calls.push(["set-cell-recalculate", JSON.parse(json)]);
    return JSON.stringify({ workbook, editState, recalculation: recalculation() });
  };
  if (!legacy) Session.prototype.setRangeRecalculateJson = function (json) {
    calls.push(["set-range-recalculate", JSON.parse(json)]);
    return JSON.stringify({ workbook, editState, recalculation: recalculation() });
  };
  const capabilities = structuredClone(EXPECTED_CAPABILITIES);
  Object.assign(capabilities.limits, limits);
  const runtime = new RenderWorkerRuntime({
    wasm: { RenderSession: Session, capabilitiesJson: () => JSON.stringify(capabilities) },
    send: (message) => messages.push(message)
  });
  const request = (requestId, operation, payload = {}) => runtime.receive({
    protocol: protocol.PROTOCOL, type: "request", requestId, operation, payload
  });
  const result = (requestId) => messages.find((message) => message.type === "result" && message.requestId === requestId);
  const settle = async () => {
    for (let i = 0; i < 8; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
  };
  request("open", "open", { documentId: "doc", bytes: Uint8Array.of(80, 75, 3, 4) });
  await settle();
  assert.equal(result("open")?.ok, true, JSON.stringify(result("open")) ?? "open did not settle");
  return { runtime, request, result, settle, calls };
}

test("runtime returns bounded interactive SVG and geometry with exact identity", async () => {
  const h = await runtimeHarness();
  h.request("interactive", "render-sheet-interactive", {
    documentId: "doc", sheetIndex: 0, options: { range: { firstRow: 0, firstCol: 0, lastRow: 1, lastCol: 1 }, limits: { maxCells: 3 } }
  });
  await h.settle();
  assert.deepEqual(h.result("interactive"), response("interactive"));
  assert.equal(h.calls[0][0], "interactive");
  assert.equal(h.calls[0][2].limits.maxCells, 3);
  h.runtime.closeAll();
});

test("runtime keeps old adapters working and fails only their unsupported interactive path", async () => {
  const h = await runtimeHarness({ legacy: true });
  h.request("interactive", "render-sheet-interactive", { documentId: "doc", sheetIndex: 0 });
  h.request("sheet", "render-sheet", { documentId: "doc", sheetIndex: 0 });
  await h.settle();
  assert.equal(h.result("interactive").error.code, "wasm_api_mismatch");
  assert.deepEqual(h.result("sheet").result, { documentId: "doc", sheetIndex: 0, mimeType: "image/svg+xml", svg: SVG });
  h.runtime.closeAll();
});

for (const [name, value, options, limits] of [
  ["non-string output", {}, {}, {}],
  ["malformed JSON", "{", {}, {}],
  ["extra envelope fields", JSON.stringify({ ...output(), text: "hidden" }), {}, {}],
  ["unknown schema", JSON.stringify({ ...output(), interaction: { ...output().interaction, schemaVersion: 2 } }), {}, {}],
  ["tuple shape", JSON.stringify({ ...output(), interaction: { ...output().interaction, cells: [[0, 0, 0, 0, 50, 20, 9]] } }), {}, {}],
  ["request cell budget", JSON.stringify(output()), { limits: { maxCells: 2 } }, {}],
  ["advertised cell budget", JSON.stringify(output()), {}, { maxCells: 2 }],
  ["request canvas budget", JSON.stringify(output()), { limits: { maxDimensionRaw: 50 * 1024 } }, {}],
  ["combined output budget", JSON.stringify(output()), { limits: { maxOutputBytes: SVG.length + 10 } }, {}],
  ["UTF-8 output budget", JSON.stringify({ ...output(), svg: `<svg>${"한".repeat(150)}</svg>` }), { limits: { maxOutputBytes: 450 } }, {}],
  ["raw JSON whitespace budget", `${JSON.stringify(output())}${" ".repeat(1024)}`, { limits: { maxOutputBytes: 1024 } }, {}]
]) {
  test(`runtime rejects interactive ${name} without closing the document`, async () => {
    const h = await runtimeHarness({ render: () => value, limits });
    h.request("bad", "render-sheet-interactive", { documentId: "doc", sheetIndex: 0, options });
    h.request("plain", "render-sheet", { documentId: "doc", sheetIndex: 0 });
    await h.settle();
    assert.equal(h.result("bad").ok, false);
    assert.equal(h.result("plain").ok, true);
    h.runtime.closeAll();
  });
}

test("runtime cancellation suppresses in-flight interactive output and continues queued work", async () => {
  let release;
  const h = await runtimeHarness({ render: () => new Promise((resolve) => { release = resolve; }) });
  h.request("cancelled", "render-sheet-interactive", { documentId: "doc", sheetIndex: 0 });
  await h.settle();
  h.runtime.receive({ protocol: protocol.PROTOCOL, type: "cancel", requestId: "cancelled" });
  h.request("next", "render-sheet", { documentId: "doc", sheetIndex: 0 });
  release(JSON.stringify(output()));
  await h.settle();
  assert.equal(h.result("cancelled").error.code, "cancelled");
  assert.equal(h.result("next").ok, true);
  h.runtime.closeAll();
});

test("runtime forwards formula-auto without inventing an explicit cached result", async () => {
  const h = await runtimeHarness();
  const value = { kind: "formula-auto", formula: "=SUM(A1:A2)" };
  h.request("formula", "set-cell", { documentId: "doc", sheetIndex: 0, row: 2, col: 0, value });
  await h.settle();
  assert.equal(h.result("formula").ok, true);
  assert.deepEqual(h.calls[0], ["set-cell", { sheetIndex: 0, row: 2, col: 0, value }]);
  h.runtime.closeAll();
});

test("client forwards formula-auto but rejects it as an inspected output kind", async () => {
  const { client, worker } = clientHarness();
  const value = { kind: "formula-auto", formula: "=1+2" };
  const edited = client.setCell("doc", 0, 0, 0, value);
  assert.deepEqual(worker.sent[0].payload.value, value);
  const read = client.readCell("doc", 0, 0, 0);
  worker.emit({ protocol: protocol.PROTOCOL, type: "result", requestId: read.requestId, ok: true, error: null,
    result: { documentId: "doc", schemaVersion: 1, sheetIndex: 0, row: 0, col: 0, value, formatted: "3" } });
  await assert.rejects(read, { code: "worker_message_error" });
  await assert.rejects(edited, { code: "worker_message_error" });
});

test("pre-ready formula-auto input and interactive limits are isolated from caller mutation", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  const controller = new AbortController();
  const value = { kind: "formula-auto", formula: "=1+2" };
  const edited = client.setCell("doc", 0, 0, 0, value, { signal: controller.signal });
  const options = { limits: { maxCells: 3 } };
  const rendered = client.renderSheetInteractive("doc", 0, options);
  value.formula = "=UNSUPPORTED()";
  options.limits.maxCells = 1;
  assert.equal(worker.sent.length, 0);
  worker.emit({ protocol: protocol.PROTOCOL, type: "ready", capabilities: structuredClone(EXPECTED_CAPABILITIES) });
  assert.equal(worker.sent[0].payload.value.formula, "=1+2");
  assert.equal(worker.sent[1].payload.options.limits.maxCells, 3);
  controller.abort();
  assert.equal(worker.sent.some((message) => message.type === "cancel"), false,
    "dispatched automatic formula mutation is authoritative, not locally cancellable");
  worker.emit(response(rendered.requestId));
  await rendered;
  client.terminate();
  await assert.rejects(edited, { code: "client_closed" });
});

test("recalculating edit is additive, authoritative, and forwards the atomic summary", async () => {
  const h = await runtimeHarness();
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  worker.emit({ protocol: protocol.PROTOCOL, type: "ready", capabilities: structuredClone(EXPECTED_CAPABILITIES) });
  const request = client.setCellAndRecalculate("doc", 0, 0, 0, { kind: "number", value: 4 });
  assert.equal(worker.sent[0].operation, "set-cell-recalculate");
  assert.equal(client.cancel(request.requestId), false);
  h.runtime.receive(worker.sent[0]);
  await h.settle();
  worker.emit(h.result(request.requestId));
  assert.deepEqual((await request).recalculation,
    { computedCells: 2, unchangedCells: 1, unsupportedCells: 1, reasons: ["volatile"] });
  client.terminate();
  h.runtime.closeAll();
});

for (const summary of [
  {}, { computedCells: 1, unchangedCells: 2, unsupportedCells: 0, reasons: [] },
  { computedCells: 10_001, unchangedCells: 0, unsupportedCells: 0, reasons: [] },
  { computedCells: 1, unchangedCells: 0, unsupportedCells: 1, reasons: [] },
  { computedCells: 1, unchangedCells: 0, unsupportedCells: 0, reasons: ["volatile"] },
  { computedCells: 1, unchangedCells: 0, unsupportedCells: 2, reasons: ["volatile", "volatile"] },
  { computedCells: 1, unchangedCells: 0, unsupportedCells: 1, reasons: ["text_limit_exceeded"] },
  { computedCells: 1, unchangedCells: 0, unsupportedCells: 1, reasons: ["=PRIVATE_FORMULA()"] }
]) {
  test(`recalculation summaries reject malformed or misleading counters/reasons ${JSON.stringify(summary)}`, async () => {
    const h = await runtimeHarness({ recalculation: () => summary });
    h.request("bad", "set-cell-recalculate", { documentId: "doc", sheetIndex: 0, row: 0, col: 0, value: { kind: "number", value: 1 } });
    await h.settle();
    assert.equal(h.result("bad").ok, false);
    assert.equal(h.result("bad").error.code, "invalid_recalculation");
    h.runtime.closeAll();
  });
}

test("recalculating edit rejects unsupported legacy adapters without changing plain set-cell", async () => {
  const h = await runtimeHarness({ legacy: true });
  const payload = { documentId: "doc", sheetIndex: 0, row: 0, col: 0, value: { kind: "number", value: 1 } };
  h.request("recalc", "set-cell-recalculate", payload);
  h.request("plain", "set-cell", payload);
  await h.settle();
  assert.equal(h.result("recalc").error.code, "wasm_api_mismatch");
  assert.equal(h.result("plain").ok, true);
  assert.equal(h.calls.filter(([operation]) => operation === "set-cell").length, 1);
  h.runtime.closeAll();
});

test("client fails closed on recalculation summary and document-identity drift", async () => {
  for (const mutate of [
    (result) => { result.recalculation.unchangedCells = 3; },
    (result) => { result.documentId = "other"; },
    (result) => { result.recalculation.reasons = ["formula=secret"]; }
  ]) {
    const h = await runtimeHarness();
    const { client, worker } = clientHarness();
    const pending = client.setCellAndRecalculate("doc", 0, 0, 0, { kind: "number", value: 1 });
    h.runtime.receive(worker.sent[0]);
    await h.settle();
    const reply = h.result(pending.requestId);
    mutate(reply.result);
    worker.emit(reply);
    await assert.rejects(pending, { code: "worker_message_error" });
    h.runtime.closeAll();
  }
});

test("plain set-cell still rejects a recalculation summary as an extra field", async () => {
  const h = await runtimeHarness();
  const { client, worker } = clientHarness();
  const pending = client.setCell("doc", 0, 0, 0, { kind: "number", value: 1 });
  h.runtime.receive({ ...worker.sent[0], operation: "set-cell-recalculate" });
  await h.settle();
  worker.emit(h.result(pending.requestId));
  await assert.rejects(pending, { code: "worker_message_error" });
  h.runtime.closeAll();
});

const rangePayload = (values = [[{ kind: "number", value: 7 }]]) => ({
  documentId: "doc", sheetIndex: 0, startRow: 0, startCol: 0, values
});

test("rectangular edits are authoritative, isolated, and preserve the recalculation response", async () => {
  const h = await runtimeHarness();
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  const values = [[{ kind: "number", value: 7 }, { kind: "formula-auto", formula: "=A1+1" }]];
  const pending = client.setRangeAndRecalculate("doc", 0, 0, 0, values);
  values[0][1].formula = "mutated";
  values.push([{ kind: "blank" }]);
  worker.emit({ protocol: protocol.PROTOCOL, type: "ready", capabilities: structuredClone(EXPECTED_CAPABILITIES) });
  assert.equal(worker.sent[0].operation, "set-range-recalculate");
  assert.equal(worker.sent[0].payload.values.length, 1);
  assert.equal(worker.sent[0].payload.values[0][1].formula, "=A1+1");
  assert.equal(client.cancel(pending.requestId), false);
  h.runtime.receive(worker.sent[0]);
  await h.settle();
  worker.emit(h.result(pending.requestId));
  assert.equal((await pending).recalculation.computedCells, 2);
  assert.deepEqual(h.calls.find(([op]) => op === "set-range-recalculate")[1], {
    sheetIndex: 0, startRow: 0, startCol: 0,
    values: [[{ kind: "number", value: 7 }, { kind: "formula-auto", formula: "=A1+1" }]]
  });
  client.terminate(); h.runtime.closeAll();
});

test("range preflight rejects empty, jagged, sparse, oversized, unknown and out-of-grid input", () => {
  const invalid = [
    rangePayload([]), rangePayload([[]]), rangePayload([[{ kind: "blank" }], []]),
    rangePayload(new Array(2)), rangePayload([new Array(1)]),
    rangePayload([{ get length() { throw new Error("row getter executed"); } }]),
    rangePayload(Array.from({ length: 101 }, () => Array(100).fill({ kind: "blank" }))),
    rangePayload([[{ kind: "number", value: Infinity }]]),
    rangePayload([[{ kind: "formula-auto", formula: "=" }]]),
    rangePayload([[{ kind: "blank", extra: "secret" }]]),
    { ...rangePayload(), startRow: 1_048_576 },
    { ...rangePayload([[{ kind: "blank" }, { kind: "blank" }]]), startCol: 16_383 },
    { ...rangePayload(), extra: 1 },
    rangePayload(Object.assign([[{ kind: "blank" }]], { extra: 1 })),
    rangePayload([[Object.defineProperty({}, "kind", { enumerable: true, get() { throw new Error("getter executed"); } })]])
  ];
  for (const payload of invalid) {
    assert.throws(() => protocol.preflightRequest({ operation: "set-range-recalculate", payload }),
      (error) => error instanceof protocol.RenderProtocolError && error.code !== "unknown_operation");
  }
});

test("range requests have an exact 1MiB UTF-8/JSON budget without raising the single-cell limit", () => {
  const values = Array.from({ length: 10 }, () => [{ kind: "text", value: "x".repeat(20_000) }]);
  const payload = rangePayload(values);
  const request = { sheetIndex: 0, startRow: 0, startCol: 0, values };
  assert.equal(protocol.preflightRequest({ operation: "set-range-recalculate", payload }),
    new TextEncoder().encode(JSON.stringify(request)).byteLength);
  const expanded = rangePayload(Array.from({ length: 10 }, () => [{ kind: "text", value: "\u0000".repeat(20_000) }]));
  assert.throws(() => protocol.preflightRequest({ operation: "set-range-recalculate", payload: expanded }),
    (error) => error.resource === "editRequestBytes" && error.limit === 1_048_576);
  assert.throws(() => protocol.preflightRequest({ operation: "set-cell", payload: {
    documentId: "doc", sheetIndex: 0, row: 0, col: 0, value: { kind: "text", value: "x".repeat(140_000) }
  } }), (error) => error.limit === 131_072);
  const unicode = rangePayload([[{ kind: "text", value: "한국😀\ud800\n" }]]);
  assert.equal(protocol.preflightRequest({ operation: "set-range-recalculate", payload: unicode }),
    new TextEncoder().encode(JSON.stringify({ sheetIndex: 0, startRow: 0, startCol: 0, values: unicode.values })).byteLength);
  const boundary = rangePayload(Array.from({ length: 9 }, () => [{ kind: "text", value: "x".repeat(120_000) }]));
  boundary.values[8][0].value = "";
  const prefixBytes = protocol.preflightRequest({ operation: "set-range-recalculate", payload: boundary });
  boundary.values[8][0].value = "x".repeat(1_048_576 - prefixBytes);
  assert.equal(protocol.preflightRequest({ operation: "set-range-recalculate", payload: boundary }), 1_048_576);
  boundary.values[8][0].value += "x";
  assert.throws(() => protocol.preflightRequest({ operation: "set-range-recalculate", payload: boundary }),
    (error) => error.limit === 1_048_576 && error.actual === 1_048_577);
  assert.throws(() => protocol.preflightRequest({ operation: "set-range-recalculate", payload: rangePayload([[{
    kind: "text", value: "\u0000".repeat(25_000)
  }]]) }), (error) => error.limit === 131_072);
  const tenThousand = rangePayload(Array.from({ length: 100 }, () => Array(100).fill({ kind: "blank" })));
  assert.ok(protocol.preflightRequest({ operation: "set-range-recalculate", payload: tenThousand }) < 1_048_576);
});

test("range response validation rejects malformed summaries and document drift", async () => {
  for (const mutate of [
    (result) => { result.documentId = "other"; },
    (result) => { result.recalculation.unchangedCells = 9; },
    (result) => { result.extra = 1; }
  ]) {
    const h = await runtimeHarness();
    const { client, worker } = clientHarness();
    const pending = client.setRangeAndRecalculate("doc", 0, 0, 0, [[{ kind: "blank" }]]);
    h.runtime.receive(worker.sent[0]); await h.settle();
    const reply = h.result(pending.requestId); mutate(reply.result); worker.emit(reply);
    await assert.rejects(pending, { code: "worker_message_error" });
    h.runtime.closeAll();
  }
});

test("range editing is optional for legacy adapters and malformed requests never enter WASM", async () => {
  const h = await runtimeHarness({ legacy: true });
  h.request("range", "set-range-recalculate", rangePayload());
  h.request("invalid", "set-range-recalculate", rangePayload([[]]));
  h.request("plain", "set-cell", { documentId: "doc", sheetIndex: 0, row: 0, col: 0, value: { kind: "blank" } });
  await h.settle();
  assert.equal(h.result("range").error.code, "wasm_api_mismatch");
  assert.equal(h.result("invalid").ok, false);
  assert.equal(h.result("plain").ok, true);
  assert.equal(h.calls.filter(([op]) => op === "set-range-recalculate").length, 0);
  h.runtime.closeAll();
});
