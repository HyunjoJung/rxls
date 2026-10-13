import assert from "node:assert/strict";
import test from "node:test";

import {
  NETWORK_NEGATIVE_URL,
  PRE_NAVIGATION_PROOF_SCHEMA,
  RENDER_WORKER_CSP_NEGATIVE_URL,
  RENDER_WORKER_CSP_PROOF_SCHEMA,
  createRenderWorkerCspProbeExpression,
  normalizeRenderWorkerContexts,
  renderPageCspFetchInstrumentationCommand,
  renderWorkerNetworkInstrumentationCommands,
  validateCspNetworkSilence,
  validateOfflineNetworkBlock,
  validatePreNavigationInstrumentation,
  validateRenderWorkerCspEvidence,
  validateSameOriginRouteEvidence
} from "./browser/network-evidence.mjs";

const nonce = "0123456789abcdef0123456789abcdef";
const pageUrl = "http://127.0.0.1:4173/tests/browser/index.html";
const instrumentation = {
  schema: PRE_NAVIGATION_PROOF_SCHEMA,
  pageSessionId: "page-session",
  networkEnabledSequence: 1,
  pageFetchEnabledSequence: 2,
  autoAttachEnabledSequence: 3,
  autoAttachWaitForDebugger: true,
  navigationSequence: 4
};
const sourceContexts = [
  {
    targetId: "primary-target",
    sessionId: "primary-session",
    type: "worker",
    url: "http://127.0.0.1:4173/js/worker.mjs",
    attachedSequence: 20,
    runtimeEnabledSequence: 21,
    networkEnabledSequence: 22,
    resumedSequence: 23
  },
  {
    targetId: "hard-stop-target",
    sessionId: "hard-stop-session",
    type: "worker",
    url:
      "http://127.0.0.1:4173/js/worker.mjs" +
      `?rxls-hard-stop=${nonce}`,
    attachedSequence: 40,
    runtimeEnabledSequence: 41,
    networkEnabledSequence: 42,
    resumedSequence: 43
  }
];

test("pre-navigation proof requires bounded Network and paused auto-attach before navigation", () => {
  assert.deepEqual(
    validatePreNavigationInstrumentation({
      mode: "source",
      pageUrl,
      instrumentation
    }),
    {
      schema: PRE_NAVIGATION_PROOF_SCHEMA,
      mode: "source",
      pageUrl,
      pageSessionId: "page-session",
      networkEnabledSequence: 1,
      pageFetchEnabledSequence: 2,
      autoAttachEnabledSequence: 3,
      navigationSequence: 4,
      autoAttachWaitForDebugger: true
    }
  );
  assert.throws(
    () =>
      validatePreNavigationInstrumentation({
        mode: "source",
        pageUrl,
        instrumentation: {
          ...instrumentation,
          autoAttachEnabledSequence: 5
        }
      }),
    /precede navigation/
  );
  assert.throws(
    () =>
      validatePreNavigationInstrumentation({
        mode: "source",
        pageUrl,
        instrumentation: {
          ...instrumentation,
          autoAttachWaitForDebugger: false
        }
      }),
    /pause new contexts/
  );
  assert.throws(
    () =>
      validatePreNavigationInstrumentation({
        mode: "source",
        pageUrl,
        instrumentation: { ...instrumentation, unknown: true }
      }),
    /unknown fields/
  );
});

test("render worker contexts are exactly primary then nonce-bound and instrumented before resume", () => {
  const contexts = normalizeRenderWorkerContexts({
    mode: "source",
    pageUrl,
    hardStopNonce: nonce,
    instrumentation,
    contexts: sourceContexts
  });
  assert.deepEqual(
    contexts.map(({ role, targetId }) => ({ role, targetId })),
    [
      { role: "primary", targetId: "primary-target" },
      { role: "hard-stop", targetId: "hard-stop-target" }
    ]
  );
  assert.throws(
    () =>
      normalizeRenderWorkerContexts({
        mode: "source",
        pageUrl,
        hardStopNonce: nonce,
        instrumentation,
        contexts: [
          sourceContexts[0],
          { ...sourceContexts[1], sessionId: "primary-session" }
        ]
      }),
    /duplicated/
  );
  assert.throws(
    () =>
      normalizeRenderWorkerContexts({
        mode: "source",
        pageUrl,
        hardStopNonce: nonce,
        instrumentation,
        contexts: [
          sourceContexts[0],
          { ...sourceContexts[1], attachedSequence: 19 }
        ]
      }),
    /out of order/
  );
  assert.throws(
    () =>
      normalizeRenderWorkerContexts({
        mode: "source",
        pageUrl,
        hardStopNonce: nonce,
        instrumentation,
        contexts: [
          sourceContexts[0],
          {
            ...sourceContexts[1],
            url:
              "http://127.0.0.1:4173/js/worker.mjs" +
              `?rxls-hard-stop=${nonce}&extra=1`
          }
        ]
      }),
    /nonce/
  );
  assert.throws(
    () =>
      normalizeRenderWorkerContexts({
        mode: "source",
        pageUrl,
        hardStopNonce: nonce,
        instrumentation,
        contexts: [
          {
            ...sourceContexts[0],
            networkEnabledSequence: 25
          },
          sourceContexts[1]
        ]
      }),
    /instrumentation/
  );
});

test("worker CSP probe uses page Fetch plus child Runtime/Network instrumentation", () => {
  const expression = createRenderWorkerCspProbeExpression();
  assert.ok(expression.length < 4_096);
  assert.match(expression, /securitypolicyviolation/);
  assert.match(expression, /error instanceof TypeError/);
  assert.match(expression, /attempt < 100/);
  assert.match(expression, new RegExp(RENDER_WORKER_CSP_PROOF_SCHEMA));
  assert.deepEqual(renderWorkerNetworkInstrumentationCommands(), [
    { method: "Runtime.enable", params: {} },
    {
      method: "Network.enable",
      params: {
        maxTotalBufferSize: 64 * 1024,
        maxResourceBufferSize: 32 * 1024,
        maxPostDataSize: 4 * 1024
      }
    }
  ]);
  assert.deepEqual(renderPageCspFetchInstrumentationCommand(), {
    method: "Fetch.enable",
    params: {
      patterns: [{
        urlPattern: RENDER_WORKER_CSP_NEGATIVE_URL,
        requestStage: "Request"
      }],
      handleAuthRequests: false
    }
  });
  assert.throws(
    () =>
      createRenderWorkerCspProbeExpression(
        "https://example.invalid/not-the-control"
      ),
    /locked negative control/
  );
});

test("each actual render worker proves TypeError plus connect-src with zero CDP events", () => {
  const contexts = normalizeRenderWorkerContexts({
    mode: "source",
    pageUrl,
    hardStopNonce: nonce,
    instrumentation,
    contexts: sourceContexts
  });
  const evidence = validateRenderWorkerCspEvidence({
    contexts,
    proofs: workerProofs(contexts)
  });
  assert.equal(evidence.schema, "rxls.render-worker-csp-evidence.v1");
  assert.equal(
    evidence.fetchInstrumentation,
    "owning-page-before-navigation"
  );
  assert.match(evidence.cspSha256, /^[0-9a-f]{64}$/);
  assert.deepEqual(
    evidence.workers.map(({
      role,
      controlNetworkRequests,
      owningPageFetchPauses
    }) => ({
      role,
      controlNetworkRequests,
      owningPageFetchPauses
    })),
    [
      {
        role: "primary",
        controlNetworkRequests: 0,
        owningPageFetchPauses: 0
      },
      {
        role: "hard-stop",
        controlNetworkRequests: 0,
        owningPageFetchPauses: 0
      }
    ]
  );

  const noisy = workerProofs(contexts);
  noisy[0] = {
    ...noisy[0],
    network: {
      ...noisy[0].network,
      failures: [{ requestId: "escaped" }]
    }
  };
  assert.throws(
    () => validateRenderWorkerCspEvidence({ contexts, proofs: noisy }),
    /emitted 1 failures/
  );

  const duplicate = workerProofs(contexts);
  duplicate[1] = {
    ...duplicate[1],
    targetId: duplicate[0].targetId,
    sessionId: duplicate[0].sessionId
  };
  assert.throws(
    () => validateRenderWorkerCspEvidence({ contexts, proofs: duplicate }),
    /unknown or duplicate/
  );

  const outOfOrder = workerProofs(contexts);
  outOfOrder[1] = {
    ...outOfOrder[1],
    probeStartedSequence: 31,
    probeCompletedSequence: 32
  };
  assert.throws(
    () => validateRenderWorkerCspEvidence({ contexts, proofs: outOfOrder }),
    /out of order/
  );

  const invalidViolation = workerProofs(contexts);
  invalidViolation[0] = {
    ...invalidViolation[0],
    proof: {
      ...invalidViolation[0].proof,
      violation: {
        ...invalidViolation[0].proof.violation,
        effectiveDirective: "default-src"
      }
    }
  };
  assert.throws(
    () =>
      validateRenderWorkerCspEvidence({
        contexts,
        proofs: invalidViolation
      }),
    /connect-src/
  );

  const syntheticViolation = workerProofs(contexts);
  syntheticViolation[0] = {
    ...syntheticViolation[0],
    proof: {
      ...syntheticViolation[0].proof,
      violation: {
        ...syntheticViolation[0].proof.violation,
        isTrusted: false
      }
    }
  };
  assert.throws(
    () =>
      validateRenderWorkerCspEvidence({
        contexts,
        proofs: syntheticViolation
      }),
    /not trusted/
  );

  const duplicateViolation = workerProofs(contexts);
  duplicateViolation[0] = {
    ...duplicateViolation[0],
    proof: {
      ...duplicateViolation[0].proof,
      violationCount: 2
    }
  };
  assert.throws(
    () =>
      validateRenderWorkerCspEvidence({
        contexts,
        proofs: duplicateViolation
      }),
    /rejection proof/
  );
});

test("source route evidence is an exact same-origin multiset with one nonce query", () => {
  const routes = sourceRouteEvidence();
  const contexts = normalizedSourceContexts();
  const result = validateSameOriginRouteEvidence({
    mode: "source",
    pageUrl,
    hardStopNonce: nonce,
    instrumentation,
    contexts,
    routes
  });
  assert.equal(result.requestCount, 22);
  assert.match(result.routeSha256, /^[0-9a-f]{64}$/);
  assert.equal(
    result.routes.filter(({ routeKey }) =>
      routeKey.endsWith("?rxls-hard-stop=<nonce>")
    ).length,
    1
  );

  const unknown = sourceRouteEvidence();
  unknown[3] = { ...unknown[3], url: "/favicon.ico" };
  assert.throws(
    () =>
      validateSameOriginRouteEvidence({
        mode: "source",
        pageUrl,
        hardStopNonce: nonce,
        instrumentation,
        contexts,
        routes: unknown
      }),
    /unknown page route/
  );

  const offOrigin = sourceRouteEvidence();
  offOrigin[3] = {
    ...offOrigin[3],
    url: "https://unexpected.invalid/browser-proof"
  };
  assert.throws(
    () =>
      validateSameOriginRouteEvidence({
        mode: "source",
        pageUrl,
        hardStopNonce: nonce,
        instrumentation,
        contexts,
        routes: offOrigin
      }),
    /escaped the same-origin server/
  );

  const misowned = sourceRouteEvidence();
  misowned[11] = {
    ...misowned[11],
    sessionId: instrumentation.pageSessionId
  };
  assert.throws(
    () =>
      validateSameOriginRouteEvidence({
        mode: "source",
        pageUrl,
        hardStopNonce: nonce,
        instrumentation,
        contexts,
        routes: misowned
      }),
    /unknown page route/
  );

  const duplicate = sourceRouteEvidence();
  duplicate[4] = {
    ...duplicate[4],
    requestId: duplicate[3].requestId
  };
  assert.throws(
    () =>
      validateSameOriginRouteEvidence({
        mode: "source",
        pageUrl,
        hardStopNonce: nonce,
        instrumentation,
        contexts,
        routes: duplicate
      }),
    /duplicated/
  );

  const outOfOrder = sourceRouteEvidence();
  outOfOrder[4] = {
    ...outOfOrder[4],
    sequence: outOfOrder[3].sequence
  };
  assert.throws(
    () =>
      validateSameOriginRouteEvidence({
        mode: "source",
        pageUrl,
        hardStopNonce: nonce,
        instrumentation,
        contexts,
        routes: outOfOrder
      }),
    /out of order/
  );

  const extraQuery = sourceRouteEvidence();
  extraQuery[0] = {
    ...extraQuery[0],
    url: `${extraQuery[0].url}?cache=1`
  };
  assert.throws(
    () =>
      validateSameOriginRouteEvidence({
        mode: "source",
        pageUrl,
        hardStopNonce: nonce,
        instrumentation,
        contexts,
        routes: extraQuery
      }),
    /only the hard-stop/
  );
});

test("installed route policy uses only installed package runtime paths", () => {
  const installedPageUrl =
    "http://127.0.0.1:4173/tests/browser/installed-package.html";
  const result = validateSameOriginRouteEvidence({
    mode: "installed",
    pageUrl: installedPageUrl,
    hardStopNonce: nonce,
    instrumentation,
    contexts: normalizedInstalledContexts(installedPageUrl),
    routes: installedRouteEvidence()
  });
  assert.equal(result.requestCount, 22);
  assert.ok(
    result.routes.some(
      ({ routeKey }) => routeKey === "/installed-package/js/client.mjs"
    )
  );
  assert.ok(
    result.routes.every(
      ({ routeKey }) =>
        !routeKey.startsWith("/js/") && !routeKey.startsWith("/pkg/")
    )
  );
});

for (const mode of ["source", "installed"]) {
  test(`${mode} response contracts load exactly once in the page and each worker`, () => {
    const installed = mode === "installed";
    const modePageUrl = installed
      ? "http://127.0.0.1:4173/tests/browser/installed-package.html"
      : pageUrl;
    const contexts = installed
      ? normalizedInstalledContexts(modePageUrl)
      : normalizedSourceContexts();
    const routes = installed ? installedRouteEvidence() : sourceRouteEvidence();
    const validate = (evidence) => validateSameOriginRouteEvidence({
      mode,
      pageUrl: modePageUrl,
      hardStopNonce: nonce,
      instrumentation,
      contexts,
      routes: evidence
    });
    const contractRoutes = validate(routes).routes.filter(({ routeKey }) =>
      routeKey.endsWith("/js/response-contracts.mjs")
    );
    assert.deepEqual(
      contractRoutes.map(({ owner }) => owner),
      ["page", "primary", "hard-stop"]
    );

    for (const { requestId, sessionId } of contractRoutes) {
      assert.throws(
        () => validate(routes.filter((route) => route.requestId !== requestId)),
        /response-contracts\.mjs was observed 0 times instead of 1/
      );
      const original = routes.find((route) => route.requestId === requestId);
      assert.throws(
        () => validate([
          ...routes,
          {
            ...original,
            requestId: `duplicate-${sessionId}`,
            sequence: routes.at(-1).sequence + 1
          }
        ]),
        /response-contracts\.mjs was observed 2 times instead of 1/
      );
      assert.throws(
        () => validate(routes.map((route) => route.requestId === requestId
          ? { ...route, url: route.url.replace("response-contracts.mjs", "unknown.mjs") }
          : route)),
        /unknown .* route/
      );
      if (installed) {
        assert.throws(
          () => validate(routes.map((route) => route.requestId === requestId
            ? { ...route, url: "/js/response-contracts.mjs" }
            : route)),
          /unknown .* route/
        );
      }
    }
  });
}

test("CSP page control cannot emit any Network or Fetch event", () => {
  const proof = {
    schema: "rxls.render-csp-negative.v1",
    fetchRejected: true,
    rejectionName: "TypeError",
    url: RENDER_WORKER_CSP_NEGATIVE_URL,
    violationCount: 1,
    violation: {
      blockedURI: RENDER_WORKER_CSP_NEGATIVE_URL,
      disposition: "enforce",
      effectiveDirective: "connect-src",
      isTrusted: true,
      statusCode: 200,
      violatedDirective: "connect-src"
    }
  };
  assert.deepEqual(
    validateCspNetworkSilence({
      proof,
      requests: [],
      responses: [],
      failures: [],
      pauses: []
    }),
    {
      url: proof.url,
      networkRequestEmitted: false,
      networkResponseEmitted: false,
      networkFailureEmitted: false,
      fetchPauseEmitted: false
    }
  );
  assert.throws(
    () =>
      validateCspNetworkSilence({
        proof: { ...proof, url: "https://unrelated.invalid/" },
        requests: [],
        responses: [],
        failures: [],
        pauses: []
      }),
    /invalid/
  );
  assert.throws(
    () =>
      validateCspNetworkSilence({
        proof: {
          ...proof,
          violation: { ...proof.violation, isTrusted: false }
        },
        requests: [],
        responses: [],
        failures: [],
        pauses: []
      }),
    /trusted connect-src/
  );
  for (const spoofed of [
    { ...proof, rejectionName: "Error" },
    { ...proof, violationCount: 2 }
  ]) {
    assert.throws(
      () =>
        validateCspNetworkSilence({
          proof: spoofed,
          requests: [],
          responses: [],
          failures: [],
          pauses: []
        }),
      /invalid/
    );
  }
  for (const [key, event] of [
    ["requests", { requestId: "escaped", url: proof.url }],
    ["responses", { requestId: "escaped", url: proof.url }],
    ["failures", { requestId: "escaped", url: proof.url }],
    ["pauses", { requestId: "escaped", url: proof.url }]
  ]) {
    assert.throws(
      () =>
        validateCspNetworkSilence({
          proof,
          requests: [],
          responses: [],
          failures: [],
          pauses: [],
          [key]: [event]
        }),
      /escaped/
    );
  }
});

test("CDP Network off-origin control preserves offline rejection and no response", () => {
  const requests = [{
    requestId: "control",
    sessionId: "network-session",
    url: NETWORK_NEGATIVE_URL
  }];
  const pauses = [{
    requestId: "fetch-control",
    sessionId: "network-session",
    networkId: "control",
    resourceType: "XHR",
    url: NETWORK_NEGATIVE_URL
  }];
  const failures = [{
    requestId: "control",
    sessionId: "network-session",
    blockedReason: null,
    canceled: false,
    errorText: "net::ERR_INTERNET_DISCONNECTED"
  }];
  assert.deepEqual(
    validateOfflineNetworkBlock({ requests, failures, responses: [], pauses }),
    {
      url: NETWORK_NEGATIVE_URL,
      requestId: "control",
      errorText: "net::ERR_INTERNET_DISCONNECTED",
      responseReceived: false,
      interceptedBeforeResponse: true
    }
  );
  assert.throws(
    () =>
      validateOfflineNetworkBlock({
        requests,
        failures: [{ ...failures[0], errorText: "net::ERR_FAILED" }],
        responses: [],
        pauses
      }),
    /exact offline rejection/
  );
  assert.throws(
    () =>
      validateOfflineNetworkBlock({
        requests,
        failures,
        responses: [{ requestId: "control" }],
        pauses
      }),
    /received a response/
  );
  assert.throws(
    () =>
      validateOfflineNetworkBlock({
        requests,
        failures,
        responses: [],
        pauses: [{ ...pauses[0], networkId: "decoy" }]
      }),
    /identities/
  );
  assert.throws(
    () =>
      validateOfflineNetworkBlock({
        requests,
        failures: [{ ...failures[0], sessionId: "decoy-session" }],
        responses: [],
        pauses
      }),
    /exact offline rejection/
  );
});

function workerProofs(contexts) {
  return contexts.map((context, index) => ({
    targetId: context.targetId,
    sessionId: context.sessionId,
    probeStartedSequence: index === 0 ? 30 : 50,
    probeCompletedSequence: index === 0 ? 31 : 51,
    proof: {
      schema: RENDER_WORKER_CSP_PROOF_SCHEMA,
      locationHref: context.url,
      url: RENDER_WORKER_CSP_NEGATIVE_URL,
      fetchRejected: true,
      rejectionName: "TypeError",
      violationCount: 1,
      violation: {
        blockedURI: RENDER_WORKER_CSP_NEGATIVE_URL,
        disposition: "enforce",
        effectiveDirective: "connect-src",
        isTrusted: true,
        violatedDirective: "connect-src"
      }
    },
    network: {
      requests: [],
      responses: [],
      failures: [],
      pauses: []
    }
  }));
}

function sourceRouteEvidence() {
  return routeEvidence([
    "/tests/browser/index.html",
    "/tests/browser/bootstrap.mjs",
    "/tests/browser/smoke.mjs",
    "/tests/browser/scenario.mjs",
    "/tests/browser/contract.mjs",
    "/tests/browser/fixture.mjs",
    "/tests/browser/proof.mjs",
    "/js/client.mjs",
    "/js/protocol.mjs",
    "/js/response-contracts.mjs",
    "/js/worker.mjs",
    "/js/worker-runtime.mjs",
    "/js/protocol.mjs",
    "/js/response-contracts.mjs",
    "/pkg/rxls_render_wasm.js",
    "/pkg/rxls_render_wasm_bg.wasm",
    `/js/worker.mjs?rxls-hard-stop=${nonce}`,
    "/js/worker-runtime.mjs",
    "/js/protocol.mjs",
    "/js/response-contracts.mjs",
    "/pkg/rxls_render_wasm.js",
    "/pkg/rxls_render_wasm_bg.wasm"
  ]);
}

function normalizedSourceContexts() {
  return normalizeRenderWorkerContexts({
    mode: "source",
    pageUrl,
    hardStopNonce: nonce,
    instrumentation,
    contexts: sourceContexts
  });
}

function normalizedInstalledContexts(installedPageUrl) {
  return normalizeRenderWorkerContexts({
    mode: "installed",
    pageUrl: installedPageUrl,
    hardStopNonce: nonce,
    instrumentation,
    contexts: sourceContexts.map((context) => ({
      ...context,
      url: context.url.replace("/js/worker.mjs", "/installed-package/js/worker.mjs")
    }))
  });
}

function installedRouteEvidence() {
  return routeEvidence([
    "/tests/browser/installed-package.html",
    "/tests/browser/installed-package-bootstrap.mjs",
    "/tests/browser/installed-package.mjs",
    "/tests/browser/scenario.mjs",
    "/tests/browser/contract.mjs",
    "/tests/browser/fixture.mjs",
    "/tests/browser/proof.mjs",
    "/installed-package/js/client.mjs",
    "/installed-package/js/protocol.mjs",
    "/installed-package/js/response-contracts.mjs",
    "/installed-package/js/worker.mjs",
    "/installed-package/js/worker-runtime.mjs",
    "/installed-package/js/protocol.mjs",
    "/installed-package/js/response-contracts.mjs",
    "/installed-package/pkg/rxls_render_wasm.js",
    "/installed-package/pkg/rxls_render_wasm_bg.wasm",
    `/installed-package/js/worker.mjs?rxls-hard-stop=${nonce}`,
    "/installed-package/js/worker-runtime.mjs",
    "/installed-package/js/protocol.mjs",
    "/installed-package/js/response-contracts.mjs",
    "/installed-package/pkg/rxls_render_wasm.js",
    "/installed-package/pkg/rxls_render_wasm_bg.wasm"
  ]);
}

function routeEvidence(paths) {
  return paths.map((url, index) => ({
    sequence: index + 5,
    requestId: `route-${index}`,
    sessionId:
      index <= 10 || index === 16
        ? instrumentation.pageSessionId
        : index <= 15
          ? "primary-session"
          : "hard-stop-session",
    method: "GET",
    url
  }));
}
