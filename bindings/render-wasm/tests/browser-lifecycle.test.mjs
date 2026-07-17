import { EventEmitter } from "node:events";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  OperationTimeoutError,
  closeServer,
  createCdpClient,
  terminateChild,
  waitForWebSocketOpen
} from "./browser/lifecycle.mjs";

const packageMetadata = JSON.parse(
  await readFile(new URL("../package.json", import.meta.url), "utf8")
);

class FakeChild extends EventEmitter {
  exitCode = null;
  signalCode = null;
  signals = [];

  constructor(closeOnSignal) {
    super();
    this.closeOnSignal = closeOnSignal;
  }

  kill(signal) {
    this.signals.push(signal);
    if (this.closeOnSignal === signal) {
      this.signalCode = signal;
      this.emit("close", null, signal);
    }
    return true;
  }
}

class FakeSocket {
  readyState = 0;
  sent = [];
  closed = false;
  listeners = new Map();

  addEventListener(type, listener, options = {}) {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push({ listener, once: options.once === true });
    this.listeners.set(type, listeners);
  }

  removeEventListener(type, listener) {
    this.listeners.set(
      type,
      (this.listeners.get(type) ?? []).filter((entry) => entry.listener !== listener)
    );
  }

  emit(type, event = {}) {
    const listeners = [...(this.listeners.get(type) ?? [])];
    for (const entry of listeners) {
      entry.listener(event);
      if (entry.once) {
        this.removeEventListener(type, entry.listener);
      }
    }
  }

  send(message) {
    this.sent.push(JSON.parse(message));
  }

  close() {
    this.closed = true;
  }
}

test("Chromium close listener is registered before SIGTERM", async () => {
  const child = new FakeChild("SIGTERM");
  await terminateChild(child, { gracefulTimeoutMs: 5, forceTimeoutMs: 5 });
  assert.deepEqual(child.signals, ["SIGTERM"]);
});

test("Node 20 browser smoke explicitly enables the WebSocket API", () => {
  assert.equal(
    packageMetadata.scripts["test:browser"],
    "node --experimental-websocket tests/browser/run.mjs"
  );
});

test("Chromium termination escalates to SIGKILL within a deadline", async () => {
  const child = new FakeChild("SIGKILL");
  await terminateChild(child, { gracefulTimeoutMs: 5, forceTimeoutMs: 5 });
  assert.deepEqual(child.signals, ["SIGTERM", "SIGKILL"]);
});

test("HTTP cleanup tears down connections when graceful close stalls", async () => {
  let closeCallback;
  const server = {
    listening: true,
    forced: false,
    close(callback) {
      closeCallback = callback;
    },
    closeAllConnections() {
      this.forced = true;
      this.listening = false;
      closeCallback();
    }
  };
  await closeServer(server, { gracefulTimeoutMs: 5, forceTimeoutMs: 5 });
  assert.equal(server.forced, true);
});

test("DevTools open and commands have independent deadlines", async () => {
  const unopened = new FakeSocket();
  await assert.rejects(
    waitForWebSocketOpen(unopened, 5),
    (error) => error instanceof OperationTimeoutError
  );
  assert.equal(unopened.closed, true);

  const socket = new FakeSocket();
  socket.readyState = 1;
  const client = createCdpClient(socket, { commandTimeoutMs: 5 });
  const successful = client.command("Runtime.enable");
  socket.emit("message", {
    data: JSON.stringify({ id: socket.sent[0].id, result: { enabled: true } })
  });
  assert.deepEqual(await successful, { enabled: true });
  await assert.rejects(
    client.command("Runtime.getHeapUsage"),
    (error) => error instanceof OperationTimeoutError
  );
  client.dispose();
});
