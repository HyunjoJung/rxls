"use strict";

const fs = require("node:fs");
const path = require("node:path");

const MIME = {
  xls: "application/vnd.ms-excel",
  xlsx: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
  xlsm: "application/vnd.ms-excel.sheet.macroEnabled.12",
  xlsb: "application/vnd.ms-excel.sheet.binary.macroEnabled.12",
  ods: "application/vnd.oasis.opendocument.spreadsheet",
};

function prepareFixtures(repoRoot) {
  const generatedDir = path.join(repoRoot, "target/wasm-test-fixtures");
  fs.mkdirSync(generatedDir, { recursive: true });
  const xlsmPath = path.join(generatedDir, "macro-enabled.xlsm");
  const encoded = fs.readFileSync(
    path.join(repoRoot, "bindings/wasm/tests/fixtures/macro-enabled.xlsm.b64"),
    "utf8",
  );
  const xlsmBytes = Buffer.from(encoded.replace(/\s/g, ""), "base64");
  if (!fs.existsSync(xlsmPath) || !fs.readFileSync(xlsmPath).equals(xlsmBytes)) {
    fs.writeFileSync(xlsmPath, xlsmBytes);
  }

  return [
    {
      id: "xls",
      format: "xls",
      path: path.join(repoRoot, "tests/fixtures/xls/reader-basic.xls"),
      mimeType: MIME.xls,
    },
    {
      id: "xlsx",
      format: "xlsx",
      path: path.join(repoRoot, "tests/fixtures/xlsx/reader-structural.xlsx"),
      mimeType: MIME.xlsx,
    },
    {
      id: "xlsm",
      // The byte adapter has no filename and therefore reports the OOXML
      // parser family (`xlsx`); package inventory still proves VBA presence.
      format: "xlsx",
      nativeFormat: "xlsm",
      path: xlsmPath,
      mimeType: MIME.xlsm,
    },
    {
      id: "xlsb",
      format: "xlsb",
      path: path.join(repoRoot, "tests/fixtures/xlsb/reader-basic.xlsb"),
      mimeType: MIME.xlsb,
    },
    {
      id: "ods",
      format: "ods",
      path: path.join(repoRoot, "tests/fixtures/ods/repeated-hidden.ods"),
      mimeType: MIME.ods,
    },
  ];
}

function adapterOutputs(api, bytes) {
  return {
    text: api.extractText(bytes),
    csv: api.toCsv(bytes, 0),
    html: api.toHtml(bytes, 0),
    report: JSON.parse(api.reportJson(bytes)),
  };
}

function reportSemantics(report, fixture) {
  const normalized = structuredClone(report);
  if (fixture.id === "xlsm") delete normalized.format;
  return normalized;
}

module.exports = {
  adapterOutputs,
  prepareFixtures,
  reportSemantics,
};
