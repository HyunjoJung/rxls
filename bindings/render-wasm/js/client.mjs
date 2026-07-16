import {
  MAX_INPUT_BYTES,
  MAX_PENDING_REQUESTS,
  MAX_PENDING_RESOURCE_BYTES,
  PROTOCOL,
  RenderProtocolError,
  asBytes,
  fontPackByteLength,
  limitError,
  validateFontPack
} from "./protocol.mjs";

export class RenderWorkerClient {
  #worker;
  #pending = new Map();
  #nextRequest = 1;
  #nextDocument = 1;
  #closed = false;
  #terminalError = null;
  #ready = false;
  #outbox = [];
  #pendingResourceBytes = 0;

  constructor(workerOrUrl, { WorkerClass = globalThis.Worker } = {}) {
    if (workerOrUrl && typeof workerOrUrl.postMessage === "function") {
      this.#worker = workerOrUrl;
    } else {
      if (typeof WorkerClass !== "function") {
        throw new TypeError("Worker is unavailable; pass a Worker-compatible instance");
      }
      this.#worker = new WorkerClass(workerOrUrl, {
        type: "module",
        name: "rxls-render-worker"
      });
    }
    this.#worker.addEventListener("message", (event) => this.#receive(event.data));
    this.#worker.addEventListener("error", (event) => {
      this.#closeWithError(
        new RenderProtocolError("worker_crashed", event.message || "render worker crashed")
      );
    });
    this.#worker.addEventListener("messageerror", () => {
      this.#failAll(
        new RenderProtocolError("worker_message_error", "render worker message was not cloneable")
      );
    });
  }

  capabilities(options = {}) {
    return this.request("capabilities", {}, options);
  }

  open(bytes, { documentId, fontPack, ...requestOptions } = {}) {
    this.#assertAllocationCapacity(0);
    const input = asBytes(bytes, "bytes");
    if (input.byteLength > MAX_INPUT_BYTES) {
      throw limitError("inputBytes", MAX_INPUT_BYTES, input.byteLength, "bytes");
    }
    const resourceBytes = input.byteLength + fontPackByteLength(fontPack);
    this.#assertAllocationCapacity(resourceBytes);
    const copiedFontPack = copyFontPack(fontPack);
    const workbook = input.slice();
    const id = documentId ?? `document-${this.#nextDocument++}`;
    const transfer = [workbook.buffer];
    if (copiedFontPack) {
      transfer.push(copiedFontPack.manifest.buffer);
      for (const member of copiedFontPack.members) {
        transfer.push(member.bytes.buffer);
      }
    }
    return this.request(
      "open",
      { documentId: id, bytes: workbook, fontPack: copiedFontPack },
      { ...requestOptions, transfer, resourceBytes }
    );
  }

  closeDocument(documentId, options = {}) {
    return this.request("close", { documentId }, options);
  }

  preparePages(documentId, sheetIndex, renderOptions = {}, requestOptions = {}) {
    return this.request(
      "prepare-pages",
      { documentId, sheetIndex, options: renderOptions },
      requestOptions
    );
  }

  renderSheet(documentId, sheetIndex, renderOptions = {}, requestOptions = {}) {
    return this.request(
      "render-sheet",
      { documentId, sheetIndex, options: renderOptions },
      requestOptions
    );
  }

  renderTile(documentId, sheetIndex, range, renderOptions = {}, requestOptions = {}) {
    return this.request(
      "render-tile",
      { documentId, sheetIndex, range, options: renderOptions },
      requestOptions
    );
  }

  renderPage(documentId, sheetIndex, pageIndex, renderOptions = {}, requestOptions = {}) {
    return this.request(
      "render-page",
      { documentId, sheetIndex, pageIndex, options: renderOptions },
      requestOptions
    );
  }

  renderPagePng(
    documentId,
    sheetIndex,
    pageIndex,
    dpi = 96,
    renderOptions = {},
    requestOptions = {}
  ) {
    return this.request(
      "render-page-png",
      { documentId, sheetIndex, pageIndex, dpi, options: renderOptions },
      requestOptions
    );
  }

  request(operation, payload, { signal, onProgress, transfer = [], resourceBytes = 0 } = {}) {
    if (this.#closed) {
      return Promise.reject(this.#closedError());
    }
    if (this.#pending.size >= MAX_PENDING_REQUESTS) {
      return Promise.reject(
        limitError(
          "pendingRequests",
          MAX_PENDING_REQUESTS,
          this.#pending.size + 1,
          "client"
        )
      );
    }
    validateResourceBytes(resourceBytes);
    if (this.#pendingResourceBytes + resourceBytes > MAX_PENDING_RESOURCE_BYTES) {
      return Promise.reject(
        limitError(
          "pendingResourceBytes",
          MAX_PENDING_RESOURCE_BYTES,
          this.#pendingResourceBytes + resourceBytes,
          "client"
        )
      );
    }
    const requestId = `request-${this.#nextRequest++}`;
    let abortListener;
    const promise = new Promise((resolve, reject) => {
      if (signal?.aborted) {
        reject(abortError());
        return;
      }
      abortListener = () => {
        if (this.#ready) {
          this.#sendCancel(requestId);
        } else {
          this.#removeFromOutbox(requestId);
        }
        this.#pendingResourceBytes -= resourceBytes;
        this.#pending.delete(requestId);
        reject(abortError());
      };
      signal?.addEventListener("abort", abortListener, { once: true });
      this.#pendingResourceBytes += resourceBytes;
      this.#pending.set(requestId, {
        resolve,
        reject,
        onProgress,
        signal,
        abortListener,
        resourceBytes
      });
      const row = {
        message: { protocol: PROTOCOL, type: "request", requestId, operation, payload },
        transfer
      };
      if (this.#ready) {
        this.#dispatch(row);
      } else {
        this.#outbox.push(row);
      }
    });
    Object.defineProperty(promise, "requestId", { value: requestId });
    return promise;
  }

  cancel(requestId) {
    const pending = this.#pending.get(requestId);
    if (!pending) {
      return false;
    }
    if (this.#ready) {
      this.#sendCancel(requestId);
    } else {
      this.#removeFromOutbox(requestId);
    }
    this.#pendingResourceBytes -= pending.resourceBytes;
    this.#pending.delete(requestId);
    pending.signal?.removeEventListener("abort", pending.abortListener);
    pending.reject(abortError());
    return true;
  }

  terminate() {
    this.#closeWithError(
      new RenderProtocolError("client_closed", "render worker was terminated")
    );
  }

  #receive(message) {
    if (this.#closed) {
      return;
    }
    if (message?.protocol !== PROTOCOL) {
      return;
    }
    if (message.type === "ready") {
      this.#ready = true;
      for (const row of this.#outbox.splice(0)) {
        this.#dispatch(row);
      }
      return;
    }
    if (typeof message.requestId !== "string") {
      return;
    }
    const pending = this.#pending.get(message.requestId);
    if (!pending) {
      return;
    }
    if (message.type === "progress") {
      pending.onProgress?.({
        completed: message.completed,
        total: message.total,
        stage: message.stage
      });
      return;
    }
    if (message.type !== "result") {
      return;
    }
    this.#pending.delete(message.requestId);
    this.#pendingResourceBytes -= pending.resourceBytes;
    pending.signal?.removeEventListener("abort", pending.abortListener);
    if (message.ok) {
      pending.resolve(message.result);
      return;
    }
    const error = new RenderProtocolError(
      message.error?.code ?? "worker_failed",
      message.error?.message ?? "render worker request failed",
      message.error?.location ?? "worker",
      message.error ?? {}
    );
    pending.reject(error);
  }

  #failAll(error) {
    for (const pending of this.#pending.values()) {
      pending.signal?.removeEventListener("abort", pending.abortListener);
      pending.reject(error);
    }
    this.#pending.clear();
    this.#outbox = [];
    this.#pendingResourceBytes = 0;
  }

  #closeWithError(error) {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    this.#terminalError = error;
    this.#ready = false;
    this.#worker.terminate?.();
    this.#failAll(error);
  }

  #closedError() {
    return this.#terminalError ?? new RenderProtocolError("client_closed", "client is closed");
  }

  #removeFromOutbox(requestId) {
    this.#outbox = this.#outbox.filter((row) => row.message.requestId !== requestId);
  }

  #dispatch(row) {
    try {
      this.#worker.postMessage(row.message, row.transfer);
    } catch {
      const pending = this.#pending.get(row.message.requestId);
      if (!pending) {
        return;
      }
      this.#pending.delete(row.message.requestId);
      this.#pendingResourceBytes -= pending.resourceBytes;
      pending.signal?.removeEventListener("abort", pending.abortListener);
      pending.reject(
        new RenderProtocolError(
          "worker_message_error",
          "render worker request could not be cloned"
        )
      );
    }
  }

  #sendCancel(requestId) {
    try {
      this.#worker.postMessage({ protocol: PROTOCOL, type: "cancel", requestId });
    } catch {
      // The local promise is still cancelled and all retained resources are released.
    }
  }

  #assertAllocationCapacity(resourceBytes) {
    if (this.#closed) {
      throw this.#closedError();
    }
    if (this.#pending.size >= MAX_PENDING_REQUESTS) {
      throw limitError(
        "pendingRequests",
        MAX_PENDING_REQUESTS,
        this.#pending.size + 1,
        "client"
      );
    }
    if (this.#pendingResourceBytes + resourceBytes > MAX_PENDING_RESOURCE_BYTES) {
      throw limitError(
        "pendingResourceBytes",
        MAX_PENDING_RESOURCE_BYTES,
        this.#pendingResourceBytes + resourceBytes,
        "client"
      );
    }
  }
}

export function getRenderWorkerUrl() {
  return new URL("./worker.mjs", import.meta.url);
}

export function createRenderWorkerClient(workerUrl = getRenderWorkerUrl(), options) {
  return new RenderWorkerClient(workerUrl, options);
}

function copyFontPack(fontPack) {
  if (fontPack === undefined || fontPack === null) {
    return undefined;
  }
  const validated = validateFontPack(fontPack);
  return {
    manifest: validated.manifest.slice(),
    members: validated.members.map((member, index) => ({
      name: fontPack.members[index].name,
      bytes: member.bytes.slice()
    }))
  };
}

function abortError() {
  if (typeof DOMException === "function") {
    return new DOMException("render request was cancelled", "AbortError");
  }
  const error = new Error("render request was cancelled");
  error.name = "AbortError";
  return error;
}

function validateResourceBytes(value) {
  if (!Number.isSafeInteger(value) || value < 0 || value > MAX_PENDING_RESOURCE_BYTES) {
    throw new RenderProtocolError(
      "invalid_resource_bytes",
      "resourceBytes must be a non-negative safe integer within the pending byte limit",
      "client"
    );
  }
}
