"use strict";

const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const { execFileSync } = require("node:child_process");
const {
  adapterOutputs,
  prepareFixtures,
  reportSemantics,
} = require("./fixture-matrix.cjs");

const repoRoot = path.resolve(__dirname, "../../..");
const packageDir = path.resolve(process.argv[2] || path.join(repoRoot, "target/wasm-package"));
const nativeReportPath = process.argv[3] && path.resolve(process.argv[3]);
const fixtures = prepareFixtures(repoRoot);
const api = require(packageDir);

function sha256(value) {
  return crypto.createHash("sha256").update(value).digest("hex");
}

function collectNativeOutputs() {
  const outputDir = fs.mkdtempSync(path.join(repoRoot, "target/wasm-native-oracle-"));
  try {
    const args = [
      "run",
      "--quiet",
      "--locked",
      "--manifest-path",
      path.join(repoRoot, "bindings/wasm/Cargo.toml"),
      "--example",
      "native-fixture-outputs",
      "--",
      outputDir,
    ];
    for (const fixture of fixtures) {
      args.push(
        fixture.id,
        fixture.nativeFormat || fixture.format,
        fixture.path,
      );
    }
    execFileSync("cargo", args, { stdio: ["ignore", "pipe", "pipe"] });
    return Object.fromEntries(fixtures.map((fixture) => [
      fixture.id,
      {
        text: fs.readFileSync(path.join(outputDir, `${fixture.id}.text`), "utf8"),
        csv: fs.readFileSync(path.join(outputDir, `${fixture.id}.csv`), "utf8"),
        html: fs.readFileSync(path.join(outputDir, `${fixture.id}.html`), "utf8"),
        report: JSON.parse(
          fs.readFileSync(path.join(outputDir, `${fixture.id}.report.json`), "utf8"),
        ),
      },
    ]));
  } finally {
    fs.rmSync(outputDir, { recursive: true, force: true });
  }
}

assert.equal(api.maxInputBytes(), 32 * 1024 * 1024);
const nativeOutputs = collectNativeOutputs();

const matrix = {};
for (const fixture of fixtures) {
  const bytes = fs.readFileSync(fixture.path);
  const output = adapterOutputs(api, bytes);
  assert.ok(output.text.length > 0, `${fixture.id} text export is empty`);
  assert.equal(typeof output.csv, "string", `${fixture.id} CSV export is not text`);
  assert.match(output.html, /^<table>/, `${fixture.id} HTML export is not a table`);
  assert.equal(output.report.schema_version, 1);
  assert.equal(output.report.format, fixture.format);
  assert.ok(output.report.stats.sheets > 0);
  if (fixture.id === "xlsm") {
    assert.equal(output.report.features.vba_project, true);
    assert.ok(output.report.warnings.includes("MacrosPresentNotExecuted"));
  }

  const native = nativeOutputs[fixture.id];
  assert.equal(output.text, native.text, `${fixture.id} native and WASM text differ`);
  assert.equal(output.csv, native.csv, `${fixture.id} native and WASM CSV differ`);
  assert.equal(output.html, native.html, `${fixture.id} native and WASM HTML differ`);
  assert.deepEqual(
    reportSemantics(output.report, fixture),
    reportSemantics(native.report, fixture),
    `${fixture.id} native and WASM reports differ`,
  );

  matrix[fixture.id] = {
    report: output.report,
    text_sha256: sha256(output.text),
    csv_sha256: sha256(output.csv),
    html_sha256: sha256(output.html),
  };
}

if (nativeReportPath) {
  const expected = JSON.parse(fs.readFileSync(nativeReportPath, "utf8"));
  assert.deepEqual(matrix.xlsx.report, expected, "provided native XLSX report differs");
}

const malformed = Buffer.from("not a spreadsheet");
for (const [name, call] of [
  ["extractText", () => api.extractText(malformed)],
  ["toCsv", () => api.toCsv(malformed, 0)],
  ["toHtml", () => api.toHtml(malformed, 0)],
  ["reportJson", () => api.reportJson(malformed)],
]) {
  assert.throws(call, (error) => {
    assert.ok(error instanceof Error, `${name} must throw Error`);
    assert.equal(error.name, "RxlsError");
    assert.equal(error.kind, "not_ole2");
    assert.equal(error.location, "container");
    assert.equal(error.cause, null);
    return true;
  });
}

const xlsxBytes = fs.readFileSync(fixtures.find((fixture) => fixture.id === "xlsx").path);
for (const call of [
  () => api.toCsv(xlsxBytes, 999),
  () => api.toHtml(xlsxBytes, 999),
]) {
  assert.throws(call, (error) => {
    assert.equal(error.name, "RxlsError");
    assert.equal(error.kind, "sheet_out_of_range");
    assert.equal(error.location, "sheet_index");
    return true;
  });
}

const limitProbe = new Uint8Array(api.maxInputBytes() + 1);
assert.throws(
  () => api.reportJson(limitProbe.subarray(0, api.maxInputBytes())),
  (error) => error.kind === "not_ole2",
  "the documented maximum must reach the parser",
);
for (const call of [
  () => api.extractText(limitProbe),
  () => api.toCsv(limitProbe, 0),
  () => api.toHtml(limitProbe, 0),
  () => api.reportJson(limitProbe),
]) {
  assert.throws(call, (error) => {
    assert.equal(error.name, "RxlsError");
    assert.equal(error.kind, "input_too_large");
    assert.equal(error.location, "input");
    assert.equal(error.cause, null);
    return true;
  });
}

const consumerScript = path.join(__dirname, "package-consumer-smoke.mjs");
const packedConsumer = JSON.parse(
  execFileSync(
    process.execPath,
    [consumerScript, packageDir, ...fixtures.map((fixture) => fixture.path)],
    { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
  ),
);

process.stdout.write(`${JSON.stringify({ runtime: "node", matrix, packedConsumer })}\n`);
