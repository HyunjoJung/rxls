import test from "node:test";
import assert from "node:assert/strict";
import {
  parseClipboardRange,
  rangePasteRequest,
  MAX_PASTE_BYTES,
} from "../src/clipboard.js";

test("clipboard TSV keeps quoted separators, blank cells and CRLF rows", () => {
  assert.deepEqual(
    parseClipboardRange('1\t"two\tparts"\t\r\n3\t"two\r\nlines"\t"a""b"\r\n'),
    [
      ["1", "two\tparts", ""],
      ["3", "two\nlines", 'a"b'],
    ],
  );
  assert.deepEqual(parseClipboardRange("a\n\n"), [["a"], [""]]);
  assert.deepEqual(parseClipboardRange("\t\n"), [["", ""]]);
  assert.deepEqual(parseClipboardRange('"line one\nline two"'), [
    ["line one\nline two"],
  ]);
  assert.deepEqual(parseClipboardRange('ordinary "quote"'), [
    ['ordinary "quote"'],
  ]);
});

test("clipboard rejects malformed, ragged and oversized rectangles before application", () => {
  for (const text of [
    "",
    '"unfinished',
    '"closed"suffix',
    "a\tb\nc",
    "a\nb\tc",
  ])
    assert.throws(
      () => parseClipboardRange(text),
      /clipboard|rectang|quot|empty/i,
    );
  assert.throws(
    () => parseClipboardRange("x".repeat(MAX_PASTE_BYTES + 1)),
    /size/i,
  );
  assert.throws(
    () => parseClipboardRange("한".repeat(Math.floor(MAX_PASTE_BYTES / 3) + 1)),
    /size/i,
  );
  assert.throws(
    () => parseClipboardRange(Array(10_001).fill("1").join("\t")),
    /10,000/,
  );
});

test("range paste uses existing scalar rules and bounded worksheet coordinates", () => {
  const request = rangePasteRequest("001\tTRUE\t\n=1+2\t'004\t-2.5\n", 3, 1);
  assert.equal(request.reference, "B4:D5");
  assert.equal(request.cellCount, 6);
  assert.deepEqual(request.values, [
    [
      { kind: "text", value: "001" },
      { kind: "boolean", value: true },
      { kind: "blank" },
    ],
    [
      { kind: "formula-auto", formula: "1+2" },
      { kind: "text", value: "004" },
      { kind: "number", value: -2.5 },
    ],
  ]);
  assert.throws(() => rangePasteRequest("1\t2", 0, 16_383), /worksheet/i);
  assert.throws(() => rangePasteRequest("1\n2", 1_048_575, 0), /worksheet/i);
  for (const coordinate of [-1, 0.5, NaN, Infinity])
    assert.throws(() => rangePasteRequest("1", coordinate, 0), /worksheet/i);
  assert.throws(() => rangePasteRequest("1e999\t2", 0, 0), /finite/i);
  assert.throws(() => rangePasteRequest("=\t2", 0, 0), /formula/i);
});

test("serialized range budget includes escaping and typed-cell overhead", () => {
  assert.throws(
    () => rangePasteRequest("\\".repeat(MAX_PASTE_BYTES / 2 + 1), 0, 0),
    /size/i,
  );
  assert.equal(
    rangePasteRequest("a\nb", 0, 0, { asText: true }).values[0][0].value,
    "a\nb",
  );
  assert.equal(
    rangePasteRequest("=1+2\t3", 0, 0, { asText: true }).values[0][0].kind,
    "text",
  );
});
