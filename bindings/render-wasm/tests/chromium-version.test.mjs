import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  CHROMIUM_VERSION_TIMEOUT_MS,
  probeChromiumVersion
} from "./browser/chromium-version.mjs";

const { chromium } = JSON.parse(
  await readFile(new URL("../toolchain-lock.json", import.meta.url), "utf8")
);
const expectedVersion = `${chromium.testingProduct} ${chromium.version}`;
const successful = { status: 0, signal: null, stdout: `${expectedVersion}\n`, stderr: "" };

function probe(result) {
  return probeChromiumVersion("test-chromium", chromium, { run: () => result });
}

test("version probe accepts only the exact locked Chrome identities", () => {
  for (const product of [chromium.product, chromium.testingProduct]) {
    const identity = `${product} ${chromium.version}`;
    assert.equal(probe({ ...successful, stdout: `${identity}\n` }), identity);
  }
  for (const stdout of [
    `${chromium.testingProduct} 0.0.0.0`,
    `Other Chromium ${chromium.version}`,
    `${expectedVersion}\nextra output`,
    ""
  ]) {
    assert.throws(() => probe({ ...successful, stdout }), /expected .*; got /);
  }
});

test("version startup has its own finite timeout and unchanged process output cap", () => {
  assert.equal(CHROMIUM_VERSION_TIMEOUT_MS, 10_000);
  assert.equal(
    probeChromiumVersion("test-chromium", chromium, {
      run(executable, args, options) {
        assert.equal(executable, "test-chromium");
        assert.deepEqual(args, ["--version"]);
        assert.deepEqual(options, {
          encoding: "utf8",
          timeout: 10_000,
          maxBuffer: 16 * 1024,
          killSignal: "SIGKILL"
        });
        return successful;
      }
    }),
    expectedVersion
  );
});

test("matching stdout cannot hide a version probe timeout", () => {
  assert.throws(
    () => probe({
      ...successful,
      status: null,
      signal: "SIGKILL",
      error: Object.assign(new Error("spawnSync timed out"), { code: "ETIMEDOUT" })
    }),
    /Chromium version probe failed .*timeoutMs=10000.*status=null.*signal="SIGKILL".*error="ETIMEDOUT".*stdout="Google Chrome/
  );
});

test("matching stdout cannot hide nonzero exit or signal termination", () => {
  assert.throws(
    () => probe({ ...successful, status: 7, stderr: "startup failed" }),
    /Chromium version probe failed .*status=7.*stderr="startup failed"/
  );
  assert.throws(
    () => probe({ ...successful, signal: "SIGTERM" }),
    /Chromium version probe failed .*signal="SIGTERM"/
  );
});

test("process errors remain failures even if status and stdout look successful", () => {
  assert.throws(
    () => probe({ ...successful, error: { code: "ENOBUFS" } }),
    /Chromium version probe failed .*error="ENOBUFS"/
  );
  assert.throws(
    () => probe({ status: null, signal: null, error: { code: "ENOENT" } }),
    /Chromium version probe failed .*error="ENOENT".*stdout="".*stderr=""/
  );
});

test("probe diagnostics retain bounded escaped stdout and stderr", () => {
  assert.throws(
    () => probe({
      ...successful,
      status: 1,
      stdout: `unexpected\n${"x".repeat(16 * 1024)}`,
      stderr: `failure\n${"y".repeat(16 * 1024)}`
    }),
    (error) => {
      assert.match(error.message, /stdout="unexpected\\n/);
      assert.match(error.message, /stderr="failure\\n/);
      assert.match(error.message, /\[truncated\]/);
      assert.ok(Buffer.byteLength(error.message) < 16 * 1024);
      return true;
    }
  );
  assert.throws(
    () => probe({ ...successful, stdout: "z".repeat(16 * 1024) }),
    (error) => {
      assert.match(error.message, /expected .*; got /);
      assert.ok(Buffer.byteLength(error.message) < 4 * 1024);
      return true;
    }
  );
});

test("a successful cold-start probe may take longer than the CDP HTTP deadline", () => {
  const started = Date.now();
  const actual = probeChromiumVersion("test-chromium", chromium, {
    run(_executable, _args, options) {
      return spawnSync(process.execPath, [
        "-e",
        `process.stdout.write(${JSON.stringify(`${expectedVersion}\n`)}); setTimeout(() => {}, 2_150);`
      ], options);
    }
  });
  assert.equal(actual, expectedVersion);
  assert.ok(Date.now() - started >= 2_000);
});

test("real excessive probe output fails within the unchanged output cap", () => {
  assert.throws(
    () => probeChromiumVersion("test-chromium", chromium, {
      run(_executable, _args, options) {
        return spawnSync(process.execPath, [
          "-e",
          `process.stdout.write(${JSON.stringify(`${expectedVersion}\n`)} + "x".repeat(32 * 1024));`
        ], options);
      }
    }),
    /Chromium version probe failed .*error="ENOBUFS"/
  );
});

test("browser harness uses the dedicated probe without changing runtime deadlines", async () => {
  const source = await readFile(new URL("./browser/run.mjs", import.meta.url), "utf8");
  assert.match(source, /actualVersion = probeChromiumVersion\(chrome, lock\.chromium\)/);
  assert.match(source, /const CDP_HTTP_TIMEOUT_MS = 2_000;/);
  assert.match(source, /const CDP_COMMAND_TIMEOUT_MS = 5_000;/);
  assert.match(source, /const BROWSER_START_TIMEOUT_MS = 20_000;/);
  assert.match(source, /const BROWSER_RUN_TIMEOUT_MS = 45_000;/);
});
