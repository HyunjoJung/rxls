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
  const controller = new AbortController();
  const first = client.request("capabilities", {}, {
    signal: controller.signal,
    resourceBytes: MAX_PENDING_RESOURCE_BYTES
  });
  await assert.rejects(
    client.request("capabilities", {}, { resourceBytes: 1 }),
    (error) => error.code === "limit_exceeded" && error.resource === "pendingResourceBytes"
  );
  controller.abort();
  await assert.rejects(first, (error) => error.name === "AbortError");
  const second = client.request("capabilities", {}, {
    resourceBytes: MAX_PENDING_RESOURCE_BYTES
  });
  client.terminate();
  await assert.rejects(second, (error) => error.code === "client_closed");
});

test("postMessage failures reject and release pending capacity", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  worker.emit({ protocol: PROTOCOL, type: "ready", capabilities: {} });
  worker.postError = new DOMException("not cloneable", "DataCloneError");
  await assert.rejects(
    client.request("capabilities", {}, { resourceBytes: MAX_PENDING_RESOURCE_BYTES }),
    (error) => error.code === "worker_message_error"
  );
  worker.postError = null;
  const pending = client.request("capabilities", {}, {
    resourceBytes: MAX_PENDING_RESOURCE_BYTES
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
