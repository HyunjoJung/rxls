import test from "node:test";
import assert from "node:assert/strict";
import { copyWorkbook, requireBuffer, requireWorkbookName, requireState, parseBootstrap, publicError,
  requireEnvelope, envelope } from "../src/embed/protocol.mjs";
const identity = { instanceId: "11111111-1111-4111-8111-111111111111", nonce: "22222222-2222-4222-8222-222222222222" };

test("binary admission rejects overflow and copies only the intended view without detaching its source", () => {
  const input = new Uint8Array([11, 22, 33, 44]);
  const owned = copyWorkbook(input.subarray(1, 3));
  assert.deepEqual([...new Uint8Array(owned)], [22, 33]);
  structuredClone(owned, { transfer: [owned] });
  assert.deepEqual([...input], [11, 22, 33, 44]);
  for (const invalid of [new Uint8Array(), new Uint8Array(32 * 1024 * 1024 + 1), [1], new ArrayBuffer(1)]) {
    assert.throws(() => copyWorkbook(invalid), { code: "invalid_bytes" });
  }
  assert.throws(() => requireBuffer(new ArrayBuffer(0)), { code: "invalid_bytes" });
});

test("identity/version boundaries reject sibling messages and opaque parent origins", () => {
  assert.throws(() => requireEnvelope(envelope({ ...identity, instanceId: identity.nonce }, "ready"), identity), { code: "invalid_message" });
  assert.throws(() => requireEnvelope({ ...envelope(identity, "ready"), protocol: "rxls.editor-embed.v0" }, identity), { code: "protocol_mismatch" });
  const query = new URLSearchParams({ instance: identity.instanceId, nonce: identity.nonce, parentOrigin: "https://consumer.example" });
  assert.deepEqual(parseBootstrap({ hash: "#" + query }), { identity, parentOrigin: "https://consumer.example" });
  for (const parentOrigin of ["null", "file:///tmp/editor", "https://consumer.example/path"]) {
    query.set("parentOrigin", parentOrigin);
    assert.throws(() => parseBootstrap({ hash: "#" + query }), { code: "invalid_bootstrap" });
  }
});

test("logical workbook names exclude paths and diagnostics exclude workbook exception text", () => {
  assert.equal(requireWorkbookName("한국어.XLSM"), "한국어.XLSM");
  const absoluteName = "C:" + "\\a.xlsx";
  for (const name of ["../a.xlsx", absoluteName, "a\u0000.xlsx", "a.csv", "xls", "a".repeat(256) + ".xlsx"]) {
    assert.throws(() => requireWorkbookName(name), { code: "invalid_filename" });
  }
  assert.deepEqual(publicError({ code: "wasm_api_mismatch", message: "SECRET WORKBOOK TEXT" }),
    { code: "wasm_api_mismatch", message: "The editor operation failed." });
  const state = requireState({ loaded: false, busy: false, dirty: false, draft: false, pendingMutation: false,
    canUndo: false, canRedo: false, fileName: null, format: null, sheetCount: 0, sheetIndex: 0, capability: null,
    reason: null, privateWorkbookXml: "SECRET" });
  assert.equal("privateWorkbookXml" in state, false);
});
