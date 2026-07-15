import assert from "node:assert/strict";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "../../..");
const require = createRequire(import.meta.url);
const { adapterOutputs, prepareFixtures } = require("./fixture-matrix.cjs");
const playwrightPath = process.env.RXLS_PLAYWRIGHT_PATH
  || path.join(repoRoot, "target/playwright/node_modules/playwright");
const { chromium } = require(playwrightPath);
const packageDir = path.resolve(process.argv[2] || path.join(repoRoot, "target/wasm-package"));
const nativeReportPath = process.argv[3] && path.resolve(process.argv[3]);
const fixtures = prepareFixtures(repoRoot);
const nodeApi = require(packageDir);
const expected = Object.fromEntries(
  fixtures.map((fixture) => [
    fixture.id,
    adapterOutputs(nodeApi, fs.readFileSync(fixture.path)),
  ]),
);

const fixtureRoutes = new Map(
  fixtures.map((fixture) => [`/fixtures/${fixture.id}`, fixture]),
);
const fixtureManifest = fixtures.map(({ id, format }) => ({ id, format }));
const smokeHtml = `<!doctype html>
<meta charset="utf-8">
<title>rxls-wasm browser smoke</title>
<script type="module">
  import init, {
    extractText,
    maxInputBytes,
    reportJson,
    toCsv,
    toHtml,
  } from "/package/web/rxls_wasm.js";

  const capture = (call) => {
    try {
      call();
      return null;
    } catch (error) {
      return {
        isError: error instanceof Error,
        name: error?.name,
        kind: error?.kind,
        location: error?.location,
        cause: error?.cause ?? null,
      };
    }
  };

  try {
    await init();
    const outputs = {};
    const bytesById = {};
    for (const fixture of ${JSON.stringify(fixtureManifest)}) {
      const response = await fetch(\`/fixtures/\${fixture.id}\`);
      if (!response.ok) throw new Error(\`fixture fetch failed: \${fixture.id}\`);
      const bytes = new Uint8Array(await response.arrayBuffer());
      bytesById[fixture.id] = bytes;
      outputs[fixture.id] = {
        text: extractText(bytes),
        csv: toCsv(bytes, 0),
        html: toHtml(bytes, 0),
        report: JSON.parse(reportJson(bytes)),
      };
    }

    const malformed = new TextEncoder().encode("not a spreadsheet");
    const malformedErrors = {
      extractText: capture(() => extractText(malformed)),
      toCsv: capture(() => toCsv(malformed, 0)),
      toHtml: capture(() => toHtml(malformed, 0)),
      reportJson: capture(() => reportJson(malformed)),
    };
    const rangeErrors = {
      toCsv: capture(() => toCsv(bytesById.xlsx, 999)),
      toHtml: capture(() => toHtml(bytesById.xlsx, 999)),
    };
    const overLimit = new Uint8Array(maxInputBytes() + 1);
    const limitErrors = {
      extractText: capture(() => extractText(overLimit)),
      toCsv: capture(() => toCsv(overLimit, 0)),
      toHtml: capture(() => toHtml(overLimit, 0)),
      reportJson: capture(() => reportJson(overLimit)),
    };
    window.__rxls = {
      outputs,
      maxInputBytes: maxInputBytes(),
      malformedErrors,
      rangeErrors,
      limitErrors,
    };
  } catch (error) {
    window.__rxlsFailure = String(error?.stack ?? error);
  }
</script>`;

const contentTypes = new Map([
  [".html", "text/html; charset=utf-8"],
  [".js", "text/javascript; charset=utf-8"],
  [".json", "application/json; charset=utf-8"],
  [".wasm", "application/wasm"],
]);

function serveFile(response, filename, contentType) {
  response.writeHead(200, {
    "cache-control": "no-store",
    "content-type": contentType || contentTypes.get(path.extname(filename)) || "application/octet-stream",
  });
  fs.createReadStream(filename).pipe(response);
}

const server = http.createServer((request, response) => {
  const pathname = new URL(request.url, "http://127.0.0.1").pathname;
  if (pathname === "/smoke/") {
    response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
    response.end(smokeHtml);
    return;
  }
  const fixture = fixtureRoutes.get(pathname);
  if (fixture) {
    serveFile(response, fixture.path, fixture.mimeType);
    return;
  }
  if (pathname.startsWith("/package/")) {
    const relative = decodeURIComponent(pathname.slice("/package/".length));
    const filename = path.resolve(packageDir, relative);
    if (
      filename.startsWith(`${packageDir}${path.sep}`)
      && fs.existsSync(filename)
      && fs.statSync(filename).isFile()
    ) {
      serveFile(response, filename);
      return;
    }
  }
  response.writeHead(404).end();
});

await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const origin = `http://127.0.0.1:${server.address().port}`;
const launchOptions = { headless: true };
if (process.env.RXLS_CHROMIUM_EXECUTABLE) {
  launchOptions.executablePath = process.env.RXLS_CHROMIUM_EXECUTABLE;
}
const browser = await chromium.launch(launchOptions);

function assertErrorContract(error, kind, location) {
  assert.deepEqual(error, {
    isError: true,
    name: "RxlsError",
    kind,
    location,
    cause: null,
  });
}

try {
  const apiPage = await browser.newPage();
  const apiPageErrors = [];
  apiPage.on("pageerror", (error) => apiPageErrors.push(String(error)));
  await apiPage.goto(`${origin}/smoke/`);
  await apiPage.waitForFunction(() => window.__rxls || window.__rxlsFailure);
  const failure = await apiPage.evaluate(() => window.__rxlsFailure);
  assert.equal(failure, undefined);
  assert.deepEqual(apiPageErrors, []);
  const result = await apiPage.evaluate(() => window.__rxls);
  assert.equal(result.maxInputBytes, 32 * 1024 * 1024);
  assert.deepEqual(result.outputs, expected, "browser and CommonJS package outputs differ");
  for (const error of Object.values(result.malformedErrors)) {
    assertErrorContract(error, "not_ole2", "container");
  }
  for (const error of Object.values(result.rangeErrors)) {
    assertErrorContract(error, "sheet_out_of_range", "sheet_index");
  }
  for (const error of Object.values(result.limitErrors)) {
    assertErrorContract(error, "input_too_large", "input");
  }
  if (nativeReportPath) {
    const nativeReport = JSON.parse(fs.readFileSync(nativeReportPath, "utf8"));
    assert.deepEqual(result.outputs.xlsx.report, nativeReport, "native and browser WASM reports differ");
  }

  const demoPage = await browser.newPage();
  const demoPageErrors = [];
  demoPage.on("pageerror", (error) => demoPageErrors.push(String(error)));
  await demoPage.goto(`${origin}/package/demo/index.html`);
  await demoPage.waitForFunction(() => document.querySelector("#status")?.textContent.startsWith("Ready."));
  assert.equal(
    await demoPage.locator("#status").textContent(),
    "Ready. Maximum input: 32 MiB.",
  );
  assert.equal(await demoPage.locator("#output").textContent(), "");
  for (const selector of ["#show-report", "#show-text", "#show-csv", "#show-html"]) {
    assert.equal(await demoPage.locator(selector).isDisabled(), true);
  }

  for (const fixture of fixtures) {
    await demoPage.locator("#file").setInputFiles(fixture.path);
    const filename = path.basename(fixture.path);
    await demoPage.waitForFunction(
      (status) => document.querySelector("#status")?.textContent === status,
      `Parsed ${filename}.`,
    );
    assert.deepEqual(
      JSON.parse(await demoPage.locator("#output").textContent()),
      expected[fixture.id].report,
      `${fixture.id} demo report differs`,
    );
    for (const selector of ["#show-report", "#show-text", "#show-csv", "#show-html"]) {
      assert.equal(await demoPage.locator(selector).isEnabled(), true);
    }
  }

  const xlsx = fixtures.find((fixture) => fixture.id === "xlsx");
  await demoPage.locator("#file").setInputFiles(xlsx.path);
  await demoPage.locator("#show-report").click();
  assert.equal(await demoPage.locator("#status").textContent(), `Showing report for ${path.basename(xlsx.path)}.`);
  assert.deepEqual(JSON.parse(await demoPage.locator("#output").textContent()), expected.xlsx.report);
  for (const [selector, label, output] of [
    ["#show-text", "text", expected.xlsx.text],
    ["#show-csv", "CSV", expected.xlsx.csv],
    ["#show-html", "HTML", expected.xlsx.html],
  ]) {
    await demoPage.locator(selector).click();
    assert.equal(
      await demoPage.locator("#status").textContent(),
      `Exported ${label} from ${path.basename(xlsx.path)}.`,
    );
    assert.equal(await demoPage.locator("#output").textContent(), output);
  }

  await demoPage.locator("#file").setInputFiles({
    name: "malformed.xlsx",
    mimeType: fixtures.find((fixture) => fixture.id === "xlsx").mimeType,
    buffer: Buffer.from("not a spreadsheet"),
  });
  await demoPage.waitForFunction(
    () => document.querySelector("#status")?.textContent.startsWith("Failed:"),
  );
  assert.match(
    await demoPage.locator("#status").textContent(),
    /^Failed: not_ole2 at container:/,
  );
  assert.equal(await demoPage.locator("#output").textContent(), "");
  for (const selector of ["#show-report", "#show-text", "#show-csv", "#show-html"]) {
    assert.equal(await demoPage.locator(selector).isDisabled(), true);
  }

  await demoPage.locator("#file").setInputFiles({
    name: "oversized.xlsx",
    mimeType: xlsx.mimeType,
    buffer: Buffer.alloc(32 * 1024 * 1024 + 1),
  });
  await demoPage.waitForFunction(
    () => document.querySelector("#status")?.textContent.startsWith("Rejected:"),
  );
  assert.equal(
    await demoPage.locator("#status").textContent(),
    "Rejected: oversized.xlsx exceeds the input limit.",
  );
  assert.equal(await demoPage.locator("#output").textContent(), "");
  for (const selector of ["#show-report", "#show-text", "#show-csv", "#show-html"]) {
    assert.equal(await demoPage.locator(selector).isDisabled(), true);
  }
  assert.deepEqual(demoPageErrors, []);

  process.stdout.write(`${JSON.stringify({
    runtime: "browser",
    fixtures: fixtures.map((fixture) => fixture.id),
    exports: ["extractText", "maxInputBytes", "reportJson", "toCsv", "toHtml"],
    parity: "commonjs",
    demo: ["ready", "success", "report", "text", "csv", "html", "malformed", "limit"],
  })}\n`);
} finally {
  await browser.close();
  await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
}
