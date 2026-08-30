import test from "node:test";
import assert from "node:assert/strict";

import { RenderWorkerClient, getRenderWorkerUrl } from "../js/client.mjs";
import {
  MAX_FONT_FILES,
  MAX_INPUT_BYTES,
  MAX_MANUAL_PAGE_BREAKS,
  MAX_PENDING_REQUESTS,
  MAX_PENDING_RESOURCE_BYTES,
  PROTOCOL
} from "../js/protocol.mjs";
import { EXPECTED_CAPABILITIES } from "./browser/contract.mjs";

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

function emitReady(worker, capabilities = structuredClone(EXPECTED_CAPABILITIES)) {
  worker.emit({ protocol: PROTOCOL, type: "ready", capabilities });
}

function fakeWorkbook() {
  return {
    schemaVersion: 1,
    sheetCount: 1,
    sheets: [{ index: 0, name: "Sheet1", embeddedImages: 0 }],
    embeddedImages: 0,
    embeddedImageBytes: 0,
    fontPackSha256: null,
    fontFaces: 0,
    properties: {
      title: null,
      subject: null,
      creator: null,
      keywords: null,
      description: null,
      lastModifiedBy: null,
      company: null,
      created: null
    }
  };
}

function fakeEditState(overrides = {}) {
  return {
    schemaVersion: 1,
    capability: "read-write",
    reason: null,
    dirty: false,
    canUndo: false,
    canRedo: false,
    undoDepth: 0,
    redoDepth: 0,
    historyBytes: 0,
    editedParts: [],
    ...overrides
  };
}

function fakeOpenResult(documentId) {
  return {
    documentId,
    workbook: fakeWorkbook(),
    editState: fakeEditState()
  };
}

function fakePrintManifest(sheetIndex = 0) {
  const sourceReport = {
    schema_version: 2,
    sheet_index: sheetIndex,
    sheet_name: "Sheet1",
    range: { first_row: 0, first_col: 0, last_row: 0, last_col: 0 },
    rows_considered: 1,
    columns_considered: 1,
    cells_considered: 1,
    visible_rows: 1,
    visible_columns: 1,
    rendered_regions: 1,
    hidden_rows_skipped: 0,
    hidden_columns_skipped: 0,
    merged_regions: 0,
    text_bytes: 0,
    glyphs: 0,
    scene_nodes: 1,
    svg_bytes: 0,
    font_pack_sha256: null,
    font_faces: [],
    warnings: []
  };
  return {
    schema_version: 2,
    sheet_index: sheetIndex,
    sheet_name: "Sheet1",
    source_report: sourceReport,
    source_reports: [structuredClone(sourceReport)],
    paper: { code: 1, width_raw: 1, height_raw: 1 },
    content_rect: { x_raw: 0, y_raw: 0, width_raw: 1, height_raw: 1 },
    page_order: "down_then_over",
    manual_row_breaks: [],
    manual_col_breaks: [],
    scale_permille: 0,
    logical_pages: 1,
    sparse_pages_omitted: 0,
    pages: [
      {
        output_index: 0,
        displayed_page_number: 1,
        area_index: 0,
        horizontal_index: 0,
        vertical_index: 0,
        manual_col_break_before: false,
        manual_row_break_before: false,
        body_range: { first_row: 0, first_col: 0, last_row: 0, last_col: 0 },
        repeat_rows: null,
        repeat_cols: null,
        scale_permille: 1_000
      }
    ],
    warnings: []
  };
}

function emitSuccess(worker, pending, result) {
  worker.emit({
    protocol: PROTOCOL,
    type: "result",
    requestId: pending.requestId,
    ok: true,
    result,
    error: null
  });
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
  emitReady(worker);
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
  const result = fakeOpenResult("document-1");
  emitSuccess(worker, pending, result);
  assert.deepEqual(await pending, result);
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

  emitReady(worker);
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
  emitReady(worker);
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
  emitReady(worker);
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
  emitReady(worker);
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
  const result = fakeOpenResult("clone-recovery");
  emitSuccess(worker, pending, result);
  assert.deepEqual(await pending, result);
});

test("dispatched state mutations ignore local cancellation and resolve authoritatively", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  emitReady(worker);
  const controller = new AbortController();
  const pending = client.setCell(
    "doc",
    0,
    4,
    2,
    { kind: "text", value: "after" },
    { signal: controller.signal }
  );

  controller.abort();
  assert.equal(client.cancel(pending.requestId), false);
  assert.equal(worker.sent.length, 1);
  assert.equal(worker.sent[0].message.operation, "set-cell");

  const result = {
    documentId: "doc",
    workbook: fakeWorkbook(),
    editState: fakeEditState({
      dirty: true,
      canUndo: true,
      undoDepth: 1,
      historyBytes: 4,
      editedParts: ["xl/worksheets/sheet1.xml"]
    })
  };
  emitSuccess(worker, pending, result);
  assert.deepEqual(await pending, result);
});

test("queued state mutations remain cancellable before worker readiness", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  const controller = new AbortController();
  const pending = client.setCell(
    "doc",
    0,
    0,
    0,
    { kind: "blank" },
    { signal: controller.signal }
  );

  controller.abort();
  await assert.rejects(pending, (error) => error.name === "AbortError");
  emitReady(worker);
  assert.equal(worker.sent.length, 0);
});

test("queued requests isolate nested payloads from later caller mutation", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  const range = { firstRow: 0, firstCol: 1, lastRow: 4, lastCol: 3 };
  const options = { gridlines: true, limits: { maxPages: 2 } };
  const pending = client.renderTile("doc", 0, range, options);

  range.firstRow = 3;
  options.gridlines = false;
  options.limits.maxPages = 9;
  emitReady(worker);

  assert.deepEqual(worker.sent[0].message.payload.range, {
    firstRow: 0,
    firstCol: 1,
    lastRow: 4,
    lastCol: 3
  });
  assert.deepEqual(worker.sent[0].message.payload.options, {
    gridlines: true,
    limits: { maxPages: 2 }
  });
  const result = {
    documentId: "doc",
    sheetIndex: 0,
    range: { firstRow: 0, firstCol: 1, lastRow: 4, lastCol: 3 },
    mimeType: "image/svg+xml",
    svg: "<svg></svg>"
  };
  emitSuccess(worker, pending, result);
  assert.deepEqual(await pending, result);
});

test("prepare-pages results require the current print manifest schema", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  emitReady(worker);
  const pending = client.preparePages("doc", 0);
  const result = {
    documentId: "doc",
    sheetIndex: 0,
    manifest: fakePrintManifest()
  };
  result.manifest.manual_row_breaks = Array.from(
    { length: MAX_MANUAL_PAGE_BREAKS },
    (_, index) => index
  );
  emitSuccess(worker, pending, result);
  assert.deepEqual(await pending, result);

  const malformedWorker = new FakeWorker();
  const malformedClient = new RenderWorkerClient(malformedWorker);
  emitReady(malformedWorker);
  const malformed = malformedClient.preparePages("doc", 0);
  emitSuccess(malformedWorker, malformed, {
    ...result,
    manifest: { ...result.manifest, schema_version: 1 }
  });
  await assert.rejects(malformed, (error) => error.code === "worker_message_error");
  assert.equal(malformedWorker.terminated, true);
});

test("saved workbook results require an OOXML ZIP header", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  emitReady(worker);
  const pending = client.saveDocument("doc");
  emitSuccess(worker, pending, {
    documentId: "doc",
    mimeType: "application/octet-stream",
    bytes: new Uint8Array()
  });
  await assert.rejects(pending, (error) => error.code === "worker_message_error");
  assert.equal(worker.terminated, true);
});

test("responses before worker readiness fail closed", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  const pending = client.capabilities();

  emitSuccess(worker, pending, structuredClone(EXPECTED_CAPABILITIES));

  await assert.rejects(pending, (error) => error.code === "worker_message_error");
  assert.equal(worker.terminated, true);
  assert.equal(worker.sent.length, 0);
});

test("protocol mismatches close the client instead of leaving queued work unresolved", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  const pending = client.capabilities();

  worker.emit({
    protocol: "rxls.render-worker.v1",
    type: "ready",
    capabilities: structuredClone(EXPECTED_CAPABILITIES)
  });

  await assert.rejects(pending, (error) => error.code === "worker_message_error");
  assert.equal(worker.terminated, true);
  await assert.rejects(
    client.capabilities(),
    (error) => error.code === "worker_message_error"
  );
});

test("malformed worker results fail closed before they reach callers", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  emitReady(worker);
  const pending = client.open(Uint8Array.of(1), { documentId: "malformed" });

  emitSuccess(worker, pending, {
    documentId: "malformed",
    workbook: { schemaVersion: 1 },
    editState: fakeEditState()
  });

  await assert.rejects(
    pending,
    (error) => error.code === "worker_message_error" && error.location === "worker"
  );
  assert.equal(worker.terminated, true);
});

test("SHA-256 response fields require strings instead of coercible values", async () => {
  const workbookWorker = new FakeWorker();
  const workbookClient = new RenderWorkerClient(workbookWorker);
  emitReady(workbookWorker);
  const open = workbookClient.open(Uint8Array.of(1), { documentId: "hash-workbook" });
  const openResult = fakeOpenResult("hash-workbook");
  openResult.workbook.fontPackSha256 = ["a".repeat(64)];
  emitSuccess(workbookWorker, open, openResult);
  await assert.rejects(open, (error) => error.code === "worker_message_error");

  const manifestWorker = new FakeWorker();
  const manifestClient = new RenderWorkerClient(manifestWorker);
  emitReady(manifestWorker);
  const prepare = manifestClient.preparePages("hash-manifest", 0);
  const manifest = fakePrintManifest();
  manifest.source_report.font_pack_sha256 = ["b".repeat(64)];
  emitSuccess(manifestWorker, prepare, {
    documentId: "hash-manifest",
    sheetIndex: 0,
    manifest
  });
  await assert.rejects(prepare, (error) => error.code === "worker_message_error");
});

test("valid long OpenDocument sheet names are not constrained by Excel limits", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  emitReady(worker);
  const pending = client.open(Uint8Array.of(1), { documentId: "long-ods" });
  const result = fakeOpenResult("long-ods");
  result.workbook.sheets[0].name = "Long OpenDocument sheet ".repeat(32);
  result.editState = fakeEditState({
    capability: "read-only",
    reason: "open-document"
  });
  emitSuccess(worker, pending, result);
  assert.deepEqual(await pending, result);
});

test("non-monotonic progress is treated as a worker protocol failure", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  emitReady(worker);
  const pending = client.open(Uint8Array.of(1));
  worker.emit({
    protocol: PROTOCOL,
    type: "progress",
    requestId: pending.requestId,
    completed: 2,
    total: 3,
    stage: "finalizing"
  });
  worker.emit({
    protocol: PROTOCOL,
    type: "progress",
    requestId: pending.requestId,
    completed: 1,
    total: 3,
    stage: "parsing"
  });

  await assert.rejects(pending, (error) => error.code === "worker_message_error");
  assert.equal(worker.terminated, true);
});

test("generic requests are validated before postMessage and cannot declare accounting", async () => {
  const worker = new FakeWorker();
  const client = new RenderWorkerClient(worker);
  emitReady(worker);

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
