import { preview } from "vite";
import { chromium } from "playwright-core";
import { mkdir, readFile, stat } from "node:fs/promises";
import { fileURLToPath } from "node:url";

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
  await page.goto(`http://127.0.0.1:${port}${basePath}`, { waitUntil: "networkidle" });
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
    state.sheetCount !== 2
  ) {
    throw new Error(`unexpected initial viewer state: ${JSON.stringify(state)}`);
  }

  await page.locator('#sheet-list button[data-index="1"]').click();
  await waitForViewerState(page, (snapshot) => snapshot.sheetIndex === 1 && !snapshot.busy, "Metrics");
  await page.locator('#sheet-list button[data-index="0"]').click();
  await waitForViewerState(
    page,
    (snapshot) => snapshot.sheetIndex === 0 && !snapshot.busy,
    "Operations"
  );

  await assertDownload(page, "#export-svg", ".svg");
  await assertDownload(page, "#export-png", ".png");

  for (const sample of [
    { id: "legacy-korean", format: "xls" },
    { id: "binary-workbook", format: "xlsb" },
    { id: "open-document", format: "ods" },
    { id: "operations-report", format: "xlsx" }
  ]) {
    await page.locator("#sample-select").selectOption(sample.id);
    await waitForViewerState(
      page,
      (snapshot) => snapshot.format === sample.format && snapshot.rendered && !snapshot.busy,
      sample.id
    );
  }

  await page.locator("#file-input").setInputFiles(
    fileURLToPath(new URL("../samples/operations-report.xlsx", import.meta.url))
  );
  await waitForViewerState(
    page,
    (snapshot) => snapshot.source === "Local file" && snapshot.rendered && !snapshot.busy,
    "local workbook"
  );

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
