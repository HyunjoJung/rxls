import test from "node:test";
import assert from "node:assert/strict";
import {
  acceptsWorkbook,
  clampZoom,
  createLatestRequestGate,
  describeError,
  editableCell,
  editReasonLabel,
  extensionOf,
  fitZoom,
  formatBytes,
  formatLabel,
  parseCellReference,
  sameCellTarget,
  savedWorkbookName,
  safeBaseName
} from "../src/core.js";

test("accepts the documented workbook extensions case-insensitively", () => {
  for (const name of ["legacy.XLS", "book.xlsx", "macro.xlsm", "binary.xlsb", "open.ods"]) {
    assert.equal(acceptsWorkbook(name), true, name);
  }
  assert.equal(acceptsWorkbook("notes.csv"), false);
  assert.equal(acceptsWorkbook("xlsx"), false);
});

test("formats metadata deterministically", () => {
  assert.equal(extensionOf("quarter.final.XLSB"), "xlsb");
  assert.equal(formatLabel("quarter.final.XLSB"), "XLSB");
  assert.equal(formatBytes(0), "0 B");
  assert.equal(formatBytes(1024), "1.00 KB");
  assert.equal(formatBytes(12 * 1024), "12.0 KB");
});

test("creates path-neutral export names", () => {
  assert.equal(safeBaseName("../Quarter report.xlsx"), "Quarter-report");
  assert.equal(safeBaseName("한글.xlsx"), "workbook");
  assert.equal(safeBaseName("safe_name-1.xls"), "safe_name-1");
});

test("parses bounded A1 references", () => {
  assert.deepEqual(parseCellReference(" $b$12 "), { row: 11, col: 1, normalized: "B12" });
  assert.deepEqual(parseCellReference("XFD1048576"), {
    row: 1_048_575,
    col: 16_383,
    normalized: "XFD1048576"
  });
  for (const invalid of ["A0", "XFE1", "A1048577", "1A", "", "A-1"]) {
    assert.throws(() => parseCellReference(invalid), RangeError, invalid);
  }
});

test("builds strict typed cell edits", () => {
  assert.deepEqual(editableCell("blank"), { kind: "blank" });
  assert.deepEqual(editableCell("number", "42.5"), { kind: "number", value: 42.5 });
  assert.deepEqual(editableCell("boolean", "false"), { kind: "boolean", value: false });
  assert.deepEqual(
    editableCell("formula", "=SUM(A1:A2)", {
      cachedKind: "number",
      cachedValue: "3"
    }),
    {
      kind: "formula",
      formula: "SUM(A1:A2)",
      cached: { kind: "number", value: 3 }
    }
  );
  assert.throws(() => editableCell("number", "not-a-number"), TypeError);
  assert.throws(() => editableCell("formula", "="), TypeError);
  assert.throws(() => editableCell("formula", "A1", { cachedKind: "blank" }), TypeError);
});

test("preserves macro-enabled save-as extensions and describes read-only reasons", () => {
  assert.equal(savedWorkbookName("Quarter.xlsm"), "Quarter-edited.xlsm");
  assert.equal(savedWorkbookName("Quarter.xlsx"), "Quarter-edited.xlsx");
  assert.equal(savedWorkbookName("legacy.xls"), "legacy-edited.xlsx");
  assert.match(editReasonLabel("legacy-biff"), /XLS/);
  assert.match(editReasonLabel("unknown"), /read-only/);
});

test("accepts only the latest delayed request", async () => {
  const gate = createLatestRequestGate();
  const accepted = [];
  const settle = async (name, token, delay) => {
    await new Promise((resolve) => setTimeout(resolve, delay));
    if (gate.isCurrent(token)) {
      accepted.push(name);
    }
  };

  const slowToken = gate.begin();
  const slow = settle("slow", slowToken, 20);
  const fastToken = gate.begin();
  const fast = settle("fast", fastToken, 0);
  await Promise.all([slow, fast]);

  assert.deepEqual(accepted, ["fast"]);
  gate.invalidate();
  assert.equal(gate.isCurrent(fastToken), false);
});

test("matches loaded cells by client, document, sheet, and coordinate", () => {
  const client = {};
  const target = { client, documentId: "one", sheetIndex: 0, row: 3, col: 4 };
  assert.equal(sameCellTarget(target, { ...target }), true);
  assert.equal(sameCellTarget(target, { ...target, client: {} }), false);
  assert.equal(sameCellTarget(target, { ...target, documentId: "two" }), false);
  assert.equal(sameCellTarget(target, { ...target, sheetIndex: 1 }), false);
  assert.equal(sameCellTarget(target, { ...target, row: 4 }), false);
  assert.equal(sameCellTarget(target, { ...target, col: 5 }), false);
  assert.equal(sameCellTarget(null, target), false);
});

test("clamps explicit and fitted zoom", () => {
  assert.equal(clampZoom(0), 0.25);
  assert.equal(clampZoom(8), 3);
  assert.equal(fitZoom(1048, 1000, 48), 0.98);
  assert.equal(fitZoom(Number.NaN, 1000), 1);
});

test("maps stable worker failures to user-facing messages", () => {
  assert.match(describeError({ code: "parse_failed" }), /could not be read/);
  assert.match(describeError({ code: "limit_exceeded" }), /safety limit/);
  assert.equal(describeError(new Error("specific failure")), "specific failure");
});
