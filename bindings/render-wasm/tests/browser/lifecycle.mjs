export class OperationTimeoutError extends Error {
  constructor(label, timeoutMs) {
    super(`${label} timed out after ${timeoutMs}ms`);
    this.name = "OperationTimeoutError";
  }
}

export async function withTimeout(operation, timeoutMs, label, onTimeout = null) {
  if (!Number.isSafeInteger(timeoutMs) || timeoutMs <= 0) {
    throw new TypeError("timeout must be a positive safe integer");
  }
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => {
      try {
        onTimeout?.();
      } catch {
        // The deadline remains authoritative even if cancellation itself fails.
      }
      reject(new OperationTimeoutError(label, timeoutMs));
    }, timeoutMs);
  });
  try {
    return await Promise.race([operation, timeout]);
  } finally {
    clearTimeout(timer);
  }
}

async function resolvesWithin(operation, timeoutMs) {
  let timer;
  try {
    return await Promise.race([
      operation.then(() => true),
      new Promise((resolve) => {
        timer = setTimeout(() => resolve(false), timeoutMs);
      })
    ]);
  } finally {
    clearTimeout(timer);
  }
}

export async function terminateChild(
  child,
  { gracefulTimeoutMs = 5_000, forceTimeoutMs = 5_000 } = {}
) {
  if (child.exitCode !== null || child.signalCode !== null) {
    return;
  }
  let onClose;
  const closed = new Promise((resolve) => {
    onClose = resolve;
    child.once("close", onClose);
  });
  if (child.exitCode !== null || child.signalCode !== null) {
    child.removeListener("close", onClose);
    return;
  }
  child.kill("SIGTERM");
  if (await resolvesWithin(closed, gracefulTimeoutMs)) {
    return;
  }
  child.kill("SIGKILL");
  try {
    await withTimeout(closed, forceTimeoutMs, "Chromium process close after SIGKILL");
  } finally {
    child.removeListener("close", onClose);
  }
}

export async function closeServer(
  server,
  { gracefulTimeoutMs = 2_000, forceTimeoutMs = 2_000 } = {}
) {
  if (!server.listening) {
    return;
  }
  const closed = new Promise((resolve, reject) => {
    server.close((error) => (error ? reject(error) : resolve()));
  });
  if (await resolvesWithin(closed, gracefulTimeoutMs)) {
    return;
  }
  server.closeAllConnections();
  await withTimeout(closed, forceTimeoutMs, "HTTP server close after connection teardown");
}

export async function waitForWebSocketOpen(socket, timeoutMs) {
  if (socket.readyState === 1) {
    return;
  }
  let onOpen;
  let onError;
  let onClose;
  const opened = new Promise((resolve, reject) => {
    onOpen = resolve;
    onError = () => reject(new Error("Chromium DevTools WebSocket failed to open"));
    onClose = () => reject(new Error("Chromium DevTools WebSocket closed before opening"));
    socket.addEventListener("open", onOpen, { once: true });
    socket.addEventListener("error", onError, { once: true });
    socket.addEventListener("close", onClose, { once: true });
  });
  try {
    await withTimeout(opened, timeoutMs, "Chromium DevTools WebSocket open", () =>
      socket.close()
    );
  } finally {
    socket.removeEventListener("open", onOpen);
    socket.removeEventListener("error", onError);
    socket.removeEventListener("close", onClose);
  }
}

export function createCdpClient(socket, { commandTimeoutMs = 5_000, onEvent } = {}) {
  let nextId = 1;
  let terminalError = null;
  const pending = new Map();
  const failPending = (error) => {
    terminalError = error;
    for (const { reject, timer } of pending.values()) {
      clearTimeout(timer);
      reject(error);
    }
    pending.clear();
  };
  const onMessage = (event) => {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch (error) {
      failPending(new Error(`invalid Chromium DevTools message: ${error.message}`));
      return;
    }
    const entry = pending.get(message.id);
    if (entry) {
      pending.delete(message.id);
      clearTimeout(entry.timer);
      if (message.error) {
        entry.reject(new Error(`${entry.method}: ${message.error.message}`));
      } else {
        entry.resolve(message.result ?? {});
      }
    } else if (message.method) {
      onEvent?.(message);
    }
  };
  const onClose = () => failPending(new Error("Chromium DevTools WebSocket closed"));
  const onError = () => failPending(new Error("Chromium DevTools WebSocket failed"));
  socket.addEventListener("message", onMessage);
  socket.addEventListener("close", onClose, { once: true });
  socket.addEventListener("error", onError, { once: true });

  return {
    command(method, params = {}, sessionId = undefined) {
      if (terminalError) {
        return Promise.reject(terminalError);
      }
      const id = nextId++;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(id);
          reject(new OperationTimeoutError(`Chromium DevTools ${method}`, commandTimeoutMs));
        }, commandTimeoutMs);
        pending.set(id, { method, resolve, reject, timer });
        const message = { id, method, params };
        if (sessionId !== undefined) {
          message.sessionId = sessionId;
        }
        try {
          socket.send(JSON.stringify(message));
        } catch (error) {
          clearTimeout(timer);
          pending.delete(id);
          reject(error);
        }
      });
    },
    abort(error) {
      failPending(error);
    },
    dispose() {
      socket.removeEventListener("message", onMessage);
      socket.removeEventListener("close", onClose);
      socket.removeEventListener("error", onError);
      failPending(new Error("Chromium DevTools client disposed"));
    }
  };
}
