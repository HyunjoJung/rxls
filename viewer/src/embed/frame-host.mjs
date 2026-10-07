import {
  READY_TIMEOUT_MS, envelope, requireEnvelope, requireRequestId,
  requireCondition, requireWorkbookName, requireBuffer, requireState,
  requireRuntimeIdentity, parseBootstrap, sameIdentity, publicError,
} from "./protocol.mjs";

/** Own the iframe protocol; all workbook/draft/worker operations remain viewer callbacks. */
export function createEmbedHost({ window, runtime, getState, load, save, dispose }) {
  const { identity, parentOrigin } = parseBootstrap(window.location);
  requireCondition(window.parent !== window, "invalid_bootstrap", "Open the editor inside its parent iframe.");
  requireRuntimeIdentity(runtime);
  let port = null;
  let closed = false;
  let inFlight = false;
  let lastRequest = 0;
  let sequence = 0;
  let publishQueued = false;
  let previousState = "";
  let viewerDisposed = false;
  const timer = window.setTimeout(() => stop(), READY_TIMEOUT_MS);

  function stateData() {
    const state = requireState(getState());
    const text = JSON.stringify(state);
    if (text !== previousState) { previousState = text; sequence += 1; }
    return { state, sequence };
  }
  function post(type, data = {}, transfer = []) {
    if (!closed && port) port.postMessage(envelope(identity, type, data), transfer);
  }
  function stop() {
    if (closed) return;
    closed = true;
    window.clearTimeout(timer);
    window.removeEventListener("message", connect);
    window.removeEventListener("pagehide", stop);
    port?.close();
    port = null;
    releaseViewer();
  }
  function releaseViewer() {
    if (viewerDisposed) return;
    viewerDisposed = true;
    dispose();
  }
  function connect(event) {
    if (closed || port || event.source !== window.parent || event.origin !== parentOrigin ||
      event.data?.type !== "connect" || !sameIdentity(event.data, identity)) return;
    try {
      requireEnvelope(event.data, identity);
      requireCondition(event.ports.length === 1, "invalid_message", "Use one owned editor MessagePort.");
      port = event.ports[0];
      port.onmessage = (event) => { void receive(event.data); };
      port.onmessageerror = () => stop();
      port.start();
      window.clearTimeout(timer);
      window.removeEventListener("message", connect);
      post("ready", { runtime, ...stateData() });
    } catch { stop(); }
  }
  async function receive(message) {
    if (closed) return;
    let accepted = false;
    let validRequest = false;
    try {
      requireEnvelope(message, identity);
      requireCondition(message.type === "request", "invalid_message", "Use an editor request envelope.");
      requireRequestId(message.requestId);
      requireCondition(message.requestId > lastRequest, "invalid_message", "The editor request is stale.");
      lastRequest = message.requestId;
      requireCondition(["load", "save", "dispose"].includes(message.operation),
        "invalid_message", "The editor operation is invalid.");
      validRequest = true;
      if (message.operation === "dispose") {
        // Dispose is allowed during a pending load/save; viewer invalidates generations synchronously.
        releaseViewer();
        post("result", { requestId: message.requestId, operation: "dispose", ok: true });
        stop();
        return;
      }
      requireCondition(!inFlight, "busy", "Wait for the current editor operation to finish.");
      inFlight = true;
      accepted = true;
      let saved;
      if (message.operation === "load") {
        requireBuffer(message.bytes);
        requireWorkbookName(message.fileName);
        requireCondition(["reject", "discard"].includes(message.replace),
          "invalid_option", "Choose reject or discard replacement.");
        await load(new Uint8Array(message.bytes), { fileName: message.fileName, replace: message.replace });
      } else {
        saved = await save();
        requireCondition(saved?.bytes instanceof Uint8Array, "invalid_bytes", "The editor save returned invalid bytes.");
        requireCondition(saved.bytes.byteLength > 0 && saved.bytes.byteLength <= 32 * 1024 * 1024,
          "invalid_bytes", "The editor save returned invalid bytes.");
        requireWorkbookName(saved.fileName);
        requireCondition(["xlsx", "xlsm"].includes(saved.format), "invalid_message", "The editor save format is invalid.");
      }
      if (closed) return;
      if (saved) {
        const owned = saved.bytes.byteOffset === 0 && saved.bytes.byteLength === saved.bytes.buffer.byteLength
          ? saved.bytes.buffer : new Uint8Array(saved.bytes).buffer;
        post("result", { requestId: message.requestId, operation: "save", ok: true,
          bytes: owned, fileName: saved.fileName, format: saved.format, ...stateData() }, [owned]);
      } else post("result", { requestId: message.requestId, operation: "load", ok: true, ...stateData() });
    } catch (error) {
      if (closed) return;
      if (validRequest && ["load", "save"].includes(message.operation)) {
        post("result", { requestId: message.requestId, operation: message.operation, ok: false, ...publicError(error) });
      } else stop();
    } finally {
      if (accepted) inFlight = false;
    }
  }
  window.addEventListener("message", connect);
  window.addEventListener("pagehide", stop, { once: true });
  window.parent.postMessage(envelope(identity, "boot"), parentOrigin);
  return Object.freeze({
    publishState() {
      if (closed || !port || publishQueued) return;
      publishQueued = true;
      window.queueMicrotask(() => {
        publishQueued = false;
        if (!closed && port) post("state", stateData());
      });
    },
    diagnostic(error) { if (!closed) post("diagnostic", publicError(error)); },
    stop,
  });
}
