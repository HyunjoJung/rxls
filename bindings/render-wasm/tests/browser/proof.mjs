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
