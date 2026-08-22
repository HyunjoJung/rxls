import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  BROWSER_FIXTURE_PROVENANCE,
  FIXTURE_COLUMNS,
  FIXTURE_ROWS,
  FIXTURE_TILE,
  FIXTURE_TILE_MEASURED_CELLS,
  FIXTURE_TILE_PAINT_CELLS,
  createBrowserFixture
} from "./browser/fixture.mjs";

const EXPECTED = Object.freeze({
  workbookSha256: "0ebe6fbebffa0edcb9492d0a970f4b962af619d0123448451c90ca427a0a09f6",
  workbookBytes: 555_126,
  imageSha256: "13147132a9fe28f4f854077ff5ef7d469015c732b88aff421b4f5103dbd196ff",
  imageBytes: 16_516,
  imageWidth: 64,
  imageHeight: 64,
  decodedImageSha256:
    "82cb3eaa0d7317f95945a978587f7e5b0aa790f5c9d3a2c777323d7ef70a0975",
  decodedImageBytes: 16_384,
  renderedImageSha256:
    "b848eb79a6b54cefd9772661737bed9d9273a48df8ee3082cb70023f1b7c8530",
  renderedImageBytes: 13_029,
  fontSha256: "66611ec933da0c18c6558ba71991f3273aa45bbd27fffaa56caea0b8b095f4cc",
  fontBytes: 500,
  fontPackSha256: "87d155e63c124554a065145a45eb7b1c617d0dff468749da9c3f97d93402f12a"
});
const EXPECTED_LICENSE = Object.freeze({
  bytes: 4_050,
  sha256: "3979c54100272556277f65c0a6c8e3c92bc1e1d2a4f8252f03f4ec0f0599b2ae"
});

test("project-owned browser fixture is deterministic, nontrivial, and self-described", async () => {
  const first = await createBrowserFixture();
  const second = await createBrowserFixture();

  assert.deepEqual(first.metadata, {
    ...EXPECTED,
    rows: FIXTURE_ROWS,
    columns: FIXTURE_COLUMNS,
    cells: FIXTURE_ROWS * FIXTURE_COLUMNS,
    tilePaintCells: FIXTURE_TILE_PAINT_CELLS,
    tileMeasuredCells: FIXTURE_TILE_MEASURED_CELLS
  });
  assert.deepEqual(second.metadata, first.metadata);
  assert.notEqual(first.workbook, second.workbook);
  assert.deepEqual(first.workbook, second.workbook);
  assert.deepEqual([...first.workbook.subarray(0, 4)], [0x50, 0x4b, 0x03, 0x04]);
  assert.equal(BROWSER_FIXTURE_PROVENANCE.externalAssets, false);
  assert.equal(
    BROWSER_FIXTURE_PROVENANCE.ownership,
    "project-authored synthetic workbook, image, and font"
  );
  assert.equal(FIXTURE_TILE.lastRow - FIXTURE_TILE.firstRow + 1, 64);
  assert.equal(FIXTURE_TILE.lastCol - FIXTURE_TILE.firstCol + 1, 32);
  assert.equal(FIXTURE_TILE_PAINT_CELLS, 2_048);
  assert.equal(FIXTURE_TILE_MEASURED_CELLS, 4_096);

  const manifest = JSON.parse(new TextDecoder().decode(first.fontPack.manifest));
  assert.equal(manifest.schema, "rxls.render-font-pack.v1");
  assert.equal(manifest.license, "SIL-OFL-1.1");
  assert.equal(manifest.pack_sha256, EXPECTED.fontPackSha256);
  assert.deepEqual(manifest.licenses, [
    {
      bytes: EXPECTED_LICENSE.bytes,
      output: "licenses/OFL.txt",
      sha256: EXPECTED_LICENSE.sha256
    }
  ]);
  assert.deepEqual(
    first.fontPack.members.map(({ name, bytes }) => [name, bytes.byteLength]),
    [
      ["fonts/RxlsFixtureSans.ttf", 500],
      ["licenses/OFL.txt", EXPECTED_LICENSE.bytes],
      ["fonts.conf", 42]
    ]
  );
  const license = new TextDecoder().decode(first.fontPack.members[1].bytes);
  assert.match(license, /^Copyright 2026 rxls contributors\n\nSIL OPEN FONT LICENSE\n/);
  assert.match(license, /\nPREAMBLE\n/);
  assert.match(license, /\nDEFINITIONS\n/);
  assert.match(license, /\nPERMISSION & CONDITIONS\n/);
  assert.match(license, /\nTERMINATION\n/);
  assert.match(license, /\nDISCLAIMER\n/);
  assert.match(license, /FROM OTHER DEALINGS IN THE FONT SOFTWARE\.\n$/);
  assert.notEqual(first.fontPack.manifest, second.fontPack.manifest);
  assert.notEqual(first.fontPack.members[0].bytes, second.fontPack.members[0].bytes);
  first.workbook[0] = 0;
  first.fontPack.members[0].bytes[0] = 0;
  const third = await createBrowserFixture();
  assert.deepEqual([...third.workbook.subarray(0, 4)], [0x50, 0x4b, 0x03, 0x04]);
  assert.equal(third.fontPack.members[0].bytes[0], second.fontPack.members[0].bytes[0]);
});

test("browser-only fixtures remain outside the explicit npm artifact", async () => {
  const packageMetadata = JSON.parse(
    await readFile(new URL("../package.json", import.meta.url), "utf8")
  );
  assert.deepEqual(packageMetadata.files, [
    "js/",
    "pkg/",
    "README.md",
    "LICENSE",
    "THIRD_PARTY_NOTICES.txt"
  ]);
  assert.equal(packageMetadata.files.some((entry) => entry.startsWith("tests")), false);
});
