import {
  READY_TIMEOUT_MS, OPERATION_TIMEOUT_MS, DISPOSE_TIMEOUT_MS,
  EditorEmbedError, copyWorkbook, envelope, requireEnvelope, requireCondition,
  requireWorkbookName, requireBuffer, requireState, requireRuntimeIdentity,
  sameIdentity,
} from "./protocol.mjs";

/** Create one isolated editor iframe from a copied, versioned package asset directory. */
export function createEditor(container, { assetsUrl, title = "Spreadsheet editor", expectedBundleId } = {}) {
  const document = container?.ownerDocument;
  const window = document?.defaultView;
  requireCondition(document && window && container.isConnected, "invalid_container", "Use a connected DOM container.");
  requireCondition(["http:", "https:"].includes(window.location.protocol),
    "invalid_origin", "Serve the editor consumer over HTTP or HTTPS.");
  requireCondition(typeof title === "string" && title.length <= 255,
    "invalid_option", "Use a short iframe title.");
  requireCondition(typeof assetsUrl === "string" || assetsUrl instanceof URL,
    "invalid_assets_url", "Supply the copied editor asset-directory URL.");
  const base = new URL(assetsUrl, document.baseURI);
  requireCondition(["http:", "https:"].includes(base.protocol) && !base.username &&
    !base.password && !base.search && !base.hash && base.pathname.endsWith("/"),
  "invalid_assets_url", "Use an HTTP(S) asset-directory URL ending with a slash.");
  requireCondition(expectedBundleId === undefined || /^[0-9a-f]{64}$/.test(expectedBundleId),
    "invalid_option", "Use the packaged bundle digest as expectedBundleId.");
  const identity = { instanceId: window.crypto.randomUUID(), nonce: window.crypto.randomUUID() };
  const target = new URL("embed.html", base);
  target.hash = new URLSearchParams({ instance: identity.instanceId, nonce: identity.nonce,
    parentOrigin: window.location.origin }).toString();
  const frame = document.createElement("iframe");
  frame.title = title;
  frame.src = target.href;
  frame.referrerPolicy = "no-referrer";
  frame.style.width = "100%";
  frame.style.height = "100%";
  frame.style.border = "0";
  // Trusted package assets: the iframe is an ownership boundary, not a sandbox for untrusted code.
  let port = null;
  let closed = false;
  let closing = false;
  let loadCount = 0;
  let nextRequest = 1;
  let latestSequence = -1;
  let snapshot = null;
  let runtime = null;
  let disposal = null;
  const pending = new Map();
  const changes = new Set();
  const diagnostics = new Set();
  let readyResolve;
  let readyReject;
  let readySettled = false;
  const ready = new Promise((resolve, reject) => { readyResolve = resolve; readyReject = reject; });
  // A consumer may call dispose before awaiting ready; retain rejection without unhandled noise.
  void ready.catch(() => {});
  const readyTimer = window.setTimeout(() => fail(new EditorEmbedError("ready_timeout", "The editor did not become ready.")), READY_TIMEOUT_MS);

  function notify(listeners, value) {
    for (const listener of listeners) {
      try { listener(value); } catch { /* Host callbacks cannot break protocol cleanup. */ }
    }
  }
  function cleanup() {
    window.clearTimeout(readyTimer);
    window.removeEventListener("message", onBootstrap);
    frame.removeEventListener("load", onFrameLoad);
    port?.close();
    port = null;
    frame.remove();
    changes.clear();
    diagnostics.clear();
  }
  function rejectPending(error) {
    for (const entry of pending.values()) {
      window.clearTimeout(entry.timer);
      entry.reject(error);
    }
    pending.clear();
  }
  function fail(error) {
    if (closed) return;
    notify(diagnostics, Object.freeze({ code: error.code ?? "operation_failed", message: error.message }));
    closed = true;
    if (!readySettled) { readySettled = true; readyReject(error); }
    rejectPending(error);
    cleanup();
  }
  function onFrameLoad() {
    loadCount += 1;
    if (loadCount > 1) fail(new EditorEmbedError("frame_reloaded", "The editor iframe reloaded; create a new instance."));
  }
  function onBootstrap(event) {
    if (closed || port || event.source !== frame.contentWindow || event.origin !== target.origin ||
      !sameIdentity(event.data, identity) || event.data.type !== "boot") return;
    try {
      requireEnvelope(event.data, identity);
      const channel = new window.MessageChannel();
      port = channel.port1;
      port.onmessage = onMessage;
      port.onmessageerror = () => fail(new EditorEmbedError("invalid_message", "The editor response could not be cloned."));
      port.start();
      frame.contentWindow.postMessage(envelope(identity, "connect"), target.origin, [channel.port2]);
      window.removeEventListener("message", onBootstrap);
    } catch (error) { fail(error); }
  }
  function readState(message) {
    requireCondition(Number.isSafeInteger(message.sequence) && message.sequence >= 0,
      "invalid_message", "The editor state sequence is invalid.");
    const parsed = requireState(message.state);
    if (message.sequence > latestSequence) {
      latestSequence = message.sequence;
      snapshot = parsed;
      if (!closing) notify(changes, parsed);
    }
    return parsed;
  }
  function onMessage(event) {
    if (closed) return;
    const message = event.data;
    try {
      requireEnvelope(message, identity);
      if (message.type === "ready") {
        requireCondition(!readySettled && !closing, "invalid_message", "The editor sent a duplicate ready response.");
        runtime = requireRuntimeIdentity(message.runtime);
        requireCondition(!expectedBundleId || runtime.bundleId === expectedBundleId,
          "runtime_mismatch", "The editor bundle differs from expectedBundleId.");
        readState(message);
        window.clearTimeout(readyTimer);
        readySettled = true;
        readyResolve(Object.freeze({ state: snapshot, runtime }));
      } else if (message.type === "state") {
        requireCondition(readySettled, "invalid_message", "The editor sent state before ready.");
        readState(message);
      } else if (message.type === "diagnostic") {
        requireCondition(typeof message.code === "string" && /^[a-z][a-z0-9_]{0,63}$/.test(message.code) &&
          typeof message.message === "string" && message.message.length <= 256,
        "invalid_message", "The editor diagnostic is invalid.");
        if (!closing) notify(diagnostics, Object.freeze({ code: message.code, message: message.message }));
      } else if (message.type === "result") {
        const entry = pending.get(message.requestId);
        if (!entry) return; // Already settled or stale response; never satisfy another instance/request.
        requireCondition(message.operation === entry.operation && typeof message.ok === "boolean",
          "invalid_message", "The editor response operation is invalid.");
        let value;
        let rejection;
        if (message.ok) {
          if (entry.operation === "save") {
            const bytes = requireBuffer(message.bytes);
            requireWorkbookName(message.fileName);
            requireCondition(["xlsx", "xlsm"].includes(message.format) &&
              message.fileName.toLowerCase().endsWith(`.${message.format}`),
            "invalid_message", "The editor save format is invalid.");
            value = Object.freeze({ bytes: new Uint8Array(bytes), fileName: message.fileName,
              format: message.format, mimeType: message.format === "xlsm"
                ? "application/vnd.ms-excel.sheet.macroEnabled.12"
                : "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
              state: readState(message) });
          } else value = entry.operation === "dispose" ? undefined : readState(message);
        } else {
          requireCondition(typeof message.code === "string" && /^[a-z][a-z0-9_]{0,63}$/.test(message.code) &&
            typeof message.message === "string" && message.message.length <= 256,
          "invalid_message", "The editor failure response is invalid.");
          rejection = new EditorEmbedError(message.code, message.message);
        }
        pending.delete(message.requestId);
        window.clearTimeout(entry.timer);
        if (rejection) entry.reject(rejection); else entry.resolve(value);
      } else throw new EditorEmbedError("invalid_message", "The editor response type is invalid.");
    } catch (error) { fail(error); }
  }
  function request(operation, data = {}, transfer = []) {
    requireCondition(!closed && !closing, "disposed", "The editor instance has been disposed.");
    requireCondition(runtime && readySettled, "not_ready", "Await editor.ready before submitting an operation.");
    requireCondition(pending.size === 0, "busy", "Wait for the current editor operation to finish.");
    return dispatch(operation, data, transfer, OPERATION_TIMEOUT_MS);
  }
  function dispatch(operation, data, transfer, timeout) {
    const requestId = nextRequest++;
    requireCondition(Number.isSafeInteger(requestId), "request_limit", "Recreate the editor instance.");
    return new Promise((resolve, reject) => {
      const timer = window.setTimeout(() => {
        fail(new EditorEmbedError(operation === "dispose" ? "dispose_timeout" : "operation_timeout",
          "The editor operation did not finish in time."));
      }, timeout);
      pending.set(requestId, { operation, resolve, reject, timer });
      try { port.postMessage(envelope(identity, "request", { ...data, operation, requestId }), transfer); }
      catch { fail(new EditorEmbedError("invalid_message", "The editor request could not be cloned.")); }
    });
  }
  window.addEventListener("message", onBootstrap);
  frame.addEventListener("load", onFrameLoad);
  container.append(frame);
  return Object.freeze({
    instanceId: identity.instanceId,
    ready,
    getState: () => snapshot,
    getRuntimeIdentity: () => runtime,
    onChange(listener) {
      requireCondition(typeof listener === "function" && !closed && !closing,
        "invalid_listener", "Use a callback on a live editor.");
      requireCondition(changes.size < 32, "listener_limit", "Keep at most 32 change subscribers per editor.");
      changes.add(listener);
      return () => changes.delete(listener);
    },
    onDiagnostic(listener) {
      requireCondition(typeof listener === "function" && !closed && !closing,
        "invalid_listener", "Use a callback on a live editor.");
      requireCondition(diagnostics.size < 32, "listener_limit", "Keep at most 32 diagnostic subscribers per editor.");
      diagnostics.add(listener);
      return () => diagnostics.delete(listener);
    },
    async load(bytes, { fileName, replace = "reject" } = {}) {
      requireWorkbookName(fileName);
      requireCondition(["reject", "discard"].includes(replace), "invalid_option", "Choose reject or discard replacement.");
      // There is no queue: await ready externally, then reserve a slot before any binary copy.
      requireCondition(bytes instanceof Uint8Array && bytes.byteLength > 0 && bytes.byteLength <= 32 * 1024 * 1024,
        "invalid_bytes", "Pass between 1 byte and 32 MiB as Uint8Array.");
      requireCondition(!closed && !closing, "disposed", "The editor instance has been disposed.");
      requireCondition(runtime && readySettled, "not_ready", "Await editor.ready before submitting an operation.");
      requireCondition(pending.size === 0, "busy", "Wait for the current editor operation to finish.");
      const owned = copyWorkbook(bytes);
      return dispatch("load", { bytes: owned, fileName, replace }, [owned], OPERATION_TIMEOUT_MS);
    },
    save: async () => request("save"),
    dispose() {
      if (disposal) return disposal;
      if (closed) return Promise.resolve();
      disposal = (async () => {
        closing = true;
        const disposed = new EditorEmbedError("disposed", "The editor instance has been disposed.");
        rejectPending(disposed);
        if (!readySettled) { readySettled = true; readyReject(disposed); }
        try {
          if (port) await dispatch("dispose", {}, [], DISPOSE_TIMEOUT_MS);
        } finally {
          closed = true;
          cleanup();
        }
      })();
      return disposal;
    },
  });
}
