export const CSP_NEGATIVE_URL =
  "https://rxls-csp-negative.invalid/render-worker-control";

const EXPECTED_CAPABILITIES = Object.freeze({
  schemaVersion: 1,
  protocol: "rxls.render-worker.v1",
  outputs: ["sheet-svg", "tile-svg", "page-svg", "page-png"],
  limits: {
    maxInputBytes: 32 * 1024 * 1024,
    maxImageBytes: 16 * 1024 * 1024,
    maxImages: 256,
    maxFontBytes: 64 * 1024 * 1024,
    maxRows: 4_096,
    maxColumns: 512,
    maxCells: 250_000,
    maxConditionalRules: 2_048,
    maxConditionalEvaluations: 500_000,
    maxDrawingObjects: 2_048,
    maxMediaBytes: 16 * 1024 * 1024,
    maxImageDimension: 8_192,
    maxImagePixels: 16 * 1024 * 1024,
    maxDecodedMediaBytes: 64 * 1024 * 1024,
    maxChartSeries: 128,
    maxChartPoints: 250_000,
    maxTextBytes: 8 * 1024 * 1024,
    maxGlyphs: 1_000_000,
    maxTextRuns: 500_000,
    maxTextLines: 250_000,
    maxPathCommands: 4_000_000,
    maxSceneNodes: 1_000_000,
    maxDimensionRaw: 2_000_000 * 1_024,
    maxOutputBytes: 16 * 1024 * 1024,
    maxSheets: 255,
    maxLogicalPages: 2_048,
    maxPages: 512,
    maxTotalSceneNodes: 2_000_000,
    maxBackendCommands: 4_000_000,
    maxRasterDimension: 8_192,
    maxRasterPixels: 32 * 1024 * 1024,
    maxPngBytes: 16 * 1024 * 1024,
    minDpi: 36,
    maxDpi: 300
  },
  fontUploads: {
    supported: true,
    verified: true,
    maxBytes: 64 * 1024 * 1024,
    bundleSchema: "rxls.font-bundle.v1"
  },
  embeddedImages: {
    bounded: true,
    painted: true
  }
});

export function assertFullCapabilities(capabilities) {
  assertExactValue(capabilities, EXPECTED_CAPABILITIES, "capabilities");
}

export function captureCspViolation(event) {
  return {
    blockedURI: event.blockedURI,
    disposition: event.disposition,
    effectiveDirective: event.effectiveDirective,
    statusCode: event.statusCode,
    violatedDirective: event.violatedDirective
  };
}

export async function proveCspNegativeControl(policyViolations) {
  if (policyViolations.length !== 0) {
    throw new Error(`unexpected pre-control CSP violation: ${JSON.stringify(policyViolations)}`);
  }
  let rejection;
  try {
    await fetch(CSP_NEGATIVE_URL, {
      cache: "no-store",
      credentials: "omit",
      redirect: "error",
      referrerPolicy: "no-referrer"
    });
  } catch (error) {
    rejection = error;
  }
  if (!(rejection instanceof TypeError)) {
    throw new Error("CSP negative-control fetch was not rejected with TypeError");
  }
  for (let attempt = 0; attempt < 100 && policyViolations.length === 0; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  if (policyViolations.length !== 1) {
    throw new Error(
      `CSP negative control produced ${policyViolations.length} violations instead of one`
    );
  }
  const [violation] = policyViolations;
  const expected = {
    blockedURI: CSP_NEGATIVE_URL,
    disposition: "enforce",
    effectiveDirective: "connect-src",
    statusCode: 200,
    violatedDirective: "connect-src"
  };
  assertExactValue(violation, expected, "CSP negative-control violation");
  const proof = {
    schema: "rxls.render-csp-negative.v1",
    url: CSP_NEGATIVE_URL,
    violation,
    fetchRejected: true
  };
  globalThis.__rxlsCspProof = proof;
  return proof;
}

export function assertNoUnexpectedExternalResources() {
  const external = performance
    .getEntriesByType("resource")
    .map(({ name }) => new URL(name, location.href))
    .filter(
      (url) =>
        url.protocol !== "data:" &&
        url.origin !== location.origin &&
        url.href !== CSP_NEGATIVE_URL
    );
  if (external.length !== 0) {
    throw new Error(`external resource requested: ${external[0].href}`);
  }
}

export async function assertExactEmbeddedPng(svg, metadata) {
  const matches = [
    ...svg.matchAll(/\b(?:href|xlink:href)="data:image\/png;base64,([^"]+)"/g)
  ];
  if (matches.length !== 1) {
    throw new Error(`expected exactly one embedded PNG, found ${matches.length}`);
  }
  const bytes = decodeBase64(matches[0][1], metadata.renderedImageBytes);
  const encodedSha256 = await sha256Hex(bytes);
  if (
    bytes.byteLength !== metadata.renderedImageBytes ||
    encodedSha256 !== metadata.renderedImageSha256
  ) {
    throw new Error(
      `rendered PNG identity changed: bytes=${bytes.byteLength} sha256=${encodedSha256}`
    );
  }
  const dimensions = parsePngHeader(bytes);
  if (
    dimensions.width !== metadata.imageWidth ||
    dimensions.height !== metadata.imageHeight
  ) {
    throw new Error(
      `embedded PNG dimensions changed: ${dimensions.width}x${dimensions.height}`
    );
  }
  const bitmap = await createImageBitmap(
    new Blob([bytes], { type: "image/png" }),
    { colorSpaceConversion: "none", premultiplyAlpha: "none" }
  );
  try {
    if (bitmap.width !== metadata.imageWidth || bitmap.height !== metadata.imageHeight) {
      throw new Error(`decoded PNG dimensions changed: ${bitmap.width}x${bitmap.height}`);
    }
    const canvas = new OffscreenCanvas(bitmap.width, bitmap.height);
    const context = canvas.getContext("2d", {
      colorSpace: "srgb",
      willReadFrequently: true
    });
    if (context === null) {
      throw new Error("Chromium did not provide an OffscreenCanvas 2D context");
    }
    context.drawImage(bitmap, 0, 0);
    const decoded = context.getImageData(0, 0, bitmap.width, bitmap.height).data;
    if (decoded.byteLength !== metadata.decodedImageBytes) {
      throw new Error("decoded PNG byte length changed");
    }
    const decodedSha256 = await sha256Hex(decoded);
    if (decodedSha256 !== metadata.decodedImageSha256) {
      throw new Error(`decoded PNG pixel digest changed: ${decodedSha256}`);
    }
  } finally {
    bitmap.close();
  }
}

function decodeBase64(encoded, maxBytes) {
  if (
    typeof encoded !== "string" ||
    encoded.length === 0 ||
    encoded.length > Math.ceil(maxBytes / 3) * 4 + 4
  ) {
    throw new Error("embedded PNG base64 length is invalid");
  }
  const binary = atob(encoded);
  if (binary.length > maxBytes) {
    throw new Error("embedded PNG exceeds its exact byte bound");
  }
  return Uint8Array.from(binary, (character) => character.charCodeAt(0));
}

function parsePngHeader(bytes) {
  const signature = [137, 80, 78, 71, 13, 10, 26, 10];
  if (
    bytes.byteLength < 33 ||
    signature.some((byte, index) => bytes[index] !== byte) ||
    readU32(bytes, 8) !== 13 ||
    new TextDecoder().decode(bytes.subarray(12, 16)) !== "IHDR" ||
    bytes[24] !== 8 ||
    bytes[25] !== 6 ||
    bytes[26] !== 0 ||
    bytes[27] !== 0 ||
    bytes[28] !== 0
  ) {
    throw new Error("embedded PNG does not have the exact RGBA8 IHDR contract");
  }
  return { width: readU32(bytes, 16), height: readU32(bytes, 20) };
}

function readU32(bytes, offset) {
  return (
    bytes[offset] * 0x1000000 +
    (bytes[offset + 1] << 16) +
    (bytes[offset + 2] << 8) +
    bytes[offset + 3]
  );
}

async function sha256Hex(bytes) {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return [...digest].map((value) => value.toString(16).padStart(2, "0")).join("");
}

function assertExactValue(actual, expected, path) {
  if (
    actual === null ||
    expected === null ||
    typeof actual !== "object" ||
    typeof expected !== "object"
  ) {
    if (!Object.is(actual, expected)) {
      throw new Error(`${path} changed: ${JSON.stringify(actual)}`);
    }
    return;
  }
  if (Array.isArray(actual) !== Array.isArray(expected)) {
    throw new Error(`${path} container type changed`);
  }
  const actualKeys = Object.keys(actual).sort();
  const expectedKeys = Object.keys(expected).sort();
  if (JSON.stringify(actualKeys) !== JSON.stringify(expectedKeys)) {
    throw new Error(
      `${path} keys changed: ${JSON.stringify(actualKeys)} != ${JSON.stringify(expectedKeys)}`
    );
  }
  for (const key of expectedKeys) {
    assertExactValue(actual[key], expected[key], `${path}.${key}`);
  }
}
