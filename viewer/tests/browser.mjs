import { preview } from "vite";
import { chromium } from "playwright-core";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdir, readFile, stat } from "node:fs/promises";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { PRESERVATION_FIXTURE, PRESERVED_PARTS } from "../scripts/preservation-fixture.mjs";
import { readZipEntries } from "../scripts/zip.mjs";

const execFileAsync = promisify(execFile);
const port = Number(process.env.RXLS_VIEWER_PORT || 4173);
const basePath = await builtBasePath();
const server = await preview({
  base: basePath,
  preview: { host: "127.0.0.1", port, strictPort: true }
});

const launchOptions = { headless: true };
if (process.env.RXLS_CHROMIUM_EXECUTABLE) {
  launchOptions.executablePath = process.env.RXLS_CHROMIUM_EXECUTABLE;
} else {
  launchOptions.channel = "chrome";
}

const browser = await chromium.launch(launchOptions);
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  const pageErrors = [];
  const consoleErrors = [];
  const failedResponses = [];
  page.on("pageerror", (error) => pageErrors.push(error));
  page.on("console", (message) => {
    if (message.type() === "error") {
      consoleErrors.push(message.text());
    }
  });
  page.on("response", (response) => {
    if (response.status() >= 400) {
      failedResponses.push({ status: response.status(), url: response.url() });
    }
  });
  await page.goto(`http://127.0.0.1:${port}${basePath}`, { waitUntil: "domcontentloaded" });
  try {
    await page.locator("#document-surface svg").waitFor({ state: "visible", timeout: 30_000 });
  } catch (error) {
    const diagnostics = {
      url: page.url(),
      body: await page.locator("body").innerText(),
      banner: await page.locator("#error-message").textContent(),
      pageErrors: pageErrors.map(String),
      consoleErrors,
      failedResponses
    };
    throw new Error(`viewer did not render: ${JSON.stringify(diagnostics)}`, { cause: error });
  }

  const state = await page.evaluate(() => globalThis.__rxlsViewerState());
  if (
    !state.rendered ||
    state.busy ||
    state.format !== "xlsx" ||
    state.mode !== "sheet" ||
    state.sheetCount !== 2 ||
    state.editCapability !== "read-write" ||
    state.dirty
  ) {
    throw new Error(`unexpected initial viewer state: ${JSON.stringify(state)}`);
  }

  const delayedLegacyStarted = deferred();
  const releaseDelayedLegacy = deferred();
  const delayedLegacyFinished = deferred();
  const delayedLegacyRoute = async (route) => {
    delayedLegacyStarted.resolve();
    await releaseDelayedLegacy.promise;
    try {
      await route.continue();
    } catch (error) {
      if (!/aborted|closed|handled/i.test(String(error))) {
        throw error;
      }
    } finally {
      delayedLegacyFinished.resolve();
    }
  };
  await page.evaluate(() => {
    const nativeFetch = globalThis.fetch.bind(globalThis);
    globalThis.fetch = (resource, options = {}) => {
      const url = resource instanceof Request ? resource.url : String(resource);
      const requestOptions = url.endsWith("/samples/legacy-korean.xls")
        ? { ...options, signal: undefined }
        : options;
      return nativeFetch(resource, requestOptions);
    };
    globalThis.__restoreRxlsFetch = () => {
      globalThis.fetch = nativeFetch;
      delete globalThis.__restoreRxlsFetch;
    };
  });
  await page.route("**/samples/legacy-korean.xls", delayedLegacyRoute);
  await page.locator("#sample-select").selectOption("legacy-korean");
  await delayedLegacyStarted.promise;
  await page.locator("#sample-select").selectOption("binary-workbook");
  releaseDelayedLegacy.resolve();
  await waitForViewerState(
    page,
    (snapshot) => snapshot.format === "xlsb" && snapshot.rendered && !snapshot.busy,
    "latest sample request"
  );
  await delayedLegacyFinished.promise;
  assert.equal((await page.evaluate(() => globalThis.__rxlsViewerState())).format, "xlsb");
  assert.equal(await page.locator("#error-banner").isHidden(), true);
  await page.unroute("**/samples/legacy-korean.xls", delayedLegacyRoute);
  await page.evaluate(() => globalThis.__restoreRxlsFetch());
  await page.locator("#sample-select").selectOption("operations-report");
  await waitForViewerState(
    page,
    (snapshot) =>
      snapshot.format === "xlsx" &&
      snapshot.editCapability === "read-write" &&
      snapshot.rendered &&
      !snapshot.busy,
    "workbook after delayed request"
  );

  await page.evaluate(async () => {
    const runtimeUrl = new URL("runtime/js/client.mjs", document.baseURI).href;
    const { RenderWorkerClient } = await import(runtimeUrl);
    const originalOpen = RenderWorkerClient.prototype.open;
    let delayNextOpen = true;
    globalThis.__rxlsOpenDelay = { started: false, finished: false };
    RenderWorkerClient.prototype.open = async function (...args) {
      const delayed = delayNextOpen;
      if (delayed) {
        delayNextOpen = false;
        globalThis.__rxlsOpenDelay.started = true;
        await new Promise((resolve) => setTimeout(resolve, 350));
      }
      try {
        return await originalOpen.apply(this, args);
      } finally {
        if (delayed) {
          globalThis.__rxlsOpenDelay.finished = true;
        }
      }
    };
    globalThis.__restoreRxlsOpen = () => {
      RenderWorkerClient.prototype.open = originalOpen;
      delete globalThis.__restoreRxlsOpen;
    };
  });
  await page.locator("#sample-select").selectOption("legacy-korean");
  await waitForCondition(
    () => page.evaluate(() => globalThis.__rxlsOpenDelay.started),
    "delayed worker open start"
  );
  await page.locator("#sample-select").selectOption("open-document");
  await waitForViewerState(
    page,
    (snapshot) => snapshot.format === "ods" && snapshot.rendered && !snapshot.busy,
    "latest worker open"
  );
  await waitForCondition(
    () => page.evaluate(() => globalThis.__rxlsOpenDelay.finished),
    "delayed worker open completion"
  );
  assert.equal((await page.evaluate(() => globalThis.__rxlsViewerState())).format, "ods");
  assert.equal(await page.locator("#error-banner").isHidden(), true);
  await page.evaluate(() => globalThis.__restoreRxlsOpen());
  await page.locator("#sample-select").selectOption("operations-report");
  await waitForViewerState(
    page,
    (snapshot) => snapshot.format === "xlsx" && snapshot.rendered && !snapshot.busy,
    "workbook after delayed worker open"
  );

  await page.locator('#sheet-list button[data-index="1"]').click();
  await waitForViewerState(
    page,
    (snapshot) => snapshot.sheetIndex === 1 && !snapshot.busy,
    "Metrics"
  );
  await page.locator('#sheet-list button[data-index="0"]').click();
  await waitForViewerState(
    page,
    (snapshot) => snapshot.sheetIndex === 0 && !snapshot.busy,
    "Operations"
  );

  await assertDownload(page, "#export-svg", ".svg");
  await assertDownload(page, "#export-png", ".png");

  await page.evaluate(async () => {
    const runtimeUrl = new URL("runtime/js/client.mjs", document.baseURI).href;
    const { RenderWorkerClient } = await import(runtimeUrl);
    const originalReadCell = RenderWorkerClient.prototype.readCell;
    let delayNextRead = true;
    let releaseDelayedRead;
    const delayedRead = new Promise((resolve) => {
      releaseDelayedRead = resolve;
    });
    globalThis.__rxlsCellReadDelay = { started: false, finished: false };
    RenderWorkerClient.prototype.readCell = async function (...args) {
      const delayed = delayNextRead;
      if (delayed) {
        delayNextRead = false;
        globalThis.__rxlsCellReadDelay.started = true;
        await delayedRead;
      }
      try {
        return await originalReadCell.apply(this, args);
      } finally {
        if (delayed) {
          globalThis.__rxlsCellReadDelay.finished = true;
        }
      }
    };
    globalThis.__releaseRxlsCellRead = releaseDelayedRead;
    globalThis.__restoreRxlsReadCell = () => {
      RenderWorkerClient.prototype.readCell = originalReadCell;
      delete globalThis.__releaseRxlsCellRead;
      delete globalThis.__restoreRxlsReadCell;
    };
  });
  await page.locator("#edit-cell").click();
  await page.locator("#cell-dialog").waitFor({ state: "visible" });
  await waitForCondition(
    () => page.evaluate(() => globalThis.__rxlsCellReadDelay.started),
    "delayed cell read start"
  );
  assert.equal(await page.locator("#apply-cell-edit").isDisabled(), true);
  assert.equal(await page.locator("#cell-value").inputValue(), "");
  await page.locator("#cell-reference").fill("B1");
  assert.equal(await page.locator("#apply-cell-edit").isDisabled(), true);
  assert.equal(await page.locator("#cell-value").inputValue(), "");
  assert.equal(await page.locator("#cell-formula").inputValue(), "");
  assert.equal(await page.locator("#cell-cached-value").inputValue(), "");
  await page.locator("#read-cell").click();
  await waitForCondition(
    async () =>
      (await page.locator("#cell-current-value").textContent()).startsWith("B1:") &&
      !(await page.locator("#apply-cell-edit").isDisabled()),
    "B1 cell read"
  );
  const b1Editor = await cellEditorSnapshot(page);
  await page.evaluate(() => globalThis.__releaseRxlsCellRead());
  await waitForCondition(
    () => page.evaluate(() => globalThis.__rxlsCellReadDelay.finished),
    "delayed cell read completion"
  );
  assert.deepEqual(await cellEditorSnapshot(page), b1Editor);
  assert.equal(await page.locator("#apply-cell-edit").isDisabled(), false);
  await page.evaluate(() => globalThis.__restoreRxlsReadCell());
  await page.locator("#cell-reference").fill("A1");
  assert.equal(await page.locator("#apply-cell-edit").isDisabled(), true);
  assert.equal(await page.locator("#cell-value").inputValue(), "");
  assert.equal(await page.locator("#cell-formula").inputValue(), "");
  assert.equal(await page.locator("#cell-cached-value").inputValue(), "");
  await page.locator("#read-cell").click();
  await waitForCondition(
    async () => !(await page.locator("#apply-cell-edit").isDisabled()),
    "A1 cell read"
  );
  if (process.env.RXLS_VIEWER_SCREENSHOTS) {
    await mkdir(new URL("../../target/viewer-e2e/", import.meta.url), { recursive: true });
    await page.screenshot({
      path: fileURLToPath(new URL("../../target/viewer-e2e/edit-dialog.png", import.meta.url)),
      fullPage: true
    });
  }
  await page.locator("#cell-kind").selectOption("text");
  await page.locator("#cell-value").fill("Browser edited XLSX");
  await page.locator("#apply-cell-edit").click();
  await waitForViewerState(
    page,
    (snapshot) => snapshot.dirty && snapshot.canUndo && !snapshot.busy,
    "XLSX cell edit"
  );
  assert.match(await page.locator("#document-surface").textContent(), /Browser edited XLSX/);

  await page.locator("#undo-edit").click();
  await waitForViewerState(
    page,
    (snapshot) => !snapshot.dirty && snapshot.canRedo && !snapshot.busy,
    "XLSX undo"
  );
  await page.locator("#redo-edit").click();
  await waitForViewerState(
    page,
    (snapshot) => snapshot.dirty && snapshot.canUndo && !snapshot.busy,
    "XLSX redo"
  );

  await page.locator("#document-properties").click();
  await page.locator("#properties-dialog").waitFor({ state: "visible" });
  await page.locator("#property-title").fill("rxls browser preservation proof");
  await page.locator('#properties-form button[type="submit"]').click();
  const xlsxEdited = await waitForViewerState(
    page,
    (snapshot) =>
      snapshot.dirty &&
      snapshot.editedParts.includes("xl/worksheets/sheet1.xml") &&
      snapshot.editedParts.includes("docProps/core.xml") &&
      !snapshot.busy,
    "XLSX document properties"
  );
  assert.equal(xlsxEdited.editCapability, "read-write");

  const xlsxDownload = await downloadWorkbook(page, ".xlsx");
  assert.equal(xlsxDownload.fileName, "operations-report-edited.xlsx");
  const xlsxSource = await readFile(new URL("../samples/operations-report.xlsx", import.meta.url));
  assertZipPartsEqual(xlsxSource, xlsxDownload.bytes, ["xl/styles.xml"]);
  assertZipPartChanged(xlsxSource, xlsxDownload.bytes, "xl/worksheets/sheet1.xml");

  await Promise.all([
    page.waitForEvent("dialog").then(async (prompt) => {
      assert.match(prompt.message(), /Discard unsaved workbook edits/);
      await prompt.dismiss();
    }),
    page.locator("#sample-select").selectOption("legacy-korean")
  ]);
  assert.equal(await page.locator("#sample-select").inputValue(), "operations-report");
  await waitForViewerState(
    page,
    (snapshot) => snapshot.format === "xlsx" && snapshot.dirty && !snapshot.busy,
    "cancelled sample switch"
  );

  await Promise.all([
    page.waitForEvent("dialog").then(async (prompt) => {
      assert.match(prompt.message(), /Discard unsaved workbook edits/);
      await prompt.accept();
    }),
    page.locator("#sample-select").selectOption("legacy-korean")
  ]);

  for (const sample of [
    {
      id: "legacy-korean",
      format: "xls",
      reason: "legacy-biff",
      message: "XLS is read-only in the browser"
    },
    {
      id: "binary-workbook",
      format: "xlsb",
      reason: "binary-package",
      message: "XLSB is read-only in the browser"
    },
    {
      id: "open-document",
      format: "ods",
      reason: "open-document",
      message: "ODS is read-only in the browser"
    }
  ]) {
    if (sample.id !== "legacy-korean") {
      await page.locator("#sample-select").selectOption(sample.id);
    }
    const readOnly = await waitForViewerState(
      page,
      (snapshot) =>
        snapshot.format === sample.format &&
        snapshot.editCapability === "read-only" &&
        snapshot.editReason === sample.reason &&
        snapshot.rendered &&
        !snapshot.busy,
      sample.id
    );
    assert.equal(readOnly.dirty, false);
    assert.equal(await page.locator("#edit-cell").isDisabled(), true);
    assert.equal(await page.locator("#save-document").isDisabled(), true);
    assert.equal(await page.locator("#editing-reason").isVisible(), true);
    assert.equal(await page.locator("#editing-reason").textContent(), sample.message);
    assert.equal(
      await page.locator("#edit-cell").getAttribute("aria-describedby"),
      "editing-reason"
    );
    assert.equal(
      await page.locator("#save-document").getAttribute("aria-describedby"),
      "editing-reason"
    );
    if (process.env.RXLS_VIEWER_SCREENSHOTS && sample.id === "legacy-korean") {
      await page.screenshot({
        path: fileURLToPath(new URL("../../target/viewer-e2e/read-only.png", import.meta.url)),
        fullPage: true
      });
    }
  }

  await page.locator("#sample-select").selectOption("macro-preservation");
  await waitForViewerState(
    page,
    (snapshot) =>
      snapshot.format === "xlsm" &&
      snapshot.editCapability === "read-write" &&
      snapshot.rendered &&
      !snapshot.busy,
    "macro preservation sample"
  );
  assert.equal(await page.locator("#editing-reason").isHidden(), true);
  assert.equal(await page.locator("#edit-cell").getAttribute("aria-describedby"), null);
  await page.locator("#edit-cell").click();
  await page.locator("#cell-reference").fill("A1");
  await page.locator("#read-cell").click();
  await page.locator("#cell-kind").selectOption("text");
  await page.locator("#cell-value").fill("VBA package preserved");
  await page.locator('#cell-form button[type="submit"]').click();
  await waitForViewerState(
    page,
    (snapshot) => snapshot.dirty && snapshot.canUndo && !snapshot.busy,
    "XLSM cell edit"
  );
  const xlsmDownload = await downloadWorkbook(page, ".xlsm");
  assert.equal(xlsmDownload.fileName, "macro-preservation-edited.xlsm");
  const xlsmSource = await readFile(
    new URL(`../samples/${PRESERVATION_FIXTURE.sourceFile}`, import.meta.url)
  );
  assertZipPartsEqual(xlsmSource, xlsmDownload.bytes, PRESERVED_PARTS);
  assertZipPartChanged(xlsmSource, xlsmDownload.bytes, "xl/worksheets/sheet1.xml");
  await assertOpenpyxlReopens(xlsmDownload.path, "VBA package preserved");
  await page.locator("#undo-edit").click();
  await waitForViewerState(
    page,
    (snapshot) => !snapshot.dirty && snapshot.canRedo && !snapshot.busy,
    "XLSM undo to clean source"
  );

  await page.locator("#file-input").setInputFiles(
    fileURLToPath(new URL("../samples/operations-report.xlsx", import.meta.url))
  );
  await waitForViewerState(
    page,
    (snapshot) => snapshot.source === "Local file" && snapshot.rendered && !snapshot.busy,
    "local workbook"
  );
  assert.equal(await page.locator("#sample-select").inputValue(), "");

  await page.locator("#page-view").click();
  await page.locator("#page-controls").waitFor({ state: "visible" });
  await page.locator("#document-surface svg").waitFor({ state: "visible" });
  const pageState = await page.evaluate(() => globalThis.__rxlsViewerState());
  if (pageState.mode !== "page" || pageState.pageIndex !== 0) {
    throw new Error(`page mode did not settle: ${JSON.stringify(pageState)}`);
  }

  if (process.env.RXLS_VIEWER_SCREENSHOTS) {
    await mkdir(new URL("../../target/viewer-e2e/", import.meta.url), { recursive: true });
    await page.screenshot({
      path: fileURLToPath(new URL("../../target/viewer-e2e/desktop.png", import.meta.url)),
      fullPage: true
    });
  }

  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator("#sidebar-toggle").click();
  await page.locator("body.sidebar-open #sidebar").waitFor({ state: "visible" });
  await page.waitForTimeout(250);
  const sidebarBox = await page.locator("#sidebar").boundingBox();
  if (!sidebarBox || sidebarBox.x < -1 || sidebarBox.width > 390) {
    throw new Error(`mobile sidebar is outside the viewport: ${JSON.stringify(sidebarBox)}`);
  }
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth);
  if (overflow) {
    throw new Error("viewer introduces horizontal page overflow on mobile");
  }
  if (process.env.RXLS_VIEWER_SCREENSHOTS) {
    await page.screenshot({
      path: fileURLToPath(new URL("../../target/viewer-e2e/mobile.png", import.meta.url)),
      fullPage: true
    });
  }
  const scrimX = Math.min(389, Math.ceil(sidebarBox.x + sidebarBox.width + 12));
  await page.mouse.click(scrimX, Math.floor(844 / 2));
  await waitForCondition(
    async () =>
      !(await page
        .locator("body")
        .evaluate((body) => body.classList.contains("sidebar-open"))),
    "mobile sidebar close"
  );
  await page.locator("#edit-cell").click();
  await page.locator("#cell-dialog").waitFor({ state: "visible" });
  const dialogBox = await page.locator("#cell-dialog").boundingBox();
  if (
    !dialogBox ||
    dialogBox.x < -1 ||
    dialogBox.y < -1 ||
    dialogBox.x + dialogBox.width > 391 ||
    dialogBox.y + dialogBox.height > 845
  ) {
    throw new Error(`mobile edit dialog is outside the viewport: ${JSON.stringify(dialogBox)}`);
  }
  if (process.env.RXLS_VIEWER_SCREENSHOTS) {
    await page.screenshot({
      path: fileURLToPath(
        new URL("../../target/viewer-e2e/mobile-edit-dialog.png", import.meta.url)
      ),
      fullPage: true
    });
  }
  await page.locator("#close-cell-dialog").click();
  if (!(await page.locator("#error-banner").isHidden())) {
    throw new Error(
      `viewer reported an error: ${(await page.locator("#error-banner").textContent())?.trim()}`
    );
  }
  if (pageErrors.length > 0) {
    throw pageErrors[0];
  }
  if (consoleErrors.length > 0) {
    throw new Error(`viewer logged console errors: ${JSON.stringify(consoleErrors)}`);
  }
  if (failedResponses.length > 0) {
    throw new Error(`viewer returned failed responses: ${JSON.stringify(failedResponses)}`);
  }
  console.log("viewer browser smoke passed");
} finally {
  await browser.close();
  await new Promise((resolve) => server.httpServer.close(resolve));
}

async function builtBasePath() {
  const html = await readFile(new URL("../dist/index.html", import.meta.url), "utf8");
  const match = html.match(/(?:src|href)="([^\"]*\/assets\/[^\"]+)"/);
  if (!match) {
    throw new Error("built viewer does not reference a versioned asset");
  }
  const pathname = new URL(match[1], "http://viewer.invalid/").pathname;
  return `${pathname.slice(0, pathname.indexOf("/assets/")) || ""}/`;
}

async function waitForViewerState(page, predicate, label) {
  const deadline = Date.now() + 30_000;
  let snapshot = null;
  while (Date.now() < deadline) {
    snapshot = await page.evaluate(() => globalThis.__rxlsViewerState());
    if (predicate(snapshot)) {
      return snapshot;
    }
    await page.waitForTimeout(50);
  }
  throw new Error(`viewer state did not settle for ${label}: ${JSON.stringify(snapshot)}`);
}

async function waitForCondition(predicate, label) {
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    if (await predicate()) {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`viewer condition did not settle for ${label}`);
}

async function cellEditorSnapshot(page) {
  return {
    status: await page.locator("#cell-current-value").textContent(),
    kind: await page.locator("#cell-kind").inputValue(),
    value: await page.locator("#cell-value").inputValue(),
    formula: await page.locator("#cell-formula").inputValue(),
    cachedKind: await page.locator("#cell-cached-kind").inputValue(),
    cachedValue: await page.locator("#cell-cached-value").inputValue()
  };
}

async function assertDownload(page, buttonSelector, extension) {
  await page.locator("#export-menu summary").click();
  const [download] = await Promise.all([
    page.waitForEvent("download"),
    page.locator(buttonSelector).click()
  ]);
  if (!download.suggestedFilename().endsWith(extension)) {
    throw new Error(`unexpected export name: ${download.suggestedFilename()}`);
  }
  const failure = await download.failure();
  const path = await download.path();
  const info = path ? await stat(path) : null;
  if (failure || !info?.isFile() || info.size < 100) {
    throw new Error(`export failed: ${failure ?? "empty download"}`);
  }
}

async function downloadWorkbook(page, extension) {
  await page.locator("#export-menu summary").click();
  const [download] = await Promise.all([
    page.waitForEvent("download"),
    page.locator("#save-document").click()
  ]);
  const fileName = download.suggestedFilename();
  if (!fileName.endsWith(extension)) {
    throw new Error(`unexpected workbook download name: ${fileName}`);
  }
  const failure = await download.failure();
  const path = await download.path();
  if (failure || !path) {
    throw new Error(`workbook download failed: ${failure ?? "missing path"}`);
  }
  const bytes = await readFile(path);
  if (bytes.length < 100) {
    throw new Error("workbook download is empty");
  }
  await waitForViewerState(page, (snapshot) => !snapshot.busy, "workbook download completion");
  return { fileName, bytes, path };
}

async function assertOpenpyxlReopens(workbookPath, expected) {
  const python =
    process.env.RXLS_PYTHON || (process.platform === "win32" ? "python" : "python3");
  const script = fileURLToPath(
    new URL("../scripts/verify-openpyxl-xlsm.py", import.meta.url)
  );
  const { stdout } = await execFileAsync(
    python,
    [script, workbookPath, "--cell", "A1", "--expected", expected],
    { timeout: 30_000 }
  );
  const report = JSON.parse(stdout);
  assert.equal(report.schema, "rxls.viewer-openpyxl-reopen.v1");
  assert.equal(report.openpyxl, "3.1.5");
  assert.equal(report.value, expected);
  assert.ok(report.vba_bytes >= 512);
}

function assertZipPartsEqual(sourceBytes, savedBytes, partNames) {
  const source = readZipEntries(sourceBytes);
  const saved = readZipEntries(savedBytes);
  for (const partName of partNames) {
    assert.ok(source.has(partName), `source ZIP is missing ${partName}`);
    assert.ok(saved.has(partName), `saved ZIP is missing ${partName}`);
    assert.deepEqual(saved.get(partName), source.get(partName), partName);
  }
}

function assertZipPartChanged(sourceBytes, savedBytes, partName) {
  const source = readZipEntries(sourceBytes);
  const saved = readZipEntries(savedBytes);
  assert.ok(source.has(partName), `source ZIP is missing ${partName}`);
  assert.ok(saved.has(partName), `saved ZIP is missing ${partName}`);
  assert.notDeepEqual(saved.get(partName), source.get(partName), partName);
}

function deferred() {
  let resolve;
  const promise = new Promise((settle) => {
    resolve = settle;
  });
  return { promise, resolve };
}
