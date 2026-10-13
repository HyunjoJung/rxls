import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

const html = await readFile(new URL("../index.html", import.meta.url), "utf8");
for (const id of ["cell-value", "cell-cached-value"]) {
  test(`${id} uses a bounded multiline control`, () => {
    const tag = html.match(new RegExp(`<textarea\\s+[^>]*id="${id}"[^>]*>`));
    assert.ok(tag, "a single-line input would strip existing line breaks");
    assert.match(tag[0], /maxlength="32767"/);
    assert.match(tag[0], /rows="2"/);
    assert.doesNotMatch(html, new RegExp(`<input\\s+[^>]*id="${id}"`));
  });
}
