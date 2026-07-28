import assert from "node:assert/strict";
import test from "node:test";

import {
  combineHeaps,
  IndependentRssSampler,
  MAX_BOUNDARY_RSS_DURATION_MS,
  MAX_BOUNDARY_RSS_GAP_MS,
  MAX_RSS_SAMPLE_INTERVAL_MS,
  MIN_BOUNDARY_RSS_SAMPLES,
  MIN_PENDING_BOUNDARY_RSS_GROWTH_BYTES,
  normalizeHeap,
  processTreeRssFromPs,
  resolveProcessMemoryGate,
  RSS_SAMPLE_INTERVAL_MS,
  summarizeIndependentRssWindow,
  summarizePendingBoundaryRssMateriality,
  summarizeProcessMemory
} from "./browser/memory-evidence.mjs";

const heap = {
  usedSize: 10,
  totalSize: 20,
  embedderHeapUsedSize: 30,
  backingStorageSize: 40
};

test("heap accounting includes every exact field and fails closed", () => {
  assert.deepEqual(normalizeHeap(heap, "page"), {
    ...heap,
    accountedBytes: 100
  });
  assert.equal(
    combineHeaps([
      { label: "page", sample: heap },
      { label: "worker:a", sample: heap },
      { label: "worker:b", sample: heap }
    ]).accountedBytes,
    300
  );
  for (const invalid of [undefined, NaN, Infinity, -1, 1.5]) {
    assert.throws(
      () => normalizeHeap({ ...heap, backingStorageSize: invalid }, "worker"),
      /backingStorageSize is invalid/
    );
  }
  assert.throws(() => combineHeaps([{ label: "page", sample: heap }]), /at least one worker/);
});

test("process-tree RSS parser includes descendants and rejects malformed evidence", () => {
  const output = "100 1 10\n101 100 20\n102 101 30\n200 1 999\n";
  assert.deepEqual(processTreeRssFromPs(output, 100), {
    rootPid: 100,
    processCount: 3,
    rssBytes: 60 * 1024
  });
  assert.throws(() => processTreeRssFromPs("100 nope 10\n", 100), /malformed ps row/);
  assert.throws(() => processTreeRssFromPs("101 1 10\n", 100), /root PID is absent/);
});

test("process-memory high-water and retention gates pass and fail closed", () => {
  const gate = {
    maxProcessTreePeakGrowthBytes: 1_000,
    maxProcessTreeRetainedGrowthBytes: 200
  };
  const sample = (rssBytes, processCount = 4, rootPid = 100) => ({
    rootPid,
    processCount,
    rssBytes
  });
  const summary = summarizeProcessMemory(
    {
      baseline: sample(1_500),
      peak: sample(2_400, 6),
      retained: sample(1_650, 5)
    },
    gate
  );
  assert.equal(summary.peakGrowthBytes, 900);
  assert.equal(summary.retainedGrowthBytes, 150);
  assert.throws(
    () =>
      summarizeProcessMemory({
        baseline: sample(1_500),
        peak: sample(2_501, 6),
        retained: sample(1_650, 5)
      }, gate),
    /RSS peak growth/
  );
  assert.throws(
    () =>
      summarizeProcessMemory({
        baseline: sample(1_500),
        peak: sample(2_400, 6),
        retained: sample(1_701, 5)
      }, gate),
    /retained growth/
  );
  assert.throws(
    () =>
      summarizeProcessMemory({
        baseline: sample(Number.NaN),
        peak: sample(2_400, 6),
        retained: sample(1_650, 5)
      }, gate),
    /sample is invalid/
  );
  assert.throws(
    () =>
      summarizeProcessMemory({
        baseline: sample(1_500),
        peak: sample(2_400, 6, 101),
        retained: sample(1_650, 5)
      }, gate),
    /one ordered browser tree/
  );
  assert.throws(
    () =>
      summarizeProcessMemory({
        baseline: sample(1_500),
        peak: sample(1_600, 6),
        retained: sample(1_700, 5)
      }, gate),
    /one ordered browser tree/
  );
});

test("process-memory platform overrides preserve the stricter Linux release gate", () => {
  const gate = {
    maxProcessTreePeakGrowthBytes: 1_000,
    maxProcessTreeRetainedGrowthBytes: 200,
    platformOverrides: {
      darwin: {
        maxProcessTreePeakGrowthBytes: 2_000,
        maxProcessTreeRetainedGrowthBytes: 300
      }
    }
  };
  assert.deepEqual(resolveProcessMemoryGate(gate, "linux"), {
    maxProcessTreePeakGrowthBytes: 1_000,
    maxProcessTreeRetainedGrowthBytes: 200
  });
  assert.deepEqual(resolveProcessMemoryGate(gate, "darwin"), {
    maxProcessTreePeakGrowthBytes: 2_000,
    maxProcessTreeRetainedGrowthBytes: 300
  });
  assert.throws(
    () =>
      resolveProcessMemoryGate({
        ...gate,
        platformOverrides: { darwin: { maxProcessTreePeakGrowthBytes: 2_000 } }
      }, "darwin"),
    /override is invalid/
  );
  assert.throws(() => resolveProcessMemoryGate(gate, "win32"), /unsupported/);
});

test("independent RSS evidence binds a five-sample near-boundary window", () => {
  const samples = Array.from({ length: 6 }, (_, sequence) => ({
    sequence,
    sampledAtEpochMs: 1_000 + sequence * 20,
    rootPid: 100,
    processCount: 4 + (sequence % 2),
    rssBytes: 10_000 + sequence * 1_000
  }));
  const summary = summarizeIndependentRssWindow(samples, {
    rootPid: 100,
    intervalMs: RSS_SAMPLE_INTERVAL_MS,
    boundaryStartEpochMs: 1_010,
    boundaryEndEpochMs: 1_100
  });
  assert.equal(RSS_SAMPLE_INTERVAL_MS <= MAX_RSS_SAMPLE_INTERVAL_MS, true);
  assert.deepEqual(summary, {
    intervalMs: 10,
    sampleCount: 6,
    boundarySampleCount: MIN_BOUNDARY_RSS_SAMPLES,
    boundaryStartEpochMs: 1_010,
    boundaryEndEpochMs: 1_100,
    sampledDurationMs: 80,
    maxGapMs: 20,
    peak: {
      rootPid: 100,
      processCount: 5,
      rssBytes: 15_000
    }
  });
  assert.throws(
    () =>
      summarizeIndependentRssWindow(samples.slice(0, 5), {
        rootPid: 100,
        intervalMs: RSS_SAMPLE_INTERVAL_MS,
        boundaryStartEpochMs: 1_010,
        boundaryEndEpochMs: 1_080
      }),
    /captured 4\/5/
  );
  assert.throws(
    () =>
      summarizeIndependentRssWindow(
        samples.map((sample, index) => ({
          ...sample,
          sequence: index === 2 ? 3 : sample.sequence
        })),
        {
          rootPid: 100,
          intervalMs: RSS_SAMPLE_INTERVAL_MS,
          boundaryStartEpochMs: 1_000,
          boundaryEndEpochMs: 1_100
        }
      ),
    /sequence is not contiguous/
  );
  const delayed = summarizeIndependentRssWindow(
    samples.map((sample, sequence) => ({
      ...sample,
      sampledAtEpochMs: 1_000 + sequence * 26
    })),
    {
      rootPid: 100,
      intervalMs: RSS_SAMPLE_INTERVAL_MS,
      boundaryStartEpochMs: 1_000,
      boundaryEndEpochMs: 1_130
    }
  );
  assert.equal(delayed.maxGapMs, 26);
  assert.throws(
    () =>
      summarizeIndependentRssWindow(
        samples.map((sample, sequence) => ({
          ...sample,
          sampledAtEpochMs: 1_000 + sequence * (MAX_BOUNDARY_RSS_GAP_MS + 1)
        })),
        {
          rootPid: 100,
          intervalMs: RSS_SAMPLE_INTERVAL_MS,
          boundaryStartEpochMs: 1_000,
          boundaryEndEpochMs:
            1_000 + (samples.length - 1) * (MAX_BOUNDARY_RSS_GAP_MS + 1)
        }
      ),
    /max gap 101ms exceeds 100ms/
  );
  const longWindow = Array.from({ length: 22 }, (_, sequence) => ({
    sequence,
    sampledAtEpochMs: 1_000 + sequence * MAX_BOUNDARY_RSS_GAP_MS,
    rootPid: 100,
    processCount: 4,
    rssBytes: 10_000 + sequence
  }));
  assert.throws(
    () =>
      summarizeIndependentRssWindow(longWindow, {
        rootPid: 100,
        intervalMs: RSS_SAMPLE_INTERVAL_MS,
        boundaryStartEpochMs: 1_000,
        boundaryEndEpochMs: 1_000 + MAX_BOUNDARY_RSS_DURATION_MS + 100
      }),
    /sampled duration 2100ms is invalid/
  );
  assert.throws(
    () =>
      summarizeIndependentRssWindow(
        samples.slice(0, MIN_BOUNDARY_RSS_SAMPLES).map((sample) => ({
          ...sample,
          sampledAtEpochMs: 1_000
        })),
        {
          rootPid: 100,
          intervalMs: RSS_SAMPLE_INTERVAL_MS,
          boundaryStartEpochMs: 1_000,
          boundaryEndEpochMs: 1_000
        }
      ),
    /sampled duration 0ms is invalid/
  );
});

test("independent RSS sampler is bounded and fails closed on protocol drift", async () => {
  const sampler = new IndependentRssSampler(100, {
    WorkerClass: FakeRssWorker
  });
  const worker = FakeRssWorker.latest;
  const boundaryStartEpochMs = Date.now();
  const waiting = sampler.waitForSamplesSince(boundaryStartEpochMs);
  for (let sequence = 0; sequence < MIN_BOUNDARY_RSS_SAMPLES; sequence += 1) {
    worker.emit("message", {
      type: "sample",
      sequence,
      sampledAtEpochMs: boundaryStartEpochMs + sequence * 20,
      rootPid: 100,
      processCount: 3,
      rssBytes: 1_000 + sequence
    });
  }
  assert.equal((await waiting).length, MIN_BOUNDARY_RSS_SAMPLES);
  assert.equal((await sampler.stop()).length, MIN_BOUNDARY_RSS_SAMPLES);
  assert.deepEqual(worker.workerData, {
    rootPid: 100,
    intervalMs: RSS_SAMPLE_INTERVAL_MS,
    maxSamples: 256
  });
  assert.equal(worker.terminated, true);

  assert.throws(
    () =>
      new IndependentRssSampler(100, {
        intervalMs: MAX_RSS_SAMPLE_INTERVAL_MS + 1,
        WorkerClass: FakeRssWorker
      }),
    /interval must be at most 25ms/
  );

  const invalid = new IndependentRssSampler(100, {
    WorkerClass: FakeRssWorker
  });
  FakeRssWorker.latest.emit("message", {
    type: "sample",
    sequence: 1,
    sampledAtEpochMs: Date.now(),
    rootPid: 100,
    processCount: 3,
    rssBytes: 1_000
  });
  assert.throws(() => invalid.samples(), /sequence or sample bound changed/);
  await assert.rejects(invalid.stop(), /sequence or sample bound changed/);
});

test("pending-resource RSS materiality requires 96 MiB over baseline", () => {
  const baseline = { rootPid: 100, processCount: 4, rssBytes: 200_000_000 };
  const peak = {
    rootPid: 100,
    processCount: 5,
    rssBytes: baseline.rssBytes + MIN_PENDING_BOUNDARY_RSS_GROWTH_BYTES
  };
  assert.deepEqual(summarizePendingBoundaryRssMateriality(baseline, peak), {
    baselineRssBytes: baseline.rssBytes,
    peakGrowthBytes: MIN_PENDING_BOUNDARY_RSS_GROWTH_BYTES,
    minimumGrowthBytes: MIN_PENDING_BOUNDARY_RSS_GROWTH_BYTES
  });
  assert.throws(
    () =>
      summarizePendingBoundaryRssMateriality(baseline, {
        ...peak,
        rssBytes: peak.rssBytes - 1
      }),
    /RSS growth .* is below/
  );
  assert.throws(
    () =>
      summarizePendingBoundaryRssMateriality(baseline, {
        ...peak,
        rootPid: 101
      }),
    /materiality evidence is invalid/
  );
});

class FakeRssWorker {
  static latest = null;

  constructor(_url, { workerData }) {
    this.workerData = workerData;
    this.listeners = new Map();
    this.sampleCount = 0;
    this.terminated = false;
    FakeRssWorker.latest = this;
  }

  on(type, listener) {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  emit(type, value) {
    if (type === "message" && value?.type === "sample") {
      this.sampleCount += 1;
    }
    for (const listener of this.listeners.get(type) ?? []) {
      listener(value);
    }
  }

  postMessage(message) {
    if (message?.type !== "stop") {
      throw new Error("unexpected fake worker message");
    }
    queueMicrotask(() => {
      this.emit("message", {
        type: "stopped",
        sampleCount: this.sampleCount
      });
    });
  }

  async terminate() {
    this.terminated = true;
    return 0;
  }
}
