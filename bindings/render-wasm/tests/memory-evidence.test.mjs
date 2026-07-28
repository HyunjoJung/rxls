import assert from "node:assert/strict";
import test from "node:test";

import {
  combineHeaps,
  normalizeHeap,
  processTreeRssFromPs,
  resolveProcessMemoryGate,
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
