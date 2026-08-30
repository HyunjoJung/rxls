import { createHash } from "node:crypto";
import { cp, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { PRESERVATION_FIXTURE } from "./preservation-fixture.mjs";

const viewerRoot = fileURLToPath(new URL("..", import.meta.url));
const repositoryRoot = path.resolve(viewerRoot, "..");
const publicRoot = path.join(viewerRoot, "public");
const runtimeRoot = path.join(publicRoot, "runtime");
const sampleRoot = path.join(publicRoot, "samples");
const wasmPackage = path.join(repositoryRoot, "bindings", "render-wasm", "pkg");
const projectLicense = path.join(repositoryRoot, "LICENSE");
const workerNotices = path.join(repositoryRoot, "bindings", "render-wasm", "THIRD_PARTY_NOTICES.txt");
const viewerNotices = path.join(viewerRoot, "THIRD_PARTY_NOTICES.txt");

const samples = [
  {
    id: "operations-report",
    label: "Operations report",
    format: "XLSX",
    file: "operations-report.xlsx",
    source: path.join(viewerRoot, "samples", "operations-report.xlsx")
  },
  {
    id: "legacy-korean",
    label: "Legacy Korean",
    format: "XLS",
    file: "legacy-korean.xls",
    source: path.join(repositoryRoot, "tests", "fixtures", "xls", "korean-cp949-biff5.xls")
  },
  {
    id: "binary-workbook",
    label: "Binary workbook",
    format: "XLSB",
    file: "binary-workbook.xlsb",
    source: path.join(repositoryRoot, "tests", "fixtures", "xlsb", "reader-basic.xlsb")
  },
  {
    id: "open-document",
    label: "OpenDocument",
    format: "ODS",
    file: "open-document.ods",
    source: path.join(repositoryRoot, "tests", "fixtures", "ods", "repeated-hidden.ods")
  },
  {
    id: "macro-preservation",
    label: "Macro preservation",
    format: "XLSM",
    file: PRESERVATION_FIXTURE.outputFile,
    source: path.join(viewerRoot, "samples", PRESERVATION_FIXTURE.sourceFile),
    bytes: PRESERVATION_FIXTURE.bytes,
    sha256: PRESERVATION_FIXTURE.sha256
  }
];

await requireFile(path.join(wasmPackage, "rxls_render_wasm.js"));
await requireFile(path.join(wasmPackage, "rxls_render_wasm_bg.wasm"));
await requireFile(projectLicense);
await requireFile(workerNotices);
await requireFile(viewerNotices);

await rm(runtimeRoot, { recursive: true, force: true });
await rm(sampleRoot, { recursive: true, force: true });
await mkdir(path.join(runtimeRoot, "js"), { recursive: true });
await mkdir(path.join(runtimeRoot, "pkg"), { recursive: true });
await mkdir(sampleRoot, { recursive: true });

await cp(path.join(repositoryRoot, "bindings", "render-wasm", "js"), path.join(runtimeRoot, "js"), {
  recursive: true
});
await cp(wasmPackage, path.join(runtimeRoot, "pkg"), { recursive: true });
await cp(projectLicense, path.join(publicRoot, "LICENSE.txt"));

const combinedNotices = [
  "RXLS VIEWER THIRD-PARTY NOTICES",
  "",
  "Viewer UI dependencies",
  "======================",
  await readFile(viewerNotices, "utf8"),
  "",
  "Rust and WebAssembly renderer dependencies",
  "==========================================",
  await readFile(workerNotices, "utf8")
].join("\n");
await writeFile(path.join(publicRoot, "THIRD_PARTY_NOTICES.txt"), combinedNotices, "utf8");

const manifestRows = [];
for (const sample of samples) {
  await requireFile(sample.source);
  const destination = path.join(sampleRoot, sample.file);
  if (sample.sha256) {
    const payload = await readFile(sample.source);
    const digest = createHash("sha256").update(payload).digest("hex");
    if (payload.byteLength !== sample.bytes || digest !== sample.sha256) {
      throw new Error(
        `viewer sample identity mismatch: ${path.relative(repositoryRoot, sample.source)}`
      );
    }
  }
  await cp(sample.source, destination);
  const info = await stat(destination);
  manifestRows.push({
    id: sample.id,
    label: sample.label,
    format: sample.format,
    file: sample.file,
    name: sample.file,
    bytes: info.size
  });
}

const manifest = `${JSON.stringify({ schemaVersion: 1, samples: manifestRows }, null, 2)}\n`;
await writeFile(path.join(sampleRoot, "manifest.json"), manifest, "utf8");

const packageJson = JSON.parse(
  await readFile(path.join(repositoryRoot, "bindings", "render-wasm", "package.json"), "utf8")
);
console.log(
  `prepared ${packageJson.name}@${packageJson.version} and ${manifestRows.length} samples`
);

async function requireFile(file) {
  const info = await stat(file).catch(() => null);
  if (!info?.isFile()) {
    throw new Error(`required viewer input is missing: ${path.relative(repositoryRoot, file)}`);
  }
}
