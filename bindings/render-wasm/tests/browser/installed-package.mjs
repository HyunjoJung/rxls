import { RenderWorkerClient, getRenderWorkerUrl } from "@rxls/render-worker";
import { validateSvgOutput } from "@rxls/render-worker/protocol";
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

globalThis.__rxlsWorkerReadyForHeapProbe = false;
globalThis.__rxlsHeapProbeReady = false;
globalThis.__rxlsHeapProbeRelease = false;
globalThis.__rxlsHardStopProof = null;
globalThis.__rxlsCspProof = null;

let client;
try {
  const workerUrl = getRenderWorkerUrl();
  const expectedWorkerUrl = new URL("/installed-package/js/worker.mjs", location.href);
  if (!(workerUrl instanceof URL) || workerUrl.href !== expectedWorkerUrl.href) {
    throw new Error(`installed worker URL mismatch: ${workerUrl}`);
  }
  client = new RenderWorkerClient(workerUrl);
  const capabilities = await timed(client.capabilities(), "worker ready");
  assertFullCapabilities(capabilities);
  globalThis.__rxlsWorkerReadyForHeapProbe = true;
  await waitForHeapProbe("__rxlsHeapProbeReady", "heap probe ready");

  let fixture = await createBrowserFixture();
  const imageLimits = {
    maxImages: 1,
    maxImageBytes: fixture.metadata.imageBytes,
    maxMediaBytes: fixture.metadata.imageBytes,
    maxImageDimension: fixture.metadata.imageWidth,
    maxImagePixels: fixture.metadata.imageWidth * fixture.metadata.imageHeight,
    maxDecodedMediaBytes: fixture.metadata.decodedImageBytes
  };
  const opened = await timed(
    client.open(fixture.workbook, {
      documentId: "installed-package-readme",
      fontPack: fixture.fontPack
    }),
    "open workbook"
  );
  assertRichInspection(opened, fixture.metadata);
  const pageMap = await timed(
    client.preparePages(opened.documentId, 0),
    "prepare pages"
  );
  if (pageMap.manifest.pages.length < 8) {
    throw new Error(
      `installed package pagination was not material: ${pageMap.manifest.pages.length} pages`
    );
  }
  let tile = await timed(
    client.renderTile(opened.documentId, 0, FIXTURE_TILE, {
      limits: { maxCells: fixture.metadata.tileMeasuredCells, ...imageLimits }
    }),
    "render 2048-cell tile"
  );
  assertShapedSelfContainedSvg(tile.svg);
  await assertExactEmbeddedPng(tile.svg, fixture.metadata);
  validateSvgOutput(tile.svg);
  if (new TextEncoder().encode(tile.svg).byteLength < 250_000) {
    throw new Error("installed package tile did not produce a material SVG workload");
  }
  tile = null;
  let firstPage = await timed(
    client.renderPage(opened.documentId, 0, 0, {
      limits: { maxCells: fixture.metadata.cells, ...imageLimits }
    }),
    "render first page"
  );
  assertShapedSelfContainedSvg(firstPage.svg);
  await assertExactEmbeddedPng(firstPage.svg, fixture.metadata);
  validateSvgOutput(firstPage.svg);
  const parsed = new DOMParser().parseFromString(firstPage.svg, "image/svg+xml");
  if (parsed.querySelector("parsererror") || parsed.documentElement.localName !== "svg") {
    throw new Error("installed package returned invalid page SVG");
  }
  viewer.replaceChildren(document.importNode(parsed.documentElement, true));
  firstPage = null;
  await client.closeDocument(opened.documentId);
  assertExternalSvgIsRejected(validateSvgOutput);
  const hardStop = await proveHardStop({
    RenderWorkerClient,
    workerUrl,
    fixture,
    timed
  });
  if (hardStop.rejectedRequests !== 2 || hardStop.elapsedMs > hardStop.deadlineMs) {
    throw new Error(`installed hard-stop proof changed: ${JSON.stringify(hardStop)}`);
  }
  await proveCspNegativeControl(policyViolations);
  viewer.replaceChildren();
  fixture = null;
  assertNoUnexpectedExternalResources();
  result.textContent =
    "PASS installed @rxls/render-worker rich font/image render, virtual stress, CSP and hard stop";
  result.id = "pass";
  await waitForHeapProbe("__rxlsHeapProbeRelease", "heap probe release");
} catch (error) {
  result.textContent = `FAIL ${error?.code ?? error?.name ?? "error"}: ${
    error?.message ?? error
  }`;
  result.id = "fail";
  document.title = "FAIL";
} finally {
  client?.terminate();
}

function timed(promise, stage) {
  return Promise.race([
    promise,
    new Promise((_, reject) =>
      setTimeout(() => reject(new Error(`${stage} timed out`)), 10_000)
    )
  ]);
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
