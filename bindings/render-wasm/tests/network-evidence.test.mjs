import assert from "node:assert/strict";
import test from "node:test";

import {
  NETWORK_NEGATIVE_URL,
  validateCspNetworkSilence,
  validateOfflineNetworkBlock
} from "./browser/network-evidence.mjs";

test("CSP control cannot emit an off-origin Network request", () => {
  const proof = {
    schema: "rxls.render-csp-negative.v1",
    fetchRejected: true,
    url: "https://rxls-csp-negative.invalid/render-worker-control"
  };
  assert.deepEqual(
    validateCspNetworkSilence({ proof, requests: [], responses: [] }),
    { url: proof.url, networkRequestEmitted: false }
  );
  assert.throws(
    () =>
      validateCspNetworkSilence({
        proof,
        requests: [{ requestId: "escaped", url: proof.url }],
        responses: []
      }),
    /escaped/
  );
});

test("CDP Network off-origin control requires offline rejection and no response", () => {
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
