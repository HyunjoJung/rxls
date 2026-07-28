import { RenderWorkerClient } from "../../js/client.mjs";
import { MAX_FONT_FILES, validateSvgOutput } from "../../js/protocol.mjs";
import {
  assertExactEmbeddedPng,
  assertFullCapabilities,
  assertNoUnexpectedExternalResources,
  captureCspViolation,
  proveCspNegativeControl
} from "./contract.mjs";
import { createBrowserFixture, FIXTURE_TILE } from "./fixture.mjs";
import {
  assertExternalSvgIsRejected,
  assertRichInspection,
  assertShapedSelfContainedSvg,
  proveHardStop
} from "./proof.mjs";

const result = document.querySelector("#result");
const viewer = document.querySelector("#viewer");
const policyViolations = [];
addEventListener("securitypolicyviolation", (event) => {
  policyViolations.push(captureCspViolation(event));
});
const client = new RenderWorkerClient(new URL("../../js/worker.mjs", import.meta.url));
globalThis.__rxlsWorkerReadyForHeapProbe = false;
globalThis.__rxlsHeapProbeReady = false;
globalThis.__rxlsHeapProbeRelease = false;
globalThis.__rxlsHardStopProof = null;
globalThis.__rxlsCspProof = null;

try {
  result.textContent = "STEP waiting for worker";
  const capabilities = await timed(client.capabilities(), "worker ready");
  assertFullCapabilities(capabilities);
  globalThis.__rxlsWorkerReadyForHeapProbe = true;
  await waitForHeapProbe("__rxlsHeapProbeReady", "heap probe ready");
  result.textContent = "STEP generating rich fixture";
  let fixture = await createBrowserFixture();
  let bytes = fixture.workbook;
  const imageLimits = {
    maxImages: 1,
    maxImageBytes: fixture.metadata.imageBytes,
    maxMediaBytes: fixture.metadata.imageBytes,
    maxImageDimension: fixture.metadata.imageWidth,
    maxImagePixels: fixture.metadata.imageWidth * fixture.metadata.imageHeight,
    maxDecodedMediaBytes: fixture.metadata.decodedImageBytes
  };
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
      fontPack: fixture.fontPack,
      onProgress: (update) => {
        progress.push(update);
        result.textContent = `STEP open ${update.stage}`;
      }
    }),
    "open workbook"
  );
  assertRichInspection(opened, fixture.metadata);
  if (
    progress.length !== 4 ||
    progress.some(({ completed, total }, index) => completed !== index || total !== 3) ||
    progress.map(({ stage }) => stage).join(",") !== "accepted,parsing,finalizing,complete"
  ) {
    throw new Error(`unexpected progress sequence: ${JSON.stringify(progress)}`);
  }
  result.textContent = "STEP pagination";
  const pagination = await timed(client.preparePages("browser-smoke", 0), "pagination");
  if (pagination.manifest.pages.length < 8) {
    throw new Error(
      `rich fixture pagination was not material: ${pagination.manifest.pages.length} pages`
    );
  }
  result.textContent = "STEP 2048-cell tile";
  let tile = await timed(
    client.renderTile("browser-smoke", 0, FIXTURE_TILE, {
      limits: { maxCells: fixture.metadata.tileMeasuredCells, ...imageLimits }
    }),
    "2048-cell tile"
  );
  assertShapedSelfContainedSvg(tile.svg);
  await assertExactEmbeddedPng(tile.svg, fixture.metadata);
  validateSvgOutput(tile.svg);
  if (new TextEncoder().encode(tile.svg).byteLength < 250_000) {
    throw new Error("2048-cell tile did not produce a material SVG workload");
  }
  mountSvg(tile.svg);
  tile = null;
  await new Promise((resolve) => requestAnimationFrame(resolve));
  result.textContent = "STEP rich page";
  let page = await timed(
    client.renderPage("browser-smoke", 0, 0, {
      limits: { maxCells: fixture.metadata.cells, ...imageLimits }
    }),
    "page"
  );
  assertShapedSelfContainedSvg(page.svg);
  await assertExactEmbeddedPng(page.svg, fixture.metadata);
  validateSvgOutput(page.svg);
  mountSvg(page.svg);
  page = null;
  let png = await timed(
    client.renderPagePng("browser-smoke", 0, 0, 96, {
      gridlines: false,
      limits: { maxCells: fixture.metadata.cells, ...imageLimits }
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
  assertExternalSvgIsRejected(validateSvgOutput);
  result.textContent = "STEP active hard stop";
  const hardStop = await proveHardStop({
    RenderWorkerClient,
    workerUrl: new URL("../../js/worker.mjs", import.meta.url),
    fixture,
    timed
  });
  if (hardStop.rejectedRequests !== 2 || hardStop.elapsedMs > hardStop.deadlineMs) {
    throw new Error(`hard-stop proof changed: ${JSON.stringify(hardStop)}`);
  }
  result.textContent = "STEP CSP negative control";
  await proveCspNegativeControl(policyViolations);
  await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
  assertNoUnexpectedExternalResources();
  await client.closeDocument("browser-smoke");
  viewer.replaceChildren();
  bytes = null;
  fixture = null;
  result.textContent =
    "PASS rxls-render-worker rich font/image CSP, 2048-cell tile, virtual pages, PNG, limits and hard stop";
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
