import { spawn, spawnSync } from "node:child_process";
import { createServer } from "node:http";
import { access, mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { extname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

import {
  OperationTimeoutError,
  closeServer,
  createCdpClient,
  terminateChild,
  waitForWebSocketOpen,
  withTimeout
} from "./lifecycle.mjs";

const CDP_HTTP_TIMEOUT_MS = 2_000;
const CDP_COMMAND_TIMEOUT_MS = 5_000;
const BROWSER_RUN_TIMEOUT_MS = 30_000;
const CLEANUP_TIMEOUT_MS = 5_000;

const packageRoot = resolve(fileURLToPath(new URL("../..", import.meta.url)));
const installedPackageRoot = process.env.RXLS_RENDER_INSTALLED_PACKAGE_ROOT
  ? resolve(process.env.RXLS_RENDER_INSTALLED_PACKAGE_ROOT)
  : null;
const fixture = fileURLToPath(
  new URL("../../../wasm/tests/fixtures/macro-enabled.xlsm.b64", import.meta.url)
);
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
const version = spawnSync(chrome, ["--version"], { encoding: "utf8" });
const expected = `${lock.chromium.product} ${lock.chromium.version}`;
const acceptedProducts = [lock.chromium.product, lock.chromium.testingProduct].filter(Boolean);
const actualVersion = (version.stdout ?? "").trim();
if (
  version.status !== 0 ||
  !acceptedProducts.some((product) => actualVersion === `${product} ${lock.chromium.version}`)
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
  heapGate.maxRetainedGrowthBytes > heapGate.maxAccountedBytes
) {
  console.error("invalid Chromium heap gate in toolchain-lock.json");
  process.exit(2);
}

const requestedPaths = [];
const server = createServer(async (request, response) => {
  try {
    const url = new URL(request.url, "http://127.0.0.1");
    requestedPaths.push(url.pathname);
    const target = requestTarget(url.pathname);
    const metadata = await stat(target);
    if (!metadata.isFile()) {
      throw new Error("not a file");
    }
    response.writeHead(200, {
      "content-type": contentType(target),
      "cache-control": "no-store",
      "content-security-policy":
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'" +
        (installedPackageRoot === null ? "" : " 'nonce-rxls-installed-package'") +
        "; worker-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'none'; object-src 'none'; base-uri 'none'",
      "cross-origin-opener-policy": "same-origin",
      "x-content-type-options": "nosniff"
    });
    response.end(await readFile(target));
  } catch {
    response.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
    response.end("not found");
  }
});

await new Promise((resolveListen) => server.listen(0, "127.0.0.1", resolveListen));
const address = server.address();
const browserEntry =
  installedPackageRoot === null
    ? "/tests/browser/index.html"
    : "/tests/browser/installed-package.html";
const url = `http://127.0.0.1:${address.port}${browserEntry}`;
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
    "--no-first-run",
    "--js-flags=--max-old-space-size=192",
    "--remote-debugging-port=0",
    `--user-data-dir=${profile}`,
    url
  ],
  { stdio: ["ignore", "ignore", "pipe"] }
);
let stderr = "";
child.stderr.setEncoding("utf8");
child.stderr.on("data", (chunk) => (stderr += chunk));
let browserResult = { message: "", heap: null };
try {
  const portFile = join(profile, "DevToolsActivePort");
  const port = Number.parseInt((await waitForFile(portFile)).split("\n")[0], 10);
  const pages = await waitForPages(port);
  const page = pages.find((entry) => entry.url === url);
  if (!page?.id) {
    throw new Error("browser smoke page did not expose a DevTools endpoint");
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
    page.id
  );
} finally {
  await terminateChild(child);
  await closeServer(server);
  await withTimeout(
    rm(profile, { recursive: true, force: true }),
    CLEANUP_TIMEOUT_MS,
    "Chromium profile cleanup"
  );
}
if (!browserResult.message.startsWith("PASS ")) {
  console.error(`requests: ${requestedPaths.join(", ")}`);
  console.error(browserResult.message || "browser returned no result");
  console.error(stderr.slice(-4_000));
  process.exit(1);
}
console.log(
  `PASS ${actualVersion} ${
    installedPackageRoot === null
      ? "worker/WASM CSP, limits, cancellation, progress, tile and page smoke"
      : "installed package README worker URL and render-page smoke"
  }; ` +
    `heap baseline=${browserResult.heap.baseline.accountedBytes} ` +
    `peak=${browserResult.heap.peak.accountedBytes} ` +
    `retained=${browserResult.heap.retained.accountedBytes} ` +
    `growth=${browserResult.heap.retainedGrowthBytes} bytes`
);

async function waitForFile(path) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    try {
      return await readFile(path, "utf8");
    } catch {
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

async function waitForBrowserResult(webSocketUrl, pageTargetId) {
  const socket = new WebSocket(webSocketUrl);
  await waitForWebSocketOpen(socket, CDP_COMMAND_TIMEOUT_MS);
  const attachedTargets = [];
  const client = createCdpClient(socket, {
    commandTimeoutMs: CDP_COMMAND_TIMEOUT_MS,
    onEvent(message) {
      if (message.method === "Target.attachedToTarget") {
        attachedTargets.push(message.params);
      }
    }
  });
  const command = client.command;
  const browserDeadline = setTimeout(() => {
    client.abort(
      new OperationTimeoutError("Chromium DevTools browser smoke", BROWSER_RUN_TIMEOUT_MS)
    );
    socket.close();
  }, BROWSER_RUN_TIMEOUT_MS);
  const attach = async (targetId) =>
    (await command("Target.attachToTarget", { targetId, flatten: true })).sessionId;
  const sampleHeap = async (sessionId) =>
    normalizeHeap(await command("Runtime.getHeapUsage", {}, sessionId));
  try {
    const pageSession = await attach(pageTargetId);
    const evaluate = (expression) =>
      command("Runtime.evaluate", { expression, returnByValue: true }, pageSession);
    await command("Target.setDiscoverTargets", { discover: true });
    await command("Runtime.enable", {}, pageSession);
    await command("HeapProfiler.enable", {}, pageSession);
    await command(
      "Target.setAutoAttach",
      { autoAttach: true, waitForDebuggerOnStart: false, flatten: true },
      pageSession
    );
    const workerTarget = await waitForWorkerTarget(attachedTargets);
    const workerSession = workerTarget.sessionId;
    await command("Runtime.enable", {}, workerSession);
    await command("HeapProfiler.enable", {}, workerSession);
    await waitForWorkerProbeReady(evaluate);
    await command("HeapProfiler.collectGarbage", {}, pageSession);
    await command("HeapProfiler.collectGarbage", {}, workerSession);
    const sampleAllHeaps = async () =>
      combineHeaps(
        await Promise.all([sampleHeap(pageSession), sampleHeap(workerSession)])
      );
    const baseline = await sampleAllHeaps();
    let peak = baseline;
    let lastValue = "";
    await evaluate("globalThis.__rxlsHeapProbeReady = true");
    for (let attempt = 0; attempt < 120; attempt += 1) {
      const response = await evaluate("document.querySelector('pre')?.textContent ?? ''");
      const value = response.result?.value ?? "";
      lastValue = value;
      peak = largerHeap(peak, await sampleAllHeaps());
      if (value.startsWith("PASS ") || value.startsWith("FAIL ")) {
        await command("HeapProfiler.collectGarbage", {}, pageSession);
        await command("HeapProfiler.collectGarbage", {}, workerSession);
        const retained = await sampleAllHeaps();
        peak = largerHeap(peak, retained);
        const retainedGrowthBytes = Math.max(0, retained.accountedBytes - baseline.accountedBytes);
        const heap = { baseline, peak, retained, retainedGrowthBytes };
        await evaluate("globalThis.__rxlsHeapProbeRelease = true");
        if (peak.accountedBytes > heapGate.maxAccountedBytes) {
          return {
            message: `FAIL heap_limit: accounted heap peak ${peak.accountedBytes} exceeds ${heapGate.maxAccountedBytes}`,
            heap
          };
        }
        if (retainedGrowthBytes > heapGate.maxRetainedGrowthBytes) {
          return {
            message: `FAIL heap_retention: retained growth ${retainedGrowthBytes} exceeds ${heapGate.maxRetainedGrowthBytes}`,
            heap
          };
        }
        return { message: value, heap };
      }
      await delay(50);
    }
    const diagnostic = await evaluate(
      "JSON.stringify({href: location.href, title: document.title, body: document.body?.innerText ?? null})"
    );
    const detail = diagnostic.result?.value ?? "no diagnostic";
    return {
      message: `FAIL timeout: browser smoke did not complete (${lastValue}; ${detail})`,
      heap: null
    };
  } finally {
    clearTimeout(browserDeadline);
    client.dispose();
    socket.close();
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
  return withTimeout(response.json(), timeoutMs, `${label} JSON`, () => controller.abort());
}

async function waitForWorkerTarget(attachedTargets) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    const worker = attachedTargets.find(
      ({ targetInfo }) =>
        targetInfo?.type === "worker" && targetInfo.url.includes("worker.mjs")
    );
    if (worker) {
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
    const response = await evaluate(
      "JSON.stringify({ready: globalThis.__rxlsWorkerReadyForHeapProbe === true, result: document.querySelector('pre')?.textContent ?? ''})"
    );
    const state = JSON.parse(response.result?.value ?? "{}");
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

function normalizeHeap(sample) {
  const normalized = {
    usedSize: finiteBytes(sample.usedSize),
    totalSize: finiteBytes(sample.totalSize),
    embedderHeapUsedSize: finiteBytes(sample.embedderHeapUsedSize),
    backingStorageSize: finiteBytes(sample.backingStorageSize)
  };
  normalized.accountedBytes =
    normalized.usedSize + normalized.embedderHeapUsedSize + normalized.backingStorageSize;
  return normalized;
}

function combineHeaps([page, worker]) {
  const combined = {
    usedSize: page.usedSize + worker.usedSize,
    totalSize: page.totalSize + worker.totalSize,
    embedderHeapUsedSize: page.embedderHeapUsedSize + worker.embedderHeapUsedSize,
    backingStorageSize: page.backingStorageSize + worker.backingStorageSize,
    targets: { page, worker }
  };
  combined.accountedBytes =
    combined.usedSize + combined.embedderHeapUsedSize + combined.backingStorageSize;
  return combined;
}

function finiteBytes(value) {
  return Number.isFinite(value) && value >= 0 ? Math.ceil(value) : 0;
}

function largerHeap(left, right) {
  return right.accountedBytes > left.accountedBytes ? right : left;
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

function requestTarget(pathname) {
  if (pathname === "/fixture.b64") {
    return fixture;
  }
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
