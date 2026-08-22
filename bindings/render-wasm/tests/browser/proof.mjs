export function assertRichInspection(opened, metadata) {
  const workbook = opened?.workbook;
  if (
    workbook?.sheetCount !== 1 ||
    workbook?.embeddedImages !== 1 ||
    workbook?.embeddedImageBytes !== metadata.imageBytes ||
    workbook?.fontFaces !== 1 ||
    workbook?.fontPackSha256 !== metadata.fontPackSha256 ||
    workbook?.sheets?.[0]?.embeddedImages !== 1
  ) {
    throw new Error(`rich fixture inspection mismatch: ${JSON.stringify(workbook)}`);
  }
}

export function assertShapedSelfContainedSvg(svg, { requireImage = true } = {}) {
  if (
    typeof svg !== "string" ||
    !svg.includes("<svg") ||
    !svg.includes('<g role="text"') ||
    !svg.includes("<path ")
  ) {
    throw new Error("verified font pack did not produce shaped SVG outlines");
  }
  if (/<text\b[^>]*\bfont-family=/i.test(svg)) {
    throw new Error("shaped SVG unexpectedly fell back to host-font text");
  }
  if (
    requireImage &&
    (!svg.includes("<image") || !svg.includes('href="data:image/png;base64,'))
  ) {
    throw new Error("embedded workbook image was not emitted as self-contained PNG data");
  }
  if (/\b(?:href|xlink:href)=["'](?:https?:|file:|\/\/)/i.test(svg)) {
    throw new Error("rendered SVG contains an external resource URL");
  }
}

export function assertExternalSvgIsRejected(validateSvgOutput) {
  try {
    validateSvgOutput('<svg><image href="https://example.invalid/pixel.png"/></svg>');
  } catch (error) {
    if (error?.code === "external_svg_resource") {
      return;
    }
    throw error;
  }
  throw new Error("browser SVG validator accepted an external image resource");
}

export const PENDING_BOUNDARY_INPUT_BYTES = 32 * 1024 * 1024;
export const PENDING_BOUNDARY_REQUESTS = 4;
export const PENDING_BOUNDARY_RESOURCE_BYTES =
  PENDING_BOUNDARY_INPUT_BYTES * PENDING_BOUNDARY_REQUESTS;
const PENDING_BOUNDARY_SCHEMA = "rxls.pending-resource-boundary.v1";
const PENDING_BOUNDARY_FILL_BYTE = 0xa5;
const PENDING_BOUNDARY_TOUCH_STRIDE = 4 * 1024;

export async function provePendingResourceBoundary({
  RenderWorkerClient,
  deadlineMs = 5_000
}) {
  if (typeof RenderWorkerClient !== "function") {
    throw new TypeError("RenderWorkerClient must be a constructor");
  }
  if (!Number.isSafeInteger(deadlineMs) || deadlineMs <= 0 || deadlineMs > 10_000) {
    throw new TypeError("pending-resource boundary deadline is invalid");
  }
  const nonce = randomNonce();
  let dispatchedRequests = 0;
  let transportTerminated = false;
  const transport = new EventTarget();
  transport.postMessage = () => {
    dispatchedRequests += 1;
  };
  transport.terminate = () => {
    transportTerminated = true;
  };
  const client = new RenderWorkerClient(transport);
  const proof = {
    schema: PENDING_BOUNDARY_SCHEMA,
    phase: "allocating",
    nonce,
    heldEpochMs: null,
    releaseEpochMs: null,
    completedEpochMs: null,
    inputBytes: PENDING_BOUNDARY_INPUT_BYTES,
    queuedRequests: 0,
    pendingResourceBytes: 0,
    overflowBytes: 1,
    overflowOutcome: null,
    rejectedRequests: 0,
    rejectionCode: null,
    dispatchedRequests: 0,
    transportTerminated: false
  };
  globalThis.__rxlsPendingBoundaryProof = proof;
  globalThis.__rxlsPendingBoundaryRelease = null;
  const observedClones = [];
  const observedOverflowClones = [];
  let source = createTouchedPendingBoundarySource(observedClones);
  let overflowSource = createObservedPendingBoundarySource(
    Uint8Array.of(1),
    observedOverflowClones
  );
  const queued = [];
  const queuedSettlements = [];
  let queuedOutcomes = null;
  try {
    for (let index = 0; index < PENDING_BOUNDARY_REQUESTS; index += 1) {
      const request = client.open(source, {
        documentId: `pending-boundary-${nonce}-${index}`
      });
      queued.push(request);
      queuedSettlements.push(settle(request));
    }
    assertDistinctTouchedPendingBoundaryClones(source, observedClones);
    queuedOutcomes = Promise.all(queuedSettlements);
    proof.queuedRequests = queued.length;
    proof.pendingResourceBytes =
      PENDING_BOUNDARY_INPUT_BYTES * queued.length;

    let overflowRequest;
    try {
      overflowRequest = client.open(overflowSource, {
        documentId: `pending-boundary-${nonce}-overflow`
      });
    } catch (error) {
      if (
        error?.code !== "limit_exceeded" ||
        error?.resource !== "pendingResourceBytes"
      ) {
        throw error;
      }
      proof.overflowOutcome = {
        synchronous: true,
        code: error.code,
        resource: error.resource
      };
    }
    if (overflowRequest !== undefined) {
      void overflowRequest.catch(() => {});
      throw new Error(
        "pending-resource overflow was not rejected synchronously before cloning"
      );
    }
    if (observedOverflowClones.length !== 0) {
      throw new Error(
        "pending-resource overflow cloned bytes before capacity rejection"
      );
    }
    if (
      proof.pendingResourceBytes !== PENDING_BOUNDARY_RESOURCE_BYTES ||
      dispatchedRequests !== 0
    ) {
      throw new Error("pending-resource queue boundary was not held before dispatch");
    }

    proof.heldEpochMs = Date.now();
    proof.phase = "held";
    await waitForControlWithin(
      "__rxlsPendingBoundaryRelease",
      nonce,
      deadlineMs,
      "pending-resource RSS evidence"
    );
    proof.releaseEpochMs = Date.now();
    proof.phase = "terminating";
    client.terminate();
    const outcomes = await within(
      queuedOutcomes,
      2_000,
      "pending-resource termination"
    );
    for (const outcome of outcomes) {
      if (
        outcome.status !== "rejected" ||
        outcome.reason?.code !== "client_closed"
      ) {
        throw new Error(
          `pending-resource request survived termination: ${JSON.stringify(
            summarizeOutcome(outcome)
          )}`
        );
      }
    }
    if (!transportTerminated || dispatchedRequests !== 0) {
      throw new Error("pending-resource transport lifecycle changed");
    }
    proof.rejectedRequests = outcomes.length;
    proof.rejectionCode = "client_closed";
    proof.dispatchedRequests = dispatchedRequests;
    proof.transportTerminated = transportTerminated;
    proof.completedEpochMs = Date.now();
    proof.phase = "complete";
    validatePendingResourceBoundaryProof(proof, "complete");
    return {
      inputBytes: proof.inputBytes,
      queuedRequests: proof.queuedRequests,
      pendingResourceBytes: proof.pendingResourceBytes,
      overflowBytes: proof.overflowBytes,
      overflowOutcome: proof.overflowOutcome,
      rejectedRequests: proof.rejectedRequests,
      rejectionCode: proof.rejectionCode,
      dispatchedRequests: proof.dispatchedRequests,
      transportTerminated: proof.transportTerminated
    };
  } catch (error) {
    proof.phase = "failed";
    proof.failure = `${error?.name ?? "Error"}: ${error?.message ?? error}`;
    throw error;
  } finally {
    client.terminate();
    try {
      await within(
        Promise.all(queuedSettlements),
        2_000,
        "pending-resource cleanup"
      );
    } finally {
      clearPendingBoundaryBuffers(source, observedClones);
      clearPendingBoundaryBuffers(overflowSource, observedOverflowClones);
      source = null;
      overflowSource = null;
    }
  }
}

export function validatePendingResourceBoundaryProof(proof, expectedPhase) {
  const complete = expectedPhase === "complete";
  if (expectedPhase !== "held" && !complete) {
    throw new Error("pending-resource expected phase is invalid");
  }
  const expectedKeys = [
    "schema",
    "phase",
    "nonce",
    "heldEpochMs",
    "releaseEpochMs",
    "completedEpochMs",
    "inputBytes",
    "queuedRequests",
    "pendingResourceBytes",
    "overflowBytes",
    "overflowOutcome",
    "rejectedRequests",
    "rejectionCode",
    "dispatchedRequests",
    "transportTerminated"
  ];
  if (
    proof === null ||
    typeof proof !== "object" ||
    Array.isArray(proof) ||
    Object.keys(proof).sort().join(",") !== expectedKeys.sort().join(",") ||
    proof.schema !== PENDING_BOUNDARY_SCHEMA ||
    proof.phase !== expectedPhase ||
    typeof proof.nonce !== "string" ||
    !/^[0-9a-f]{32}$/.test(proof.nonce) ||
    !Number.isSafeInteger(proof.heldEpochMs) ||
    proof.heldEpochMs <= 0 ||
    proof.inputBytes !== PENDING_BOUNDARY_INPUT_BYTES ||
    proof.queuedRequests !== PENDING_BOUNDARY_REQUESTS ||
    proof.pendingResourceBytes !== PENDING_BOUNDARY_RESOURCE_BYTES ||
    proof.overflowBytes !== 1 ||
    JSON.stringify(proof.overflowOutcome) !==
      JSON.stringify({
        synchronous: true,
        code: "limit_exceeded",
        resource: "pendingResourceBytes"
      }) ||
    proof.dispatchedRequests !== 0
  ) {
    throw new Error("pending-resource boundary proof is invalid");
  }
  if (
    complete
      ? !Number.isSafeInteger(proof.releaseEpochMs) ||
        proof.releaseEpochMs < proof.heldEpochMs ||
        !Number.isSafeInteger(proof.completedEpochMs) ||
        proof.completedEpochMs < proof.releaseEpochMs ||
        proof.rejectedRequests !== PENDING_BOUNDARY_REQUESTS ||
        proof.rejectionCode !== "client_closed" ||
        proof.transportTerminated !== true
      : proof.releaseEpochMs !== null ||
        proof.completedEpochMs !== null ||
        proof.rejectedRequests !== 0 ||
        proof.rejectionCode !== null ||
        proof.transportTerminated !== false
  ) {
    throw new Error(
      `pending-resource ${expectedPhase} lifecycle proof is invalid`
    );
  }
  return proof;
}

function createTouchedPendingBoundarySource(observedClones) {
  const source = new Uint8Array(PENDING_BOUNDARY_INPUT_BYTES);
  source.fill(PENDING_BOUNDARY_FILL_BYTE);
  return createObservedPendingBoundarySource(source, observedClones);
}

function createObservedPendingBoundarySource(source, observedClones) {
  const slice = source.slice.bind(source);
  Object.defineProperty(source, "slice", {
    configurable: false,
    enumerable: false,
    writable: false,
    value(start, end) {
      const clone = slice(start, end);
      observedClones.push(clone);
      return clone;
    }
  });
  return source;
}

function assertDistinctTouchedPendingBoundaryClones(source, clones) {
  if (
    clones.length !== PENDING_BOUNDARY_REQUESTS ||
    new Set(clones).size !== PENDING_BOUNDARY_REQUESTS ||
    new Set(clones.map(({ buffer }) => buffer)).size !==
      PENDING_BOUNDARY_REQUESTS
  ) {
    throw new Error("pending-resource client clones are aliased or missing");
  }
  for (const clone of clones) {
    if (
      !(clone instanceof Uint8Array) ||
      clone === source ||
      clone.buffer === source.buffer ||
      clone.byteOffset !== 0 ||
      clone.byteLength !== PENDING_BOUNDARY_INPUT_BYTES
    ) {
      throw new Error("pending-resource client clone identity changed");
    }
    for (
      let offset = 0;
      offset < clone.byteLength;
      offset += PENDING_BOUNDARY_TOUCH_STRIDE
    ) {
      if (clone[offset] !== PENDING_BOUNDARY_FILL_BYTE) {
        throw new Error(
          "pending-resource client clone was not eagerly materialized"
        );
      }
    }
    if (clone.at(-1) !== PENDING_BOUNDARY_FILL_BYTE) {
      throw new Error("pending-resource client clone tail was not initialized");
    }
  }
}

function clearPendingBoundaryBuffers(source, clones) {
  if (source instanceof Uint8Array) {
    source.fill(0);
  }
  for (const clone of clones) {
    clone.fill(0);
  }
  clones.length = 0;
}

export async function proveHardStop({
  RenderWorkerClient,
  workerUrl,
  fixture,
  timed,
  deadlineMs = 2_000
}) {
  const nonce = randomNonce();
  const uniqueWorkerUrl = new URL(workerUrl);
  uniqueWorkerUrl.searchParams.set("rxls-hard-stop", nonce);
  const documentId = `hard-stop-${nonce}`;
  const client = new RenderWorkerClient(uniqueWorkerUrl);
  const proof = {
    schema: "rxls.render-hard-stop.v2",
    phase: "opening",
    nonce,
    workerUrl: uniqueWorkerUrl.href,
    documentId,
    activeRequestId: null,
    queuedRequestId: null,
    activeEpochMs: null,
    startedEpochMs: null,
    completedEpochMs: null,
    rejectedRequests: 0,
    activeOutcome: null,
    queuedOutcome: null,
    futureOutcome: null,
    deadlineMs
  };
  globalThis.__rxlsHardStopProof = proof;
  globalThis.__rxlsHardStopBegin = null;
  globalThis.__rxlsHardStopTerminate = null;
  let active;
  let queued;
  try {
    const opened = await timed(
      client.open(fixture.workbook, {
        documentId,
        fontPack: fixture.fontPack
      }),
      "hard-stop worker open"
    );
    assertRichInspection(opened, fixture.metadata);
    proof.phase = "armed";
    proof.armedEpochMs = Date.now();
    await within(
      waitForControl("__rxlsHardStopBegin", nonce),
      5_000,
      "hard-stop debugger arm"
    );

    let markActive;
    const activeStage = new Promise((resolve) => {
      markActive = resolve;
    });
    active = client.renderSheet(
      opened.documentId,
      0,
      {
        limits: {
          maxCells: fixture.metadata.cells,
          maxImages: 1,
          maxImageBytes: fixture.metadata.imageBytes,
          maxMediaBytes: fixture.metadata.imageBytes,
          maxImageDimension: 64,
          maxImagePixels: 64 * 64,
          maxDecodedMediaBytes: 64 * 64 * 4
        }
      },
      {
        onProgress(update) {
          if (update.completed === 1 && update.stage === "rendering") {
            proof.activeEpochMs = Date.now();
            proof.phase = "wasm-active";
            markActive();
          }
        }
      }
    );
    queued = client.preparePages(opened.documentId, 0);
    proof.activeRequestId = active.requestId;
    proof.queuedRequestId = queued.requestId;
    const outcomes = Promise.all([settle(active), settle(queued)]);
    await timed(activeStage, "hard-stop active render stage");
    await waitForTerminationControlOrNaturalCompletion(outcomes, nonce);

    const started = performance.now();
    const startedEpochMs = Date.now();
    proof.startedEpochMs = startedEpochMs;
    proof.phase = "terminating";
    client.terminate();
    const [activeOutcome, queuedOutcome] = await within(
      outcomes,
      deadlineMs,
      "hard-stop pending rejection"
    );
    const elapsedMs = performance.now() - started;
    proof.activeOutcome = summarizeOutcome(activeOutcome);
    proof.queuedOutcome = summarizeOutcome(queuedOutcome);
    for (const [label, outcome] of [
      ["active", activeOutcome],
      ["queued", queuedOutcome]
    ]) {
      if (outcome.status !== "rejected" || outcome.reason?.code !== "client_closed") {
        throw new Error(
          `${label} request survived hard stop: ${JSON.stringify({
            status: outcome.status,
            code: outcome.reason?.code
          })}`
        );
      }
    }
    if (elapsedMs > deadlineMs) {
      throw new Error(`hard stop took ${elapsedMs}ms, deadline ${deadlineMs}ms`);
    }
    const futureOutcome = await settle(client.capabilities());
    proof.futureOutcome = summarizeOutcome(futureOutcome);
    if (
      futureOutcome.status !== "rejected" ||
      futureOutcome.reason?.code !== "client_closed"
    ) {
      throw new Error("hard-stopped client accepted future work");
    }
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    proof.rejectedRequests = 2;
    proof.elapsedMs = Math.ceil(elapsedMs);
    proof.completedEpochMs = Date.now();
    proof.phase = "complete";
    return proof;
  } catch (error) {
    proof.phase = "failed";
    proof.failure = `${error?.name ?? "Error"}: ${error?.message ?? error}`;
    throw error;
  } finally {
    client.terminate();
    active = null;
    queued = null;
  }
}

async function settle(promise) {
  try {
    return { status: "fulfilled", value: await promise };
  } catch (reason) {
    return { status: "rejected", reason };
  }
}

async function within(promise, timeoutMs, label) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`${label} timed out`)), timeoutMs);
      })
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function waitForControl(name, nonce) {
  while (globalThis[name] !== nonce) {
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

async function waitForControlWithin(name, nonce, timeoutMs, label) {
  const deadline = performance.now() + timeoutMs;
  while (globalThis[name] !== nonce) {
    if (performance.now() >= deadline) {
      throw new Error(`${label} timed out`);
    }
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

async function waitForTerminationControlOrNaturalCompletion(outcomes, nonce) {
  const completion = outcomes.then(([activeOutcome, queuedOutcome]) => {
    throw new Error(
      `render naturally completed before hard stop: ${JSON.stringify({
        active: summarizeOutcome(activeOutcome),
        queued: summarizeOutcome(queuedOutcome)
      })}`
    );
  });
  await within(
    Promise.race([waitForControl("__rxlsHardStopTerminate", nonce), completion]),
    5_000,
    "hard-stop debugger termination"
  );
}

function summarizeOutcome(outcome) {
  return {
    status: outcome.status,
    code: outcome.status === "rejected" ? outcome.reason?.code ?? null : null
  };
}

function randomNonce() {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return [...bytes].map((value) => value.toString(16).padStart(2, "0")).join("");
}
