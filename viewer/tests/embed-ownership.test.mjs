// Transport/ownership doubles only. Packed real-browser edit/save acceptance is a separate lane.
import test from "node:test";
import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { MessageChannel } from "node:worker_threads";
import { createEditor } from "../../packages/editor-embed/client/client.mjs";
import { createEmbedHost } from "../src/embed/frame-host.mjs";

function deferred() {
  let resolve;
  const promise = new Promise((yes) => { resolve = yes; });
  return { promise, resolve };
}
function events(target = {}) {
  const listeners = new Map();
  target.addEventListener = (kind, handler) => {
    if (!listeners.has(kind)) listeners.set(kind, new Set());
    listeners.get(kind).add(handler);
  };
  target.removeEventListener = (kind, handler) => listeners.get(kind)?.delete(handler);
  target.emit = (kind, event) => { for (const handler of [...(listeners.get(kind) ?? [])]) handler(event); };
  return target;
}
const emptyState = () => ({ loaded: false, busy: false, dirty: false, draft: false, pendingMutation: false,
  canUndo: false, canRedo: false, fileName: null, format: null, sheetCount: 0, sheetIndex: 0, capability: null, reason: null });

function setup({ delayBoot = false } = {}) {
  const parent = events({ location: new URL("https://consumer.example/app/"), crypto: { randomUUID },
    MessageChannel, setTimeout, clearTimeout });
  const records = [];
  let nextGate = null;
  const document = { defaultView: parent, baseURI: parent.location.href, createElement() {
    const frame = events({ style: {}, contentWindow: {} });
    frame.remove = () => {
      const index = container.children.indexOf(frame);
      if (index >= 0) container.children.splice(index, 1);
    };
    return frame;
  } };
  const container = { ownerDocument: document, isConnected: true, children: [], append(frame) {
    this.children.push(frame);
    const childWindow = events({ location: new URL(frame.src), setTimeout, clearTimeout, queueMicrotask });
    const owner = { postMessage(data, origin) {
      assert.equal(origin, parent.location.origin);
      const boot = { source: frame.contentWindow, origin: childWindow.location.origin, data };
      frame.boot = boot;
      if (!delayBoot) parent.emit("message", boot);
    } };
    childWindow.parent = owner;
    frame.contentWindow.postMessage = (data, origin, ports) => {
      assert.equal(origin, childWindow.location.origin);
      frame.connects = (frame.connects ?? 0) + 1;
      childWindow.emit("message", { source: owner, origin: parent.location.origin, data, ports });
    };
    const record = { frame, state: emptyState(), loads: [], saves: 0, disposed: 0, gate: nextGate };
    nextGate = null;
    records.push(record);
    frame.emit("load", {});
    record.host = createEmbedHost({ window: childWindow,
      runtime: { bundleId: "a".repeat(64), packageVersion: "0.0.0-dev", workerProtocol: "rxls.render-worker.v2" },
      getState: () => record.state,
      async load(bytes, { fileName }) {
        record.loads.push({ bytes: [...bytes], fileName });
        await record.gate?.promise;
        record.state = { ...emptyState(), loaded: true, fileName, format: fileName.split(".").at(-1),
          sheetCount: 1, capability: "read-write" };
      },
      async save() { record.saves++; return { bytes: new Uint8Array([80, 75, record.saves]), fileName: "saved.xlsx", format: "xlsx" }; },
      dispose() { record.disposed++; },
    });
  } };
  return { parent, records, container, gateNext(gate) { nextGate = gate; } };
}

test("one binary request slot rejects concurrent saves without a queued copy or cross-instance settlement", async () => {
  const fixture = setup();
  const gate = deferred();
  fixture.gateNext(gate);
  const a = createEditor(fixture.container, { assetsUrl: "/nested/v1/" });
  const b = createEditor(fixture.container, { assetsUrl: "/nested/v1/" });
  await Promise.all([a.ready, b.ready]);
  assert.notEqual(a.instanceId, b.instanceId);
  const input = new Uint8Array([1, 2, 3]);
  const pending = a.load(input, { fileName: "a.xlsx" });
  await assert.rejects(a.save(), { code: "busy" });
  await b.load(new Uint8Array([4]), { fileName: "b.xlsx" });
  assert.equal(b.getState().fileName, "b.xlsx");
  assert.equal(a.getState().loaded, false);
  assert.deepEqual([...input], [1, 2, 3]);
  gate.resolve();
  assert.equal((await pending).fileName, "a.xlsx");
  assert.equal(fixture.records[0].saves, 0);
  assert.equal(fixture.records[0].loads.length, 1);
  await Promise.all([a.dispose(), b.dispose()]);
  assert.equal(fixture.container.children.length, 0);
});

test("disposal rejects A's late load, releases exactly A once and leaves B operational through recreation", async () => {
  const fixture = setup();
  const gate = deferred();
  fixture.gateNext(gate);
  const a = createEditor(fixture.container, { assetsUrl: "/nested/v1/" });
  const b = createEditor(fixture.container, { assetsUrl: "/nested/v1/" });
  await Promise.all([a.ready, b.ready]);
  const load = a.load(new Uint8Array([1]), { fileName: "old.xlsx" });
  const rejected = assert.rejects(load, { code: "disposed" });
  const dispose = a.dispose();
  assert.equal(a.dispose(), dispose);
  await Promise.all([dispose, rejected]);
  gate.resolve();
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(fixture.records[0].disposed, 1);
  assert.equal(fixture.records[1].disposed, 0);
  assert.equal(fixture.container.children.length, 1);
  await b.load(new Uint8Array([2]), { fileName: "b.xlsx" });
  const saved = await b.save();
  assert.deepEqual([...saved.bytes], [80, 75, 1]);
  const recreated = createEditor(fixture.container, { assetsUrl: "/nested/v1/" });
  await recreated.ready;
  assert.notEqual(recreated.instanceId, a.instanceId);
  await recreated.load(new Uint8Array([3]), { fileName: "new.xlsx" });
  assert.equal(recreated.getState().fileName, "new.xlsx");
  assert.equal(b.getState().fileName, "b.xlsx");
  await Promise.all([b.dispose(), recreated.dispose()]);
  assert.deepEqual(fixture.records.map((record) => record.disposed), [1, 1, 1]);
});

test("early commands do not enqueue binary work and an unexpected frame reload is terminal", async () => {
  const fixture = setup();
  const editor = createEditor(fixture.container, { assetsUrl: "/nested/v1/" });
  await assert.rejects(editor.load(new Uint8Array([1]), { fileName: "a.xlsx" }), { code: "not_ready" });
  await editor.ready;
  assert.equal(fixture.records[0].loads.length, 0);
  fixture.records[0].frame.emit("load", {});
  await assert.rejects(editor.save(), { code: "disposed" });
  assert.equal(fixture.container.children.length, 0);
  fixture.records[0].host.stop(); // Simulate pagehide: browser iframe removal tears down its own realm.
  assert.equal(fixture.records[0].disposed, 1);
});

test("boot from wrong window/origin/sibling identity never transfers the owned port", async () => {
  const fixture = setup({ delayBoot: true });
  const editor = createEditor(fixture.container, { assetsUrl: "/nested/v1/" });
  const frame = fixture.records[0].frame;
  const boot = frame.boot;
  fixture.parent.emit("message", { ...boot, source: {} });
  fixture.parent.emit("message", { ...boot, origin: "https://other.example" });
  fixture.parent.emit("message", { ...boot, data: { ...boot.data, instanceId: randomUUID() } });
  assert.equal(frame.connects ?? 0, 0);
  fixture.parent.emit("message", boot);
  await editor.ready;
  assert.equal(frame.connects, 1);
  // Ambient post-handshake window messages do not reach the owned request handler.
  fixture.parent.emit("message", { ...boot, data: { ...boot.data, type: "request", operation: "load", requestId: 1 } });
  assert.equal(fixture.records[0].loads.length, 0);
  await editor.dispose();
});

test("a mismatched bootstrap version rejects ready and removes only its frame", async () => {
  const fixture = setup({ delayBoot: true });
  const editor = createEditor(fixture.container, { assetsUrl: "/nested/v1/" });
  const frame = fixture.records[0].frame;
  const rejected = assert.rejects(editor.ready, { code: "protocol_mismatch" });
  fixture.parent.emit("message", { ...frame.boot, data: { ...frame.boot.data, protocol: "rxls.editor-embed.v0" } });
  await rejected;
  assert.equal(frame.connects ?? 0, 0);
  assert.equal(fixture.container.children.length, 0);
  fixture.records[0].host.stop();
});
