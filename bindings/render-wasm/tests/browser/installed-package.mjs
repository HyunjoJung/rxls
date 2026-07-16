import { RenderWorkerClient, getRenderWorkerUrl } from "@rxls/render-worker";

const result = document.querySelector("#result");
const viewer = document.querySelector("#viewer");
const policyViolations = [];
addEventListener("securitypolicyviolation", (event) => {
  policyViolations.push(`${event.violatedDirective}:${event.blockedURI}`);
});

globalThis.__rxlsWorkerReadyForHeapProbe = false;
globalThis.__rxlsHeapProbeReady = false;
globalThis.__rxlsHeapProbeRelease = false;

let client;
try {
  const workerUrl = getRenderWorkerUrl();
  const expectedWorkerUrl = new URL("/installed-package/js/worker.mjs", location.href);
  if (!(workerUrl instanceof URL) || workerUrl.href !== expectedWorkerUrl.href) {
    throw new Error(`installed worker URL mismatch: ${workerUrl}`);
  }
  client = new RenderWorkerClient(workerUrl);
  const capabilities = await timed(client.capabilities(), "worker ready");
  if (capabilities.protocol !== "rxls.render-worker.v1") {
    throw new Error("installed worker protocol changed");
  }
  globalThis.__rxlsWorkerReadyForHeapProbe = true;
  await waitForHeapProbe("__rxlsHeapProbeReady", "heap probe ready");

  const encoded = (await (await fetch("/fixture.b64")).text()).trim();
  const binary = atob(encoded);
  const bytes = Uint8Array.from(binary, (value) => value.charCodeAt(0));
  const opened = await timed(
    client.open(bytes, { documentId: "installed-package-readme" }),
    "open workbook"
  );
  const pageMap = await timed(
    client.preparePages(opened.documentId, 0),
    "prepare pages"
  );
  if (pageMap.manifest.pages.length < 1) {
    throw new Error("installed package returned no print pages");
  }
  const firstPage = await timed(
    client.renderPage(opened.documentId, 0, 0),
    "render first page"
  );
  const parsed = new DOMParser().parseFromString(firstPage.svg, "image/svg+xml");
  if (parsed.querySelector("parsererror") || parsed.documentElement.localName !== "svg") {
    throw new Error("installed package returned invalid page SVG");
  }
  viewer.replaceChildren(document.importNode(parsed.documentElement, true));
  await client.closeDocument(opened.documentId);
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
  result.textContent = "PASS installed @rxls/render-worker README URL and page render";
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
