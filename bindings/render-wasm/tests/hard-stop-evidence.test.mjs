import assert from "node:assert/strict";
import test from "node:test";

import {
  correlateHardStopTarget,
  decideHardStopObservation,
  findNonceBoundWorker,
  hardStopObservationDeadlineEpochMs
} from "./browser/hard-stop-evidence.mjs";

const nonce = "0123456789abcdef0123456789abcdef";
const workerUrl = `http://127.0.0.1/js/worker.mjs?rxls-hard-stop=${nonce}`;
const proof = Object.freeze({
  schema: "rxls.render-hard-stop.v2",
  phase: "complete",
  nonce,
  workerUrl,
  documentId: `hard-stop-${nonce}`,
  activeRequestId: "request-2",
  queuedRequestId: "request-3",
  activeEpochMs: 9_800,
  startedEpochMs: 10_000,
  completedEpochMs: 10_050,
  elapsedMs: 1,
  deadlineMs: 1_000,
  rejectedRequests: 2,
  activeOutcome: { status: "rejected", code: "client_closed" },
  queuedOutcome: { status: "rejected", code: "client_closed" },
  futureOutcome: { status: "rejected", code: "client_closed" }
});
const primary = target("primary", "http://127.0.0.1/js/worker.mjs", 9_000);
const hardStop = target("hard-stop", workerUrl, 9_500);
const wasmPause = {
  targetId: "hard-stop",
  sessionId: "session-hard-stop",
  activeObservedAtEpochMs: 9_750,
  observedAtEpochMs: 9_900,
  resumedAtEpochMs: 9_950,
  debuggerDisabledAtEpochMs: 9_975,
  terminationCommandEpochMs: 10_000,
  wasmFrame: {
    scriptId: "42",
    url: "wasm://wasm/rxls_render_wasm_bg.wasm"
  }
};

test("hard-stop lifecycle requires nonce, active WASM pause, destruction, and absence", () => {
  assert.deepEqual(
    correlateHardStopTarget({
      attachedTargets: [primary, hardStop],
      primaryTargetId: "primary",
      destroyedTargets: new Map([["hard-stop", 10_400]]),
      currentTargets: [{ targetId: "primary", url: primary.targetInfo.url }],
      pauseEvidence: wasmPause,
      proof
    }),
    {
      targetId: "hard-stop",
      nonce,
      activeRequestId: "request-2",
      queuedRequestId: "request-3",
      rejectedRequests: 2,
      wasmFrame: wasmPause.wasmFrame,
      elapsedMs: 400,
      deadlineMs: 1_000,
      absentFromTargetInventory: true
    }
  );
});

test("hard-stop deadline starts at the public termination call, not CDP delivery", () => {
  const deliveredProof = {
    ...proof,
    startedEpochMs: 10_600,
    completedEpochMs: 10_650,
    deadlineMs: 2_000
  };
  const deliveredPause = {
    ...wasmPause,
    terminationCommandEpochMs: 10_000
  };
  const base = {
    attachedTargets: [primary, hardStop],
    primaryTargetId: "primary",
    currentTargets: [],
    pauseEvidence: deliveredPause,
    proof: deliveredProof
  };

  assert.equal(hardStopObservationDeadlineEpochMs(deliveredProof, 500), 13_100);
  assert.equal(
    decideHardStopObservation({
      destructionRecorded: true,
      nowEpochMs: 13_101,
      observationDeadlineEpochMs: 13_100
    }),
    "inspect"
  );
  assert.equal(
    decideHardStopObservation({
      destructionRecorded: false,
      nowEpochMs: 13_101,
      observationDeadlineEpochMs: 13_100
    }),
    "expired"
  );
  assert.equal(
    correlateHardStopTarget({
      ...base,
      destroyedTargets: new Map([["hard-stop", 12_600]])
    }).elapsedMs,
    2_000
  );
  assert.throws(
    () =>
      correlateHardStopTarget({
        ...base,
        destroyedTargets: new Map([["hard-stop", 12_601]])
      }),
    /ended after 2001ms/
  );
  assert.throws(
    () =>
      correlateHardStopTarget({
        ...base,
        destroyedTargets: new Map([["hard-stop", 10_100]]),
        proof: { ...deliveredProof, startedEpochMs: 9_999 }
      }),
    /before the controller command/
  );
});

test("detach-only and natural completion cannot satisfy hard-stop evidence", () => {
  assert.equal(
    correlateHardStopTarget({
      attachedTargets: [primary, hardStop],
      primaryTargetId: "primary",
      destroyedTargets: new Map(),
      currentTargets: [],
      pauseEvidence: wasmPause,
      proof
    }),
    null
  );
  assert.throws(
    () =>
      correlateHardStopTarget({
        attachedTargets: [primary, hardStop],
        primaryTargetId: "primary",
        destroyedTargets: new Map([["hard-stop", 10_100]]),
        currentTargets: [],
        pauseEvidence: wasmPause,
        proof: {
          ...proof,
          activeOutcome: { status: "fulfilled", code: null }
        }
      }),
    /active request was not rejected/
  );
});

test("decoy and wrong-nonce workers cannot satisfy the nonce-bound proof", () => {
  const decoy = target(
    "decoy",
    "http://127.0.0.1/js/worker.mjs?rxls-hard-stop=ffffffffffffffffffffffffffffffff",
    9_500
  );
  assert.equal(
    findNonceBoundWorker({
      attachedTargets: [primary, decoy],
      primaryTargetId: "primary",
      proof
    }),
    null
  );
  assert.throws(
    () =>
      findNonceBoundWorker({
        attachedTargets: [primary, hardStop],
        primaryTargetId: "primary",
        proof: {
          ...proof,
          nonce: "ffffffffffffffffffffffffffffffff",
          documentId: "hard-stop-ffffffffffffffffffffffffffffffff"
        }
      }),
    /not bound to the proof nonce/
  );
  assert.throws(
    () =>
      findNonceBoundWorker({
        attachedTargets: [primary, hardStop],
        primaryTargetId: "primary",
        proof: { ...proof, documentId: "decoy-document" }
      }),
    /nonce binding/
  );
});

test("ambiguous targets, missing WASM frames, and retained targets fail closed", () => {
  assert.throws(
    () =>
      findNonceBoundWorker({
        attachedTargets: [primary, hardStop, target("duplicate", workerUrl, 9_600)],
        primaryTargetId: "primary",
        proof
      }),
    /ambiguous/
  );
  assert.throws(
    () =>
      correlateHardStopTarget({
        attachedTargets: [primary, hardStop],
        primaryTargetId: "primary",
        destroyedTargets: new Map([["hard-stop", 10_100]]),
        currentTargets: [],
        pauseEvidence: {
          ...wasmPause,
          wasmFrame: { scriptId: "7", url: "file:///worker.mjs" }
        },
        proof
      }),
    /active WebAssembly frame/
  );
  assert.throws(
    () =>
      correlateHardStopTarget({
        attachedTargets: [primary, hardStop],
        primaryTargetId: "primary",
        destroyedTargets: new Map([["hard-stop", 10_100]]),
        currentTargets: [{ targetId: "hard-stop", url: workerUrl }],
        pauseEvidence: wasmPause,
        proof
      }),
    /remains in Target.getTargets/
  );
  assert.throws(
    () =>
      correlateHardStopTarget({
        attachedTargets: [primary, hardStop],
        primaryTargetId: "primary",
        destroyedTargets: new Map([["hard-stop", 11_001]]),
        currentTargets: [],
        pauseEvidence: wasmPause,
        proof
      }),
    /ended after/
  );
  assert.throws(
    () =>
      correlateHardStopTarget({
        attachedTargets: [primary, hardStop],
        primaryTargetId: "primary",
        destroyedTargets: new Map([["hard-stop", 10_100]]),
        currentTargets: [],
        pauseEvidence: { ...wasmPause, observedAtEpochMs: 10_001 },
        proof
      }),
    /active WebAssembly frame/
  );
  assert.throws(
    () =>
      correlateHardStopTarget({
        attachedTargets: [primary, hardStop],
        primaryTargetId: "primary",
        destroyedTargets: new Map([["hard-stop", 10_100]]),
        currentTargets: [],
        pauseEvidence: { ...wasmPause, resumedAtEpochMs: 10_001 },
        proof
      }),
    /not resumed after/
  );
  assert.throws(
    () =>
      correlateHardStopTarget({
        attachedTargets: [primary, hardStop],
        primaryTargetId: "primary",
        destroyedTargets: new Map([["hard-stop", 10_100]]),
        currentTargets: [],
        pauseEvidence: { ...wasmPause, debuggerDisabledAtEpochMs: 10_001 },
        proof
      }),
    /debugger was not disabled/
  );
});

function target(targetId, url, observedAtEpochMs) {
  return {
    sessionId: `session-${targetId}`,
    observedAtEpochMs,
    targetInfo: {
      targetId,
      type: "worker",
      url
    }
  };
}
