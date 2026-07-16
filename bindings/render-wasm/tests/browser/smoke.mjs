import { RenderWorkerClient } from "../../js/client.mjs";
import { MAX_FONT_FILES } from "../../js/protocol.mjs";

const result = document.querySelector("#result");
const viewer = document.querySelector("#viewer");
const policyViolations = [];
addEventListener("securitypolicyviolation", (event) => {
  policyViolations.push(`${event.violatedDirective}:${event.blockedURI}`);
});
const client = new RenderWorkerClient(new URL("../../js/worker.mjs", import.meta.url));
globalThis.__rxlsWorkerReadyForHeapProbe = false;
globalThis.__rxlsHeapProbeReady = false;
globalThis.__rxlsHeapProbeRelease = false;

try {
  result.textContent = "STEP loading fixture";
  const encoded = (await (await fetch("/fixture.b64")).text()).trim();
  const binary = atob(encoded);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  result.textContent = "STEP waiting for worker";
  const capabilities = await timed(client.capabilities(), "worker ready");
  if (
    capabilities.protocol !== "rxls.render-worker.v1" ||
    capabilities.limits.maxInputBytes !== 32 * 1024 * 1024 ||
    capabilities.limits.maxSheets !== 255 ||
    capabilities.limits.maxPages !== 512 ||
    capabilities.limits.maxOutputBytes !== 16 * 1024 * 1024
  ) {
    throw new Error("worker capability limits changed");
  }
  globalThis.__rxlsWorkerReadyForHeapProbe = true;
  await waitForHeapProbe("__rxlsHeapProbeReady", "heap probe ready");
  expectSynchronousFailure(
    () =>
      client.open(bytes, {
        documentId: "font-limit",
        fontPack: {
          manifest: new Uint8Array(),
          members: Array.from({ length: MAX_FONT_FILES + 1 }, (_, index) => ({
            name: `font-${index}.ttf`,
            bytes: new Uint8Array()
          }))
        }
      }),
    (error) => error.code === "limit_exceeded" && error.resource === "fontFiles",
    "font upload limit"
  );
  const cancelledController = new AbortController();
  const cancelled = client.capabilities({ signal: cancelledController.signal });
  cancelledController.abort();
  await expectRejection(cancelled, (error) => error.name === "AbortError", "cancellation");
  const cancelledOpen = client.open(bytes, { documentId: "browser-cancelled" });
  if (!client.cancel(cancelledOpen.requestId)) {
    throw new Error("active open request was not cancellable");
  }
  await expectRejection(
    cancelledOpen,
    (error) => error.name === "AbortError",
    "active open cancellation"
  );
  const reopened = await timed(
    client.open(bytes, { documentId: "browser-cancelled" }),
    "reopen cancelled document id"
  );
  if (reopened.documentId !== "browser-cancelled") {
    throw new Error("cancelled open retained a document session");
  }
  await client.closeDocument("browser-cancelled");
  result.textContent = "STEP opening workbook";
  const progress = [];
  const opened = await timed(
    client.open(bytes, {
      documentId: "browser-smoke",
      onProgress: (update) => {
        progress.push(update);
        result.textContent = `STEP open ${update.stage}`;
      }
    }),
    "open workbook"
  );
  if (opened.workbook.sheetCount !== 1) {
    throw new Error("unexpected sheet count");
  }
  if (
    progress.length !== 4 ||
    progress.some(({ completed, total }, index) => completed !== index || total !== 3) ||
    progress.map(({ stage }) => stage).join(",") !== "accepted,parsing,finalizing,complete"
  ) {
    throw new Error(`unexpected progress sequence: ${JSON.stringify(progress)}`);
  }
  result.textContent = "STEP pagination";
  const pagination = await timed(client.preparePages("browser-smoke", 0), "pagination");
  if (pagination.manifest.pages.length < 1) {
    throw new Error("no print pages");
  }
  result.textContent = "STEP tile";
  let tile = await timed(client.renderTile(
    "browser-smoke",
    0,
    { firstRow: 0, firstCol: 0, lastRow: 4, lastCol: 4 },
    { limits: { maxCells: 25 } }
  ), "tile");
  if (!tile.svg.includes("<svg")) {
    throw new Error("tile SVG output missing");
  }
  mountSvg(tile.svg);
  tile = null;
  await new Promise((resolve) => requestAnimationFrame(resolve));
  result.textContent = "STEP page";
  let page = await timed(
    client.renderPage("browser-smoke", 0, 0, {
      limits: { maxFontBytes: 0, maxImages: 0, maxImageBytes: 0 }
    }),
    "page"
  );
  if (!page.svg.includes("<svg")) {
    throw new Error("page SVG output missing");
  }
  mountSvg(page.svg);
  page = null;
  let png = await timed(
    client.renderPagePng("browser-smoke", 0, 0, 96, {
      range: { firstRow: 100, firstCol: 100, lastRow: 100, lastCol: 100 },
      gridlines: false,
      limits: { maxFontBytes: 0, maxImages: 0, maxImageBytes: 0 }
    }),
    "PNG page"
  );
  if (
    png.bytes.byteLength < 8 ||
    [137, 80, 78, 71, 13, 10, 26, 10].some((byte, index) => png.bytes[index] !== byte)
  ) {
    throw new Error("page PNG output missing or invalid");
  }
  png = null;
  await expectRejection(
    client.renderPage("browser-smoke", 0, 512),
    (error) => error.code === "limit_exceeded" && error.resource === "pages",
    "page limit"
  );
  await expectRejection(
    client.renderPagePng("browser-smoke", 0, 0, 301),
    (error) => error.code === "dpi_out_of_range",
    "DPI limit"
  );
  await expectRejection(
    client.renderSheet("browser-smoke", 0, { limits: { maxOutputBytes: 1 } }),
    (error) => error.code === "limit_exceeded" && error.resource === "output_bytes",
    "output limit"
  );
  await expectRejection(
    client.renderSheet("browser-smoke", 0, { limits: { maxImages: 257 } }),
    (error) => error.code === "limit_exceeded" && error.resource === "maxImages",
    "image limit"
  );
  await expectRejection(
    client.renderSheet("browser-smoke", 0, {
      limits: { maxImageBytes: 16 * 1024 * 1024 + 1 }
    }),
    (error) => error.code === "limit_exceeded" && error.resource === "maxImageBytes",
    "image byte limit"
  );
  await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
  if (policyViolations.length !== 0) {
    throw new Error(`CSP violation: ${policyViolations.join(",")}`);
  }
  const externalResources = performance
    .getEntriesByType("resource")
    .map(({ name }) => new URL(name, location.href))
    .filter((url) => url.protocol !== "data:" && url.origin !== location.origin);
  if (externalResources.length !== 0) {
    throw new Error(`external resource requested: ${externalResources[0].href}`);
  }
  await client.closeDocument("browser-smoke");
  result.textContent = "PASS rxls-render-worker CSP, limits, cancellation, progress, tile and page smoke";
  result.id = "pass";
  await waitForHeapProbe("__rxlsHeapProbeRelease", "heap probe release");
} catch (error) {
  result.textContent = `FAIL ${error?.code ?? error?.name ?? "error"}: ${error?.message ?? error}`;
  result.id = "fail";
  document.title = "FAIL";
} finally {
  client.terminate();
}

function timed(promise, stage) {
  return Promise.race([
    promise,
    new Promise((_, reject) =>
      setTimeout(() => reject(new Error(`${stage} timed out`)), 10_000)
    )
  ]);
}

async function expectRejection(promise, predicate, stage) {
  try {
    await timed(promise, stage);
  } catch (error) {
    if (predicate(error)) {
      return;
    }
    throw error;
  }
  throw new Error(`${stage} unexpectedly succeeded`);
}

function expectSynchronousFailure(action, predicate, stage) {
  try {
    action();
  } catch (error) {
    if (predicate(error)) {
      return;
    }
    throw error;
  }
  throw new Error(`${stage} unexpectedly succeeded`);
}

function mountSvg(svg) {
  const parsed = new DOMParser().parseFromString(svg, "image/svg+xml");
  if (parsed.querySelector("parsererror")) {
    throw new Error("SVG did not parse in Chromium");
  }
  viewer.replaceChildren(document.importNode(parsed.documentElement, true));
}

async function waitForHeapProbe(flag, stage) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    if (globalThis[flag] === true) {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`${stage} timed out`);
}
