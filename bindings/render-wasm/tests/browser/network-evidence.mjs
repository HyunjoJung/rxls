import { createHash } from "node:crypto";

export const NETWORK_NEGATIVE_URL =
  "https://rxls-network-negative.invalid/render-worker-control";
export const RENDER_WORKER_CSP_NEGATIVE_URL =
  "https://rxls-csp-negative.invalid/render-worker-control";

export const PRE_NAVIGATION_PROOF_SCHEMA =
  "rxls.render-pre-navigation-network.v1";
export const RENDER_WORKER_CSP_PROOF_SCHEMA =
  "rxls.render-worker-csp-negative.v1";

const MAX_EVIDENCE_ENTRIES = 128;
const MAX_EVIDENCE_SEQUENCE = 1_000_000;
const MAX_IDENTIFIER_BYTES = 256;
const MAX_URL_BYTES = 4_096;
const HARD_STOP_NONCE_PATTERN = /^[0-9a-f]{32}$/;
const MODES = new Set(["source", "installed"]);

const SHARED_PAGE_ROUTES = Object.freeze([
  "/tests/browser/contract.mjs",
  "/tests/browser/fixture.mjs",
  "/tests/browser/proof.mjs",
  "/tests/browser/scenario.mjs"
]);

export function validatePreNavigationInstrumentation({
  mode,
  pageUrl,
  instrumentation
}) {
  validateMode(mode);
  const page = parsePageUrl(pageUrl, mode);
  assertExactKeys(
    instrumentation,
    [
      "schema",
      "pageSessionId",
      "networkEnabledSequence",
      "pageFetchEnabledSequence",
      "autoAttachEnabledSequence",
      "autoAttachWaitForDebugger",
      "navigationSequence"
    ],
    "pre-navigation instrumentation"
  );
  if (instrumentation.schema !== PRE_NAVIGATION_PROOF_SCHEMA) {
    throw new Error("pre-navigation instrumentation schema is invalid");
  }
  validateIdentifier(instrumentation.pageSessionId, "page session id");
  for (const key of [
    "networkEnabledSequence",
    "pageFetchEnabledSequence",
    "autoAttachEnabledSequence",
    "navigationSequence"
  ]) {
    validateSequence(instrumentation[key], key);
  }
  if (
    instrumentation.networkEnabledSequence >=
      instrumentation.pageFetchEnabledSequence ||
    instrumentation.pageFetchEnabledSequence >=
      instrumentation.autoAttachEnabledSequence ||
    instrumentation.autoAttachEnabledSequence >=
      instrumentation.navigationSequence
  ) {
    throw new Error(
      "page Network/Fetch and worker auto-attach instrumentation must precede navigation"
    );
  }
  if (instrumentation.autoAttachWaitForDebugger !== true) {
    throw new Error(
      "worker auto-attach must pause new contexts until instrumentation is active"
    );
  }
  return Object.freeze({
    schema: PRE_NAVIGATION_PROOF_SCHEMA,
    mode,
    pageUrl: page.href,
    pageSessionId: instrumentation.pageSessionId,
    networkEnabledSequence: instrumentation.networkEnabledSequence,
    pageFetchEnabledSequence: instrumentation.pageFetchEnabledSequence,
    autoAttachEnabledSequence: instrumentation.autoAttachEnabledSequence,
    navigationSequence: instrumentation.navigationSequence,
    autoAttachWaitForDebugger: true
  });
}

export function normalizeRenderWorkerContexts({
  mode,
  pageUrl,
  hardStopNonce,
  instrumentation,
  contexts
}) {
  const preNavigation = validatePreNavigationInstrumentation({
    mode,
    pageUrl,
    instrumentation
  });
  validateHardStopNonce(hardStopNonce);
  if (!Array.isArray(contexts) || contexts.length !== 2) {
    throw new Error(
      "exactly the primary and nonce-bound render worker contexts are required"
    );
  }
  const workerPath = workerPathForMode(mode);
  const seenTargets = new Set();
  const seenSessions = new Set();
  const seenRoles = new Set();
  let priorAttachSequence = preNavigation.navigationSequence;
  const normalized = contexts.map((context) => {
    assertExactKeys(
      context,
      [
        "targetId",
        "sessionId",
        "type",
        "url",
        "attachedSequence",
        "runtimeEnabledSequence",
        "networkEnabledSequence",
        "resumedSequence"
      ],
      "render worker context"
    );
    validateIdentifier(context.targetId, "worker target id");
    validateIdentifier(context.sessionId, "worker session id");
    if (context.type !== "worker") {
      throw new Error("render network evidence contains a non-worker context");
    }
    if (
      seenTargets.has(context.targetId) ||
      seenSessions.has(context.sessionId)
    ) {
      throw new Error("render worker context identity is duplicated");
    }
    seenTargets.add(context.targetId);
    seenSessions.add(context.sessionId);

    const workerUrl = parseBoundedUrl(context.url, "render worker URL");
    if (
      workerUrl.origin !== new URL(preNavigation.pageUrl).origin ||
      workerUrl.pathname !== workerPath ||
      workerUrl.hash !== "" ||
      workerUrl.username !== "" ||
      workerUrl.password !== ""
    ) {
      throw new Error("render worker URL is not an allowed same-origin route");
    }
    const role = classifyWorkerQuery(workerUrl, hardStopNonce);
    if (seenRoles.has(role)) {
      throw new Error(`render worker role is duplicated: ${role}`);
    }
    seenRoles.add(role);

    for (const key of [
      "attachedSequence",
      "runtimeEnabledSequence",
      "networkEnabledSequence",
      "resumedSequence"
    ]) {
      validateSequence(context[key], key);
    }
    if (context.attachedSequence <= priorAttachSequence) {
      throw new Error("render worker attachment evidence is out of order");
    }
    const instrumentationSequences = [
      context.runtimeEnabledSequence,
      context.networkEnabledSequence
    ];
    if (
      new Set(instrumentationSequences).size !== instrumentationSequences.length ||
      instrumentationSequences.some(
        (sequence) =>
          sequence <= context.attachedSequence ||
          sequence >= context.resumedSequence
      )
    ) {
      throw new Error(
        "render worker Runtime/Network instrumentation is missing or out of order"
      );
    }
    priorAttachSequence = context.attachedSequence;
    return Object.freeze({
      role,
      targetId: context.targetId,
      sessionId: context.sessionId,
      url: workerUrl.href,
      attachedSequence: context.attachedSequence,
      runtimeEnabledSequence: context.runtimeEnabledSequence,
      networkEnabledSequence: context.networkEnabledSequence,
      resumedSequence: context.resumedSequence
    });
  });
  if (!seenRoles.has("primary") || !seenRoles.has("hard-stop")) {
    throw new Error(
      "render worker contexts do not contain one primary and one hard-stop worker"
    );
  }
  if (
    normalized[0].role !== "primary" ||
    normalized[1].role !== "hard-stop"
  ) {
    throw new Error("render worker contexts were observed in an invalid order");
  }
  return Object.freeze(normalized);
}

export function renderWorkerNetworkInstrumentationCommands() {
  return [
    { method: "Runtime.enable", params: {} },
    {
      method: "Network.enable",
      params: {
        maxTotalBufferSize: 64 * 1024,
        maxResourceBufferSize: 32 * 1024,
        maxPostDataSize: 4 * 1024
      }
    }
  ];
}

export function renderPageCspFetchInstrumentationCommand(
  controlUrl = RENDER_WORKER_CSP_NEGATIVE_URL
) {
  validateCspControlUrl(controlUrl);
  return {
    method: "Fetch.enable",
    params: {
      patterns: [{ urlPattern: controlUrl, requestStage: "Request" }],
      handleAuthRequests: false
    }
  };
}

export function createRenderWorkerCspProbeExpression(
  controlUrl = RENDER_WORKER_CSP_NEGATIVE_URL
) {
  validateCspControlUrl(controlUrl);
  return `(() => {
    const schema = ${JSON.stringify(RENDER_WORKER_CSP_PROOF_SCHEMA)};
    const url = ${JSON.stringify(controlUrl)};
    return (async () => {
      let violation = null;
      let violationCount = 0;
      const onViolation = (event) => {
        violationCount = Math.min(2, violationCount + 1);
        if (violation === null) {
          violation = {
            blockedURI: event.blockedURI,
            disposition: event.disposition,
            effectiveDirective: event.effectiveDirective,
            isTrusted: event.isTrusted,
            violatedDirective: event.violatedDirective
          };
        }
      };
      globalThis.addEventListener("securitypolicyviolation", onViolation);
      let rejectionName = null;
      let fetchRejected = false;
      try {
        await fetch(url, {
          cache: "no-store",
          credentials: "omit",
          redirect: "error",
          referrerPolicy: "no-referrer"
        });
      } catch (error) {
        rejectionName = error?.name ?? null;
        fetchRejected = error instanceof TypeError;
      }
      for (let attempt = 0; attempt < 100 && violationCount === 0; attempt += 1) {
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
      await new Promise((resolve) => setTimeout(resolve, 25));
      globalThis.removeEventListener("securitypolicyviolation", onViolation);
      return {
        schema,
        locationHref: globalThis.location?.href ?? null,
        url,
        fetchRejected,
        rejectionName,
        violationCount,
        violation
      };
    })();
  })()`;
}

export function validateRenderWorkerCspEvidence({ contexts, proofs }) {
  if (!Array.isArray(contexts) || contexts.length !== 2) {
    throw new Error("normalized render worker contexts are required");
  }
  if (!Array.isArray(proofs) || proofs.length !== contexts.length) {
    throw new Error("every actual render worker context requires one CSP proof");
  }
  const contextByTarget = new Map();
  const contextSessions = new Set();
  let primaryUrl = null;
  let priorContextSequence = -1;
  for (const [index, context] of contexts.entries()) {
    assertExactKeys(
      context,
      [
        "role",
        "targetId",
        "sessionId",
        "url",
        "attachedSequence",
        "runtimeEnabledSequence",
        "networkEnabledSequence",
        "resumedSequence"
      ],
      "normalized render worker context"
    );
    if (contextByTarget.has(context.targetId)) {
      throw new Error("normalized render worker context is duplicated");
    }
    validateIdentifier(context.targetId, "normalized worker target id");
    validateIdentifier(context.sessionId, "normalized worker session id");
    if (contextSessions.has(context.sessionId)) {
      throw new Error("normalized render worker session is duplicated");
    }
    contextSessions.add(context.sessionId);
    const contextUrl = parseBoundedUrl(
      context.url,
      "normalized render worker URL"
    );
    for (const key of [
      "attachedSequence",
      "runtimeEnabledSequence",
      "networkEnabledSequence",
      "resumedSequence"
    ]) {
      validateSequence(context[key], key);
    }
    const instrumentationSequences = [
      context.runtimeEnabledSequence,
      context.networkEnabledSequence
    ];
    if (
      context.attachedSequence <= priorContextSequence ||
      new Set(instrumentationSequences).size !== instrumentationSequences.length ||
      instrumentationSequences.some(
        (sequence) =>
          sequence <= context.attachedSequence ||
          sequence >= context.resumedSequence
      )
    ) {
      throw new Error("normalized render worker context is out of order");
    }
    priorContextSequence = context.attachedSequence;
    if (index === 0) {
      if (
        context.role !== "primary" ||
        contextUrl.search !== "" ||
        contextUrl.hash !== "" ||
        contextUrl.username !== "" ||
        contextUrl.password !== ""
      ) {
        throw new Error("normalized primary render worker context is invalid");
      }
      primaryUrl = contextUrl;
    } else {
      const hardStopNonce =
        contextUrl.searchParams.get("rxls-hard-stop") ?? "";
      if (
        context.role !== "hard-stop" ||
        primaryUrl === null ||
        contextUrl.origin !== primaryUrl.origin ||
        contextUrl.pathname !== primaryUrl.pathname ||
        contextUrl.hash !== "" ||
        contextUrl.username !== "" ||
        contextUrl.password !== "" ||
        contextUrl.searchParams.size !== 1 ||
        !HARD_STOP_NONCE_PATTERN.test(hardStopNonce) ||
        contextUrl.search !== `?rxls-hard-stop=${hardStopNonce}`
      ) {
        throw new Error("normalized hard-stop render worker context is invalid");
      }
    }
    contextByTarget.set(context.targetId, context);
  }

  const seenTargets = new Set();
  let priorProbeSequence = -1;
  const digestProofs = [];
  const workers = proofs.map((entry) => {
    assertExactKeys(
      entry,
      [
        "targetId",
        "sessionId",
        "probeStartedSequence",
        "probeCompletedSequence",
        "proof",
        "network"
      ],
      "render worker CSP evidence"
    );
    const context = contextByTarget.get(entry.targetId);
    if (
      context === undefined ||
      context.sessionId !== entry.sessionId ||
      seenTargets.has(entry.targetId)
    ) {
      throw new Error(
        "render worker CSP evidence has an unknown or duplicate context identity"
      );
    }
    seenTargets.add(entry.targetId);
    validateSequence(entry.probeStartedSequence, "CSP probe start sequence");
    validateSequence(entry.probeCompletedSequence, "CSP probe completion sequence");
    if (
      entry.probeStartedSequence <= context.resumedSequence ||
      entry.probeStartedSequence <= priorProbeSequence ||
      entry.probeCompletedSequence <= entry.probeStartedSequence
    ) {
      throw new Error("render worker CSP evidence is out of order");
    }
    priorProbeSequence = entry.probeCompletedSequence;
    assertExactKeys(
      entry.network,
      ["requests", "responses", "failures", "pauses"],
      "render worker CSP network window"
    );
    for (const [label, events] of Object.entries(entry.network)) {
      if (!Array.isArray(events)) {
        throw new Error(`render worker CSP ${label} evidence is invalid`);
      }
      if (events.length !== 0) {
        throw new Error(
          `render worker CSP control emitted ${events.length} ${label} events`
        );
      }
    }
    const proof = validateWorkerCspProof(entry.proof, context.url);
    const location = new URL(context.url);
    digestProofs.push({
      role: context.role,
      location: `${location.pathname}${
        context.role === "hard-stop" ? "?rxls-hard-stop=<nonce>" : ""
      }`,
      proof
    });
    return Object.freeze({
      role: context.role,
      targetId: context.targetId,
      sessionId: context.sessionId,
      fetchRejected: true,
      violationDirective: "connect-src",
      controlNetworkRequests: 0,
      controlNetworkResponses: 0,
      controlNetworkFailures: 0,
      owningPageFetchPauses: 0
    });
  });
  if (seenTargets.size !== contextByTarget.size) {
    throw new Error("a render worker context is missing CSP evidence");
  }
  return Object.freeze({
    schema: "rxls.render-worker-csp-evidence.v1",
    fetchInstrumentation: "owning-page-before-navigation",
    workers: Object.freeze(workers),
    cspSha256: sha256Canonical({
      fetchInstrumentation: "owning-page-before-navigation",
      proofs: digestProofs
    })
  });
}

export function validateSameOriginRouteEvidence({
  mode,
  pageUrl,
  hardStopNonce,
  instrumentation,
  contexts,
  routes
}) {
  const preNavigation = validatePreNavigationInstrumentation({
    mode,
    pageUrl,
    instrumentation
  });
  validateHardStopNonce(hardStopNonce);
  const page = new URL(preNavigation.pageUrl);
  const sessionRoles = validateRouteSessionOwnership({
    mode,
    page,
    hardStopNonce,
    pageSessionId: preNavigation.pageSessionId,
    contexts
  });
  if (
    !Array.isArray(routes) ||
    routes.length === 0 ||
    routes.length > MAX_EVIDENCE_ENTRIES
  ) {
    throw new Error("route evidence is missing or exceeded its entry bound");
  }
  const expected = expectedRouteSessionMultiset(mode, hardStopNonce);
  const observed = new Map();
  const seenRequestIds = new Set();
  let priorSequence = preNavigation.navigationSequence;
  const normalized = routes.map((route) => {
    assertExactKeys(
      route,
      ["sequence", "requestId", "sessionId", "method", "url"],
      "route evidence"
    );
    validateSequence(route.sequence, "route sequence");
    if (route.sequence <= priorSequence) {
      throw new Error("route evidence is duplicated or out of order");
    }
    priorSequence = route.sequence;
    validateIdentifier(route.requestId, "route request id");
    validateIdentifier(route.sessionId, "route session id");
    if (seenRequestIds.has(route.requestId)) {
      throw new Error("route request identity is duplicated");
    }
    seenRequestIds.add(route.requestId);
    if (route.method !== "GET") {
      throw new Error("browser route evidence contains a non-GET request");
    }
    const owner = sessionRoles.get(route.sessionId);
    if (owner === undefined) {
      throw new Error("browser route evidence has an unknown target session");
    }
    const url = parseBoundedUrl(route.url, "browser route URL", page);
    if (
      url.origin !== page.origin ||
      url.hash !== "" ||
      url.username !== "" ||
      url.password !== ""
    ) {
      throw new Error("browser route escaped the same-origin server");
    }
    const routeKey = routeKeyForUrl(url, hardStopNonce, mode);
    const ownedRouteKey = `${owner}\u0000${routeKey}`;
    if (!expected.has(ownedRouteKey)) {
      throw new Error(
        `browser requested an unknown ${owner} route: ${routeKey}`
      );
    }
    observed.set(ownedRouteKey, (observed.get(ownedRouteKey) ?? 0) + 1);
    return Object.freeze({
      sequence: route.sequence,
      requestId: route.requestId,
      sessionId: route.sessionId,
      owner,
      method: "GET",
      url: url.href,
      routeKey
    });
  });

  for (const [ownedRouteKey, expectedCount] of expected) {
    const observedCount = observed.get(ownedRouteKey) ?? 0;
    if (observedCount !== expectedCount) {
      const [owner, routeKey] = ownedRouteKey.split("\u0000");
      throw new Error(
        `browser ${owner} route ${routeKey} was observed ${observedCount} times instead of ${expectedCount}`
      );
    }
  }
  if (observed.size !== expected.size) {
    throw new Error("browser route evidence contains an unrecognized route");
  }
  const digestRoutes = [...observed]
    .sort(([left], [right]) => (left < right ? -1 : left > right ? 1 : 0))
    .map(([ownedRoute, count]) => {
      const [owner, route] = ownedRoute.split("\u0000");
      return { owner, route, count };
    });
  return Object.freeze({
    schema: "rxls.render-route-evidence.v1",
    mode,
    requestCount: normalized.length,
    routes: Object.freeze(normalized),
    routeSha256: sha256Canonical({ mode, routes: digestRoutes })
  });
}

export function validateCspNetworkSilence({
  proof,
  requests,
  responses,
  failures = [],
  pauses = []
}) {
  assertExactKeys(
    proof,
    [
      "schema",
      "url",
      "violation",
      "fetchRejected",
      "rejectionName",
      "violationCount"
    ],
    "CSP page proof"
  );
  if (
    proof.schema !== "rxls.render-csp-negative.v1" ||
    proof.fetchRejected !== true ||
    proof.rejectionName !== "TypeError" ||
    proof.violationCount !== 1 ||
    proof.url !== RENDER_WORKER_CSP_NEGATIVE_URL
  ) {
    throw new Error("CSP page proof is invalid");
  }
  assertExactKeys(
    proof.violation,
    [
      "blockedURI",
      "disposition",
      "effectiveDirective",
      "isTrusted",
      "statusCode",
      "violatedDirective"
    ],
    "CSP page violation"
  );
  const expectedViolation = {
    blockedURI: RENDER_WORKER_CSP_NEGATIVE_URL,
    disposition: "enforce",
    effectiveDirective: "connect-src",
    isTrusted: true,
    statusCode: 200,
    violatedDirective: "connect-src"
  };
  for (const [key, expected] of Object.entries(expectedViolation)) {
    if (proof.violation[key] !== expected) {
      throw new Error("CSP page trusted connect-src violation is invalid");
    }
  }
  if (
    requests.some(({ url }) => url === proof.url) ||
    responses.some(({ url }) => url === proof.url) ||
    failures.some(({ url }) => url === proof.url) ||
    pauses.some(({ url }) => url === proof.url)
  ) {
    throw new Error("CSP control escaped into the CDP Network request pipeline");
  }
  return {
    url: proof.url,
    networkRequestEmitted: false,
    networkResponseEmitted: false,
    networkFailureEmitted: false,
    fetchPauseEmitted: false
  };
}

export function validateOfflineNetworkBlock(
  { requests, failures, responses, pauses },
  controlUrl = NETWORK_NEGATIVE_URL
) {
  let parsedControlUrl;
  try {
    parsedControlUrl = new URL(controlUrl);
  } catch {
    throw new Error("CDP Network control URL is invalid");
  }
  if (
    parsedControlUrl.protocol !== "http:" &&
    parsedControlUrl.protocol !== "https:"
  ) {
    throw new Error("CDP Network control URL has an invalid scheme");
  }
  const matchingPauses = pauses.filter(({ url }) => url === controlUrl);
  if (
    matchingPauses.length !== 1 ||
    matchingPauses[0].networkId === null ||
    typeof matchingPauses[0].sessionId !== "string" ||
    matchingPauses[0].resourceType !== "XHR"
  ) {
    throw new Error("CDP Fetch did not safely intercept the exact network control");
  }
  const matchingRequests = requests.filter(({ url }) => url === controlUrl);
  if (matchingRequests.length !== 1) {
    throw new Error(
      `CDP Network observed ${matchingRequests.length} offline controls instead of one`
    );
  }
  const [{ requestId, sessionId }] = matchingRequests;
  if (
    matchingPauses[0].networkId !== requestId ||
    matchingPauses[0].sessionId !== sessionId
  ) {
    throw new Error("CDP Fetch and Network request identities do not match");
  }
  const matchingFailures = failures.filter(
    (failure) => failure.requestId === requestId
  );
  if (
    matchingFailures.length !== 1 ||
    matchingFailures[0].sessionId !== sessionId ||
    matchingFailures[0].blockedReason !== null ||
    matchingFailures[0].errorText !== "net::ERR_INTERNET_DISCONNECTED" ||
    matchingFailures[0].canceled !== false
  ) {
    throw new Error("CDP Network did not report the exact offline rejection");
  }
  if (responses.some((response) => response.requestId === requestId)) {
    throw new Error("Network negative control unexpectedly received a response");
  }
  return {
    url: controlUrl,
    requestId,
    errorText: "net::ERR_INTERNET_DISCONNECTED",
    responseReceived: false,
    interceptedBeforeResponse: true
  };
}

function validateWorkerCspProof(proof, expectedLocation) {
  assertExactKeys(
    proof,
    [
      "schema",
      "locationHref",
      "url",
      "fetchRejected",
      "rejectionName",
      "violationCount",
      "violation"
    ],
    "in-context worker CSP proof"
  );
  if (
    proof.schema !== RENDER_WORKER_CSP_PROOF_SCHEMA ||
    proof.locationHref !== expectedLocation ||
    proof.url !== RENDER_WORKER_CSP_NEGATIVE_URL ||
    proof.fetchRejected !== true ||
    proof.rejectionName !== "TypeError" ||
    proof.violationCount !== 1
  ) {
    throw new Error("in-context worker CSP rejection proof is invalid");
  }
  assertExactKeys(
    proof.violation,
    [
      "blockedURI",
      "disposition",
      "effectiveDirective",
      "isTrusted",
      "violatedDirective"
    ],
    "in-context worker CSP violation"
  );
  const expectedViolation = {
    blockedURI: RENDER_WORKER_CSP_NEGATIVE_URL,
    disposition: "enforce",
    effectiveDirective: "connect-src",
    isTrusted: true,
    violatedDirective: "connect-src"
  };
  if (proof.violation.isTrusted !== true) {
    throw new Error("in-context worker CSP violation is not trusted");
  }
  for (const [key, expected] of Object.entries(expectedViolation)) {
    if (proof.violation[key] !== expected) {
      throw new Error("in-context worker CSP connect-src violation is invalid");
    }
  }
  return {
    url: RENDER_WORKER_CSP_NEGATIVE_URL,
    fetchRejected: true,
    rejectionName: "TypeError",
    violation: expectedViolation
  };
}

function validateRouteSessionOwnership({
  mode,
  page,
  hardStopNonce,
  pageSessionId,
  contexts
}) {
  if (!Array.isArray(contexts) || contexts.length !== 2) {
    throw new Error("route evidence requires exactly two normalized workers");
  }
  const roles = new Map([[pageSessionId, "page"]]);
  const workerPath = workerPathForMode(mode);
  for (const [index, context] of contexts.entries()) {
    assertExactKeys(
      context,
      [
        "role",
        "targetId",
        "sessionId",
        "url",
        "attachedSequence",
        "runtimeEnabledSequence",
        "networkEnabledSequence",
        "resumedSequence"
      ],
      "route worker context"
    );
    validateIdentifier(context.targetId, "route worker target id");
    validateIdentifier(context.sessionId, "route worker session id");
    const expectedRole = index === 0 ? "primary" : "hard-stop";
    if (context.role !== expectedRole || roles.has(context.sessionId)) {
      throw new Error("route worker ownership is duplicated or out of order");
    }
    const workerUrl = parseBoundedUrl(context.url, "route worker URL");
    if (
      workerUrl.origin !== page.origin ||
      workerUrl.pathname !== workerPath ||
      classifyWorkerQuery(workerUrl, hardStopNonce) !== expectedRole
    ) {
      throw new Error("route worker ownership URL is invalid");
    }
    roles.set(context.sessionId, expectedRole);
  }
  return roles;
}

function expectedRouteSessionMultiset(mode, hardStopNonce) {
  const installed = mode === "installed";
  const prefix = installed ? "/installed-package" : "";
  const pageEntries = [
    ...SHARED_PAGE_ROUTES,
    installed
      ? "/tests/browser/installed-package.html"
      : "/tests/browser/index.html",
    installed
      ? "/tests/browser/installed-package-bootstrap.mjs"
      : "/tests/browser/bootstrap.mjs",
    installed
      ? "/tests/browser/installed-package.mjs"
      : "/tests/browser/smoke.mjs",
    `${prefix}/js/client.mjs`,
    `${prefix}/js/protocol.mjs`,
    `${prefix}/js/worker.mjs`,
    `${prefix}/js/worker.mjs?rxls-hard-stop=<nonce>`
  ];
  const workerEntries = [
    `${prefix}/js/protocol.mjs`,
    `${prefix}/js/worker-runtime.mjs`,
    `${prefix}/pkg/rxls_render_wasm.js`,
    `${prefix}/pkg/rxls_render_wasm_bg.wasm`
  ];
  validateHardStopNonce(hardStopNonce);
  return new Map([
    ...pageEntries.map((route) => [`page\u0000${route}`, 1]),
    ...workerEntries.map((route) => [`primary\u0000${route}`, 1]),
    ...workerEntries.map((route) => [`hard-stop\u0000${route}`, 1])
  ]);
}

function routeKeyForUrl(url, hardStopNonce, mode) {
  const workerPath = workerPathForMode(mode);
  if (url.pathname !== workerPath) {
    if (url.search !== "") {
      throw new Error("only the hard-stop worker route may contain a query");
    }
    return url.pathname;
  }
  if (url.search === "") {
    return workerPath;
  }
  if (
    url.searchParams.size !== 1 ||
    url.searchParams.get("rxls-hard-stop") !== hardStopNonce ||
    url.search !== `?rxls-hard-stop=${hardStopNonce}`
  ) {
    throw new Error("hard-stop worker route query is not nonce-bound");
  }
  return `${workerPath}?rxls-hard-stop=<nonce>`;
}

function classifyWorkerQuery(url, hardStopNonce) {
  if (url.search === "") {
    return "primary";
  }
  if (
    url.searchParams.size !== 1 ||
    url.searchParams.get("rxls-hard-stop") !== hardStopNonce ||
    url.search !== `?rxls-hard-stop=${hardStopNonce}`
  ) {
    throw new Error("render worker query is not bound to the hard-stop nonce");
  }
  return "hard-stop";
}

function workerPathForMode(mode) {
  validateMode(mode);
  return mode === "installed"
    ? "/installed-package/js/worker.mjs"
    : "/js/worker.mjs";
}

function parsePageUrl(pageUrl, mode) {
  const page = parseBoundedUrl(pageUrl, "browser page URL");
  const expectedPath =
    mode === "installed"
      ? "/tests/browser/installed-package.html"
      : "/tests/browser/index.html";
  if (
    page.protocol !== "http:" ||
    page.hostname !== "127.0.0.1" ||
    page.port === "" ||
    page.pathname !== expectedPath ||
    page.search !== "" ||
    page.hash !== "" ||
    page.username !== "" ||
    page.password !== ""
  ) {
    throw new Error("browser page URL does not match the selected route mode");
  }
  return page;
}

function parseBoundedUrl(value, label, base) {
  if (
    typeof value !== "string" ||
    Buffer.byteLength(value) === 0 ||
    Buffer.byteLength(value) > MAX_URL_BYTES
  ) {
    throw new Error(`${label} is missing or exceeded its byte bound`);
  }
  try {
    return base === undefined ? new URL(value) : new URL(value, base);
  } catch {
    throw new Error(`${label} is invalid`);
  }
}

function validateCspControlUrl(controlUrl) {
  const url = parseBoundedUrl(controlUrl, "worker CSP control URL");
  if (url.href !== RENDER_WORKER_CSP_NEGATIVE_URL) {
    throw new Error("worker CSP control URL is not the locked negative control");
  }
}

function validateHardStopNonce(nonce) {
  if (typeof nonce !== "string" || !HARD_STOP_NONCE_PATTERN.test(nonce)) {
    throw new Error("hard-stop nonce is invalid");
  }
}

function validateMode(mode) {
  if (!MODES.has(mode)) {
    throw new Error("browser route mode must be source or installed");
  }
}

function validateIdentifier(value, label) {
  if (
    typeof value !== "string" ||
    Buffer.byteLength(value) === 0 ||
    Buffer.byteLength(value) > MAX_IDENTIFIER_BYTES ||
    /[\u0000-\u001f\u007f]/.test(value)
  ) {
    throw new Error(`${label} is invalid or exceeded its byte bound`);
  }
}

function validateSequence(value, label) {
  if (
    !Number.isSafeInteger(value) ||
    value < 0 ||
    value > MAX_EVIDENCE_SEQUENCE
  ) {
    throw new Error(`${label} is outside the bounded evidence sequence`);
  }
}

function assertExactKeys(value, expectedKeys, label) {
  if (
    value === null ||
    typeof value !== "object" ||
    Array.isArray(value)
  ) {
    throw new Error(`${label} is not an object`);
  }
  const actual = Object.keys(value).sort();
  const expected = [...expectedKeys].sort();
  if (
    actual.length !== expected.length ||
    actual.some((key, index) => key !== expected[index])
  ) {
    throw new Error(`${label} contains missing or unknown fields`);
  }
}

function sha256Canonical(value) {
  return createHash("sha256").update(JSON.stringify(value)).digest("hex");
}
