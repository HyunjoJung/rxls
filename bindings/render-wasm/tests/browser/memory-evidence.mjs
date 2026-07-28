import { spawnSync } from "node:child_process";

const HEAP_FIELDS = [
  "usedSize",
  "totalSize",
  "embedderHeapUsedSize",
  "backingStorageSize"
];
const MAX_PS_OUTPUT_BYTES = 1024 * 1024;
const MAX_PS_ROWS = 8_192;

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
      maxProcessTreeRssBytes: gate.maxProcessTreeRssBytes,
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
        "maxProcessTreeRetainedGrowthBytes,maxProcessTreeRssBytes"
    ) {
      throw new Error(`process-memory ${overridePlatform} override is invalid`);
    }
    assertProcessMemoryGate(override);
  }
  const resolved = overrides[platform] ?? gate;
  return {
    maxProcessTreeRssBytes: resolved.maxProcessTreeRssBytes,
    maxProcessTreeRetainedGrowthBytes: resolved.maxProcessTreeRetainedGrowthBytes
  };
}

export function summarizeProcessMemory({ baseline, peak, retained }, gate) {
  for (const [label, sample] of Object.entries({ baseline, peak, retained })) {
    if (
      sample === null ||
      typeof sample !== "object" ||
      !Number.isSafeInteger(sample.rssBytes) ||
      sample.rssBytes < 0
    ) {
      throw new Error(`${label} process-memory sample is invalid`);
    }
  }
  assertProcessMemoryGate(gate);
  const retainedGrowthBytes = Math.max(0, retained.rssBytes - baseline.rssBytes);
  const summary = { baseline, peak, retained, retainedGrowthBytes };
  if (peak.rssBytes > gate.maxProcessTreeRssBytes) {
    throw new Error(
      `process-tree RSS peak ${peak.rssBytes} exceeds ${gate.maxProcessTreeRssBytes} ` +
      `(baseline=${baseline.rssBytes}, retained=${retained.rssBytes}, ` +
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

function assertProcessMemoryGate(gate) {
  if (
    !Number.isSafeInteger(gate?.maxProcessTreeRssBytes) ||
    gate.maxProcessTreeRssBytes <= 0 ||
    !Number.isSafeInteger(gate?.maxProcessTreeRetainedGrowthBytes) ||
    gate.maxProcessTreeRetainedGrowthBytes <= 0 ||
    gate.maxProcessTreeRetainedGrowthBytes > gate.maxProcessTreeRssBytes
  ) {
    throw new Error("process-memory gate is invalid");
  }
}

function assertAccountedHeap(value, label) {
  if (!Number.isSafeInteger(value?.accountedBytes) || value.accountedBytes < 0) {
    throw new Error(`${label} accounted heap is invalid`);
  }
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
