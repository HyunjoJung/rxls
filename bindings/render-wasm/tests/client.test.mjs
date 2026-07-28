import test from "node:test";
import assert from "node:assert/strict";

import { RenderWorkerClient, getRenderWorkerUrl } from "../js/client.mjs";
import {
  MAX_FONT_FILES,
  MAX_INPUT_BYTES,
  MAX_PENDING_REQUESTS,
  MAX_PENDING_RESOURCE_BYTES,
  PROTOCOL
} from "../js/protocol.mjs";

class FakeWorker {
  listeners = { message: [], error: [], messageerror: [] };
  sent = [];
  terminated = false;
  postError = null;

  addEventListener(type, listener) {
    this.listeners[type].push(listener);
  }

  postMessage(message, transfer = []) {
    if (this.postError) {
      throw this.postError;
    }
    this.sent.push({ message, transfer });
  }

  emit(message) {
    for (const listener of this.listeners.message) {
      listener({ data: message });
    }
  }

  emitError(message = "") {
    for (const listener of this.listeners.error) {
      listener({ message });
    }
  }

  terminate() {
    this.terminated = true;
  }
}

test("published worker URL resolves relative to the client module", () => {
  const first = getRenderWorkerUrl();
  const second = getRenderWorkerUrl();
  assert.ok(first instanceof URL);
  assert.notEqual(first, second);
  assert.equal(first.href, new URL("../js/worker.mjs", import.meta.url).href);
});

test("client transfers copies, reports progress, and resolves typed results", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  worker.emit({ protocol: PROTOCOL, type: "ready", capabilities: {} });
  const source = Uint8Array.of(1, 2, 3);
  const progress = [];
  const pending = client.open(source, { onProgress: (value) => progress.push(value) });
  const sent = worker.sent[0];
  assert.equal(sent.message.operation, "open");
  assert.equal(sent.transfer.length, 1);
  assert.deepEqual([...source], [1, 2, 3]);
  worker.emit({
    protocol: PROTOCOL,
    type: "progress",
    requestId: pending.requestId,
    completed: 1,
    total: 3,
    stage: "parsing"
  });
  worker.emit({
    protocol: PROTOCOL,
    type: "result",
    requestId: pending.requestId,
    ok: true,
    result: { documentId: "document-1" },
    error: null
  });
  assert.deepEqual(await pending, { documentId: "document-1" });
  assert.deepEqual(progress, [{ completed: 1, total: 3, stage: "parsing" }]);
});

test("pre-ready open retains four byte-exact source-distinct clones", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  const source = Uint8Array.of(0x11, 0x22, 0x33, 0x44);
  const expected = [...source];
  const pending = Array.from({ length: 4 }, (_, index) =>
    client.open(source, { documentId: `queued-clone-${index}` })
  );
  assert.equal(worker.sent.length, 0);
  source.fill(0xff);

  worker.emit({ protocol: PROTOCOL, type: "ready", capabilities: {} });
  assert.equal(worker.sent.length, 4);
  const workbooks = worker.sent.map(({ message, transfer }, index) => {
    assert.equal(message.operation, "open");
    assert.equal(message.payload.documentId, `queued-clone-${index}`);
    assert.ok(message.payload.bytes instanceof Uint8Array);
    assert.notEqual(message.payload.bytes, source);
    assert.notEqual(message.payload.bytes.buffer, source.buffer);
    assert.deepEqual([...message.payload.bytes], expected);
    assert.equal(transfer.length, 1);
    assert.equal(transfer[0], message.payload.bytes.buffer);
    return message.payload.bytes;
  });
  assert.equal(new Set(workbooks).size, 4);
  assert.equal(new Set(workbooks.map(({ buffer }) => buffer)).size, 4);

  client.terminate();
  const outcomes = await Promise.allSettled(pending);
  assert.equal(
    outcomes.filter(
      ({ status, reason }) =>
        status === "rejected" && reason.code === "client_closed"
    ).length,
    4
  );
});

test("AbortSignal sends cancellation and rejects without waiting for wasm", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  worker.emit({ protocol: PROTOCOL, type: "ready", capabilities: {} });
  const controller = new AbortController();
  const pending = client.renderPage("doc", 0, 0, {}, { signal: controller.signal });
  controller.abort();
  await assert.rejects(pending, (error) => error.name === "AbortError");
  assert.deepEqual(worker.sent.at(-1).message, {
    protocol: PROTOCOL,
    type: "cancel",
    requestId: pending.requestId
  });
});

test("terminate rejects all requests and stops the worker", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  worker.emit({ protocol: PROTOCOL, type: "ready", capabilities: {} });
  const pending = client.capabilities();
  client.terminate();
  await assert.rejects(pending, (error) => error.code === "client_closed");
  assert.equal(worker.terminated, true);
});

test("fatal worker errors close the client and reject pending and future work", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  const pending = client.capabilities();

  worker.emitError("module initialization failed");

  await assert.rejects(
    pending,
    (error) => error.code === "worker_crashed" && error.message === "module initialization failed"
  );
  assert.equal(worker.terminated, true);
  assert.equal(worker.sent.length, 0);
  await assert.rejects(
    client.capabilities(),
    (error) => error.code === "worker_crashed" && error.message === "module initialization failed"
  );
  assert.throws(
    () => client.open(Uint8Array.of(1)),
    (error) => error.code === "worker_crashed" && error.message === "module initialization failed"
  );
});

test("client rejects oversized input before transfer and caps pre-ready work", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  assert.throws(
    () => client.open(new Uint8Array(MAX_INPUT_BYTES + 1)),
    (error) => error.code === "limit_exceeded" && error.resource === "inputBytes"
  );
  assert.equal(worker.sent.length, 0);
  assert.throws(
    () =>
      client.open(Uint8Array.of(1), {
        fontPack: {
          manifest: new Uint8Array(),
          members: Array.from({ length: MAX_FONT_FILES + 1 }, (_, index) => ({
            name: `font-${index}.ttf`,
            bytes: new Uint8Array()
          }))
        }
      }),
    (error) => error.code === "limit_exceeded" && error.resource === "fontFiles"
  );
  assert.equal(worker.sent.length, 0);

  const pending = Array.from({ length: MAX_PENDING_REQUESTS }, () => client.capabilities());
  await assert.rejects(
    client.capabilities(),
    (error) => error.code === "limit_exceeded" && error.resource === "pendingRequests"
  );
  client.terminate();
  const outcomes = await Promise.allSettled(pending);
  assert.equal(outcomes.filter(({ status }) => status === "rejected").length, pending.length);
});

test("client accounts pending transferable bytes and releases them on cancellation", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  const source = new Uint8Array(MAX_INPUT_BYTES);
  const controller = new AbortController();
  const first = client.request(
    "open",
    { documentId: "pending-0", bytes: source },
    { signal: controller.signal }
  );
  const retained = [1, 2, 3].map((index) =>
    client.request("open", {
      documentId: `pending-${index}`,
      bytes: source
    })
  );
  assert.equal(MAX_INPUT_BYTES * 4, MAX_PENDING_RESOURCE_BYTES);
  await assert.rejects(
    client.request("open", {
      documentId: "over-capacity",
      bytes: Uint8Array.of(1)
    }),
    (error) => error.code === "limit_exceeded" && error.resource === "pendingResourceBytes"
  );
  controller.abort();
  await assert.rejects(first, (error) => error.name === "AbortError");
  const replacement = client.request("open", {
    documentId: "replacement",
    bytes: source
  });
  client.terminate();
  const outcomes = await Promise.allSettled([...retained, replacement]);
  assert.equal(
    outcomes.filter(
      ({ status, reason }) => status === "rejected" && reason.code === "client_closed"
    ).length,
    outcomes.length
  );
});

test("postMessage failures reject and release pending capacity", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  worker.emit({ protocol: PROTOCOL, type: "ready", capabilities: {} });
  worker.postError = new DOMException("not cloneable", "DataCloneError");
  await assert.rejects(
    client.request("open", {
      documentId: "clone-failure",
      bytes: Uint8Array.of(1)
    }),
    (error) => error.code === "worker_message_error"
  );
  worker.postError = null;
  const pending = client.request("open", {
    documentId: "clone-recovery",
    bytes: Uint8Array.of(1)
  });
  worker.emit({
    protocol: PROTOCOL,
    type: "result",
    requestId: pending.requestId,
    ok: true,
    result: {},
    error: null
  });
  assert.deepEqual(await pending, {});
});

test("generic requests are validated before postMessage and cannot declare accounting", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  worker.emit({ protocol: PROTOCOL, type: "ready", capabilities: {} });

  await assert.rejects(
    client.request("unsupported", {}),
    (error) => error.code === "unknown_operation"
  );
  await assert.rejects(
    client.request("render-page", {
      documentId: "doc",
      sheetIndex: 0,
      pageIndex: 0,
      options: {},
      extra: Uint8Array.of(1)
    }),
    (error) => error.code === "invalid_payload"
  );
  await assert.rejects(
    client.request("capabilities", {}, { resourceBytes: 1 }),
    (error) => error.code === "invalid_client_options"
  );
  await assert.rejects(
    client.request("capabilities", {}, { transfer: [] }),
    (error) => error.code === "invalid_client_options"
  );
  assert.equal(worker.sent.length, 0);
});
