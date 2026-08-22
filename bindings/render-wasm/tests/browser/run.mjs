import { spawn, spawnSync } from "node:child_process";
import { createServer } from "node:http";
import { access, mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { extname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

import { BoundedEntryLog, BoundedTextTail } from "./bounded-evidence.mjs";
import {
  OperationTimeoutError,
  closeServer,
  createCdpClient,
  terminateChild,
  waitForWebSocketOpen,
  withTimeout
} from "./lifecycle.mjs";
import {
  correlateHardStopTarget,
  decideHardStopObservation,
  findNonceBoundWorker,
  hardStopObservationDeadlineEpochMs
} from "./hard-stop-evidence.mjs";
import {
  combineHeaps,
  IndependentRssSampler,
  largerHeap,
  MIN_BOUNDARY_RSS_SAMPLES,
  resolveProcessMemoryGate,
  RSS_SAMPLE_INTERVAL_MS,
  sampleProcessTreeRss,
  summarizeIndependentRssWindow,
  summarizePendingBoundaryRssMateriality,
  summarizeProcessMemory
} from "./memory-evidence.mjs";
import {
  createRenderWorkerCspProbeExpression,
  normalizeRenderWorkerContexts,
  PRE_NAVIGATION_PROOF_SCHEMA,
  RENDER_WORKER_CSP_NEGATIVE_URL,
  renderPageCspFetchInstrumentationCommand,
  renderWorkerNetworkInstrumentationCommands,
  validateCspNetworkSilence,
  validateOfflineNetworkBlock,
  validateRenderWorkerCspEvidence,
  validateSameOriginRouteEvidence
} from "./network-evidence.mjs";
import { validatePendingResourceBoundaryProof } from "./proof.mjs";
import { validateBrowserBehaviorProof } from "./scenario.mjs";

const CDP_HTTP_TIMEOUT_MS = 2_000;
const CDP_COMMAND_TIMEOUT_MS = 5_000;
const BROWSER_START_TIMEOUT_MS = 20_000;
const BROWSER_RUN_TIMEOUT_MS = 45_000;
const CLEANUP_TIMEOUT_MS = 5_000;
const MAX_REQUEST_URL_BYTES = 4_096;
const MAX_ROUTE_FILE_BYTES = 32 * 1024 * 1024;
const MAX_ROUTE_ENTRIES = 256;
const MAX_ROUTE_LOG_BYTES = 64 * 1024;
const MAX_STDERR_BYTES = 64 * 1024;
const MAX_CDP_EVENTS = 4_096;
const MAX_CDP_HTTP_RESPONSE_BYTES = 256 * 1024;
const EXPECTED_ROUTE_REQUESTS = 19;

const packageRoot = resolve(fileURLToPath(new URL("../..", import.meta.url)));
const installedPackageRoot = process.env.RXLS_RENDER_INSTALLED_PACKAGE_ROOT
  ? resolve(process.env.RXLS_RENDER_INSTALLED_PACKAGE_ROOT)
  : null;
const lock = JSON.parse(await readFile(new URL("../../toolchain-lock.json", import.meta.url)));
const generatedWasm = resolve(packageRoot, "pkg/rxls_render_wasm_bg.wasm");
try {
  await access(generatedWasm);
} catch {
  console.error("generated wasm is missing; run npm run build:wasm first");
  process.exit(2);
}
if (installedPackageRoot !== null) {
  const metadata = await stat(installedPackageRoot);
  const packageMetadata = JSON.parse(
    await readFile(resolve(installedPackageRoot, "package.json"), "utf8")
  );
  if (!metadata.isDirectory() || packageMetadata.name !== "@rxls/render-worker") {
    console.error("installed render package root is invalid");
    process.exit(2);
  }
}

const chrome =
  process.env.RXLS_CHROMIUM_BIN ||
  (process.platform === "darwin"
    ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    : "chromium");
const version = spawnSync(chrome, ["--version"], {
  encoding: "utf8",
  timeout: CDP_HTTP_TIMEOUT_MS,
  maxBuffer: 16 * 1024,
  killSignal: "SIGKILL"
});
const acceptedProducts = [lock.chromium.product, lock.chromium.testingProduct].filter(Boolean);
const actualVersion = (version.stdout ?? "").trim();
if (
  version.status !== 0 ||
  !acceptedProducts.some(
    (product) => actualVersion === `${product} ${lock.chromium.version}`
  )
) {
  console.error(
    `expected ${acceptedProducts.map((product) => `${product} ${lock.chromium.version}`).join(" or ")}; got ${actualVersion || "unavailable"}`
  );
  process.exit(2);
}
const heapGate = lock.chromium.heapGate;
if (
  !Number.isSafeInteger(heapGate?.maxAccountedBytes) ||
  heapGate.maxAccountedBytes <= 0 ||
  !Number.isSafeInteger(heapGate?.maxRetainedGrowthBytes) ||
  heapGate.maxRetainedGrowthBytes <= 0 ||
  heapGate.maxRetainedGrowthBytes > heapGate.maxAccountedBytes ||
  !Number.isSafeInteger(heapGate?.maxProcessTreePeakGrowthBytes) ||
  heapGate.maxProcessTreePeakGrowthBytes <= 0 ||
  !Number.isSafeInteger(heapGate?.maxProcessTreeRetainedGrowthBytes) ||
  heapGate.maxProcessTreeRetainedGrowthBytes <= 0 ||
  heapGate.maxProcessTreeRetainedGrowthBytes >
    heapGate.maxProcessTreePeakGrowthBytes
) {
  console.error("invalid Chromium heap gate in toolchain-lock.json");
  process.exit(2);
}
const processMemoryGate = resolveProcessMemoryGate(heapGate, process.platform);
const routeMode = installedPackageRoot === null ? "source" : "installed";
const allowedRoutePaths = new Set(
  routeMode === "source"
    ? [
        "/tests/browser/index.html",
        "/tests/browser/bootstrap.mjs",
        "/tests/browser/smoke.mjs",
        "/tests/browser/contract.mjs",
        "/tests/browser/fixture.mjs",
        "/tests/browser/proof.mjs",
        "/tests/browser/scenario.mjs",
        "/js/client.mjs",
        "/js/protocol.mjs",
        "/js/worker-runtime.mjs",
        "/js/worker.mjs",
        "/pkg/rxls_render_wasm.js",
        "/pkg/rxls_render_wasm_bg.wasm"
      ]
    : [
        "/tests/browser/installed-package.html",
        "/tests/browser/installed-package-bootstrap.mjs",
        "/tests/browser/installed-package.mjs",
        "/tests/browser/contract.mjs",
        "/tests/browser/fixture.mjs",
        "/tests/browser/proof.mjs",
        "/tests/browser/scenario.mjs",
        "/installed-package/js/client.mjs",
        "/installed-package/js/protocol.mjs",
        "/installed-package/js/worker-runtime.mjs",
        "/installed-package/js/worker.mjs",
        "/installed-package/pkg/rxls_render_wasm.js",
        "/installed-package/pkg/rxls_render_wasm_bg.wasm"
      ]
);
const allowedWorkerPath =
  routeMode === "source"
    ? "/js/worker.mjs"
    : "/installed-package/js/worker.mjs";
let serverHardStopNonce = null;

const requestedPaths = new BoundedEntryLog({
  maxEntries: MAX_ROUTE_ENTRIES,
  maxBytes: MAX_ROUTE_LOG_BYTES,
  maxEntryBytes: MAX_REQUEST_URL_BYTES
});
const networkSinkRequests = new BoundedEntryLog({
  maxEntries: 8,
  maxBytes: 8 * 1024,
  maxEntryBytes: 1024
});
let networkSinkFailure = null;
let serverRouteFailure = null;
const networkSinkServer = createServer(
  { maxHeaderSize: 4 * 1024 },
  (request, response) => {
    try {
      networkSinkRequests.add(`${request.method ?? "GET"} ${request.url ?? "/"}`);
    } catch (error) {
      networkSinkFailure = error;
    }
    response.writeHead(503, {
      "content-type": "text/plain; charset=utf-8",
      connection: "close"
    });
    response.end("network control escaped CDP interception");
  }
);
const server = createServer({ maxHeaderSize: 16 * 1024 }, async (request, response) => {
  try {
    if (
      typeof request.url !== "string" ||
      Buffer.byteLength(request.url) > MAX_REQUEST_URL_BYTES
    ) {
      serverRouteFailure ??= "browser evidence server rejected a request";
      response.writeHead(431, { "content-type": "text/plain; charset=utf-8" });
      response.end("request URL too large");
      return;
    }
    const url = new URL(request.url, "http://127.0.0.1");
    requestedPaths.add(`${request.method ?? "GET"} ${url.pathname}${url.search}`);
    try {
      validateServerRoute(request.method, url);
    } catch {
      serverRouteFailure ??= "browser evidence server rejected a request";
      throw new Error(serverRouteFailure);
    }
    const target = requestTarget(url.pathname);
    const metadata = await stat(target);
    if (
      !metadata.isFile() ||
      !Number.isSafeInteger(metadata.size) ||
      metadata.size < 0 ||
      metadata.size > MAX_ROUTE_FILE_BYTES
    ) {
      throw new Error("not a file");
    }
    response.writeHead(200, {
      "content-type": contentType(target),
      "cache-control": "no-store",
      "content-security-policy":
        "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'" +
        (installedPackageRoot === null ? "" : " 'nonce-rxls-installed-package'") +
        "; worker-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'none'; frame-ancestors 'none'; form-action 'none'; font-src 'none'; media-src 'none'; manifest-src 'none'; object-src 'none'; base-uri 'none'",
      "cross-origin-opener-policy": "same-origin",
      "x-content-type-options": "nosniff"
    });
    response.end(await readFile(target));
  } catch {
    serverRouteFailure ??= "browser evidence server request failed";
    response.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
    response.end("not found");
  }
});

await new Promise((resolveListen) => server.listen(0, "127.0.0.1", resolveListen));
await new Promise((resolveListen) =>
  networkSinkServer.listen(0, "127.0.0.1", resolveListen)
);
const address = server.address();
const networkSinkAddress = networkSinkServer.address();
const browserEntry =
  installedPackageRoot === null
    ? "/tests/browser/index.html"
    : "/tests/browser/installed-package.html";
const url = `http://127.0.0.1:${address.port}${browserEntry}`;
const networkControl = new URL(
  `http://127.0.0.1:${networkSinkAddress.port}/__rxls_network_negative_control__`
);
const networkControlUrl = networkControl.href;
if (networkControl.origin === new URL(url).origin) {
  throw new Error("CDP Network negative control is not off-origin");
}
const profile = await mkdtemp(join(tmpdir(), "rxls-render-browser-"));
const child = spawn(
  chrome,
  [
    "--headless=new",
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-default-apps",
    "--disable-extensions",
    "--disable-sync",
    "--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1",
    "--no-proxy-server",
    "--no-first-run",
    "--js-flags=--max-old-space-size=192",
    "--remote-debugging-port=0",
    `--user-data-dir=${profile}`,
    "about:blank"
  ],
  { stdio: ["ignore", "ignore", "pipe"] }
);
const stderr = new BoundedTextTail(MAX_STDERR_BYTES);
let childError = null;
let childExit = null;
child.stderr.setEncoding("utf8");
child.stderr.on("data", (chunk) => stderr.append(chunk));
child.once("error", (error) => {
  childError = error;
});
child.once("exit", (code, signal) => {
  childExit = { code, signal };
});
let browserResult = {
  message: "",
  heap: null,
  processMemory: null,
  hardStop: null,
  pendingBoundary: null,
  csp: null,
  networkProof: null,
  behavior: null,
  behaviorJson: null
};
try {
  const portFile = join(profile, "DevToolsActivePort");
  const port = Number.parseInt((await waitForFile(portFile)).split("\n")[0], 10);
  const pages = await waitForPages(port);
  const page = pages.find(
    (entry) => entry.type === "page" && entry.url === "about:blank"
  );
  if (!page?.id) {
    throw new Error("blank browser page did not expose a DevTools endpoint");
  }
  const browserMetadata = await fetchJson(
    `http://127.0.0.1:${port}/json/version`,
    "Chromium DevTools browser metadata"
  );
  if (!browserMetadata?.webSocketDebuggerUrl) {
    throw new Error("Chromium did not expose a browser DevTools endpoint");
  }
  browserResult = await waitForBrowserResult(
    browserMetadata.webSocketDebuggerUrl,
    page.id,
    child.pid,
    url
  );
} catch (error) {
  browserResult = {
    message: `FAIL harness: ${error instanceof Error ? error.message : String(error)}`,
    heap: null,
    processMemory: null,
    hardStop: null,
    pendingBoundary: null,
    csp: null,
    networkProof: null,
    behavior: null,
    behaviorJson: null
  };
} finally {
  await terminateChild(child);
  await Promise.all([closeServer(server), closeServer(networkSinkServer)]);
  await withTimeout(
    rm(profile, { recursive: true, force: true }),
    CLEANUP_TIMEOUT_MS,
    "Chromium profile cleanup"
  );
}
const escapedNetworkRequests = networkSinkRequests.values();
if (
  browserResult.message.startsWith("PASS ") &&
  (networkSinkFailure !== null || escapedNetworkRequests.length !== 0)
) {
  browserResult.message =
    `FAIL network_egress: ${networkSinkFailure?.message ?? escapedNetworkRequests.join(", ")}`;
}
if (
  browserResult.message.startsWith("PASS ") &&
  serverRouteFailure !== null
) {
  browserResult.message = `FAIL server_route: ${serverRouteFailure}`;
}
if (!browserResult.message.startsWith("PASS ")) {
  console.error(`requests: ${requestedPaths.values().join(", ")}`);
  if (escapedNetworkRequests.length !== 0) {
    console.error(`network sink requests: ${escapedNetworkRequests.join(", ")}`);
  }
  console.error(browserResult.message || "browser returned no result");
  console.error(stderr.text());
  process.exit(1);
}
if (
  browserResult.behavior === null ||
  typeof browserResult.behaviorJson !== "string" ||
  browserResult.pendingBoundary === null ||
  browserResult.networkProof === null
) {
  console.error("browser returned incomplete behavior, memory, or network evidence");
  process.exit(1);
}
console.log(`PROOF ${browserResult.behaviorJson}`);
console.log(
  `RSS_BOUNDARY interval=${browserResult.pendingBoundary.sampler.intervalMs}ms ` +
    `samples=${browserResult.pendingBoundary.sampler.boundarySampleCount} ` +
    `required=${MIN_BOUNDARY_RSS_SAMPLES} ` +
    `duration=${browserResult.pendingBoundary.sampler.sampledDurationMs}ms ` +
    `max-gap=${browserResult.pendingBoundary.sampler.maxGapMs}ms ` +
    `growth=${browserResult.pendingBoundary.sampler.peakGrowthBytes} ` +
    `minimum-growth=${browserResult.pendingBoundary.sampler.minimumGrowthBytes} ` +
    `peak=${browserResult.pendingBoundary.sampler.peak.rssBytes}`
);
console.log(
  `NETWORK_PROOF route=${browserResult.networkProof.routes.routeSha256} ` +
    `csp=${browserResult.networkProof.workers.cspSha256} ` +
    `workers=${browserResult.networkProof.workers.workers.length} ` +
    `requests=${browserResult.networkProof.routes.requestCount} pre-nav=true`
);
console.log(
  `PASS ${actualVersion} ${
    installedPackageRoot === null
      ? "worker/WASM rich font/image, CSP, limits, virtual tile/page and hard-stop smoke"
      : "installed package rich font/image, CSP, limits, virtual tile/page and hard-stop smoke"
  }; ` +
    `heap baseline=${browserResult.heap.baseline.accountedBytes} ` +
    `peak=${browserResult.heap.peak.accountedBytes} ` +
    `retained=${browserResult.heap.retained.accountedBytes} ` +
    `growth=${browserResult.heap.retainedGrowthBytes} bytes; ` +
    `rss baseline=${browserResult.processMemory.baseline.rssBytes} ` +
    `peak=${browserResult.processMemory.peak.rssBytes} ` +
    `peak-growth=${browserResult.processMemory.peakGrowthBytes} ` +
    `retained=${browserResult.processMemory.retained.rssBytes} ` +
    `retained-growth=${browserResult.processMemory.retainedGrowthBytes} bytes; ` +
    `hard-stop target=${browserResult.hardStop.elapsedMs}/${browserResult.hardStop.deadlineMs}ms ` +
    `wasm=${browserResult.hardStop.wasmFrame.url}; ` +
    `CSP Network=${browserResult.csp.networkControl.errorText}`
);

async function waitForFile(path) {
  const deadline = Date.now() + BROWSER_START_TIMEOUT_MS;
  while (Date.now() < deadline) {
    try {
      return await readFile(path, "utf8");
    } catch {
      if (childError !== null) {
        throw new Error(`Chromium failed to start: ${childError.message}`);
      }
      if (childExit !== null) {
        throw new Error(
          `Chromium exited before exposing DevTools (code=${String(
            childExit.code
          )}, signal=${String(childExit.signal)})`
        );
      }
      await delay(50);
    }
  }
  throw new Error("timed out waiting for Chromium DevTools port");
}

async function waitForPages(port) {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    try {
      const pages = await fetchJson(
        `http://127.0.0.1:${port}/json/list`,
        "Chromium DevTools page targets",
        Math.max(1, Math.min(CDP_HTTP_TIMEOUT_MS, deadline - Date.now()))
      );
      if (pages.length > 0) {
        return pages;
      }
    } catch {
      // Browser startup is still in progress.
    }
    await delay(50);
  }
  throw new Error("timed out waiting for Chromium page target");
}

async function waitForBrowserResult(
  webSocketUrl,
  pageTargetId,
  browserRootPid,
  pageUrl
) {
  const socket = new WebSocket(webSocketUrl);
  await waitForWebSocketOpen(socket, CDP_COMMAND_TIMEOUT_MS);
  const attachedTargets = [];
  const targetBySession = new Map();
  const destroyedTargets = new Map();
  const detachedTargets = new Map();
  const scriptsBySession = new Map();
  const pausesBySession = new Map();
  const resumesBySession = new Map();
  const networkRequests = [];
  const networkFailures = [];
  const networkResponses = [];
  const fetchPauses = [];
  const workerContexts = [];
  const workerInstrumentation = new Map();
  let pageSession = null;
  let eventFailure = null;
  let cdpEvidenceEntries = 0;
  let evidenceSequence = 0;
  let command = null;
  const nextEvidenceSequence = () => {
    evidenceSequence += 1;
    if (evidenceSequence > 1_000_000) {
      throw new Error("network evidence sequence exceeded its bound");
    }
    return evidenceSequence;
  };
  const reserveEvidenceEntry = (label) => {
    if (cdpEvidenceEntries >= MAX_CDP_EVENTS) {
      eventFailure ??= new Error(`${label} exceeded the global ${MAX_CDP_EVENTS}-event bound`);
      return false;
    }
    cdpEvidenceEntries += 1;
    return true;
  };
  const boundedPush = (array, value, label) => {
    if (array.length >= MAX_CDP_EVENTS || !reserveEvidenceEntry(label)) {
      eventFailure ??= new Error(`${label} exceeded ${MAX_CDP_EVENTS} events`);
      return false;
    }
    array.push(value);
    return true;
  };
  const recordTargetEvent = (events, targetId, label) => {
    if (
      typeof targetId === "string" &&
      !events.has(targetId) &&
      reserveEvidenceEntry(label)
    ) {
      events.set(targetId, Date.now());
    }
  };
  const boundedPushByKey = (entries, key, value, label) => {
    const existing = entries.get(key);
    if (
      (existing?.length ?? 0) >= MAX_CDP_EVENTS ||
      !reserveEvidenceEntry(label)
    ) {
      eventFailure ??= new Error(`${label} exceeded ${MAX_CDP_EVENTS} events`);
      return;
    }
    const target = existing ?? [];
    if (!existing) {
      entries.set(key, target);
    }
    target.push(value);
  };
  const requestUrlForIdentity = (requestId, sessionId) => {
    for (let index = networkRequests.length - 1; index >= 0; index -= 1) {
      const request = networkRequests[index];
      if (
        request.requestId === requestId &&
        request.sessionId === sessionId
      ) {
        return request.url;
      }
    }
    return null;
  };
  const instrumentWorker = (attached) => {
    const targetId = attached.targetInfo?.targetId;
    const sessionId = attached.sessionId;
    if (
      typeof targetId !== "string" ||
      typeof sessionId !== "string" ||
      attached.targetInfo?.type !== "worker" ||
      command === null
    ) {
      throw new Error("worker auto-attach evidence is invalid");
    }
    if (workerInstrumentation.has(targetId)) {
      throw new Error("worker target was instrumented more than once");
    }
    const context = {
      targetId,
      sessionId,
      type: "worker",
      url: attached.targetInfo.url,
      attachedSequence: nextEvidenceSequence(),
      runtimeEnabledSequence: null,
      networkEnabledSequence: null,
      resumedSequence: null
    };
    workerContexts.push(context);
    const instrumentation = (async () => {
      for (const {
        method,
        params
      } of renderWorkerNetworkInstrumentationCommands()) {
        await command(method, params, sessionId);
        if (method === "Runtime.enable") {
          context.runtimeEnabledSequence = nextEvidenceSequence();
        } else if (method === "Network.enable") {
          context.networkEnabledSequence = nextEvidenceSequence();
        }
      }
      await command("Runtime.runIfWaitingForDebugger", {}, sessionId);
      context.resumedSequence = nextEvidenceSequence();
      return context;
    })();
    const tracked = instrumentation.catch((error) => {
      eventFailure ??= error;
      throw error;
    });
    void tracked.catch(() => {});
    workerInstrumentation.set(targetId, tracked);
  };
  const client = createCdpClient(socket, {
    commandTimeoutMs: CDP_COMMAND_TIMEOUT_MS,
    onEvent(message) {
      if (message.method === "Target.attachedToTarget") {
        const attached = {
          ...message.params,
          observedAtEpochMs: Date.now()
        };
        const recorded = boundedPush(
          attachedTargets,
          attached,
          "attached-target evidence"
        );
        if (recorded && attached.sessionId && attached.targetInfo?.targetId) {
          targetBySession.set(attached.sessionId, attached.targetInfo.targetId);
          if (attached.targetInfo.type === "worker") {
            try {
              instrumentWorker(attached);
            } catch (error) {
              eventFailure ??= error;
            }
          }
        }
      } else if (message.method === "Target.detachedFromTarget") {
        const targetId =
          message.params?.targetId ?? targetBySession.get(message.params?.sessionId);
        recordTargetEvent(detachedTargets, targetId, "detached-target evidence");
      } else if (message.method === "Target.targetDestroyed") {
        recordTargetEvent(
          destroyedTargets,
          message.params?.targetId,
          "destroyed-target evidence"
        );
      } else if (message.method === "Debugger.scriptParsed" && message.sessionId) {
        let scripts = scriptsBySession.get(message.sessionId);
        if (scripts?.has(message.params.scriptId)) {
          scripts.set(message.params.scriptId, {
            url: message.params.url,
            scriptLanguage: message.params.scriptLanguage
          });
        } else if (
          (scripts?.size ?? 0) >= MAX_CDP_EVENTS ||
          !reserveEvidenceEntry("Debugger script evidence")
        ) {
          eventFailure ??= new Error("Debugger script evidence exceeded its bound");
        } else {
          if (!scripts) {
            scripts = new Map();
            scriptsBySession.set(message.sessionId, scripts);
          }
          scripts.set(message.params.scriptId, {
            url: message.params.url,
            scriptLanguage: message.params.scriptLanguage
          });
        }
      } else if (message.method === "Debugger.paused" && message.sessionId) {
        boundedPushByKey(
          pausesBySession,
          message.sessionId,
          { ...message.params, observedAtEpochMs: Date.now() },
          "Debugger pause evidence"
        );
      } else if (message.method === "Debugger.resumed" && message.sessionId) {
        boundedPushByKey(
          resumesBySession,
          message.sessionId,
          { observedAtEpochMs: Date.now() },
          "Debugger resume evidence"
        );
      } else if (message.method === "Network.requestWillBeSent") {
        boundedPush(
          networkRequests,
          {
            sequence: nextEvidenceSequence(),
            requestId: message.params.requestId,
            sessionId: message.sessionId ?? null,
            method: message.params.request?.method,
            url: message.params.request?.url
          },
          "Network request evidence"
        );
      } else if (message.method === "Network.loadingFailed") {
        const sessionId = message.sessionId ?? null;
        boundedPush(
          networkFailures,
          {
            requestId: message.params.requestId,
            sessionId,
            url: requestUrlForIdentity(message.params.requestId, sessionId),
            blockedReason: message.params.blockedReason ?? null,
            canceled: message.params.canceled ?? false,
            errorText: message.params.errorText
          },
          "Network failure evidence"
        );
      } else if (message.method === "Network.responseReceived") {
        boundedPush(
          networkResponses,
          {
            requestId: message.params.requestId,
            sessionId: message.sessionId ?? null,
            status: message.params.response?.status,
            url: message.params.response?.url
          },
          "Network response evidence"
        );
      } else if (message.method === "Fetch.requestPaused") {
        boundedPush(
          fetchPauses,
          {
            requestId: message.params.requestId,
            sessionId: message.sessionId ?? null,
            networkId: message.params.networkId ?? null,
            url: message.params.request?.url,
            resourceType: message.params.resourceType
          },
          "Fetch interception evidence"
        );
        if (
          message.params.request?.url === RENDER_WORKER_CSP_NEGATIVE_URL &&
          typeof message.sessionId === "string" &&
          command !== null
        ) {
          void command(
            "Fetch.failRequest",
            {
              requestId: message.params.requestId,
              errorReason: "BlockedByClient"
            },
            message.sessionId
          ).catch((error) => {
            eventFailure ??= error;
          });
        }
      }
    }
  });
  command = client.command;
  const browserDeadline = setTimeout(() => {
    client.abort(
      new OperationTimeoutError("Chromium DevTools browser smoke", BROWSER_RUN_TIMEOUT_MS)
    );
    socket.close();
  }, BROWSER_RUN_TIMEOUT_MS);
  const attach = async (targetId) =>
    (await command("Target.attachToTarget", { targetId, flatten: true })).sessionId;
  try {
    pageSession = await attach(pageTargetId);
    const evaluate = (expression) =>
      command("Runtime.evaluate", { expression, returnByValue: true }, pageSession);
    await command("Target.setDiscoverTargets", { discover: true });
    await command("Runtime.enable", {}, pageSession);
    await command("HeapProfiler.enable", {}, pageSession);
    await command("Page.enable", {}, pageSession);
    await command("Network.enable", {
      maxTotalBufferSize: 1024 * 1024,
      maxResourceBufferSize: 256 * 1024,
      maxPostDataSize: 64 * 1024
    }, pageSession);
    const networkEnabledSequence = nextEvidenceSequence();
    const pageFetchInstrumentation =
      renderPageCspFetchInstrumentationCommand();
    await command(
      pageFetchInstrumentation.method,
      pageFetchInstrumentation.params,
      pageSession
    );
    const pageFetchEnabledSequence = nextEvidenceSequence();
    await command(
      "Target.setAutoAttach",
      {
        autoAttach: true,
        waitForDebuggerOnStart: true,
        flatten: true,
        filter: [
          { type: "worker", exclude: false },
          { exclude: true }
        ]
      },
      pageSession
    );
    const autoAttachEnabledSequence = nextEvidenceSequence();
    const navigationSequence = nextEvidenceSequence();
    const navigation = await command(
      "Page.navigate",
      { url: pageUrl },
      pageSession
    );
    if (typeof navigation.errorText === "string") {
      throw new Error(`browser navigation failed: ${navigation.errorText}`);
    }
    const preNavigationInstrumentation = {
      schema: PRE_NAVIGATION_PROOF_SCHEMA,
      pageSessionId: pageSession,
      networkEnabledSequence,
      pageFetchEnabledSequence,
      autoAttachEnabledSequence,
      autoAttachWaitForDebugger: true,
      navigationSequence
    };
    const workerTarget = await waitForWorkerTarget(
      attachedTargets,
      workerInstrumentation
    );
    const primaryTargetId = workerTarget.targetInfo.targetId;
    const heapEnabledSessions = new Set();
    const ensureHeapEnabled = async (target) => {
      if (heapEnabledSessions.has(target.sessionId)) {
        return;
      }
      await command("Runtime.enable", {}, target.sessionId);
      await command("HeapProfiler.enable", {}, target.sessionId);
      heapEnabledSessions.add(target.sessionId);
    };
    const liveWorkerTargets = () => {
      const targets = new Map();
      for (const attached of attachedTargets) {
        const targetId = attached.targetInfo?.targetId;
        if (
          attached.targetInfo?.type === "worker" &&
          typeof targetId === "string" &&
          !destroyedTargets.has(targetId)
        ) {
          targets.set(targetId, attached);
        }
      }
      return [...targets.values()];
    };
    const collectAllGarbage = async () => {
      await command("HeapProfiler.collectGarbage", {}, pageSession);
      for (const target of liveWorkerTargets()) {
        await ensureHeapEnabled(target);
        await command("HeapProfiler.collectGarbage", {}, target.sessionId);
      }
    };
    const sampleAllHeaps = async () => {
      const workers = liveWorkerTargets();
      if (
        workers.length === 0 ||
        !workers.some(({ targetInfo }) => targetInfo.targetId === primaryTargetId)
      ) {
        throw new Error("heap sampling lost the primary render worker");
      }
      await Promise.all(workers.map(ensureHeapEnabled));
      const samples = [
        {
          label: "page",
          sample: await command("Runtime.getHeapUsage", {}, pageSession)
        }
      ];
      for (const target of workers) {
        samples.push({
          label: `worker:${target.targetInfo.targetId}`,
          sample: await command("Runtime.getHeapUsage", {}, target.sessionId)
        });
      }
      return combineHeaps(samples);
    };
    await ensureHeapEnabled(workerTarget);
    await waitForWorkerProbeReady(evaluate);
    await collectAllGarbage();
    const baseline = await sampleAllHeaps();
    let peak = baseline;
    const processBaseline = sampleProcessTreeRss(browserRootPid);
    let processPeak = processBaseline;
    const sampleEvidence = async () => {
      if (eventFailure) {
        throw eventFailure;
      }
      peak = largerHeap(peak, await sampleAllHeaps());
      const processSample = sampleProcessTreeRss(browserRootPid);
      if (processSample.rssBytes > processPeak.rssBytes) {
        processPeak = processSample;
      }
    };
    let lastValue = "";
    let hardStop = null;
    let pendingBoundary = null;
    let workerCsp = null;
    let normalizedWorkerContexts = null;
    await evaluate("globalThis.__rxlsHeapProbeReady = true");
    for (let attempt = 0; attempt < 300; attempt += 1) {
      const state = await evaluateJson(
        evaluate,
        "JSON.stringify({result: document.querySelector('pre')?.textContent ?? '', hardStop: globalThis.__rxlsHardStopProof ?? null, pendingBoundary: globalThis.__rxlsPendingBoundaryProof ?? null})",
        "browser smoke state"
      );
      const value = state.result ?? "";
      lastValue = value;
      if (value.startsWith("FAIL ")) {
        return {
          message: value,
          heap: null,
          processMemory: null,
          hardStop: null,
          pendingBoundary: null,
          csp: null,
          networkProof: null,
          behavior: null,
          behaviorJson: null
        };
      }
      await sampleEvidence();
      if (
        pendingBoundary === null &&
        state.pendingBoundary?.phase === "held"
      ) {
        pendingBoundary = await drivePendingResourceBoundary({
          evaluate,
          browserRootPid,
          processBaseline,
          proof: state.pendingBoundary
        });
        if (
          pendingBoundary.sampler.peak.rssBytes > processPeak.rssBytes
        ) {
          processPeak = pendingBoundary.sampler.peak;
        }
      }
      if (hardStop === null && state.hardStop?.phase === "armed") {
        const nonceTarget = await waitForNonceBoundWorker({
          attachedTargets,
          primaryTargetId,
          proof: state.hardStop
        });
        const nonceInstrumentation = workerInstrumentation.get(
          nonceTarget.targetInfo.targetId
        );
        if (nonceInstrumentation === undefined) {
          throw new Error("nonce-bound worker was not instrumented before resume");
        }
        await nonceInstrumentation;
        if (workerContexts.length !== 2) {
          throw new Error(
            `expected exactly two render workers; observed ${workerContexts.length}`
          );
        }
        await Promise.all(workerInstrumentation.values());
        normalizedWorkerContexts = normalizeRenderWorkerContexts({
          mode: routeMode,
          pageUrl,
          hardStopNonce: state.hardStop.nonce,
          instrumentation: preNavigationInstrumentation,
          contexts: workerContexts
        });
        workerCsp = await runRenderWorkerCspProbes({
          command,
          contexts: normalizedWorkerContexts,
          networkRequests,
          networkFailures,
          networkResponses,
          fetchPauses,
          nextEvidenceSequence
        });
        hardStop = await driveHardStop({
          command,
          evaluate,
          attachedTargets,
          primaryTargetId,
          destroyedTargets,
          scriptsBySession,
          pausesBySession,
          resumesBySession,
          proof: state.hardStop,
          sampleEvidence
        });
      }
      if (value.startsWith("PASS ") || value.startsWith("FAIL ")) {
        if (value.startsWith("PASS ") && hardStop === null) {
          throw new Error("browser passed without nonce-bound hard-stop evidence");
        }
        if (value.startsWith("PASS ") && pendingBoundary === null) {
          throw new Error(
            "browser passed without independent pending-resource RSS evidence"
          );
        }
        let csp = null;
        let networkProof = null;
        let behavior = null;
        let behaviorJson = null;
        if (value.startsWith("PASS ")) {
          behavior = await evaluateJson(
            evaluate,
            "JSON.stringify(globalThis.__rxlsBehaviorProof)",
            "browser behavior proof"
          );
          behaviorJson = validateBrowserBehaviorProof(behavior);
          const completedPendingBoundary = await evaluateJson(
            evaluate,
            "JSON.stringify(globalThis.__rxlsPendingBoundaryProof)",
            "pending-resource browser proof"
          );
          validatePendingResourceBoundaryProof(
            completedPendingBoundary,
            "complete"
          );
          if (
            completedPendingBoundary.nonce !==
              pendingBoundary.browser.nonce ||
            completedPendingBoundary.heldEpochMs !==
              pendingBoundary.browser.heldEpochMs ||
            completedPendingBoundary.releaseEpochMs <
              pendingBoundary.sampler.boundaryEndEpochMs
          ) {
            throw new Error(
              "pending-resource browser and RSS evidence are not correlated"
            );
          }
          pendingBoundary.browser = completedPendingBoundary;
          const cspProof = await evaluateJson(
            evaluate,
            "JSON.stringify(globalThis.__rxlsCspProof)",
            "CSP page proof"
          );
          const pageControl = validateCspNetworkSilence({
            proof: cspProof,
            requests: networkRequests,
            responses: networkResponses,
            failures: networkFailures,
            pauses: fetchPauses
          });
          if (
            workerCsp === null ||
            normalizedWorkerContexts === null ||
            hardStop === null
          ) {
            throw new Error("browser passed without complete render-worker network proof");
          }
          if (serverHardStopNonce !== hardStop.nonce) {
            throw new Error("served hard-stop worker route was not nonce-correlated");
          }
          const routes = await waitForRouteProof({
            mode: routeMode,
            pageUrl,
            hardStopNonce: hardStop.nonce,
            instrumentation: preNavigationInstrumentation,
            contexts: normalizedWorkerContexts,
            requests: networkRequests
          });
          if (
            routes.requestCount !== EXPECTED_ROUTE_REQUESTS ||
            workerCsp.workers.length !== 2
          ) {
            throw new Error("browser network proof has an unexpected cardinality");
          }
          networkProof = {
            routes,
            workers: workerCsp
          };
          const networkControl = await driveOfflineNetworkControl({
            command,
            requests: networkRequests,
            failures: networkFailures,
            responses: networkResponses,
            pauses: fetchPauses,
            controlUrl: networkControlUrl,
            sampleProcessEvidence() {
              const sample = sampleProcessTreeRss(browserRootPid);
              if (sample.rssBytes > processPeak.rssBytes) {
                processPeak = sample;
              }
            }
          });
          if (eventFailure) {
            throw eventFailure;
          }
          await delay(25);
          validateFinalHttpRequestSet({
            requests: networkRequests,
            routes,
            networkControl
          });
          csp = {
            pageControl,
            networkControl,
            blockedReason: "csp+offline"
          };
        }
        await collectAllGarbage();
        if (eventFailure) {
          throw eventFailure;
        }
        const retained = await sampleAllHeaps();
        peak = largerHeap(peak, retained);
        const retainedGrowthBytes = Math.max(0, retained.accountedBytes - baseline.accountedBytes);
        const heap = { baseline, peak, retained, retainedGrowthBytes };
        const processRetained = sampleProcessTreeRss(browserRootPid);
        if (processRetained.rssBytes > processPeak.rssBytes) {
          processPeak = processRetained;
        }
        const processMemory = summarizeProcessMemory(
          {
            baseline: processBaseline,
            peak: processPeak,
            retained: processRetained
          },
          processMemoryGate
        );
        await evaluate("globalThis.__rxlsHeapProbeRelease = true");
        if (peak.accountedBytes > heapGate.maxAccountedBytes) {
          return {
            message: `FAIL heap_limit: accounted heap peak ${peak.accountedBytes} exceeds ${heapGate.maxAccountedBytes}`,
            heap,
            processMemory,
            hardStop,
            pendingBoundary,
            csp,
            networkProof
          };
        }
        if (retainedGrowthBytes > heapGate.maxRetainedGrowthBytes) {
          return {
            message: `FAIL heap_retention: retained growth ${retainedGrowthBytes} exceeds ${heapGate.maxRetainedGrowthBytes}`,
            heap,
            processMemory,
            hardStop,
            pendingBoundary,
            csp,
            networkProof
          };
        }
        return {
          message: value,
          heap,
          processMemory,
          hardStop,
          pendingBoundary,
          csp,
          networkProof,
          behavior,
          behaviorJson
        };
      }
      await delay(50);
    }
    const diagnostic = await evaluate(
      "JSON.stringify({href: location.href, title: document.title, body: document.body?.innerText ?? null})"
    );
    const detail = diagnostic.result?.value ?? "no diagnostic";
    return {
      message: `FAIL timeout: browser smoke did not complete (${lastValue}; ${detail})`,
      heap: null,
      processMemory: null,
      hardStop: null,
      pendingBoundary: null,
      csp: null,
      networkProof: null,
      behavior: null,
      behaviorJson: null
    };
  } finally {
    clearTimeout(browserDeadline);
    client.dispose();
    socket.close();
  }
}

async function runRenderWorkerCspProbes({
  command,
  contexts,
  networkRequests,
  networkFailures,
  networkResponses,
  fetchPauses,
  nextEvidenceSequence
}) {
  const proofs = [];
  for (const context of contexts) {
    const requestStart = networkRequests.length;
    const failureStart = networkFailures.length;
    const responseStart = networkResponses.length;
    const pauseStart = fetchPauses.length;
    const probeStartedSequence = nextEvidenceSequence();
    const evaluation = await command(
      "Runtime.evaluate",
      {
        expression: createRenderWorkerCspProbeExpression(),
        awaitPromise: true,
        returnByValue: true
      },
      context.sessionId
    );
    if (evaluation.exceptionDetails !== undefined) {
      throw new Error("render-worker CSP probe threw an exception");
    }
    await delay(25);
    const probeCompletedSequence = nextEvidenceSequence();
    const proof = evaluation.result?.value;
    const requests = networkRequests
      .slice(requestStart)
      .filter(({ url }) => url === RENDER_WORKER_CSP_NEGATIVE_URL);
    const failures = networkFailures
      .slice(failureStart)
      .filter(({ url }) => url === RENDER_WORKER_CSP_NEGATIVE_URL);
    const responses = networkResponses
      .slice(responseStart)
      .filter(({ url }) => url === RENDER_WORKER_CSP_NEGATIVE_URL);
    const pauses = fetchPauses
      .slice(pauseStart)
      .filter(({ url }) => url === RENDER_WORKER_CSP_NEGATIVE_URL);
    proofs.push({
      targetId: context.targetId,
      sessionId: context.sessionId,
      probeStartedSequence,
      probeCompletedSequence,
      proof,
      network: { requests, responses, failures, pauses }
    });
  }
  return validateRenderWorkerCspEvidence({ contexts, proofs });
}

async function waitForRouteProof({
  mode,
  pageUrl,
  hardStopNonce,
  instrumentation,
  contexts,
  requests
}) {
  let lastRoutes = [];
  for (let attempt = 0; attempt < 100; attempt += 1) {
    lastRoutes = requests
      .filter(({ url }) => {
        try {
          return ["http:", "https:"].includes(new URL(url).protocol);
        } catch {
          return false;
        }
      })
      .map(({ sequence, requestId, sessionId, method, url }) => ({
        sequence,
        requestId: `${sessionId ?? "page"}:${requestId}`,
        sessionId: sessionId ?? "missing-session",
        method,
        url
      }));
    if (lastRoutes.length === EXPECTED_ROUTE_REQUESTS) {
      return validateSameOriginRouteEvidence({
        mode,
        pageUrl,
        hardStopNonce,
        instrumentation,
        contexts,
        routes: lastRoutes
      });
    }
    if (lastRoutes.length > EXPECTED_ROUTE_REQUESTS) {
      break;
    }
    await delay(10);
  }
  try {
    return validateSameOriginRouteEvidence({
      mode,
      pageUrl,
      hardStopNonce,
      instrumentation,
      contexts,
      routes: lastRoutes
    });
  } catch (error) {
    throw new Error(
      `${error.message}; observed ${lastRoutes.length} same-origin requests`
    );
  }
}

function validateFinalHttpRequestSet({ requests, routes, networkControl }) {
  const routeIdentities = new Set(
    routes.routes.map(({ requestId }) => requestId)
  );
  const httpRequests = requests.filter(({ url }) => {
    try {
      return ["http:", "https:"].includes(new URL(url).protocol);
    } catch {
      return false;
    }
  });
  const controls = httpRequests.filter(
    ({ requestId, url }) =>
      requestId === networkControl.requestId && url === networkControl.url
  );
  if (controls.length !== 1) {
    throw new Error("final network control request identity is missing or duplicated");
  }
  for (const request of httpRequests) {
    if (request === controls[0]) {
      continue;
    }
    const identity = `${request.sessionId ?? "page"}:${request.requestId}`;
    if (!routeIdentities.has(identity)) {
      throw new Error("unexpected HTTP(S) request appeared after route validation");
    }
  }
  if (httpRequests.length !== routes.requestCount + 1) {
    throw new Error("final HTTP(S) request cardinality is invalid");
  }
}

async function drivePendingResourceBoundary({
  evaluate,
  browserRootPid,
  processBaseline,
  proof
}) {
  validatePendingResourceBoundaryProof(proof, "held");
  const sampler = new IndependentRssSampler(browserRootPid, {
    intervalMs: RSS_SAMPLE_INTERVAL_MS
  });
  let samples;
  try {
    await sampler.waitForSamplesSince(
      proof.heldEpochMs,
      MIN_BOUNDARY_RSS_SAMPLES
    );
    samples = await sampler.stop();
  } catch (error) {
    try {
      await sampler.stop();
    } catch (stopError) {
      throw new AggregateError(
        [error, stopError],
        "independent RSS boundary sampling and cleanup failed"
      );
    }
    throw error;
  }
  const boundaryEndEpochMs = Date.now();
  const summary = summarizeIndependentRssWindow(samples, {
    rootPid: browserRootPid,
    intervalMs: RSS_SAMPLE_INTERVAL_MS,
    boundaryStartEpochMs: proof.heldEpochMs,
    boundaryEndEpochMs,
    minimumBoundarySamples: MIN_BOUNDARY_RSS_SAMPLES
  });
  const materiality = summarizePendingBoundaryRssMateriality(
    processBaseline,
    summary.peak
  );
  await evaluate(
    `globalThis.__rxlsPendingBoundaryRelease = ${JSON.stringify(proof.nonce)}`
  );
  return {
    browser: {
      nonce: proof.nonce,
      heldEpochMs: proof.heldEpochMs
    },
    sampler: { ...summary, ...materiality }
  };
}

async function driveOfflineNetworkControl({
  command,
  requests,
  failures,
  responses,
  pauses,
  controlUrl,
  sampleProcessEvidence
}) {
  const requestStart = requests.length;
  const failureStart = failures.length;
  const responseStart = responses.length;
  const pauseStart = pauses.length;
  const { targetId } = await command("Target.createTarget", {
    url: "about:blank",
    background: true
  });
  if (typeof targetId !== "string" || targetId.length === 0) {
    throw new Error("CDP did not create the isolated network-control target");
  }
  let controlSession = null;
  let evaluation;
  let evaluationState = { status: "not-started" };
  try {
    controlSession = (
      await command("Target.attachToTarget", { targetId, flatten: true })
    ).sessionId;
    if (typeof controlSession !== "string" || controlSession.length === 0) {
      throw new Error("CDP did not attach the isolated network-control target");
    }
    await command("Runtime.enable", {}, controlSession);
    await command(
      "Network.enable",
      {
        maxTotalBufferSize: 64 * 1024,
        maxResourceBufferSize: 32 * 1024,
        maxPostDataSize: 4 * 1024
      },
      controlSession
    );
    await command(
      "Fetch.enable",
      {
        patterns: [{
          urlPattern: controlUrl,
          requestStage: "Request"
        }],
        handleAuthRequests: false
      },
      controlSession
    );
    const evaluationPromise = command(
      "Runtime.evaluate",
      {
        expression:
          `(async () => { try { await fetch(${JSON.stringify(controlUrl)}, ` +
          `{ cache: "no-store", credentials: "omit", mode: "no-cors", ` +
          `referrerPolicy: "no-referrer" }); ` +
          `return { rejected: false }; } catch (error) { ` +
          `return { rejected: true, name: error?.name ?? null, ` +
          `message: error?.message ?? null }; } })()`,
        awaitPromise: true,
        returnByValue: true
      },
      controlSession
    );
    evaluationState = { status: "pending" };
    void evaluationPromise.then(
      (value) => {
        evaluationState = { status: "fulfilled", value };
      },
      (error) => {
        evaluationState = { status: "rejected", message: error.message };
      }
    );
    let pause;
    for (let attempt = 0; attempt < 100; attempt += 1) {
      const matchingPauses = pauses
        .slice(pauseStart)
        .filter(({ url }) => url === controlUrl);
      if (matchingPauses.length > 1) {
        throw new Error("CDP Fetch intercepted the network control more than once");
      }
      if (matchingPauses.length === 1) {
        [pause] = matchingPauses;
        break;
      }
      await delay(10);
    }
    if (!pause) {
      throw new Error(
        `CDP Fetch did not intercept the network control; evaluation=${JSON.stringify(evaluationState).slice(0, 2_048)}`
      );
    }
    await command(
      "Fetch.failRequest",
      {
        requestId: pause.requestId,
        errorReason: "InternetDisconnected"
      },
      controlSession
    );
    evaluation = await evaluationPromise;
    await sampleProcessEvidence();
  } finally {
    try {
      if (controlSession !== null) {
        await command("Fetch.disable", {}, controlSession);
      }
    } finally {
      const closed = await command("Target.closeTarget", { targetId });
      if (closed.success !== true) {
        throw new Error("CDP did not close the isolated network-control target");
      }
    }
  }
  if (
    evaluation?.result?.value?.rejected !== true ||
    evaluation.result.value.name !== "TypeError"
  ) {
    throw new Error("CDP Network negative-control fetch was not rejected");
  }
  for (let attempt = 0; attempt < 100; attempt += 1) {
    const evidence = {
      requests: requests.slice(requestStart),
      failures: failures.slice(failureStart),
      responses: responses.slice(responseStart),
      pauses: pauses.slice(pauseStart)
    };
    try {
      return validateOfflineNetworkBlock(evidence, controlUrl);
    } catch (error) {
      if (attempt === 99) {
        throw new Error(
          `${error.message}; evidence=${JSON.stringify(evidence).slice(0, 8_192)}`
        );
      }
    }
    await delay(10);
  }
  throw new Error("CDP Network negative-control evidence did not arrive");
}

async function driveHardStop({
  command,
  evaluate,
  attachedTargets,
  primaryTargetId,
  destroyedTargets,
  scriptsBySession,
  pausesBySession,
  resumesBySession,
  proof,
  sampleEvidence
}) {
  const target = await waitForNonceBoundWorker({
    attachedTargets,
    primaryTargetId,
    proof
  });
  const sessionId = target.sessionId;
  await command("Debugger.enable", { maxScriptsCacheSize: 1024 * 1024 }, sessionId);
  await sampleEvidence();
  await evaluate(
    `globalThis.__rxlsHardStopBegin = ${JSON.stringify(proof.nonce)}`
  );
  const activeProof = await waitForPageProofPhase(evaluate, ["wasm-active", "failed"]);
  if (activeProof.phase === "failed") {
    throw new Error(activeProof.failure ?? "hard-stop proof failed before WASM activation");
  }
  const activeObservedAtEpochMs = Date.now();
  await command("Debugger.pause", {}, sessionId);
  const pause = await waitForDebuggerPause(pausesBySession, sessionId);
  const scripts = scriptsBySession.get(sessionId) ?? new Map();
  const frames = pause.callFrames.map((frame) => {
    const script = scripts.get(frame.location?.scriptId) ?? {};
    return {
      functionName: frame.functionName,
      scriptId: frame.location?.scriptId,
      url: frame.url || script.url || "",
      scriptLanguage: script.scriptLanguage
    };
  });
  const wasmFrame = frames.find(
    (frame) =>
      frame.url.startsWith("wasm://") || frame.scriptLanguage === "WebAssembly"
  );
  const pauseEvidence = {
    targetId: target.targetInfo.targetId,
    sessionId,
    activeObservedAtEpochMs,
    observedAtEpochMs: pause.observedAtEpochMs,
    wasmFrame,
    resumedAtEpochMs: null,
    debuggerDisabledAtEpochMs: null,
    terminationCommandEpochMs: null
  };
  if (!wasmFrame) {
    throw new Error(`Debugger pause had no WASM frame: ${JSON.stringify(frames.slice(0, 16))}`);
  }
  await sampleEvidence();
  const priorResumeCount = resumesBySession.get(sessionId)?.length ?? 0;
  await command("Debugger.resume", {}, sessionId);
  const resume = await waitForDebuggerResume(
    resumesBySession,
    sessionId,
    priorResumeCount
  );
  pauseEvidence.resumedAtEpochMs = resume.observedAtEpochMs;
  await command("Debugger.disable", {}, sessionId);
  pauseEvidence.debuggerDisabledAtEpochMs = Date.now();
  pauseEvidence.terminationCommandEpochMs = Date.now();
  await evaluate(
    `globalThis.__rxlsHardStopTerminate = ${JSON.stringify(proof.nonce)}`
  );
  const completedProof = await waitForPageProofPhase(evaluate, ["complete", "failed"]);
  if (completedProof.phase === "failed") {
    throw new Error(completedProof.failure ?? "hard-stop page proof failed");
  }
  const observationDeadline = hardStopObservationDeadlineEpochMs(
    completedProof,
    500
  );
  while (true) {
    const observation = decideHardStopObservation({
      destructionRecorded: destroyedTargets.has(target.targetInfo.targetId),
      nowEpochMs: Date.now(),
      observationDeadlineEpochMs: observationDeadline
    });
    if (observation === "inspect") {
      const inventory = await command("Target.getTargets");
      const evidence = correlateHardStopTarget({
        attachedTargets,
        primaryTargetId,
        destroyedTargets,
        currentTargets: inventory.targetInfos,
        pauseEvidence,
        proof: completedProof
      });
      if (evidence !== null) {
        return evidence;
      }
      throw new Error("recorded hard-stop destruction could not be correlated");
    }
    if (observation === "expired") {
      break;
    }
    await delay(10);
  }
  throw new Error("nonce-bound hard-stop target was not destroyed by its deadline");
}

async function waitForNonceBoundWorker({ attachedTargets, primaryTargetId, proof }) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    const target = findNonceBoundWorker({
      attachedTargets,
      primaryTargetId,
      proof
    });
    if (target !== null) {
      return target;
    }
    await delay(10);
  }
  throw new Error("nonce-bound hard-stop worker target did not attach");
}

async function waitForPageProofPhase(evaluate, phases) {
  for (let attempt = 0; attempt < 500; attempt += 1) {
    const proof = await evaluateJson(
      evaluate,
      "JSON.stringify(globalThis.__rxlsHardStopProof)",
      "hard-stop page proof"
    );
    if (phases.includes(proof?.phase)) {
      return proof;
    }
    await delay(5);
  }
  throw new Error(`hard-stop page proof did not reach ${phases.join(" or ")}`);
}

async function waitForDebuggerPause(pausesBySession, sessionId) {
  for (let attempt = 0; attempt < 500; attempt += 1) {
    const pause = pausesBySession.get(sessionId)?.[0];
    if (pause) {
      return pause;
    }
    await delay(5);
  }
  throw new Error("Debugger.pause did not pause the nonce-bound worker");
}

async function waitForDebuggerResume(resumesBySession, sessionId, priorResumeCount) {
  for (let attempt = 0; attempt < 500; attempt += 1) {
    const resume = resumesBySession.get(sessionId)?.[priorResumeCount];
    if (resume) {
      return resume;
    }
    await delay(5);
  }
  throw new Error("Debugger.resume did not resume the nonce-bound worker");
}

async function evaluateJson(evaluate, expression, label) {
  const response = await evaluate(expression);
  const value = response.result?.value;
  if (typeof value !== "string" || Buffer.byteLength(value) > 64 * 1024) {
    throw new Error(`${label} was missing or exceeded its byte bound`);
  }
  try {
    return JSON.parse(value);
  } catch {
    throw new Error(`${label} was not valid JSON`);
  }
}

async function fetchJson(url, label, timeoutMs = CDP_HTTP_TIMEOUT_MS) {
  const controller = new AbortController();
  const response = await withTimeout(
    fetch(url, { signal: controller.signal }),
    timeoutMs,
    label,
    () => controller.abort()
  );
  if (!response.ok) {
    throw new Error(`${label} returned HTTP ${response.status}`);
  }
  const declaredLength = response.headers.get("content-length");
  if (
    declaredLength !== null &&
    (!/^\d+$/.test(declaredLength) ||
      Number(declaredLength) > MAX_CDP_HTTP_RESPONSE_BYTES)
  ) {
    controller.abort();
    throw new Error(`${label} exceeded its response byte bound`);
  }
  return withTimeout(
    readBoundedJson(response, label),
    timeoutMs,
    `${label} JSON`,
    () => controller.abort()
  );
}

async function readBoundedJson(response, label) {
  if (response.body === null) {
    throw new Error(`${label} had no response body`);
  }
  const reader = response.body.getReader();
  const chunks = [];
  let total = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      if (!(value instanceof Uint8Array) || total > MAX_CDP_HTTP_RESPONSE_BYTES - value.byteLength) {
        await reader.cancel();
        throw new Error(`${label} exceeded its response byte bound`);
      }
      chunks.push(value);
      total += value.byteLength;
    }
  } finally {
    reader.releaseLock();
  }
  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  let text;
  try {
    text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    throw new Error(`${label} was not valid UTF-8`);
  }
  try {
    return JSON.parse(text);
  } catch {
    throw new Error(`${label} was not valid JSON`);
  }
}

async function waitForWorkerTarget(attachedTargets, workerInstrumentation) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    const worker = attachedTargets.find(
      ({ targetInfo }) =>
        targetInfo?.type === "worker" && targetInfo.url.includes("worker.mjs")
    );
    if (worker) {
      const instrumentation = workerInstrumentation.get(
        worker.targetInfo.targetId
      );
      if (instrumentation === undefined) {
        throw new Error("primary render worker has no auto-attach instrumentation");
      }
      await instrumentation;
      return worker;
    }
    await delay(50);
  }
  throw new Error(
    `timed out waiting for the dedicated render worker target (${JSON.stringify(
      attachedTargets.map(({ targetInfo }) => ({
        type: targetInfo?.type,
        url: targetInfo?.url
      }))
    )})`
  );
}

async function waitForWorkerProbeReady(evaluate) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    const state = await evaluateJson(
      evaluate,
      "JSON.stringify({ready: globalThis.__rxlsWorkerReadyForHeapProbe === true, result: document.querySelector('pre')?.textContent ?? ''})",
      "worker heap-probe state"
    );
    if (state.ready) {
      return;
    }
    if (state.result?.startsWith("FAIL ")) {
      throw new Error(state.result);
    }
    await delay(50);
  }
  throw new Error("timed out waiting for the initialized worker heap probe");
}

function delay(milliseconds) {
  return new Promise((resolveDelay) => setTimeout(resolveDelay, milliseconds));
}

function safeTarget(root, pathname) {
  const decoded = decodeURIComponent(pathname);
  const target = resolve(root, `.${decoded}`);
  if (target !== root && !target.startsWith(`${root}${sep}`)) {
    throw new Error("unsafe path");
  }
  return target;
}

function validateServerRoute(method, url) {
  if (method !== "GET") {
    throw new Error("only GET is allowed by the browser evidence server");
  }
  if (!allowedRoutePaths.has(url.pathname)) {
    throw new Error("browser requested a route outside the locked allowlist");
  }
  if (url.pathname !== allowedWorkerPath) {
    if (url.search !== "") {
      throw new Error("only the hard-stop worker route accepts a query");
    }
    return;
  }
  if (url.search === "") {
    return;
  }
  const match = /^\?rxls-hard-stop=([0-9a-f]{32})$/.exec(url.search);
  if (
    match === null ||
    url.searchParams.size !== 1 ||
    url.searchParams.get("rxls-hard-stop") !== match[1]
  ) {
    throw new Error("hard-stop worker query is not nonce-only");
  }
  if (serverHardStopNonce === null) {
    serverHardStopNonce = match[1];
  } else if (serverHardStopNonce !== match[1]) {
    throw new Error("hard-stop worker nonce changed during the browser run");
  }
}

function requestTarget(pathname) {
  if (pathname.startsWith("/installed-package/")) {
    if (installedPackageRoot === null) {
      throw new Error("installed package route is unavailable");
    }
    return safeTarget(
      installedPackageRoot,
      pathname.slice("/installed-package".length)
    );
  }
  const sourcePath = pathname === "/" ? "/tests/browser/index.html" : pathname;
  return safeTarget(packageRoot, sourcePath);
}

function contentType(path) {
  switch (extname(path)) {
    case ".html":
      return "text/html; charset=utf-8";
    case ".mjs":
    case ".js":
      return "text/javascript; charset=utf-8";
    case ".wasm":
      return "application/wasm";
    case ".json":
      return "application/json; charset=utf-8";
    default:
      return "application/octet-stream";
  }
}
