import { parentPort, workerData } from "node:worker_threads";

import {
  MAX_RSS_SAMPLES,
  MAX_RSS_SAMPLE_INTERVAL_MS,
  MIN_BOUNDARY_RSS_SAMPLES,
  sampleProcessTreeRss
} from "./memory-evidence.mjs";

const MAX_FAILURE_MESSAGE_BYTES = 512;

if (parentPort === null) {
  throw new Error("RSS sampler requires a parent port");
}

const { rootPid, intervalMs, maxSamples } = validateWorkerData(workerData);
let timer = null;
let sampleCount = 0;
let finished = false;

parentPort.on("message", (message) => {
  if (
    message === null ||
    typeof message !== "object" ||
    Array.isArray(message) ||
    Object.keys(message).join(",") !== "type" ||
    message.type !== "stop"
  ) {
    fail(new Error("RSS sampler received an invalid control message"));
    return;
  }
  stop();
});

sample();

function sample() {
  if (finished) {
    return;
  }
  if (sampleCount >= maxSamples) {
    fail(new Error(`RSS sampler exceeded its ${maxSamples}-sample bound`));
    return;
  }
  try {
    const startedAtEpochMs = Date.now();
    const processMemory = sampleProcessTreeRss(rootPid);
    parentPort.postMessage({
      type: "sample",
      sequence: sampleCount,
      sampledAtEpochMs: Date.now(),
      rootPid: processMemory.rootPid,
      processCount: processMemory.processCount,
      rssBytes: processMemory.rssBytes
    });
    sampleCount += 1;
    const elapsedMs = Date.now() - startedAtEpochMs;
    timer = setTimeout(sample, Math.max(0, intervalMs - elapsedMs));
  } catch (error) {
    fail(error);
  }
}

function stop() {
  if (finished) {
    return;
  }
  finished = true;
  clearTimeout(timer);
  parentPort.postMessage({ type: "stopped", sampleCount });
  parentPort.close();
}

function fail(error) {
  if (finished) {
    return;
  }
  finished = true;
  clearTimeout(timer);
  const message = boundedMessage(error);
  parentPort.postMessage({ type: "failure", message });
  parentPort.close();
}

function validateWorkerData(value) {
  if (
    value === null ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    Object.keys(value).sort().join(",") !==
      "intervalMs,maxSamples,rootPid" ||
    !Number.isSafeInteger(value.rootPid) ||
    value.rootPid <= 0 ||
    !Number.isSafeInteger(value.intervalMs) ||
    value.intervalMs <= 0 ||
    value.intervalMs > MAX_RSS_SAMPLE_INTERVAL_MS ||
    !Number.isSafeInteger(value.maxSamples) ||
    value.maxSamples < MIN_BOUNDARY_RSS_SAMPLES ||
    value.maxSamples > MAX_RSS_SAMPLES
  ) {
    throw new Error("RSS sampler worker data is invalid");
  }
  return value;
}

function boundedMessage(error) {
  const message = error instanceof Error ? error.message : String(error);
  return message
    .replace(/[^\x20-\x7e]/g, "?")
    .slice(0, MAX_FAILURE_MESSAGE_BYTES);
}
