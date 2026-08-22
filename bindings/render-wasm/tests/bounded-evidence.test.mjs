import assert from "node:assert/strict";
import test from "node:test";

import { BoundedEntryLog, BoundedTextTail } from "./browser/bounded-evidence.mjs";

test("route/request evidence is byte and entry bounded", () => {
  const log = new BoundedEntryLog({ maxEntries: 2, maxBytes: 8, maxEntryBytes: 4 });
  log.add("/one");
  log.add("/two");
  assert.deepEqual(log.values(), ["/one", "/two"]);
  assert.throws(() => log.add("/x"), /limit exceeded/);
  assert.throws(
    () =>
      new BoundedEntryLog({ maxEntries: 1, maxBytes: 8, maxEntryBytes: 4 }).add(
        "/oversized"
      ),
    /limit exceeded/
  );
});

test("stderr evidence retains only a bounded tail", () => {
  const tail = new BoundedTextTail(8);
  tail.append("abcd");
  tail.append("efghij");
  assert.equal(tail.text(), "cdefghij");
  assert.equal(tail.discardedBytes, 2);
  tail.append("0123456789abcdef");
  assert.equal(tail.text(), "89abcdef");
  assert.equal(tail.discardedBytes, 18);
});
