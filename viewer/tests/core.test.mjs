import test from "node:test";
import assert from "node:assert/strict";
import {
  acceptsWorkbook,
  clampZoom,
  describeError,
  extensionOf,
  fitZoom,
  formatBytes,
  formatLabel,
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
