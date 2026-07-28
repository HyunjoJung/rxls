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

export const BROWSER_BEHAVIOR_PROOF_SCHEMA = "rxls.render-browser-behavior.v1";
export const MAX_BROWSER_BEHAVIOR_PROOF_BYTES = 32 * 1024;

const MAX_RENDER_OUTPUT_BYTES = 16 * 1024 * 1024;
const PAGE_DPI = 96;
const FIXED_UNITS_PER_PIXEL = 1024;

export async function runBrowserScenario({
  RenderWorkerClient,
  workerUrl,
  validateSvgOutput,
  maxFontFiles,
  result = document.querySelector("#result"),
  viewer = document.querySelector("#viewer")
}) {
  if (typeof RenderWorkerClient !== "function") {
    throw new TypeError("RenderWorkerClient must be a constructor");
  }
  if (!(workerUrl instanceof URL)) {
    throw new TypeError("workerUrl must be a URL");
  }
  if (typeof validateSvgOutput !== "function") {
    throw new TypeError("validateSvgOutput must be a function");
  }
  if (!Number.isSafeInteger(maxFontFiles) || maxFontFiles <= 0) {
    throw new TypeError("maxFontFiles must be a positive safe integer");
  }
  if (!(result instanceof HTMLElement) || !(viewer instanceof HTMLElement)) {
    throw new TypeError("browser scenario result and viewer elements are required");
  }

  const policyViolations = [];
  addEventListener("securitypolicyviolation", (event) => {
    policyViolations.push(captureCspViolation(event));
  });
  globalThis.__rxlsWorkerReadyForHeapProbe = false;
  globalThis.__rxlsHeapProbeReady = false;
  globalThis.__rxlsHeapProbeRelease = false;
  globalThis.__rxlsHardStopProof = null;
  globalThis.__rxlsCspProof = null;
  globalThis.__rxlsBehaviorProof = null;

  const client = new RenderWorkerClient(workerUrl);
  let fixture = null;
  try {
    result.textContent = "STEP waiting for worker";
    const capabilities = await timed(client.capabilities(), "worker ready");
    assertFullCapabilities(capabilities);
    const capabilitiesSha256 = await sha256Text(canonicalJson(capabilities));
    globalThis.__rxlsWorkerReadyForHeapProbe = true;
    await waitForHeapProbe("__rxlsHeapProbeReady", "heap probe ready");

    result.textContent = "STEP generating rich fixture";
    fixture = await createBrowserFixture();
    const imageLimits = {
      maxImages: 1,
      maxImageBytes: fixture.metadata.imageBytes,
      maxMediaBytes: fixture.metadata.imageBytes,
      maxImageDimension: fixture.metadata.imageWidth,
      maxImagePixels: fixture.metadata.imageWidth * fixture.metadata.imageHeight,
      maxDecodedMediaBytes: fixture.metadata.decodedImageBytes
    };

    const fontFiles = captureSynchronousFailure(
      () =>
        client.open(fixture.workbook, {
          documentId: "font-limit",
          fontPack: {
            manifest: new Uint8Array(),
            members: Array.from({ length: maxFontFiles + 1 }, (_, index) => ({
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
    const abortSignal = await captureRejection(
      cancelled,
      (error) => error.name === "AbortError",
      "cancellation"
    );
    const cancelledOpen = client.open(fixture.workbook, {
      documentId: "browser-cancelled"
    });
    if (!client.cancel(cancelledOpen.requestId)) {
      throw new Error("active open request was not cancellable");
    }
    const activeOpen = await captureRejection(
      cancelledOpen,
      (error) => error.name === "AbortError",
      "active open cancellation"
    );
    const reopened = await timed(
      client.open(fixture.workbook, { documentId: "browser-cancelled" }),
      "reopen cancelled document id"
    );
    if (reopened.documentId !== "browser-cancelled") {
      throw new Error("cancelled open retained a document session");
    }
    await client.closeDocument("browser-cancelled");

    result.textContent = "STEP opening workbook";
    const progress = [];
    const opened = await timed(
      client.open(fixture.workbook, {
        documentId: "browser-contract",
        fontPack: fixture.fontPack,
        onProgress: (update) => {
          progress.push(update);
          result.textContent = `STEP open ${update.stage}`;
        }
      }),
      "open workbook"
    );
    assertRichInspection(opened, fixture.metadata);
    const expectedProgress = [
      { completed: 0, total: 3, stage: "accepted" },
      { completed: 1, total: 3, stage: "parsing" },
      { completed: 2, total: 3, stage: "finalizing" },
      { completed: 3, total: 3, stage: "complete" }
    ];
    if (JSON.stringify(progress) !== JSON.stringify(expectedProgress)) {
      throw new Error(`unexpected progress sequence: ${JSON.stringify(progress)}`);
    }

    result.textContent = "STEP pagination";
    const pageMap = await timed(
      client.preparePages(opened.documentId, 0),
      "pagination"
    );
    const manifest = validatePageManifest(pageMap, opened.documentId);
    const nonzeroPageIndex = manifest.pages.length - 1;

    result.textContent = "STEP 2048-cell tile";
    let tile = await timed(
      client.renderTile(opened.documentId, 0, FIXTURE_TILE, {
        limits: { maxCells: fixture.metadata.tileMeasuredCells, ...imageLimits }
      }),
      "2048-cell tile"
    );
    assertRenderMetadata(tile, {
      documentId: opened.documentId,
      sheetIndex: 0,
      mimeType: "image/svg+xml"
    });
    assertShapedSelfContainedSvg(tile.svg);
    await assertExactEmbeddedPng(tile.svg, fixture.metadata);
    validateSvgOutput(tile.svg);
    const tileBytes = byteLength(tile.svg);
    if (tileBytes < 250_000 || tileBytes > MAX_RENDER_OUTPUT_BYTES) {
      throw new Error(`tile SVG byte length is outside its proof bound: ${tileBytes}`);
    }
    const tileProof = {
      firstRow: FIXTURE_TILE.firstRow,
      firstCol: FIXTURE_TILE.firstCol,
      lastRow: FIXTURE_TILE.lastRow,
      lastCol: FIXTURE_TILE.lastCol,
      bytes: tileBytes,
      sha256: await sha256Text(tile.svg)
    };
    mountSvg(viewer, tile.svg);
    tile = null;
    await nextFrame();

    result.textContent = "STEP first print page";
    let firstPage = await timed(
      client.renderPage(opened.documentId, 0, 0, {
        limits: { maxCells: fixture.metadata.cells, ...imageLimits }
      }),
      "first print page"
    );
    assertPageResponse(firstPage, opened.documentId, 0);
    assertShapedSelfContainedSvg(firstPage.svg);
    await assertExactEmbeddedPng(firstPage.svg, fixture.metadata);
    validateSvgOutput(firstPage.svg);
    const firstGeometry = pageSvgGeometry(firstPage.svg, manifest.paper);
    const firstSvgBytes = boundedOutputBytes(firstPage.svg, "first page SVG");
    const firstSvgSha256 = await sha256Text(firstPage.svg);
    const firstProof = {
      pageIndex: 0,
      responsePageIndex: firstPage.pageIndex,
      pageMapSha256: await sha256Text(canonicalJson(manifest.pages[0])),
      svg: {
        bytes: firstSvgBytes,
        sha256: firstSvgSha256,
        widthRaw: firstGeometry.widthRaw,
        heightRaw: firstGeometry.heightRaw
      }
    };
    mountSvg(viewer, firstPage.svg);
    firstPage = null;
    await nextFrame();

    result.textContent = "STEP nonzero print page";
    let nonzeroPage = await timed(
      client.renderPage(opened.documentId, 0, nonzeroPageIndex, {
        limits: { maxCells: fixture.metadata.cells, ...imageLimits }
      }),
      "nonzero print page"
    );
    assertPageResponse(nonzeroPage, opened.documentId, nonzeroPageIndex);
    assertShapedSelfContainedSvg(nonzeroPage.svg, { requireImage: false });
    validateSvgOutput(nonzeroPage.svg);
    const nonzeroGeometry = pageSvgGeometry(nonzeroPage.svg, manifest.paper);
    const nonzeroSvgBytes = boundedOutputBytes(
      nonzeroPage.svg,
      "nonzero page SVG"
    );
    const nonzeroSvgSha256 = await sha256Text(nonzeroPage.svg);
    if (nonzeroSvgSha256 === firstSvgSha256) {
      throw new Error("nonzero page SVG is not isolated from the first page");
    }
    mountSvg(viewer, nonzeroPage.svg);
    nonzeroPage = null;
    await nextFrame();

    let repeatedPage = await timed(
      client.renderPage(opened.documentId, 0, nonzeroPageIndex, {
        limits: { maxCells: fixture.metadata.cells, ...imageLimits }
      }),
      "repeat nonzero print page"
    );
    assertPageResponse(repeatedPage, opened.documentId, nonzeroPageIndex);
    validateSvgOutput(repeatedPage.svg);
    const repeatedSvgSha256 = await sha256Text(repeatedPage.svg);
    if (repeatedSvgSha256 !== nonzeroSvgSha256) {
      throw new Error("nonzero page SVG was not deterministic");
    }
    repeatedPage = null;

    let png = await timed(
      client.renderPagePng(
        opened.documentId,
        0,
        nonzeroPageIndex,
        PAGE_DPI,
        {
          gridlines: false,
          limits: { maxCells: fixture.metadata.cells, ...imageLimits }
        }
      ),
      "nonzero PNG page"
    );
    assertPngResponse(png, opened.documentId, nonzeroPageIndex, PAGE_DPI);
    const pngProof = await pagePngProof(png.bytes, manifest.paper, PAGE_DPI);
    png = null;

    const actualOutOfRange = await captureRejection(
      client.renderPage(opened.documentId, 0, manifest.pages.length),
      (error) => error.code === "page_index_out_of_range",
      "actual page count"
    );
    const hardPageLimit = await captureRejection(
      client.renderPage(opened.documentId, 0, 512),
      (error) => error.code === "limit_exceeded" && error.resource === "pages",
      "hard page limit"
    );
    const dpiLimit = await captureRejection(
      client.renderPagePng(opened.documentId, 0, 0, 301),
      (error) => error.code === "dpi_out_of_range",
      "DPI limit"
    );
    const outputBytes = await captureRejection(
      client.renderSheet(opened.documentId, 0, {
        limits: { maxOutputBytes: 1 }
      }),
      (error) =>
        error.code === "limit_exceeded" && error.resource === "output_bytes",
      "output limit"
    );
    const imageCount = await captureRejection(
      client.renderSheet(opened.documentId, 0, {
        limits: { maxImages: 257 }
      }),
      (error) => error.code === "limit_exceeded" && error.resource === "maxImages",
      "image limit"
    );
    const imageBytes = await captureRejection(
      client.renderSheet(opened.documentId, 0, {
        limits: { maxImageBytes: 16 * 1024 * 1024 + 1 }
      }),
      (error) =>
        error.code === "limit_exceeded" && error.resource === "maxImageBytes",
      "image byte limit"
    );
    assertExternalSvgIsRejected(validateSvgOutput);
    await client.closeDocument(opened.documentId);

    result.textContent = "STEP active hard stop";
    const hardStop = await proveHardStop({
      RenderWorkerClient,
      workerUrl,
      fixture,
      timed
    });
    if (hardStop.rejectedRequests !== 2 || hardStop.elapsedMs > hardStop.deadlineMs) {
      throw new Error(`hard-stop proof changed: ${JSON.stringify(hardStop)}`);
    }

    result.textContent = "STEP CSP negative control";
    await proveCspNegativeControl(policyViolations);
    await nextFrame();
    assertNoUnexpectedExternalResources();
    viewer.replaceChildren();

    const proof = {
      schema: BROWSER_BEHAVIOR_PROOF_SCHEMA,
      fixture: {
        workbookBytes: fixture.metadata.workbookBytes,
        workbookSha256: fixture.metadata.workbookSha256,
        fontPackSha256: fixture.metadata.fontPackSha256,
        renderedImageBytes: fixture.metadata.renderedImageBytes,
        renderedImageSha256: fixture.metadata.renderedImageSha256
      },
      capabilitiesSha256,
      cancellation: {
        abortSignal: abortSignal.name,
        activeOpen: activeOpen.name,
        reopenedDocument: true
      },
      progress,
      limits: {
        fontFiles,
        hardPage: limitProof(hardPageLimit),
        dpi: limitProof(dpiLimit),
        outputBytes: limitProof(outputBytes),
        imageCount: limitProof(imageCount),
        imageBytes: limitProof(imageBytes)
      },
      tile: tileProof,
      pages: {
        count: manifest.pages.length,
        paper: {
          widthRaw: manifest.paper.width_raw,
          heightRaw: manifest.paper.height_raw
        },
        first: firstProof,
        nonzero: {
          pageIndex: nonzeroPageIndex,
          responsePageIndex: nonzeroPageIndex,
          pageMapSha256: await sha256Text(
            canonicalJson(manifest.pages[nonzeroPageIndex])
          ),
          svg: {
            bytes: nonzeroSvgBytes,
            sha256: nonzeroSvgSha256,
            repeatSha256: repeatedSvgSha256,
            widthRaw: nonzeroGeometry.widthRaw,
            heightRaw: nonzeroGeometry.heightRaw
          },
          png: pngProof
        },
        outOfRange: {
          pageIndex: manifest.pages.length,
          code: actualOutOfRange.code
        }
      },
      hardStop: {
        deadlineMs: hardStop.deadlineMs,
        rejectedRequests: hardStop.rejectedRequests
      },
      network: {
        cspNegativeControl: true,
        unexpectedExternalResources: 0
      }
    };
    validateBrowserBehaviorProof(proof);
    globalThis.__rxlsBehaviorProof = proof;
    fixture = null;
    result.textContent =
      "PASS shared source/installed render behavior, page isolation, limits, CSP and hard stop";
    result.id = "pass";
    await waitForHeapProbe("__rxlsHeapProbeRelease", "heap probe release");
    return proof;
  } finally {
    fixture = null;
    client.terminate();
  }
}

export function validateBrowserBehaviorProof(proof) {
  assertObjectKeys(
    proof,
    [
      "schema",
      "fixture",
      "capabilitiesSha256",
      "cancellation",
      "progress",
      "limits",
      "tile",
      "pages",
      "hardStop",
      "network"
    ],
    "behavior proof"
  );
  if (proof.schema !== BROWSER_BEHAVIOR_PROOF_SCHEMA) {
    throw new Error("behavior proof schema changed");
  }
  assertSha256(proof.capabilitiesSha256, "capabilities");
  assertObjectKeys(
    proof.fixture,
    [
      "workbookBytes",
      "workbookSha256",
      "fontPackSha256",
      "renderedImageBytes",
      "renderedImageSha256"
    ],
    "fixture proof"
  );
  assertPositiveBoundedInteger(
    proof.fixture.workbookBytes,
    32 * 1024 * 1024,
    "fixture workbook bytes"
  );
  assertPositiveBoundedInteger(
    proof.fixture.renderedImageBytes,
    16 * 1024 * 1024,
    "fixture image bytes"
  );
  for (const field of [
    "workbookSha256",
    "fontPackSha256",
    "renderedImageSha256"
  ]) {
    assertSha256(proof.fixture[field], `fixture ${field}`);
  }
  assertExactValue(
    proof.cancellation,
    {
      abortSignal: "AbortError",
      activeOpen: "AbortError",
      reopenedDocument: true
    },
    "cancellation proof"
  );
  assertExactValue(
    proof.progress,
    [
      { completed: 0, total: 3, stage: "accepted" },
      { completed: 1, total: 3, stage: "parsing" },
      { completed: 2, total: 3, stage: "finalizing" },
      { completed: 3, total: 3, stage: "complete" }
    ],
    "progress proof"
  );
  assertExactLimitProofs(proof.limits);
  assertObjectKeys(
    proof.tile,
    ["firstRow", "firstCol", "lastRow", "lastCol", "bytes", "sha256"],
    "tile proof"
  );
  assertExactValue(
    {
      firstRow: proof.tile.firstRow,
      firstCol: proof.tile.firstCol,
      lastRow: proof.tile.lastRow,
      lastCol: proof.tile.lastCol
    },
    FIXTURE_TILE,
    "tile range proof"
  );
  assertPositiveBoundedInteger(
    proof.tile.bytes,
    MAX_RENDER_OUTPUT_BYTES,
    "tile bytes"
  );
  if (proof.tile.bytes < 250_000) {
    throw new Error("tile proof is not a material workload");
  }
  assertSha256(proof.tile.sha256, "tile");

  assertObjectKeys(
    proof.pages,
    ["count", "paper", "first", "nonzero", "outOfRange"],
    "page proof"
  );
  assertPositiveBoundedInteger(proof.pages.count, 512, "page count");
  if (proof.pages.count < 8) {
    throw new Error("page proof is not a material multi-page fixture");
  }
  assertObjectKeys(proof.pages.paper, ["widthRaw", "heightRaw"], "paper proof");
  assertPositiveBoundedInteger(
    proof.pages.paper.widthRaw,
    Number.MAX_SAFE_INTEGER,
    "paper width"
  );
  assertPositiveBoundedInteger(
    proof.pages.paper.heightRaw,
    Number.MAX_SAFE_INTEGER,
    "paper height"
  );
  validateSvgPageProof(proof.pages.first, 0, proof.pages.paper, false);
  const nonzeroIndex = proof.pages.count - 1;
  validateSvgPageProof(
    proof.pages.nonzero,
    nonzeroIndex,
    proof.pages.paper,
    true
  );
  if (proof.pages.first.svg.sha256 === proof.pages.nonzero.svg.sha256) {
    throw new Error("page proof hashes are not isolated");
  }
  assertExactValue(
    proof.pages.outOfRange,
    {
      pageIndex: proof.pages.count,
      code: "page_index_out_of_range"
    },
    "page out-of-range proof"
  );
  assertExactValue(
    proof.hardStop,
    { deadlineMs: 2_000, rejectedRequests: 2 },
    "hard-stop behavior proof"
  );
  assertExactValue(
    proof.network,
    {
      cspNegativeControl: true,
      unexpectedExternalResources: 0
    },
    "network behavior proof"
  );
  const payload = canonicalJson(proof);
  if (byteLength(payload) > MAX_BROWSER_BEHAVIOR_PROOF_BYTES) {
    throw new Error("behavior proof exceeded its byte bound");
  }
  return payload;
}

function validateSvgPageProof(page, expectedIndex, paper, hasPng) {
  const keys = [
    "pageIndex",
    "responsePageIndex",
    "pageMapSha256",
    "svg",
    ...(hasPng ? ["png"] : [])
  ];
  assertObjectKeys(page, keys, `page ${expectedIndex} proof`);
  if (
    page.pageIndex !== expectedIndex ||
    page.responsePageIndex !== expectedIndex
  ) {
    throw new Error(`page ${expectedIndex} response identity changed`);
  }
  assertSha256(page.pageMapSha256, `page ${expectedIndex} map`);
  assertObjectKeys(
    page.svg,
    [
      "bytes",
      "sha256",
      ...(hasPng ? ["repeatSha256"] : []),
      "widthRaw",
      "heightRaw"
    ],
    `page ${expectedIndex} SVG proof`
  );
  assertPositiveBoundedInteger(
    page.svg.bytes,
    MAX_RENDER_OUTPUT_BYTES,
    `page ${expectedIndex} SVG bytes`
  );
  assertSha256(page.svg.sha256, `page ${expectedIndex} SVG`);
  if (hasPng) {
    assertSha256(page.svg.repeatSha256, `page ${expectedIndex} repeated SVG`);
    if (page.svg.repeatSha256 !== page.svg.sha256) {
      throw new Error(`page ${expectedIndex} SVG repeat changed`);
    }
  }
  if (
    page.svg.widthRaw !== paper.widthRaw ||
    page.svg.heightRaw !== paper.heightRaw
  ) {
    throw new Error(`page ${expectedIndex} SVG geometry differs from its manifest`);
  }
  if (!hasPng) {
    return;
  }
  assertObjectKeys(
    page.png,
    ["bytes", "sha256", "width", "height", "dpi"],
    `page ${expectedIndex} PNG proof`
  );
  assertPositiveBoundedInteger(
    page.png.bytes,
    MAX_RENDER_OUTPUT_BYTES,
    `page ${expectedIndex} PNG bytes`
  );
  assertSha256(page.png.sha256, `page ${expectedIndex} PNG`);
  if (
    page.png.dpi !== PAGE_DPI ||
    page.png.width !== rasterDimension(paper.widthRaw, PAGE_DPI) ||
    page.png.height !== rasterDimension(paper.heightRaw, PAGE_DPI)
  ) {
    throw new Error(`page ${expectedIndex} PNG geometry differs from its manifest`);
  }
}

function assertExactLimitProofs(limits) {
  const expected = {
    fontFiles: { code: "limit_exceeded", resource: "fontFiles" },
    hardPage: { code: "limit_exceeded", resource: "pages" },
    dpi: { code: "dpi_out_of_range", resource: null },
    outputBytes: { code: "limit_exceeded", resource: "output_bytes" },
    imageCount: { code: "limit_exceeded", resource: "maxImages" },
    imageBytes: { code: "limit_exceeded", resource: "maxImageBytes" }
  };
  assertExactValue(limits, expected, "limit proof");
}

function validatePageManifest(pageMap, documentId) {
  assertRenderMetadata(pageMap, { documentId, sheetIndex: 0 });
  const manifest = pageMap.manifest;
  if (
    manifest === null ||
    typeof manifest !== "object" ||
    !Array.isArray(manifest.pages) ||
    manifest.pages.length < 8 ||
    manifest.pages.length > 512 ||
    manifest.paper === null ||
    typeof manifest.paper !== "object" ||
    !Number.isSafeInteger(manifest.paper.width_raw) ||
    manifest.paper.width_raw <= 0 ||
    !Number.isSafeInteger(manifest.paper.height_raw) ||
    manifest.paper.height_raw <= 0
  ) {
    throw new Error("pagination did not return a bounded material page manifest");
  }
  for (let index = 0; index < manifest.pages.length; index += 1) {
    if (manifest.pages[index]?.output_index !== index) {
      throw new Error(`page manifest output index changed at ${index}`);
    }
  }
  return manifest;
}

function assertPageResponse(page, documentId, pageIndex) {
  assertRenderMetadata(page, {
    documentId,
    sheetIndex: 0,
    pageIndex,
    mimeType: "image/svg+xml"
  });
  if (typeof page.svg !== "string") {
    throw new Error(`page ${pageIndex} did not return SVG text`);
  }
}

function assertPngResponse(page, documentId, pageIndex, dpi) {
  assertRenderMetadata(page, {
    documentId,
    sheetIndex: 0,
    pageIndex,
    dpi,
    mimeType: "image/png"
  });
  if (!(page.bytes instanceof Uint8Array)) {
    throw new Error(`page ${pageIndex} did not return PNG bytes`);
  }
}

function assertRenderMetadata(actual, expected) {
  for (const [key, value] of Object.entries(expected)) {
    if (actual?.[key] !== value) {
      throw new Error(
        `render response ${key} changed: ${JSON.stringify(actual?.[key])}`
      );
    }
  }
}

function pageSvgGeometry(svg, paper) {
  const parsed = new DOMParser().parseFromString(svg, "image/svg+xml");
  if (parsed.querySelector("parsererror") || parsed.documentElement.localName !== "svg") {
    throw new Error("page SVG did not parse in Chromium");
  }
  const root = parsed.documentElement;
  const width = root.getAttribute("width");
  const height = root.getAttribute("height");
  const viewBox = root.getAttribute("viewBox")?.trim().split(/\s+/);
  if (
    typeof width !== "string" ||
    typeof height !== "string" ||
    viewBox?.length !== 4 ||
    viewBox[0] !== "0" ||
    viewBox[1] !== "0" ||
    viewBox[2] !== width ||
    viewBox[3] !== height
  ) {
    throw new Error("page SVG root geometry is not canonical");
  }
  const widthRaw = fixedDecimalToRaw(width);
  const heightRaw = fixedDecimalToRaw(height);
  if (widthRaw !== paper.width_raw || heightRaw !== paper.height_raw) {
    throw new Error("page SVG dimensions differ from the print manifest");
  }
  return { widthRaw, heightRaw };
}

async function pagePngProof(bytes, paper, dpi) {
  if (
    !(bytes instanceof Uint8Array) ||
    bytes.byteLength < 33 ||
    bytes.byteLength > MAX_RENDER_OUTPUT_BYTES
  ) {
    throw new Error("page PNG byte length is outside its proof bound");
  }
  const signature = [137, 80, 78, 71, 13, 10, 26, 10];
  if (signature.some((byte, index) => bytes[index] !== byte)) {
    throw new Error("page PNG signature is invalid");
  }
  const width = readU32(bytes, 16);
  const height = readU32(bytes, 20);
  if (
    width !== rasterDimension(paper.width_raw, dpi) ||
    height !== rasterDimension(paper.height_raw, dpi)
  ) {
    throw new Error("page PNG dimensions differ from the print manifest");
  }
  const bitmap = await createImageBitmap(new Blob([bytes], { type: "image/png" }));
  try {
    if (bitmap.width !== width || bitmap.height !== height) {
      throw new Error("Chromium decoded page PNG dimensions differently");
    }
  } finally {
    bitmap.close();
  }
  return {
    bytes: bytes.byteLength,
    sha256: await sha256Bytes(bytes),
    width,
    height,
    dpi
  };
}

function fixedDecimalToRaw(value) {
  if (!/^(?:0|[1-9][0-9]*)(?:\.[0-9]+)?$/.test(value)) {
    throw new Error("fixed-point SVG dimension is not canonical");
  }
  const [whole, fraction = ""] = value.split(".");
  const denominator = 10n ** BigInt(fraction.length);
  const numerator = BigInt(whole) * denominator + BigInt(fraction || "0");
  const scaled = numerator * BigInt(FIXED_UNITS_PER_PIXEL);
  if (scaled % denominator !== 0n) {
    throw new Error("SVG dimension is not exactly representable in fixed point");
  }
  const raw = Number(scaled / denominator);
  if (!Number.isSafeInteger(raw) || raw <= 0) {
    throw new Error("SVG fixed-point dimension is outside its bound");
  }
  return raw;
}

function rasterDimension(raw, dpi) {
  const numerator = raw * dpi;
  const denominator = FIXED_UNITS_PER_PIXEL * 96;
  if (!Number.isSafeInteger(numerator) || numerator <= 0) {
    throw new Error("raster dimension calculation overflowed");
  }
  return Math.floor((numerator + denominator - 1) / denominator);
}

function readU32(bytes, offset) {
  return (
    bytes[offset] * 0x1000000 +
    (bytes[offset + 1] << 16) +
    (bytes[offset + 2] << 8) +
    bytes[offset + 3]
  );
}

function boundedOutputBytes(value, label) {
  const bytes = byteLength(value);
  if (bytes <= 0 || bytes > MAX_RENDER_OUTPUT_BYTES) {
    throw new Error(`${label} is outside its byte bound`);
  }
  return bytes;
}

async function captureRejection(promise, predicate, stage) {
  try {
    await timed(promise, stage);
  } catch (error) {
    if (predicate(error)) {
      return {
        name: error.name ?? "Error",
        code: error.code ?? null,
        resource: error.resource ?? null
      };
    }
    throw error;
  }
  throw new Error(`${stage} unexpectedly succeeded`);
}

function captureSynchronousFailure(action, predicate, stage) {
  try {
    action();
  } catch (error) {
    if (predicate(error)) {
      return {
        code: error.code ?? null,
        resource: error.resource ?? null
      };
    }
    throw error;
  }
  throw new Error(`${stage} unexpectedly succeeded`);
}

function limitProof(error) {
  return { code: error.code, resource: error.resource };
}

function mountSvg(viewer, svg) {
  const parsed = new DOMParser().parseFromString(svg, "image/svg+xml");
  if (parsed.querySelector("parsererror") || parsed.documentElement.localName !== "svg") {
    throw new Error("SVG did not parse in Chromium");
  }
  viewer.replaceChildren(document.importNode(parsed.documentElement, true));
}

function canonicalJson(value) {
  return JSON.stringify(canonicalValue(value, 0, { nodes: 0 }));
}

function canonicalValue(value, depth, budget) {
  budget.nodes += 1;
  if (budget.nodes > 8_192 || depth > 24) {
    throw new Error("canonical proof exceeded its structural bound");
  }
  if (
    value === null ||
    typeof value === "string" ||
    typeof value === "boolean" ||
    (typeof value === "number" && Number.isSafeInteger(value))
  ) {
    return value;
  }
  if (Array.isArray(value)) {
    if (value.length > 1_024) {
      throw new Error("canonical proof array exceeded its bound");
    }
    return value.map((item) => canonicalValue(item, depth + 1, budget));
  }
  if (typeof value !== "object") {
    throw new Error("canonical proof contains an unsupported value");
  }
  const keys = Object.keys(value).sort();
  if (keys.length > 256) {
    throw new Error("canonical proof object exceeded its key bound");
  }
  return Object.fromEntries(
    keys.map((key) => [key, canonicalValue(value[key], depth + 1, budget)])
  );
}

function assertObjectKeys(value, expected, label) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${label} must be an object`);
  }
  const actual = Object.keys(value).sort();
  const wanted = [...expected].sort();
  if (JSON.stringify(actual) !== JSON.stringify(wanted)) {
    throw new Error(`${label} keys changed`);
  }
}

function assertExactValue(actual, expected, label) {
  if (canonicalJson(actual) !== canonicalJson(expected)) {
    throw new Error(`${label} changed`);
  }
}

function assertPositiveBoundedInteger(value, maximum, label) {
  if (!Number.isSafeInteger(value) || value <= 0 || value > maximum) {
    throw new Error(`${label} is outside its bound`);
  }
}

function assertSha256(value, label) {
  if (typeof value !== "string" || !/^[0-9a-f]{64}$/.test(value)) {
    throw new Error(`${label} SHA-256 is invalid`);
  }
}

async function sha256Text(value) {
  return sha256Bytes(new TextEncoder().encode(value));
}

async function sha256Bytes(value) {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", value));
  return [...digest]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

function byteLength(value) {
  return new TextEncoder().encode(value).byteLength;
}

function nextFrame() {
  return new Promise((resolve) =>
    requestAnimationFrame(() => requestAnimationFrame(resolve))
  );
}

export function timed(promise, stage) {
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
