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

function corruptionFixture() {
  const valid = createStoredZip([{ name: "one.txt", data: "one" }]);
  const bytes = Buffer.from(valid);
  // The writer emits a 22-byte end record without a ZIP comment.
  const endOffset = bytes.length - 22;
  const centralOffset = bytes.readUInt32LE(endOffset + 16);
  const localOffset = bytes.readUInt32LE(centralOffset + 42);
  // Skip the 30-byte local header and its filename/extra fields.
  const dataStart = localOffset + 30
    + bytes.readUInt16LE(localOffset + 26)
    + bytes.readUInt16LE(localOffset + 28);
  return { valid, bytes, centralOffset, localOffset, dataStart };
}

test("the small corruption fixture round-trips before mutation", () => {
  const { valid, bytes } = corruptionFixture();
  assert.deepEqual(bytes, valid);
  assert.deepEqual([...readZipEntries(valid)], [["one.txt", Buffer.from("one")]]);
});

test("rejects a stored payload that does not match its CRC", () => {
  const { bytes, dataStart } = corruptionFixture();
  bytes[dataStart] ^= 1;
  assert.throws(() => readZipEntries(bytes), {
    name: "Error",
    message: "ZIP entry integrity check failed: one.txt"
  });
});

test("rejects an archive with a truncated end record", () => {
  const { bytes } = corruptionFixture();
  assert.throws(() => readZipEntries(bytes.subarray(0, bytes.length - 1)), {
    name: "Error",
    message: "ZIP end record was not found"
  });
});

test("rejects an invalid local-header signature", () => {
  const { bytes, localOffset } = corruptionFixture();
  bytes.writeUInt32LE(0, localOffset);
  assert.throws(() => readZipEntries(bytes), {
    name: "Error",
    message: "invalid ZIP local file header"
  });
});

test("rejects entry data extending beyond the archive buffer", () => {
  const { bytes, centralOffset, dataStart } = corruptionFixture();
  // The central header's compressed-size field is 20 bytes from its start.
  bytes.writeUInt32LE(bytes.length - dataStart + 1, centralOffset + 20);
  assert.throws(() => readZipEntries(bytes), {
    name: "Error",
    message: "ZIP entry is out of bounds: one.txt"
  });
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
