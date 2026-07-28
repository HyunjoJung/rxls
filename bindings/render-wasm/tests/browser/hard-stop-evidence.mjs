export function findNonceBoundWorker({ attachedTargets, primaryTargetId, proof }) {
  validateNonceBinding(proof);
  const candidates = new Map();
  for (const attached of attachedTargets) {
    const target = attached?.targetInfo;
    if (
      target?.type !== "worker" ||
      target.targetId === primaryTargetId ||
      target.url !== proof.workerUrl
    ) {
      continue;
    }
    if (!Number.isFinite(attached.observedAtEpochMs)) {
      throw new Error(`hard-stop worker ${target.targetId} has no attach timestamp`);
    }
    candidates.set(target.targetId, attached);
  }
  if (candidates.size > 1) {
    throw new Error(
      `nonce-bound hard-stop worker is ambiguous: ${JSON.stringify([...candidates.keys()])}`
    );
  }
  return candidates.size === 0 ? null : candidates.values().next().value;
}

export function hardStopObservationDeadlineEpochMs(proof, graceMs) {
  validateCompletedProof(proof);
  if (!Number.isSafeInteger(graceMs) || graceMs < 0) {
    throw new Error("hard-stop observation grace must be a non-negative integer");
  }
  const deadlineEpochMs = proof.startedEpochMs + proof.deadlineMs + graceMs;
  if (!Number.isSafeInteger(deadlineEpochMs)) {
    throw new Error("hard-stop observation deadline is outside the safe timestamp range");
  }
  return deadlineEpochMs;
}

export function decideHardStopObservation({
  destructionRecorded,
  nowEpochMs,
  observationDeadlineEpochMs
}) {
  if (
    typeof destructionRecorded !== "boolean" ||
    !Number.isSafeInteger(nowEpochMs) ||
    !Number.isSafeInteger(observationDeadlineEpochMs)
  ) {
    throw new Error("hard-stop observation state is invalid");
  }
  if (destructionRecorded) {
    return "inspect";
  }
  return nowEpochMs <= observationDeadlineEpochMs ? "wait" : "expired";
}

export function correlateHardStopTarget({
  attachedTargets,
  primaryTargetId,
  destroyedTargets,
  currentTargets,
  pauseEvidence,
  proof
}) {
  validateCompletedProof(proof);
  const attached = findNonceBoundWorker({ attachedTargets, primaryTargetId, proof });
  if (attached === null) {
    return null;
  }
  const targetId = attached.targetInfo.targetId;
  const terminationCommandEpochMs = pauseEvidence?.terminationCommandEpochMs;
  if (!Number.isFinite(terminationCommandEpochMs)) {
    throw new Error("hard-stop controller termination timestamp is missing");
  }
  if (attached.observedAtEpochMs > terminationCommandEpochMs) {
    throw new Error(`hard-stop worker ${targetId} attached after termination started`);
  }

  const destroyedAtEpochMs = destroyedTargets.get(targetId);
  if (destroyedAtEpochMs === undefined) {
    return null;
  }
  if (
    !Number.isFinite(destroyedAtEpochMs) ||
    destroyedAtEpochMs < proof.startedEpochMs
  ) {
    throw new Error(`hard-stop worker ${targetId} was destroyed before termination`);
  }
  if (proof.startedEpochMs < terminationCommandEpochMs) {
    throw new Error("hard-stop page started termination before the controller command");
  }
  const elapsedMs = Math.ceil(destroyedAtEpochMs - proof.startedEpochMs);
  if (elapsedMs > proof.deadlineMs) {
    throw new Error(
      `hard-stop worker target ended after ${elapsedMs}ms, deadline ${proof.deadlineMs}ms`
    );
  }

  validateWasmPause(pauseEvidence, targetId);
  if (!Array.isArray(currentTargets)) {
    throw new Error("Target.getTargets confirmation is missing");
  }
  if (
    currentTargets.some(
      (target) => target?.targetId === targetId || target?.url === proof.workerUrl
    )
  ) {
    throw new Error("nonce-bound hard-stop worker remains in Target.getTargets");
  }
  validateRejectedOutcomes(proof);

  return {
    targetId,
    nonce: proof.nonce,
    activeRequestId: proof.activeRequestId,
    queuedRequestId: proof.queuedRequestId,
    rejectedRequests: proof.rejectedRequests,
    wasmFrame: pauseEvidence.wasmFrame,
    elapsedMs,
    deadlineMs: proof.deadlineMs,
    absentFromTargetInventory: true
  };
}

export function validateWasmPause(pauseEvidence, targetId) {
  if (
    pauseEvidence?.targetId !== targetId ||
    typeof pauseEvidence.sessionId !== "string" ||
    !Number.isFinite(pauseEvidence.activeObservedAtEpochMs) ||
    !Number.isFinite(pauseEvidence.terminationCommandEpochMs) ||
    !Number.isFinite(pauseEvidence.observedAtEpochMs) ||
    pauseEvidence.observedAtEpochMs < pauseEvidence.activeObservedAtEpochMs ||
    pauseEvidence.observedAtEpochMs > pauseEvidence.terminationCommandEpochMs ||
    typeof pauseEvidence.wasmFrame?.scriptId !== "string" ||
    typeof pauseEvidence.wasmFrame?.url !== "string" ||
    (!pauseEvidence.wasmFrame.url.startsWith("wasm://") &&
      pauseEvidence.wasmFrame.scriptLanguage !== "WebAssembly")
  ) {
    throw new Error("hard-stop worker was not paused in an active WebAssembly frame");
  }
}

export function validateRejectedOutcomes(proof) {
  for (const [label, outcome] of [
    ["active", proof.activeOutcome],
    ["queued", proof.queuedOutcome],
    ["future", proof.futureOutcome]
  ]) {
    if (outcome?.status !== "rejected" || outcome.code !== "client_closed") {
      throw new Error(`${label} request was not rejected by hard stop`);
    }
  }
  if (proof.rejectedRequests !== 2) {
    throw new Error("hard stop did not reject both pending requests");
  }
}

function validateNonceBinding(proof) {
  if (
    proof?.schema !== "rxls.render-hard-stop.v2" ||
    typeof proof.nonce !== "string" ||
    !/^[0-9a-f]{32}$/.test(proof.nonce) ||
    typeof proof.workerUrl !== "string" ||
    proof.documentId !== `hard-stop-${proof.nonce}`
  ) {
    throw new Error("hard-stop proof nonce binding is invalid");
  }
  let url;
  try {
    url = new URL(proof.workerUrl);
  } catch {
    throw new Error("hard-stop proof worker URL is invalid");
  }
  if (url.searchParams.get("rxls-hard-stop") !== proof.nonce) {
    throw new Error("hard-stop worker URL is not bound to the proof nonce");
  }
}

function validateCompletedProof(proof) {
  validateNonceBinding(proof);
  if (
    proof.phase !== "complete" ||
    typeof proof.activeRequestId !== "string" ||
    typeof proof.queuedRequestId !== "string" ||
    !Number.isFinite(proof.activeEpochMs) ||
    !Number.isFinite(proof.startedEpochMs) ||
    !Number.isFinite(proof.completedEpochMs) ||
    proof.activeEpochMs > proof.startedEpochMs ||
    proof.startedEpochMs > proof.completedEpochMs ||
    !Number.isSafeInteger(proof.elapsedMs) ||
    proof.elapsedMs < 0 ||
    proof.elapsedMs > proof.deadlineMs ||
    !Number.isSafeInteger(proof.deadlineMs) ||
    proof.deadlineMs <= 0
  ) {
    throw new Error("hard-stop proof did not reach a valid completed state");
  }
}
