import { spawnSync } from "node:child_process";
import { Worker as NodeWorker } from "node:worker_threads";

const HEAP_FIELDS = [
  "usedSize",
  "totalSize",
  "embedderHeapUsedSize",
  "backingStorageSize"
];
const MAX_PS_OUTPUT_BYTES = 1024 * 1024;
const MAX_PS_ROWS = 8_192;
export const RSS_SAMPLE_INTERVAL_MS = 10;
export const MAX_RSS_SAMPLE_INTERVAL_MS = 25;
export const MIN_BOUNDARY_RSS_SAMPLES = 5;
export const MAX_RSS_SAMPLES = 256;
export const MAX_BOUNDARY_RSS_GAP_MS = 100;
export const MAX_BOUNDARY_RSS_DURATION_MS = 2_000;
export const MIN_PENDING_BOUNDARY_RSS_GROWTH_BYTES = 96 * 1024 * 1024;
const RSS_SAMPLER_STOP_TIMEOUT_MS = 2_000;
const MAX_RSS_SAMPLER_ERROR_BYTES = 512;

export class IndependentRssSampler {
  #rootPid;
  #intervalMs;
  #maxSamples;
  #worker;
  #samples = [];
  #failure = null;
  #stopped = false;
  #stopRequested = false;
  #waiters = new Set();

  constructor(
    rootPid,
    {
      intervalMs = RSS_SAMPLE_INTERVAL_MS,
      maxSamples = MAX_RSS_SAMPLES,
      WorkerClass = NodeWorker,
      workerUrl = new URL("./rss-sampler-worker.mjs", import.meta.url)
    } = {}
  ) {
    assertRootPid(rootPid);
    assertSamplerConfiguration(intervalMs, maxSamples);
    if (typeof WorkerClass !== "function") {
      throw new TypeError("RSS sampler WorkerClass must be a constructor");
    }
    if (!(workerUrl instanceof URL)) {
      throw new TypeError("RSS sampler worker URL must be a URL");
    }
    this.#rootPid = rootPid;
    this.#intervalMs = intervalMs;
    this.#maxSamples = maxSamples;
    this.#worker = new WorkerClass(workerUrl, {
      workerData: { rootPid, intervalMs, maxSamples }
    });
    if (
      this.#worker === null ||
      typeof this.#worker.on !== "function" ||
      typeof this.#worker.postMessage !== "function" ||
      typeof this.#worker.terminate !== "function"
    ) {
      throw new TypeError("RSS sampler worker does not implement the Node Worker contract");
    }
    this.#worker.on("message", (message) => this.#receive(message));
    this.#worker.on("error", (error) => {
      this.#fail(
        `RSS sampler worker error: ${boundedErrorMessage(error)}`
      );
    });
    this.#worker.on("exit", (code) => {
      if (!this.#stopped && this.#failure === null) {
        this.#fail(`RSS sampler worker exited before acknowledgement (code=${code})`);
      }
      this.#signal();
    });
  }

  get intervalMs() {
    return this.#intervalMs;
  }

  samples() {
    this.#assertHealthy();
    return this.#samples.map((sample) => ({ ...sample }));
  }

  async waitForSamplesSince(
    boundaryStartEpochMs,
    minimum = MIN_BOUNDARY_RSS_SAMPLES,
    timeoutMs = RSS_SAMPLER_STOP_TIMEOUT_MS
  ) {
    assertEpochMilliseconds(boundaryStartEpochMs, "RSS boundary start");
    if (
      !Number.isSafeInteger(minimum) ||
      minimum <= 0 ||
      minimum > this.#maxSamples
    ) {
      throw new Error("RSS boundary minimum sample count is invalid");
    }
    if (!Number.isSafeInteger(timeoutMs) || timeoutMs <= 0 || timeoutMs > 10_000) {
      throw new Error("RSS sampler wait timeout is invalid");
    }
    const deadline = Date.now() + timeoutMs;
    while (true) {
      this.#assertHealthy();
      const samples = this.#samples.filter(
        ({ sampledAtEpochMs }) => sampledAtEpochMs >= boundaryStartEpochMs
      );
      if (samples.length >= minimum) {
        return samples.map((sample) => ({ ...sample }));
      }
      const remainingMs = deadline - Date.now();
      if (remainingMs <= 0) {
        throw new Error(
          `RSS sampler captured ${samples.length}/${minimum} required boundary samples`
        );
      }
      await this.#waitForChange(remainingMs);
    }
  }

  async stop() {
    if (!this.#stopRequested) {
      this.#stopRequested = true;
      try {
        this.#worker.postMessage({ type: "stop" });
      } catch (error) {
        this.#fail(
          `RSS sampler stop request failed: ${boundedErrorMessage(error)}`
        );
      }
    }
    const deadline = Date.now() + RSS_SAMPLER_STOP_TIMEOUT_MS;
    while (!this.#stopped && this.#failure === null) {
      const remainingMs = deadline - Date.now();
      if (remainingMs <= 0) {
        this.#fail("RSS sampler stop acknowledgement timed out");
        break;
      }
      await this.#waitForChange(remainingMs);
    }
    try {
      await this.#worker.terminate();
    } catch (error) {
      this.#fail(
        `RSS sampler worker termination failed: ${boundedErrorMessage(error)}`
      );
    }
    this.#assertHealthy();
    return this.#samples.map((sample) => ({ ...sample }));
  }

  #receive(message) {
    try {
      if (
        message === null ||
        typeof message !== "object" ||
        Array.isArray(message)
      ) {
        throw new Error("RSS sampler emitted a non-object message");
      }
      if (message.type === "sample") {
        assertExactObjectKeys(
          message,
          [
            "type",
            "sequence",
            "sampledAtEpochMs",
            "rootPid",
            "processCount",
            "rssBytes"
          ],
          "RSS sampler sample"
        );
        if (
          message.sequence !== this.#samples.length ||
          this.#samples.length >= this.#maxSamples
        ) {
          throw new Error("RSS sampler sequence or sample bound changed");
        }
        assertEpochMilliseconds(
          message.sampledAtEpochMs,
          "RSS sample timestamp"
        );
        const prior = this.#samples.at(-1);
        if (
          prior !== undefined &&
          message.sampledAtEpochMs < prior.sampledAtEpochMs
        ) {
          throw new Error("RSS sampler timestamps are not ordered");
        }
        assertProcessMemorySample(message, "independent RSS");
        if (message.rootPid !== this.#rootPid) {
          throw new Error("RSS sampler root PID changed");
        }
        this.#samples.push({
          sequence: message.sequence,
          sampledAtEpochMs: message.sampledAtEpochMs,
          rootPid: message.rootPid,
          processCount: message.processCount,
          rssBytes: message.rssBytes
        });
      } else if (message.type === "stopped") {
        assertExactObjectKeys(
          message,
          ["type", "sampleCount"],
          "RSS sampler stop"
        );
        if (
          !this.#stopRequested ||
          this.#stopped ||
          message.sampleCount !== this.#samples.length
        ) {
          throw new Error("RSS sampler stop acknowledgement changed");
        }
        this.#stopped = true;
      } else if (message.type === "failure") {
        assertExactObjectKeys(
          message,
          ["type", "message"],
          "RSS sampler failure"
        );
        if (
          typeof message.message !== "string" ||
          message.message.length === 0 ||
          Buffer.byteLength(message.message) > MAX_RSS_SAMPLER_ERROR_BYTES
        ) {
          throw new Error("RSS sampler failure message is invalid");
        }
        throw new Error(`RSS sampler failed: ${message.message}`);
      } else {
        throw new Error("RSS sampler emitted an unknown message");
      }
    } catch (error) {
      this.#fail(error instanceof Error ? error.message : String(error));
    }
    this.#signal();
  }

  #fail(message) {
    this.#failure ??= new Error(message);
    this.#signal();
  }

  #assertHealthy() {
    if (this.#failure !== null) {
      throw this.#failure;
    }
  }

  #waitForChange(timeoutMs) {
    return new Promise((resolve) => {
      const waiter = () => {
        clearTimeout(timer);
        this.#waiters.delete(waiter);
        resolve();
      };
      const timer = setTimeout(waiter, timeoutMs);
      this.#waiters.add(waiter);
    });
  }

  #signal() {
    for (const waiter of [...this.#waiters]) {
      waiter();
    }
  }
}

export function normalizeHeap(sample, label = "target") {
  if (sample === null || typeof sample !== "object" || Array.isArray(sample)) {
    throw new Error(`${label} heap sample is not an object`);
  }
  const normalized = {};
  for (const field of HEAP_FIELDS) {
    const value = sample[field];
    if (!Number.isSafeInteger(value) || value < 0) {
      throw new Error(`${label} heap field ${field} is invalid`);
    }
    normalized[field] = value;
  }
  normalized.accountedBytes = safeSum(
    HEAP_FIELDS.map((field) => normalized[field]),
    `${label} accounted heap`
  );
  return normalized;
}

export function combineHeaps(samples) {
  if (!Array.isArray(samples) || samples.length < 2) {
    throw new Error("combined heap requires the page and at least one worker");
  }
  const targets = {};
  for (const { label, sample } of samples) {
    if (typeof label !== "string" || label.length === 0 || targets[label]) {
      throw new Error("combined heap target labels must be unique");
    }
    targets[label] = normalizeHeap(sample, label);
  }
  const combined = { targets };
  for (const field of HEAP_FIELDS) {
    combined[field] = safeSum(
      Object.values(targets).map((target) => target[field]),
      `combined ${field}`
    );
  }
  combined.accountedBytes = safeSum(
    HEAP_FIELDS.map((field) => combined[field]),
    "combined accounted heap"
  );
  return combined;
}

export function largerHeap(left, right) {
  assertAccountedHeap(left, "left");
  assertAccountedHeap(right, "right");
  return right.accountedBytes > left.accountedBytes ? right : left;
}

export function sampleProcessTreeRss(rootPid) {
  if (!Number.isSafeInteger(rootPid) || rootPid <= 0) {
    throw new Error("browser root PID is invalid");
  }
  if (process.platform !== "darwin" && process.platform !== "linux") {
    throw new Error(`process-tree RSS is unsupported on ${process.platform}`);
  }
  const result = spawnSync("ps", ["-axo", "pid=,ppid=,rss="], {
    encoding: "utf8",
    maxBuffer: MAX_PS_OUTPUT_BYTES,
    timeout: 2_000
  });
  if (result.error || result.status !== 0 || result.signal !== null) {
    throw new Error(
      `process-tree RSS sampling failed: ${
        result.error?.message ?? `status=${result.status} signal=${result.signal}`
      }`
    );
  }
  return processTreeRssFromPs(result.stdout, rootPid);
}

export function processTreeRssFromPs(output, rootPid) {
  if (typeof output !== "string" || Buffer.byteLength(output) > MAX_PS_OUTPUT_BYTES) {
    throw new Error("ps output is invalid or exceeds its byte bound");
  }
  const lines = output.split("\n").filter((line) => line.trim().length > 0);
  if (lines.length === 0 || lines.length > MAX_PS_ROWS) {
    throw new Error("ps output row count is invalid");
  }
  const rows = new Map();
  for (const line of lines) {
    const fields = line.trim().split(/\s+/);
    if (fields.length !== 3 || fields.some((field) => !/^\d+$/.test(field))) {
      throw new Error(`malformed ps row: ${line.slice(0, 120)}`);
    }
    const [pid, parentPid, rssKiB] = fields.map(Number);
    if (
      !Number.isSafeInteger(pid) ||
      pid <= 0 ||
      !Number.isSafeInteger(parentPid) ||
      parentPid < 0 ||
      !Number.isSafeInteger(rssKiB) ||
      rssKiB < 0 ||
      rows.has(pid)
    ) {
      throw new Error("ps row contains an invalid or duplicate process");
    }
    rows.set(pid, { parentPid, rssKiB });
  }
  if (!rows.has(rootPid)) {
    throw new Error("browser root PID is absent from ps output");
  }
  const descendants = new Set([rootPid]);
  let changed = true;
  while (changed) {
    changed = false;
    for (const [pid, { parentPid }] of rows) {
      if (!descendants.has(pid) && descendants.has(parentPid)) {
        descendants.add(pid);
        changed = true;
      }
    }
  }
  const rssBytes = safeSum(
    [...descendants].map((pid) => rows.get(pid).rssKiB * 1024),
    "process-tree RSS"
  );
  return { rootPid, processCount: descendants.size, rssBytes };
}

export function resolveProcessMemoryGate(gate, platform) {
  assertProcessMemoryGate(gate);
  if (platform !== "darwin" && platform !== "linux") {
    throw new Error(`process-memory gate is unsupported on ${platform}`);
  }
  const overrides = gate.platformOverrides;
  if (overrides === undefined) {
    return {
      maxProcessTreePeakGrowthBytes: gate.maxProcessTreePeakGrowthBytes,
      maxProcessTreeRetainedGrowthBytes: gate.maxProcessTreeRetainedGrowthBytes
    };
  }
  if (overrides === null || typeof overrides !== "object" || Array.isArray(overrides)) {
    throw new Error("process-memory platform overrides are invalid");
  }
  for (const [overridePlatform, override] of Object.entries(overrides)) {
    if (overridePlatform !== "darwin" && overridePlatform !== "linux") {
      throw new Error(`unsupported process-memory platform override ${overridePlatform}`);
    }
    if (
      override === null ||
      typeof override !== "object" ||
      Array.isArray(override) ||
      Object.keys(override).sort().join(",") !==
        "maxProcessTreePeakGrowthBytes,maxProcessTreeRetainedGrowthBytes"
    ) {
      throw new Error(`process-memory ${overridePlatform} override is invalid`);
    }
    assertProcessMemoryGate(override);
  }
  const resolved = overrides[platform] ?? gate;
  return {
    maxProcessTreePeakGrowthBytes: resolved.maxProcessTreePeakGrowthBytes,
    maxProcessTreeRetainedGrowthBytes: resolved.maxProcessTreeRetainedGrowthBytes
  };
}

export function summarizeProcessMemory({ baseline, peak, retained }, gate) {
  for (const [label, sample] of Object.entries({ baseline, peak, retained })) {
    assertProcessMemorySample(sample, label);
  }
  if (
    peak.rootPid !== baseline.rootPid ||
    retained.rootPid !== baseline.rootPid ||
    peak.rssBytes < baseline.rssBytes ||
    peak.rssBytes < retained.rssBytes
  ) {
    throw new Error("process-memory samples are not one ordered browser tree");
  }
  assertProcessMemoryGate(gate);
  const peakGrowthBytes = peak.rssBytes - baseline.rssBytes;
  const retainedGrowthBytes = Math.max(0, retained.rssBytes - baseline.rssBytes);
  const summary = {
    baseline,
    peak,
    retained,
    peakGrowthBytes,
    retainedGrowthBytes
  };
  if (peakGrowthBytes > gate.maxProcessTreePeakGrowthBytes) {
    throw new Error(
      `process-tree RSS peak growth ${peakGrowthBytes} exceeds ` +
      `${gate.maxProcessTreePeakGrowthBytes} ` +
      `(baseline=${baseline.rssBytes}, peak=${peak.rssBytes}, ` +
      `retained=${retained.rssBytes}, ` +
      `processes=${peak.processCount ?? "unknown"})`
    );
  }
  if (retainedGrowthBytes > gate.maxProcessTreeRetainedGrowthBytes) {
    throw new Error(
      `process-tree retained growth ${retainedGrowthBytes} exceeds ${gate.maxProcessTreeRetainedGrowthBytes}`
    );
  }
  return summary;
}

export function summarizeIndependentRssWindow(
  samples,
  {
    rootPid,
    intervalMs,
    boundaryStartEpochMs,
    boundaryEndEpochMs,
    minimumBoundarySamples = MIN_BOUNDARY_RSS_SAMPLES
  }
) {
  assertRootPid(rootPid);
  assertSamplerConfiguration(intervalMs, MAX_RSS_SAMPLES);
  assertEpochMilliseconds(boundaryStartEpochMs, "RSS boundary start");
  assertEpochMilliseconds(boundaryEndEpochMs, "RSS boundary end");
  if (boundaryEndEpochMs < boundaryStartEpochMs) {
    throw new Error("RSS boundary window is reversed");
  }
  if (
    !Number.isSafeInteger(minimumBoundarySamples) ||
    minimumBoundarySamples < MIN_BOUNDARY_RSS_SAMPLES ||
    minimumBoundarySamples > MAX_RSS_SAMPLES
  ) {
    throw new Error("RSS boundary minimum sample count is invalid");
  }
  if (
    !Array.isArray(samples) ||
    samples.length === 0 ||
    samples.length > MAX_RSS_SAMPLES
  ) {
    throw new Error("independent RSS evidence sample count is invalid");
  }
  let priorEpochMs = 0;
  const normalized = samples.map((sample, sequence) => {
    assertExactObjectKeys(
      sample,
      [
        "sequence",
        "sampledAtEpochMs",
        "rootPid",
        "processCount",
        "rssBytes"
      ],
      "independent RSS sample"
    );
    if (sample.sequence !== sequence) {
      throw new Error("independent RSS sample sequence is not contiguous");
    }
    assertEpochMilliseconds(sample.sampledAtEpochMs, "RSS sample timestamp");
    if (sample.sampledAtEpochMs < priorEpochMs) {
      throw new Error("independent RSS sample timestamps are not ordered");
    }
    priorEpochMs = sample.sampledAtEpochMs;
    assertProcessMemorySample(sample, "independent RSS");
    if (sample.rootPid !== rootPid) {
      throw new Error("independent RSS sample root PID changed");
    }
    return { ...sample };
  });
  const boundarySamples = normalized.filter(
    ({ sampledAtEpochMs }) =>
      sampledAtEpochMs >= boundaryStartEpochMs &&
      sampledAtEpochMs <= boundaryEndEpochMs
  );
  if (boundarySamples.length < minimumBoundarySamples) {
    throw new Error(
      `independent RSS boundary captured ${boundarySamples.length}/${minimumBoundarySamples} required samples`
    );
  }
  const peak = boundarySamples.reduce((largest, sample) =>
    sample.rssBytes > largest.rssBytes ? sample : largest
  );
  let maxGapMs = 0;
  for (let index = 1; index < boundarySamples.length; index += 1) {
    maxGapMs = Math.max(
      maxGapMs,
      boundarySamples[index].sampledAtEpochMs -
        boundarySamples[index - 1].sampledAtEpochMs
    );
  }
  const sampledDurationMs =
    boundarySamples.at(-1).sampledAtEpochMs -
    boundarySamples[0].sampledAtEpochMs;
  if (maxGapMs > MAX_BOUNDARY_RSS_GAP_MS) {
    throw new Error(
      `independent RSS max gap ${maxGapMs}ms exceeds ${MAX_BOUNDARY_RSS_GAP_MS}ms`
    );
  }
  if (
    sampledDurationMs <= 0 ||
    sampledDurationMs > MAX_BOUNDARY_RSS_DURATION_MS ||
    sampledDurationMs >
      (boundarySamples.length - 1) * MAX_BOUNDARY_RSS_GAP_MS
  ) {
    throw new Error(
      `independent RSS sampled duration ${sampledDurationMs}ms is invalid`
    );
  }
  return {
    intervalMs,
    sampleCount: normalized.length,
    boundarySampleCount: boundarySamples.length,
    boundaryStartEpochMs,
    boundaryEndEpochMs,
    sampledDurationMs,
    maxGapMs,
    peak: {
      rootPid: peak.rootPid,
      processCount: peak.processCount,
      rssBytes: peak.rssBytes
    }
  };
}

export function summarizePendingBoundaryRssMateriality(
  baseline,
  peak,
  minimumGrowthBytes = MIN_PENDING_BOUNDARY_RSS_GROWTH_BYTES
) {
  assertProcessMemorySample(baseline, "pending-boundary baseline");
  assertProcessMemorySample(peak, "pending-boundary peak");
  if (
    baseline.rootPid !== peak.rootPid ||
    !Number.isSafeInteger(minimumGrowthBytes) ||
    minimumGrowthBytes <= 0 ||
    peak.rssBytes < baseline.rssBytes
  ) {
    throw new Error("pending-boundary RSS materiality evidence is invalid");
  }
  const peakGrowthBytes = peak.rssBytes - baseline.rssBytes;
  if (peakGrowthBytes < minimumGrowthBytes) {
    throw new Error(
      `pending-boundary RSS growth ${peakGrowthBytes} is below ${minimumGrowthBytes}`
    );
  }
  return {
    baselineRssBytes: baseline.rssBytes,
    peakGrowthBytes,
    minimumGrowthBytes
  };
}

function assertProcessMemoryGate(gate) {
  if (
    !Number.isSafeInteger(gate?.maxProcessTreePeakGrowthBytes) ||
    gate.maxProcessTreePeakGrowthBytes <= 0 ||
    !Number.isSafeInteger(gate?.maxProcessTreeRetainedGrowthBytes) ||
    gate.maxProcessTreeRetainedGrowthBytes <= 0 ||
    gate.maxProcessTreeRetainedGrowthBytes > gate.maxProcessTreePeakGrowthBytes
  ) {
    throw new Error("process-memory gate is invalid");
  }
}

function assertAccountedHeap(value, label) {
  if (!Number.isSafeInteger(value?.accountedBytes) || value.accountedBytes < 0) {
    throw new Error(`${label} accounted heap is invalid`);
  }
}

function assertProcessMemorySample(sample, label) {
  if (
    sample === null ||
    typeof sample !== "object" ||
    !Number.isSafeInteger(sample.rootPid) ||
    sample.rootPid <= 0 ||
    !Number.isSafeInteger(sample.processCount) ||
    sample.processCount <= 0 ||
    !Number.isSafeInteger(sample.rssBytes) ||
    sample.rssBytes < 0
  ) {
    throw new Error(`${label} process-memory sample is invalid`);
  }
}

function assertRootPid(rootPid) {
  if (!Number.isSafeInteger(rootPid) || rootPid <= 0) {
    throw new Error("browser root PID is invalid");
  }
}

function assertSamplerConfiguration(intervalMs, maxSamples) {
  if (
    !Number.isSafeInteger(intervalMs) ||
    intervalMs <= 0 ||
    intervalMs > MAX_RSS_SAMPLE_INTERVAL_MS
  ) {
    throw new Error(
      `RSS sampler interval must be at most ${MAX_RSS_SAMPLE_INTERVAL_MS}ms`
    );
  }
  if (
    !Number.isSafeInteger(maxSamples) ||
    maxSamples < MIN_BOUNDARY_RSS_SAMPLES ||
    maxSamples > MAX_RSS_SAMPLES
  ) {
    throw new Error("RSS sampler sample bound is invalid");
  }
}

function assertEpochMilliseconds(value, label) {
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new Error(`${label} is invalid`);
  }
}

function assertExactObjectKeys(value, expected, label) {
  if (
    value === null ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    Object.keys(value).sort().join(",") !== [...expected].sort().join(",")
  ) {
    throw new Error(`${label} keys changed`);
  }
}

function boundedErrorMessage(error) {
  const message = error instanceof Error ? error.message : String(error);
  return message
    .replace(/[^\x20-\x7e]/g, "?")
    .slice(0, MAX_RSS_SAMPLER_ERROR_BYTES);
}

function safeSum(values, label) {
  let total = 0;
  for (const value of values) {
    if (!Number.isSafeInteger(value) || value < 0 || total > Number.MAX_SAFE_INTEGER - value) {
      throw new Error(`${label} overflowed or contained invalid bytes`);
    }
    total += value;
  }
  return total;
}
