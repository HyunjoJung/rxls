import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { PRESERVATION_FIXTURE, PRESERVED_PARTS } from "../scripts/preservation-fixture.mjs";
import { createStoredZip, readZipEntries } from "../scripts/zip.mjs";

test("creates deterministic ZIP32 archives with verified entries", () => {
  const first = createStoredZip([
    { name: "one.txt", data: "one" },
    { name: "nested/two.bin", data: Uint8Array.of(0, 1, 2, 255) }
  ]);
  const second = createStoredZip([
    { name: "one.txt", data: "one" },
    { name: "nested/two.bin", data: Uint8Array.of(0, 1, 2, 255) }
  ]);
  assert.deepEqual(first, second);
  const entries = readZipEntries(first);
  assert.equal(entries.get("one.txt").toString(), "one");
  assert.deepEqual(entries.get("nested/two.bin"), Buffer.from([0, 1, 2, 255]));
});

test("pins a real macro-enabled preservation fixture", async () => {
  const fixture = await readFile(
    new URL(`../samples/${PRESERVATION_FIXTURE.sourceFile}`, import.meta.url)
  );
  assert.equal(fixture.byteLength, PRESERVATION_FIXTURE.bytes);
  assert.equal(
    createHash("sha256").update(fixture).digest("hex"),
    PRESERVATION_FIXTURE.sha256
  );
  const entries = readZipEntries(fixture);
  for (const part of PRESERVED_PARTS) {
    assert.ok(entries.has(part), part);
  }
  assert.match(entries.get("[Content_Types].xml").toString(), /macroEnabled/);
  assert.equal(
    entries.get("xl/vbaProject.bin").subarray(0, 8).toString("hex"),
    "d0cf11e0a1b11ae1"
  );
});
