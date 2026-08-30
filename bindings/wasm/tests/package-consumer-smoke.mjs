import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const TYPESCRIPT_VERSION = "5.9.3";

function run(command, args, options = {}) {
  return execFileSync(command, args, {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    ...options,
  });
}

function runNpm(args, options = {}) {
  const configuredCli = process.env.npm_execpath;
  if (configuredCli && fs.existsSync(configuredCli)) {
    return run(process.execPath, [configuredCli, ...args], options);
  }

  if (process.platform === "win32") {
    const bundledCli = path.join(
      path.dirname(process.execPath),
      "node_modules",
      "npm",
      "bin",
      "npm-cli.js",
    );
    assert.ok(fs.existsSync(bundledCli), `npm CLI not found at ${bundledCli}`);
    return run(process.execPath, [bundledCli, ...args], options);
  }

  return run("npm", args, options);
}

function writeConsumerFiles(consumerDir) {
  const common = `
const assert = require("node:assert/strict");
const fs = require("node:fs");
const api = require("rxls-wasm");
const bytes = fs.readFileSync(process.env.RXLS_WASM_FIXTURE);
assert.equal(api.maxInputBytes(), 32 * 1024 * 1024);
assert.equal(api.maxExportOutputBytes(), 16 * 1024 * 1024);
assert.ok(api.extractText(bytes).length > 0);
assert.equal(typeof api.toCsv(bytes, 0), "string");
assert.match(api.toHtml(bytes, 0), /^<table>/);
assert.equal(typeof api.toMarkdown(bytes, 0), "string");
assert.equal(JSON.parse(api.reportJson(bytes)).schema_version, 2);
`;
  fs.writeFileSync(path.join(consumerDir, "consumer.cjs"), common);

  const esm = `
import assert from "node:assert/strict";
import fs from "node:fs";
import { extractText, maxExportOutputBytes, maxInputBytes, reportJson, toCsv, toHtml, toMarkdown } from "rxls-wasm";
const bytes = fs.readFileSync(process.env.RXLS_WASM_FIXTURE);
assert.equal(maxInputBytes(), 32 * 1024 * 1024);
assert.equal(maxExportOutputBytes(), 16 * 1024 * 1024);
assert.ok(extractText(bytes).length > 0);
assert.equal(typeof toCsv(bytes, 0), "string");
assert.match(toHtml(bytes, 0), /^<table>/);
assert.equal(typeof toMarkdown(bytes, 0), "string");
assert.equal(JSON.parse(reportJson(bytes)).schema_version, 2);
`;
  fs.writeFileSync(path.join(consumerDir, "consumer.mjs"), esm);

  const browser = `
import assert from "node:assert/strict";
import fs from "node:fs";
import init, { extractText, maxExportOutputBytes, maxInputBytes, reportJson, toCsv, toHtml, toMarkdown } from "rxls-wasm";
const moduleUrl = import.meta.resolve("rxls-wasm");
const wasmBytes = fs.readFileSync(new URL("./rxls_wasm_bg.wasm", moduleUrl));
await init({ module_or_path: wasmBytes });
const bytes = fs.readFileSync(process.env.RXLS_WASM_FIXTURE);
assert.equal(maxInputBytes(), 32 * 1024 * 1024);
assert.equal(maxExportOutputBytes(), 16 * 1024 * 1024);
assert.ok(extractText(bytes).length > 0);
assert.equal(typeof toCsv(bytes, 0), "string");
assert.match(toHtml(bytes, 0), /^<table>/);
assert.equal(typeof toMarkdown(bytes, 0), "string");
assert.equal(JSON.parse(reportJson(bytes)).schema_version, 2);
`;
  fs.writeFileSync(path.join(consumerDir, "browser-consumer.mjs"), browser);

  const nodeTypescript = `
import {
  extractText,
  maxExportOutputBytes,
  maxInputBytes,
  reportJson,
  toCsv,
  toHtml,
  toMarkdown,
  type RxlsErrorObject,
} from "rxls-wasm";

declare const process: { env: Record<string, string | undefined> };
const ensure = (condition: unknown, message: string): void => {
  if (!condition) throw new Error(message);
};
const bytes = Uint8Array.from(JSON.parse(process.env.RXLS_WASM_BYTES ?? "[]") as number[]);
const text: string = extractText(bytes);
const csv: string = toCsv(bytes, 0);
const html: string = toHtml(bytes, 0);
const markdown: string = toMarkdown(bytes, 0);
const report = JSON.parse(reportJson(bytes)) as { schema_version: number };
const limit: number = maxInputBytes();
const outputLimit: number = maxExportOutputBytes();
const typedError = (error: unknown): RxlsErrorObject | null =>
  error instanceof Error && error.name === "RxlsError"
    ? error as RxlsErrorObject
    : null;
ensure(text.length > 0, "text export is empty");
ensure(typeof csv === "string", "CSV export is not text");
ensure(/^<table>/.test(html), "HTML export is not a table");
ensure(typeof markdown === "string", "Markdown export is not text");
ensure(report.schema_version === 2, "report schema mismatch");
ensure(limit === 32 * 1024 * 1024, "input limit mismatch");
ensure(outputLimit === 16 * 1024 * 1024, "output limit mismatch");
void typedError;
`;
  fs.writeFileSync(path.join(consumerDir, "node-consumer.mts"), nodeTypescript);

  const browserTypescript = `
import init, {
  extractText,
  maxExportOutputBytes,
  maxInputBytes,
  reportJson,
  toCsv,
  toHtml,
  toMarkdown,
  type InitOutput,
  type RxlsErrorObject,
} from "rxls-wasm";

async function initialize(bytes: Uint8Array): Promise<InitOutput> {
  const initialized = await init();
  void [
    extractText(bytes),
    maxExportOutputBytes(),
    maxInputBytes(),
    reportJson(bytes),
    toCsv(bytes, 0),
    toHtml(bytes, 0),
    toMarkdown(bytes, 0),
  ];
  return initialized;
}
const typedError = (error: unknown): RxlsErrorObject | null =>
  error instanceof Error && error.name === "RxlsError"
    ? error as RxlsErrorObject
    : null;
void [initialize, typedError];
`;
  fs.writeFileSync(path.join(consumerDir, "browser-consumer.mts"), browserTypescript);
  fs.writeFileSync(
    path.join(consumerDir, "tsconfig.node.json"),
    JSON.stringify({
      compilerOptions: {
        lib: ["ES2022"],
        module: "NodeNext",
        moduleResolution: "NodeNext",
        outDir: "dist",
        strict: true,
        target: "ES2022",
      },
      files: ["node-consumer.mts"],
    }),
  );
  fs.writeFileSync(
    path.join(consumerDir, "tsconfig.browser.json"),
    JSON.stringify({
      compilerOptions: {
        customConditions: ["browser"],
        lib: ["ES2022", "DOM"],
        module: "ESNext",
        moduleResolution: "Bundler",
        noEmit: true,
        strict: true,
        target: "ES2022",
      },
      files: ["browser-consumer.mts"],
    }),
  );
}

export function verifyPackedConsumer(packageDir, fixturePaths) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rxls-wasm-consumer-"));
  try {
    const archiveDir = path.join(root, "archive");
    const consumerDir = path.join(root, "consumer");
    fs.mkdirSync(archiveDir);
    fs.mkdirSync(consumerDir);
    const archiveName = runNpm([
      "pack",
      packageDir,
      "--pack-destination",
      archiveDir,
      "--silent",
    ]).trim();
    assert.match(archiveName, /^rxls-wasm-\d+\.\d+\.\d+\.tgz$/);
    const archive = path.join(archiveDir, archiveName);

    runNpm(["init", "--yes"], { cwd: consumerDir });
    runNpm(
      [
        "install",
        "--ignore-scripts",
        "--no-audit",
        "--no-fund",
        archive,
        `typescript@${TYPESCRIPT_VERSION}`,
      ],
      { cwd: consumerDir },
    );
    writeConsumerFiles(consumerDir);
    const tsc = path.join(
      consumerDir,
      "node_modules",
      "typescript",
      "bin",
      "tsc",
    );
    run(process.execPath, [tsc, "--project", "tsconfig.node.json"], {
      cwd: consumerDir,
    });
    run(process.execPath, [tsc, "--project", "tsconfig.browser.json"], {
      cwd: consumerDir,
    });
    for (const fixturePath of fixturePaths) {
      const env = {
        ...process.env,
        RXLS_WASM_BYTES: JSON.stringify([...fs.readFileSync(fixturePath)]),
        RXLS_WASM_FIXTURE: fixturePath,
      };
      run(process.execPath, ["consumer.cjs"], { cwd: consumerDir, env });
      run(process.execPath, ["consumer.mjs"], { cwd: consumerDir, env });
      run(process.execPath, ["--conditions=browser", "browser-consumer.mjs"], {
        cwd: consumerDir,
        env,
      });
      run(process.execPath, ["dist/node-consumer.mjs"], { cwd: consumerDir, env });
    }

    return {
      archive: archiveName,
      commonjs: true,
      esm: true,
      browserCondition: true,
      browserTypes: true,
      fixtures: fixturePaths.length,
      typescript: { version: TYPESCRIPT_VERSION, nodeExecuted: true },
    };
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

const here = path.dirname(fileURLToPath(import.meta.url));
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const packageDir = path.resolve(process.argv[2]);
  const fixturePaths = process.argv.slice(3).map((fixture) => path.resolve(fixture));
  assert.ok(fixturePaths.length > 0, "at least one fixture is required");
  process.stdout.write(`${JSON.stringify(verifyPackedConsumer(packageDir, fixturePaths))}\n`);
}
