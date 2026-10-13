import test from "node:test";
import assert from "node:assert/strict";
import {
  PROTOCOL, VIEWPORT_LIMITS, VIEWPORT_RESOURCE_POLICY,
  preflightRequest, validateViewportU64, validateViewportGeometryId, validateViewportRect,
  viewportOptionsJson, validateViewportDescriptor, validateViewportPrepareResult,
  validateViewportTileResult, validateViewportCapabilities
} from "../js/protocol.mjs";
import { RenderWorkerRuntime } from "../js/worker-runtime.mjs";
import { RenderWorkerClient } from "../js/client.mjs";
import { EXPECTED_CAPABILITIES } from "./browser/contract.mjs";

const U64_MAX = "18446744073709551615";
const ABOVE_SAFE = "9007199254740993";
const ID = "vp-11111111-1111-4111-8111-111111111111-1";
const RANGE = { firstRow: 0, firstCol: 0, lastRow: 1, lastCol: 1 };
const RECT = { xRaw: 0, yRaw: 0, widthRaw: 1024, heightRaw: 1024 };
const SVG = '<svg xmlns="http://www.w3.org/2000/svg"><title>S</title></svg>';
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function report() {
  return { coordinateVisits: 4, sourceRawCells: 2, sourceHyperlinks: 0,
    sourceIndexBuildPeakBytes: 128, geometryBytes: 64, textBytes: 0,
    shapedGlyphs: 0, textWork: 0, shapedRuns: 0, textLines: 0,
    pathCommands: 0, conditionalEvaluations: 0,
    fontPackSha256: null, fontFaces: [], warnings: [] };
}
function descriptor(geometryId = ID, revision = ABOVE_SAFE, overrides = {}) {
  return { schemaVersion: 1, sheetIndex: 0, geometryId, revision, sourceRange: { ...RANGE },
    widthRaw: 8192, heightRaw: 8192, sheetVisibility: "visible", preparationReport: report(), ...overrides };
}
function prepareResult(overrides = {}) {
  return { documentId: "doc", ...descriptor(), resources: { geometryReservationBytes: 8388608,
    sourceIndexReservationBytes: 8388608 }, ...overrides };
}
function tileResult(identity = {}, overrides = {}) {
  const expected = { documentId: "doc", sheetIndex: 0, geometryId: ID, revision: ABOVE_SAFE,
    rect: { ...RECT }, namespace: U64_MAX, ...identity };
  return { schemaVersion: 1, documentId: expected.documentId, sheetIndex: expected.sheetIndex,
    geometryId: expected.geometryId, revision: expected.revision, namespace: expected.namespace,
    requestedRect: { ...expected.rect }, mimeType: "image/svg+xml", logicalRect: { ...expected.rect },
    sourceRange: { ...RANGE }, svg: SVG,
    report: { schema_version: 2, sheet_index: 0, sheet_name: "S",
      range: { first_row: 0, first_col: 0, last_row: 1, last_col: 1 },
      rows_considered: 2, columns_considered: 2, cells_considered: 2, visible_rows: 2,
      visible_columns: 2, rendered_regions: 2, hidden_rows_skipped: 0, hidden_columns_skipped: 0,
      merged_regions: 0, text_bytes: 0, glyphs: 0, scene_nodes: 1,
      svg_bytes: new TextEncoder().encode(SVG).byteLength, font_pack_sha256: null, font_faces: [], warnings: [] },
    metrics: { coordinateVisits: 2, geometryBytes: 64, haloRows: 2, haloColumns: 2, haloCells: 12 }, ...overrides };
}
function capabilities() {
  return { schemaVersion: 1, unitsPerPixel: 1024, limits: { ...VIEWPORT_LIMITS } };
}
function workbook() {
  return { schemaVersion: 1, sheetCount: 1, sheets: [{ index: 0, name: "S", embeddedImages: 0 }],
    embeddedImages: 0, embeddedImageBytes: 0, fontPackSha256: null, fontFaces: 0,
    properties: { title: null, subject: null, creator: null, keywords: null, description: null,
      lastModifiedBy: null, company: null, created: null } };
}
function editState(writable = false) {
  return { schemaVersion: 1, capability: writable ? "read-write" : "read-only",
    reason: writable ? null : "legacy-biff", dirty: false, canUndo: false, canRedo: false,
    undoDepth: 0, redoDepth: 0, historyBytes: 0, editedParts: [] };
}
function fixture({ writable = false, sendHook = null, viewportSupported = true } = {}) {
  const messages = [], sessions = [];
  let sequence = 0;
  class Session {
    current = null;
    staged = null;
    freed = false;
    revision = ABOVE_SAFE;
    calls = [];
    stageHook = null;
    tileHook = null;
    commitHook = null;
    abortHook = null;
    mutateHook = null;
    constructor() { sessions.push(this); }
    inspectionJson() { return JSON.stringify(workbook()); }
    editStateJson() { return JSON.stringify(editState(writable)); }
    async stageViewportJson(sheetIndex, geometryId, optionsJson) {
      assert.equal(this.freed, false);
      this.calls.push(["stage", sheetIndex, geometryId, optionsJson]);
      const value = descriptor(geometryId, this.revision, { sheetIndex });
      this.staged = value;
      return this.stageHook ? await this.stageHook(value) : JSON.stringify(value);
    }
    commitViewport(id, revision) {
      this.calls.push(["commit", id, revision]);
      if (this.commitHook) return this.commitHook(id, revision);
      assert.equal(this.staged.geometryId, id);
      assert.equal(this.staged.revision, revision);
      this.current = this.staged;
      this.staged = null;
      return true;
    }
    abortViewport(id) {
      assert.equal(this.freed, false);
      this.calls.push(["abort", id]);
      if (this.abortHook) return this.abortHook(id);
      const found = this.staged?.geometryId === id;
      if (found) this.staged = null;
      return found;
    }
    async renderViewportTileJson(sheetIndex, geometryId, revision, rectJson, namespace) {
      assert.equal(this.freed, false);
      this.calls.push(["tile", sheetIndex, geometryId, revision, rectJson, namespace]);
      assert.equal(this.current.geometryId, geometryId);
      const rect = JSON.parse(rectJson);
      const value = tileResult({ sheetIndex, geometryId, revision, rect, namespace });
      delete value.documentId;
      if (rect.xRaw >= this.current.widthRaw || rect.yRaw >= this.current.heightRaw) {
        for (const key of ["logicalRect", "sourceRange", "svg", "report", "metrics"]) value[key] = null;
      } else {
        value.logicalRect.widthRaw = Math.min(rect.widthRaw, this.current.widthRaw - rect.xRaw);
        value.logicalRect.heightRaw = Math.min(rect.heightRaw, this.current.heightRaw - rect.yRaw);
      }
      return this.tileHook ? await this.tileHook(value) : JSON.stringify(value);
    }
    releaseViewport(sheetIndex, id, revision) {
      this.calls.push(["release", sheetIndex, id, revision]);
      const found = this.current?.geometryId === id;
      if (found) this.current = null;
      return found;
    }
    mutate() {
      if (this.mutateHook) return this.mutateHook();
      this.current = null;
      this.staged = null;
      this.revision = (BigInt(this.revision) + 1n).toString();
      return JSON.stringify({ workbook: workbook(), editState: editState(writable) });
    }
    setCellJson() { return this.mutate(); }
    setCellRecalculateJson() { const value = JSON.parse(this.mutate()); value.recalculation = { computedCells: 0, unchangedCells: 0, unsupportedCells: 0, reasons: [] }; return JSON.stringify(value); }
    setRangeRecalculateJson() { return this.setCellRecalculateJson(); }
    setDocumentPropertiesJson() { return this.mutate(); }
    undoEditJson() { return this.mutate(); }
    redoEditJson() { return this.mutate(); }
    saveDocumentBytes() { return new Uint8Array([80, 75, 3, 4]); }
    free() { this.calls.push(["free"]); this.freed = true; this.current = null; this.staged = null; }
  }
  const wasm = { RenderSession: Session, capabilitiesJson: () => JSON.stringify(EXPECTED_CAPABILITIES) };
  if (viewportSupported) wasm.viewportCapabilitiesJson = () => JSON.stringify(capabilities());
  const runtime = new RenderWorkerRuntime({ wasm, send(message) {
    if (sendHook) sendHook(message, runtime);
    messages.push(message);
  } });
  async function result(requestId) {
    for (let i = 0; i < 1000; i += 1) {
      const found = messages.find((message) => message.type === "result" && message.requestId === requestId);
      if (found) return found;
      await sleep(2);
    }
    throw new Error(`No result for ${requestId}`);
  }
  function send(operation, payload, requestId = `req-${++sequence}`) {
    runtime.receive({ protocol: PROTOCOL, type: "request", requestId, operation, payload });
    return { requestId, response: result(requestId) };
  }
  async function call(operation, payload, requestId) { return (await send(operation, payload, requestId).response); }
  async function open(id = "doc", bytes = 1) {
    const result = await call("open", { documentId: id, bytes: new Uint8Array(bytes) });
    assert.equal(result.ok, true, JSON.stringify(result.error));
    return sessions.at(-1);
  }
  async function prepare(id = "doc") {
    const result = await call("prepare-viewport", { documentId: id, sheetIndex: 0 });
    assert.equal(result.ok, true, JSON.stringify(result.error));
    return result.result;
  }
  function identity(owner, documentId = "doc") {
    return { documentId, sheetIndex: owner.sheetIndex, geometryId: owner.geometryId, revision: owner.revision };
  }
  return { runtime, messages, sessions, result, send, call, open, prepare, identity };
}

test("canonical u64 boundaries and raw coordinates preserve integer transport", () => {
  for (const value of ["0", ABOVE_SAFE, U64_MAX]) assert.equal(validateViewportU64(value), value);
  for (const value of [0, 9007199254740992, "00", "01", "-1", "1e3", "18446744073709551616", "123456789012345678901"]) {
    assert.throws(() => validateViewportU64(value), { code: "invalid_viewport" });
  }
  assert.equal(validateViewportGeometryId(ID.replace(/-1$/, `-${U64_MAX}`)).length, 60);
  for (const id of ["vp-1", ID.replace(/-1$/, "-01"), ID.replace(/-1$/, "-18446744073709551616")]) assert.throws(() => validateViewportGeometryId(id));
  const large = { xRaw: 5_000_000_000, yRaw: 0, widthRaw: 2048, heightRaw: 1024 };
  assert.deepEqual(validateViewportRect(large), large);
  assert.throws(() => validateViewportRect({ ...large, xRaw: 16_384_000_000 }));
});

test("new payload validation rejects accessors, symbols and unknown options before reads", () => {
  let reads = 0;
  const getter = { documentId: "doc", sheetIndex: 0 };
  Object.defineProperty(getter, "options", { enumerable: true, get() { reads += 1; return {}; } });
  assert.throws(() => preflightRequest({ operation: "prepare-viewport", payload: getter }), { code: "invalid_viewport" });
  assert.equal(reads, 0);
  for (const options of [null, [], { range: RANGE }, { range: null }, { omitSparsePages: null }, { singlePageSheets: null }, { print: {} }, { [Symbol("x")]: 1 }, { limits: null }, { limits: { maxUnknown: 1 } }]) {
    assert.throws(() => viewportOptionsJson(options), { code: "invalid_viewport" });
  }
  const limits = {};
  Object.defineProperty(limits, "maxRows", { enumerable: true, get() { reads += 1; return 1; } });
  assert.throws(() => viewportOptionsJson({ limits }), { code: "invalid_viewport" });
  assert.equal(reads, 0);
  assert.equal(viewportOptionsJson({ gridlines: true, limits: { maxSceneNodes: 100_000 } }), '{"gridlines":true,"limits":{"maxSceneNodes":100000}}');
  // These are lowerable effective resource/print limits, not unknown keys.
  assert.equal(viewportOptionsJson({ limits: { maxFontBytes: 1, maxImages: 1, maxLogicalPages: 1 } }), '{"limits":{"maxFontBytes":1,"maxImages":1,"maxLogicalPages":1}}');
});

test("empty Used has honest null report; all-hidden nonempty source retains actual report", () => {
  const empty = descriptor(ID, ABOVE_SAFE, { sourceRange: null, widthRaw: 0, heightRaw: 0, preparationReport: null });
  validateViewportDescriptor(empty, { sheetIndex: 0 });
  assert.throws(() => validateViewportDescriptor({ ...empty, preparationReport: report() }, { sheetIndex: 0 }));
  assert.throws(() => validateViewportDescriptor({ ...empty, widthRaw: 1 }, { sheetIndex: 0 }));
  validateViewportDescriptor(descriptor(ID, ABOVE_SAFE, { widthRaw: 0, heightRaw: 0, sheetVisibility: "veryHidden" }), { sheetIndex: 0 });
  assert.throws(() => validateViewportDescriptor(descriptor(ID, ABOVE_SAFE, { preparationReport: null }), { sheetIndex: 0 }));
});

test("tile schema rejects extra fields, unsafe output, size/count and dense-array violations", () => {
  const expected = { documentId: "doc", sheetIndex: 0, geometryId: ID, revision: ABOVE_SAFE, rect: RECT, namespace: U64_MAX };
  validateViewportTileResult(tileResult(), expected);
  const variants = [
    (v) => { v.extra = true; }, (v) => { v.revision = "9007199254740992"; },
    (v) => { v.logicalRect.widthRaw += 1; }, (v) => { v.sourceRange.lastCol += 1; },
    (v) => { v.metrics.haloCells += 1; }, (v) => { v.report.svg_bytes += 1; },
    (v) => { v.report.scene_nodes = 100_001; },
    (v) => { v.svg = '<svg><script>1</script></svg>'; },
    (v) => { v.svg = "x".repeat(2_097_153); },
    (v) => { v.report.warnings = new Array(1); },
    (v) => { v.report.font_faces = new Array(513).fill({}); },
    (v) => { v.logicalRect = null; }
  ];
  for (const alter of variants) { const v = tileResult(); alter(v); assert.throws(() => validateViewportTileResult(v, expected)); }
  const hidden = tileResult();
  Object.defineProperty(hidden.report, "sheet_name", { enumerable: true, get() { throw new Error("getter must not run"); } });
  assert.throws(() => validateViewportTileResult(hidden, expected), { code: "invalid_viewport" });
  assert.throws(() => validateViewportPrepareResult(prepareResult({ documentId: "other" }), { documentId: "doc", sheetIndex: 0 }));
  const inflated = capabilities(); inflated.limits.maxTileSvgBytes += 1;
  assert.throws(() => validateViewportCapabilities(inflated));
});

test("renderer halo metrics count complete extra border coordinates, not only halo corners", () => {
  const expected = { documentId: "doc", sheetIndex: 0, geometryId: ID, revision: ABOVE_SAFE, rect: RECT, namespace: U64_MAX };
  for (const [haloRows, haloColumns, haloCells] of [[1, 1, 3], [1, 0, 1], [0, 1, 1]]) {
    const value = tileResult();
    value.report.visible_rows = 1;
    value.report.visible_columns = 1;
    value.metrics = { coordinateVisits: 4, geometryBytes: 64, haloRows, haloColumns, haloCells };
    validateViewportTileResult(value, expected);
  }
  const tooLarge = tileResult();
  tooLarge.report.visible_rows = 4096;
  assert.throws(() => validateViewportTileResult(tooLarge, expected));
});

test("legacy capabilities stay exact and old WASM supports existing requests only", async () => {
  const f = fixture({ viewportSupported: false });
  const legacy = await f.call("capabilities", {});
  assert.deepEqual(legacy.result, EXPECTED_CAPABILITIES);
  const added = await f.call("viewport-capabilities", {});
  assert.equal(added.error.code, "wasm_api_mismatch");
  const newCaps = fixture();
  const result = await newCaps.call("viewport-capabilities", {});
  assert.deepEqual(result.result, { ...capabilities(), resourcePolicy: { ...VIEWPORT_RESOURCE_POLICY } });
});

test("published owner survives failed reprepare and ids differ across documents/attempts", async () => {
  const f = fixture(); const a = await f.open(); const old = await f.prepare();
  a.stageHook = () => { throw new Error("bounded preparation failure"); };
  const failed = await f.call("prepare-viewport", { documentId: "doc", sheetIndex: 0 });
  assert.equal(failed.ok, false); assert.equal(a.current.geometryId, old.geometryId); assert.equal(a.staged, null);
  a.stageHook = null; const replacement = await f.prepare();
  assert.notEqual(replacement.geometryId, old.geometryId);
  assert.equal(replacement.geometryId.endsWith("-3"), true);
  const b = await f.open("second"); const second = await f.prepare("second");
  assert.notEqual(second.geometryId, replacement.geometryId);
  const foreign = await f.call("render-viewport-tile", { ...f.identity(replacement, "second"), rect: RECT, namespace: U64_MAX });
  assert.equal(foreign.error.code, "viewport_not_prepared"); assert.equal(b.calls.some(([name]) => name === "tile"), false);
});

test("malformed stage/commit error aborts provisional and preserves current", async () => {
  const f = fixture(); const s = await f.open(); const old = await f.prepare();
  for (const hook of [() => "{", () => "x".repeat(65_537), (d) => JSON.stringify({ ...d, geometryId: ID }), (d) => JSON.stringify({ ...d, revision: "01" })]) {
    s.stageHook = hook;
    const bad = await f.call("prepare-viewport", { documentId: "doc", sheetIndex: 0 });
    assert.equal(bad.ok, false); assert.equal(s.current.geometryId, old.geometryId); assert.equal(s.staged, null);
  }
  s.stageHook = null;
  for (const hook of [() => false, () => Promise.resolve(true), () => { throw new Error("commit denied"); }]) {
    s.commitHook = hook;
    const bad = await f.call("prepare-viewport", { documentId: "doc", sheetIndex: 0 });
    assert.equal(bad.ok, false); assert.equal(s.current.geometryId, old.geometryId); assert.equal(s.staged, null);
  }
});

test("raw cancellation after staging aborts before publication", async () => {
  const f = fixture(); const s = await f.open(); const old = await f.prepare();
  s.stageHook = (d) => {
    setTimeout(() => f.runtime.receive({ protocol: PROTOCOL, type: "cancel", requestId: "cancel-stage" }), 0);
    return JSON.stringify(d);
  };
  const bad = await f.call("prepare-viewport", { documentId: "doc", sheetIndex: 0 }, "cancel-stage");
  assert.equal(bad.error.code, "cancelled"); assert.equal(s.current.geometryId, old.geometryId); assert.equal(s.staged, null);
});

test("closeAll during delayed native stage has no abort on freed session/double credit", async () => {
  const f = fixture(); const s = await f.open();
  let finish; const gate = new Promise((resolve) => { finish = resolve; });
  let staged; s.stageHook = (d) => { staged = d; return gate; };
  const request = f.send("prepare-viewport", { documentId: "doc", sheetIndex: 0 });
  while (!staged) await sleep(2);
  f.runtime.closeAll(); finish(JSON.stringify(staged));
  assert.equal((await request.response).error.code, "cancelled");
  assert.equal(s.freed, true); assert.equal(s.calls.filter(([name]) => name === "free").length, 1);
  assert.equal(s.calls.some(([name]) => name === "abort"), false);
  await f.open("fresh"); await f.prepare("fresh");
});

test("tile transports >2^32 coordinates and >2^53 u64 strings without coercion", async () => {
  const f = fixture(); const s = await f.open();
  s.stageHook = (d) => {
    s.staged = { ...d, widthRaw: 5_000_004_096, heightRaw: 2048, revision: U64_MAX };
    return JSON.stringify(s.staged);
  };
  const owner = await f.prepare();
  const rect = { xRaw: 5_000_000_000, yRaw: 0, widthRaw: 8192, heightRaw: 2048 };
  const result = await f.call("render-viewport-tile", { ...f.identity(owner), rect, namespace: ABOVE_SAFE });
  assert.equal(result.ok, true, JSON.stringify(result.error));
  assert.deepEqual(JSON.parse(s.calls.at(-1)[4]), rect);
  assert.equal(s.calls.at(-1)[3], U64_MAX); assert.equal(s.calls.at(-1)[5], ABOVE_SAFE);
  assert.equal(result.result.logicalRect.xRaw, 5_000_000_000);
  assert.equal(result.result.logicalRect.widthRaw, 4096); // stub coverage validation, not a renderer measurement
  s.tileHook = (v) => JSON.stringify({ ...v, logicalRect: { ...v.logicalRect, widthRaw: 1 } });
  assert.equal((await f.call("render-viewport-tile", { ...f.identity(owner), rect, namespace: ABOVE_SAFE })).error.code, "wasm_api_mismatch");
});

test("empty/outside tile uses all-null paint fields and rejects false coverage", async () => {
  const f = fixture(); const s = await f.open();
  s.stageHook = (d) => {
    s.staged = { ...d, sourceRange: null, widthRaw: 0, heightRaw: 0, preparationReport: null };
    return JSON.stringify(s.staged);
  };
  const owner = await f.prepare();
  const payload = { ...f.identity(owner), rect: RECT, namespace: "0" };
  const result = await f.call("render-viewport-tile", payload);
  assert.equal(result.ok, true); assert.equal(result.result.logicalRect, null);
  s.tileHook = () => { const v = tileResult(payload); delete v.documentId; return JSON.stringify(v); };
  assert.equal((await f.call("render-viewport-tile", payload)).error.code, "wasm_api_mismatch");
});

test("release is idempotent, wrong live identity errors and source index charge remains", async () => {
  const f = fixture(); const s = await f.open(); const owner = await f.prepare();
  const wrong = await f.call("release-viewport", { ...f.identity(owner), revision: "1" });
  assert.equal(wrong.error.code, "viewport_not_prepared"); assert.equal(s.current.geometryId, owner.geometryId);
  const released = await f.call("release-viewport", f.identity(owner)); assert.equal(released.result.released, true);
  const again = await f.call("release-viewport", f.identity(owner)); assert.equal(again.result.released, false);
  assert.equal(s.calls.filter(([name]) => name === "release").length, 1);
});

test("failed/repeated stages do not leak geometry, but retain warmed source-index budget", async () => {
  const f = fixture();
  await f.open("a", 30 * 1024 * 1024); await f.open("b", 30 * 1024 * 1024); await f.open("c", 30 * 1024 * 1024);
  const a = f.sessions[0];
  a.stageHook = () => { throw new Error("fails after possible index warmup"); };
  for (let i = 0; i < 8; i += 1) assert.equal((await f.call("prepare-viewport", { documentId: "a", sheetIndex: 0 })).ok, false);
  a.stageHook = null; const ownerA = await f.prepare("a"); const ownerB = await f.prepare("b");
  assert.equal((await f.call("prepare-viewport", { documentId: "c", sheetIndex: 0 })).error.resource, "openResourceBytes");
  await f.call("release-viewport", f.identity(ownerA, "a"));
  // 90 MiB inputs + retained a index8 + b index/geometry16 + c candidate16 =130 MiB.
  assert.equal((await f.call("prepare-viewport", { documentId: "c", sheetIndex: 0 })).error.resource, "openResourceBytes");
  await f.call("close", { documentId: "a" });
  const ownerC = await f.prepare("c"); assert.notEqual(ownerC.geometryId, ownerB.geometryId);
});

test("queued workbook bytes are included before stage reservation/native call", async () => {
  let queued = false;
  const f = fixture({ sendHook(message, runtime) {
    if (!queued && message.type === "progress" && message.requestId === "budget-prepare" && message.completed === 1) {
      queued = true;
      runtime.receive({ protocol: PROTOCOL, type: "request", requestId: "queued-open", operation: "open",
        payload: { documentId: "large-queued", bytes: new Uint8Array(32 * 1024 * 1024) } });
    }
  } });
  await f.open("a", 30 * 1024 * 1024); await f.open("b", 30 * 1024 * 1024); await f.open("c", 30 * 1024 * 1024);
  const result = await f.call("prepare-viewport", { documentId: "a", sheetIndex: 0 }, "budget-prepare");
  assert.equal(result.error.resource, "pendingResourceBytes"); assert.equal(f.sessions[0].calls.some(([name]) => name === "stage"), false);
  assert.equal((await f.result("queued-open")).ok, true);
});

test("abort adapter failure retains reservation/blocks stages until close", async () => {
  const f = fixture(); const s = await f.open(); await f.prepare();
  s.stageHook = () => "{"; s.abortHook = () => { throw new Error("unconfirmed cleanup"); };
  assert.equal((await f.call("prepare-viewport", { documentId: "doc", sheetIndex: 0 })).ok, false);
  const blocked = await f.call("prepare-viewport", { documentId: "doc", sheetIndex: 0 });
  assert.equal(blocked.error.code, "viewport_stage_exists");
  await f.call("close", { documentId: "doc" }); await f.open(); await f.prepare();
});

test("authoritative edit invalidates before bad JSON; rejected mutation preserves owner", async () => {
  const f = fixture({ writable: true }); const s = await f.open(); const owner = await f.prepare();
  s.mutateHook = () => { throw new Error("native atomic rejection"); };
  const edit = { documentId: "doc", sheetIndex: 0, row: 0, col: 0, value: { kind: "number", value: 2 } };
  assert.equal((await f.call("set-cell", edit)).ok, false); assert.equal(s.current.geometryId, owner.geometryId);
  assert.equal((await f.call("render-viewport-tile", { ...f.identity(owner), rect: RECT, namespace: "0" })).ok, true);
  s.mutateHook = () => { s.current = null; s.staged = null; s.revision = "9007199254740994"; return "{"; };
  assert.equal((await f.call("set-cell", edit)).ok, false);
  assert.equal((await f.call("render-viewport-tile", { ...f.identity(owner), rect: RECT, namespace: "0" })).error.code, "viewport_not_prepared");
  s.mutateHook = null; const replacement = await f.prepare(); assert.equal(replacement.revision, "9007199254740994");
});

test("all authoritative mutation operations clear old prepared handle; save preserves it", async () => {
  const f = fixture({ writable: true }); await f.open();
  const operations = [
    ["set-cell", { sheetIndex: 0, row: 0, col: 0, value: { kind: "number", value: 1 } }],
    ["set-cell-recalculate", { sheetIndex: 0, row: 0, col: 0, value: { kind: "number", value: 1 } }],
    ["set-range-recalculate", { sheetIndex: 0, startRow: 0, startCol: 0, values: [[{ kind: "number", value: 1 }]] }],
    ["set-document-properties", workbook().properties && { properties: { ...workbook().properties } }],
    ["undo-edit", {}], ["redo-edit", {}]
  ];
  for (const [operation, rest] of operations) {
    const owner = await f.prepare(); const result = await f.call(operation, { documentId: "doc", ...rest });
    assert.equal(result.ok, true, JSON.stringify(result.error));
    assert.equal((await f.call("render-viewport-tile", { ...f.identity(owner), rect: RECT, namespace: "0" })).error.code, "viewport_not_prepared");
  }
  const owner = await f.prepare(); assert.equal((await f.call("save-document", { documentId: "doc" })).ok, true);
  assert.equal((await f.call("render-viewport-tile", { ...f.identity(owner), rect: RECT, namespace: "0" })).ok, true);
});

test("post-commit output send failure never aborts the published native owner", async () => {
  let throwOnce = true;
  const f = fixture({ sendHook(message) {
    if (throwOnce && message.type === "result" && message.requestId === "send-failure" && message.ok) {
      throwOnce = false; throw new Error("transport rejected committed result");
    }
  } });
  const s = await f.open();
  const result = await f.call("prepare-viewport", { documentId: "doc", sheetIndex: 0 }, "send-failure");
  assert.equal(result.ok, false); assert.notEqual(s.current, null); assert.equal(s.staged, null);
  assert.equal(s.calls.some(([name]) => name === "abort"), false);
  const owner = s.current;
  assert.equal((await f.call("render-viewport-tile", { ...f.identity(owner), rect: RECT, namespace: "0" })).ok, true);
});

class WorkerDouble {
  listeners = { message: [], error: [], messageerror: [] };
  sent = [];
  terminated = false;
  addEventListener(type, fn) { this.listeners[type].push(fn); }
  postMessage(message) { this.sent.push(message); }
  emit(message) { for (const fn of this.listeners.message) fn({ data: message }); }
  terminate() { this.terminated = true; }
  ready() { this.emit({ protocol: PROTOCOL, type: "ready", capabilities: structuredClone(EXPECTED_CAPABILITIES) }); }
  result(request, value) { this.emit({ protocol: PROTOCOL, type: "result", requestId: request.requestId, ok: true, result: value, error: null }); }
}

test("client dispatched prepare/release retain original settlement; queued prepare is cancellable", async () => {
  const worker = new WorkerDouble(); const client = new RenderWorkerClient(worker);
  const queued = client.prepareViewport("doc", 0); const rejected = assert.rejects(queued, { name: "AbortError" });
  assert.equal(client.cancel(queued.requestId), true); await rejected; assert.equal(worker.sent.length, 0);
  worker.ready(); const active = client.prepareViewport("doc", 0); const request = worker.sent.at(-1);
  assert.equal(client.cancel(active.requestId), false); worker.result(request, prepareResult());
  const owner = await active;
  const release = client.releaseViewport("doc", 0, owner.geometryId, owner.revision);
  assert.equal(client.cancel(release.requestId), false);
  worker.result(worker.sent.at(-1), { schemaVersion: 1, documentId: "doc", sheetIndex: 0,
    geometryId: ID, revision: ABOVE_SAFE, released: true }); assert.equal((await release).released, true);
  client.terminate();
});

test("client snapshots new identity/rectangle and rejects malicious response identity", async () => {
  const worker = new WorkerDouble(); const client = new RenderWorkerClient(worker); worker.ready();
  const rect = { ...RECT }; const pending = client.renderViewportTile("doc", 0, ID, ABOVE_SAFE, rect, U64_MAX);
  const request = worker.sent.at(-1); rect.xRaw = 999;
  assert.equal(request.payload.rect.xRaw, 0);
  worker.result(request, tileResult()); assert.equal((await pending).requestedRect.xRaw, 0);
  const bad = client.prepareViewport("doc", 0); const rejected = assert.rejects(bad, { code: "worker_message_error" });
  worker.result(worker.sent.at(-1), prepareResult({ documentId: "other" })); await rejected;
  assert.equal(worker.terminated, true);
});

test("client refuses viewport additions to old ready/tile schemas", async () => {
  const worker = new WorkerDouble(); const client = new RenderWorkerClient(worker);
  const pending = client.capabilities(); const rejected = assert.rejects(pending, { code: "worker_message_error" });
  const polluted = structuredClone(EXPECTED_CAPABILITIES); polluted.viewport = capabilities();
  worker.emit({ protocol: PROTOCOL, type: "ready", capabilities: polluted }); await rejected;
  const worker2 = new WorkerDouble(); const client2 = new RenderWorkerClient(worker2); worker2.ready();
  const old = client2.renderTile("doc", 0, RANGE); const oldRejected = assert.rejects(old, { code: "worker_message_error" });
  worker2.result(worker2.sent.at(-1), { documentId: "doc", sheetIndex: 0, range: RANGE,
    mimeType: "image/svg+xml", svg: SVG, geometryId: ID }); await oldRejected;
});
