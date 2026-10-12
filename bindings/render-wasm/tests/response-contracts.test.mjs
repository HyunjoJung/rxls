import test from "node:test";
import assert from "node:assert/strict";
import { createResponseValidators } from "../js/response-contracts.mjs";

const LIMITS = Object.freeze({
  maxSheets: 255,
  maxOutputBytes: 16 * 1024 * 1024,
  maxHistoryEntries: 20,
  maxHistoryBytes: 32 * 1024 * 1024
});

function boundary(code, location, limits = LIMITS) {
  return createResponseValidators({
    ...limits,
    invalid: (message) => Object.assign(new Error(message), { code, location })
  });
}

const boundaries = [
  ["client", "worker_message_error", "worker"],
  ["wasm", "wasm_api_mismatch", "wasm"]
];

function properties() {
  return {
    title: null, subject: null, creator: null, keywords: null,
    description: null, lastModifiedBy: null, company: null, created: null
  };
}

function workbook() {
  return {
    schemaVersion: 1,
    sheetCount: 1,
    sheets: [{ index: 0, name: "Sheet1", embeddedImages: 0 }],
    embeddedImages: 0,
    embeddedImageBytes: 0,
    fontPackSha256: null,
    fontFaces: 0,
    properties: properties()
  };
}

function editState(overrides = {}) {
  return {
    schemaVersion: 1, capability: "read-write", reason: null,
    dirty: false, canUndo: false, canRedo: false,
    undoDepth: 0, redoDepth: 0, historyBytes: 0, editedParts: [],
    ...overrides
  };
}

const invalidWorkbooks = [
  ["missing field", (v) => { delete v.fontFaces; }],
  ["extra field", (v) => { v.extra = true; }],
  ["schema", (v) => { v.schemaVersion = 2; }],
  ["negative count", (v) => { v.sheetCount = -1; }],
  ["fractional count", (v) => { v.sheetCount = 0.5; }],
  ["sheet limit", (v) => { v.sheetCount = 256; }],
  ["mismatched count", (v) => { v.sheetCount = 0; }],
  ["wrong sheet index", (v) => { v.sheets[0].index = 1; }],
  ["empty sheet name", (v) => { v.sheets[0].name = ""; }],
  ["sheet extra field", (v) => { v.sheets[0].extra = 1; }],
  ["missing sheet name", (v) => { delete v.sheets[0].name; }],
  ["negative image count", (v) => { v.sheets[0].embeddedImages = -1; }],
  ["image total", (v) => { v.embeddedImages = 1; }],
  ["image bytes", (v) => { v.embeddedImageBytes = Infinity; }],
  ["invalid font SHA", (v) => { v.fontPackSha256 = "g".repeat(64); }],
  ["unsafe number", (v) => { v.fontFaces = Number.MAX_SAFE_INTEGER + 1; }],
  ["NaN", (v) => { v.fontFaces = NaN; }],
  ["invalid properties", (v) => { v.properties.title = 7; }],
  ["missing property", (v) => { delete v.properties.company; }],
  ["extra property", (v) => { v.properties.extra = true; }]
];

const invalidEdits = [
  ["missing field", (v) => { delete v.dirty; }],
  ["extra field", (v) => { v.extra = true; }],
  ["schema", (v) => { v.schemaVersion = 2; }],
  ["unknown capability", (v) => { v.capability = "editable"; }],
  ["unknown reason", (v) => { v.reason = "unknown"; }],
  ["read-write reason", (v) => { v.reason = "legacy-biff"; }],
  ["read-only no reason", (v) => { v.capability = "read-only"; }],
  ["dirty type", (v) => { v.dirty = 0; }],
  ["undo boolean", (v) => { v.canUndo = 0; }],
  ["redo boolean", (v) => { v.canRedo = 0; }],
  ["negative undo", (v) => { v.undoDepth = -1; }],
  ["fractional redo", (v) => { v.redoDepth = 0.5; }],
  ["unsafe history", (v) => { v.historyBytes = Infinity; }],
  ["history byte limit", (v) => { v.historyBytes = LIMITS.maxHistoryBytes + 1; }],
  ["undo contradiction", (v) => { v.canUndo = true; }],
  ["redo contradiction", (v) => { v.canRedo = true; }],
  ["history without depth", (v) => { v.historyBytes = 1; }],
  ["depth without history", (v) => { v.undoDepth = 1; v.canUndo = true; }],
  ["clean with edited parts", (v) => { v.editedParts = ["xl/workbook.xml"]; }],
  ["read-only dirty", (v) => {
    v.capability = "read-only"; v.reason = "legacy-biff"; v.dirty = true;
  }],
  ["read-only history", (v) => {
    v.capability = "read-only"; v.reason = "legacy-biff";
    v.undoDepth = 1; v.canUndo = true; v.historyBytes = 1;
  }],
  ["combined depth limit", (v) => {
    v.undoDepth = 11; v.redoDepth = 10; v.canUndo = v.canRedo = true;
    v.historyBytes = 1;
  }]
];

const invalidCells = [
  ["blank extra key", { kind: "blank", value: null }],
  ["text wrong type", { kind: "text", value: 1 }],
  ["error wrong type", { kind: "error", value: null }],
  ["number missing value", { kind: "number" }],
  ["number string", { kind: "number", value: "1" }],
  ["NaN", { kind: "number", value: NaN }],
  ["Infinity", { kind: "date", value: Infinity }],
  ["negative Infinity", { kind: "number", value: -Infinity }],
  ["boolean wrong type", { kind: "boolean", value: 1 }],
  ["unknown kind", { kind: "formula-auto", formula: "A1" }],
  ["formula wrong type", { kind: "formula", formula: 1, cached: { kind: "blank" } }],
  ["formula missing cache", { kind: "formula", formula: "A1" }],
  ["invalid cache", { kind: "formula", formula: "A1", cached: null }]
];

for (const [name, code, location] of boundaries) {
  const api = boundary(code, location);
  const error = { code, location };

  test(`${name}: valid contracts return the original objects without mutation`, () => {
    const objects = [
      ["validateWorkbookInspection", workbook()],
      ["validateDocumentProperties", properties()],
      ["validateEditState", editState()],
      ["validateInspectedCell", { kind: "formula", formula: "A1*2", cached: { kind: "number", value: 4 } }]
    ];
    for (const [method, value] of objects) {
      const before = structuredClone(value);
      assert.equal(api[method](value), value);
      assert.deepEqual(value, before);
      assert.equal(api[method](Object.assign(Object.create(null), value)).constructor, undefined);
    }
    assert.ok(Object.isFrozen(api));
  });

  for (const [label, mutate] of invalidWorkbooks) {
    test(`${name}: workbook rejects ${label}`, () => {
      const value = workbook();
      mutate(value);
      assert.throws(() => api.validateWorkbookInspection(value), error);
    });
  }
  for (const [label, mutate] of invalidEdits) {
    test(`${name}: edit state rejects ${label}`, () => {
      const value = editState();
      mutate(value);
      assert.throws(() => api.validateEditState(value), error);
    });
  }
  for (const [label, value] of invalidCells) {
    test(`${name}: cell rejects ${label}`, () => {
      assert.throws(() => api.validateInspectedCell(value), error);
    });
  }
  for (const value of [null, [], true, 1, "value", new Date(), Object.create({ kind: "blank" })]) {
    test(`${name}: rejects non-plain input ${String(value)}`, () => {
      for (const validate of Object.values(api)) assert.throws(() => validate(value), error);
    });
  }
  test(`${name}: supports every inspected scalar kind and read-only reason`, () => {
    for (const value of [
      { kind: "blank" }, { kind: "text", value: "" }, { kind: "error", value: "#REF!" },
      { kind: "number", value: -4.5 }, { kind: "date", value: 0 }, { kind: "boolean", value: false }
    ]) assert.equal(api.validateInspectedCell(value), value);
    for (const reason of ["legacy-biff", "binary-package", "open-document", "package-metadata-loss"]) {
      const value = editState({ capability: "read-only", reason });
      assert.equal(api.validateEditState(value), value);
    }
  });
  test(`${name}: permits exact history limits but rejects unsafe edited parts`, () => {
    const value = editState({
      dirty: true, canUndo: true, undoDepth: 20,
      historyBytes: LIMITS.maxHistoryBytes, editedParts: ["docProps/core.xml", "xl/workbook.xml"]
    });
    assert.equal(api.validateEditState(value), value);
    for (const path of ["", "/root", "a\\b", "a//b", "a/./b", "a/../b", "a\0b", "x".repeat(513)]) {
      assert.throws(() => api.validateEditState({ ...value, editedParts: [path] }), error);
    }
    for (const parts of [["b", "a"], ["a", "a"], Array.from({ length: 264 }, (_, i) => String(i).padStart(3, "0"))]) {
      assert.throws(() => api.validateEditState({ ...value, editedParts: parts }), error);
    }
  });
  test(`${name}: recursive formula caches are bounded`, () => {
    let cell = { kind: "blank" };
    for (let i = 0; i < 16; i++) cell = { kind: "formula", formula: "A1", cached: cell };
    assert.equal(api.validateInspectedCell(cell), cell);
    assert.throws(() => api.validateInspectedCell({ kind: "formula", formula: "A1", cached: cell }), error);
    const cyclic = { kind: "formula", formula: "A1" };
    cyclic.cached = cyclic;
    assert.throws(() => api.validateInspectedCell(cyclic), error);
  });
}

test("boundaries keep their configured limits independent", () => {
  const client = boundary("worker_message_error", "worker");
  const wasm = boundary("wasm_api_mismatch", "wasm", {
    ...LIMITS, maxHistoryEntries: 2, maxHistoryBytes: 100, maxSheets: 1, maxOutputBytes: 6
  });
  const value = editState({ dirty: true, canUndo: true, undoDepth: 2, historyBytes: 100 });
  assert.equal(wasm.validateEditState(value), value);
  for (const larger of [{ ...value, undoDepth: 3 }, { ...value, historyBytes: 101 }]) {
    assert.equal(client.validateEditState(larger), larger);
    assert.throws(() => wasm.validateEditState(larger), { code: "wasm_api_mismatch", location: "wasm" });
  }
  const book = workbook();
  book.sheets[0].name = "가나"; // Six UTF-8 bytes, not two.
  assert.equal(wasm.validateWorkbookInspection(book), book);
  book.sheets[0].name += "a";
  assert.equal(client.validateWorkbookInspection(book), book);
  assert.throws(() => wasm.validateWorkbookInspection(book), { code: "wasm_api_mismatch" });
  book.sheets = [{ index: 0, name: "A", embeddedImages: 0 }, { index: 1, name: "B", embeddedImages: 0 }];
  book.sheetCount = 2;
  assert.equal(client.validateWorkbookInspection(book), book);
  assert.throws(() => wasm.validateWorkbookInspection(book), { code: "wasm_api_mismatch" });
});

test("factory rejects invalid limit configuration and missing error factories", () => {
  for (const name of Object.keys(LIMITS)) {
    for (const value of [undefined, null, 0, -1, 0.5, Infinity, NaN, Number.MAX_SAFE_INTEGER + 1]) {
      assert.throws(() => boundary("test", "test", { ...LIMITS, [name]: value }), TypeError);
    }
  }
  assert.throws(() => createResponseValidators({ ...LIMITS }), TypeError);
});
