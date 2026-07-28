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
    maxProcessTreeRssBytes: 1_000,
    maxProcessTreeRetainedGrowthBytes: 200
  };
  assert.equal(
    summarizeProcessMemory({
      baseline: { rssBytes: 500 },
      peak: { rssBytes: 900 },
      retained: { rssBytes: 650 }
    }, gate).retainedGrowthBytes,
    150
  );
  assert.throws(
    () =>
      summarizeProcessMemory({
        baseline: { rssBytes: 500 },
        peak: { rssBytes: 1_001 },
        retained: { rssBytes: 650 }
      }, gate),
    /RSS peak/
  );
  assert.throws(
    () =>
      summarizeProcessMemory({
        baseline: { rssBytes: 500 },
        peak: { rssBytes: 900 },
        retained: { rssBytes: 701 }
      }, gate),
    /retained growth/
  );
  assert.throws(
    () =>
      summarizeProcessMemory({
        baseline: { rssBytes: Number.NaN },
        peak: { rssBytes: 900 },
        retained: { rssBytes: 650 }
      }, gate),
    /sample is invalid/
  );
});

test("process-memory platform overrides preserve the stricter Linux release gate", () => {
  const gate = {
    maxProcessTreeRssBytes: 1_000,
    maxProcessTreeRetainedGrowthBytes: 200,
    platformOverrides: {
      darwin: {
        maxProcessTreeRssBytes: 2_000,
        maxProcessTreeRetainedGrowthBytes: 300
      }
    }
  };
  assert.deepEqual(resolveProcessMemoryGate(gate, "linux"), {
    maxProcessTreeRssBytes: 1_000,
    maxProcessTreeRetainedGrowthBytes: 200
  });
  assert.deepEqual(resolveProcessMemoryGate(gate, "darwin"), {
    maxProcessTreeRssBytes: 2_000,
    maxProcessTreeRetainedGrowthBytes: 300
  });
  assert.throws(
    () =>
      resolveProcessMemoryGate({
        ...gate,
        platformOverrides: { darwin: { maxProcessTreeRssBytes: 2_000 } }
      }, "darwin"),
    /override is invalid/
  );
  assert.throws(() => resolveProcessMemoryGate(gate, "win32"), /unsupported/);
});
