import test from "node:test";
import assert from "node:assert/strict";

import {
  MAX_INPUT_BYTES,
  MAX_PAGES,
  MAX_PENDING_REQUESTS,
  PROTOCOL
} from "../js/protocol.mjs";
import { RenderWorkerRuntime } from "../js/worker-runtime.mjs";

class FakeSession {
  static calls = [];
  static sheetSvg = '<?xml version="1.0"?><svg><title>S</title></svg>';
  static pageGate = null;

  constructor(bytes, fontBundle) {
    FakeSession.calls.push(["open", bytes.byteLength, fontBundle.byteLength]);
  }

  inspectionJson() {
    return JSON.stringify({ schemaVersion: 1, sheetCount: 1, sheets: [{ index: 0, name: "S" }] });
  }

  printManifestJson(sheetIndex, options) {
    FakeSession.calls.push(["prepare", sheetIndex, options]);
    return JSON.stringify({ schema_version: 1, pages: [{ output_index: 0 }] });
  }

  renderSheetSvg(sheetIndex, options) {
    FakeSession.calls.push(["sheet", sheetIndex, options]);
    return FakeSession.sheetSvg;
  }

  renderTileSvg(sheetIndex, firstRow, firstCol, lastRow, lastCol, options) {
    FakeSession.calls.push([
      "tile",
      sheetIndex,
      firstRow,
      firstCol,
      lastRow,
      lastCol,
      options
    ]);
    return '<svg data-kind="tile"></svg>';
  }

  renderPrintPageSvg(sheetIndex, pageIndex, options) {
    FakeSession.calls.push(["page", sheetIndex, pageIndex, options]);
    if (FakeSession.pageGate) {
      return FakeSession.pageGate;
    }
    return `<svg data-page="${pageIndex}"></svg>`;
  }

  renderPrintPagePng(sheetIndex, pageIndex, dpi, options) {
    FakeSession.calls.push(["png", sheetIndex, pageIndex, dpi, options]);
    return Uint8Array.of(137, 80, 78, 71, 13, 10, 26, 10);
  }

  free() {
    FakeSession.calls.push(["free"]);
  }
}

const fakeWasm = {
  RenderSession: FakeSession,
  capabilitiesJson() {
    return JSON.stringify({
      limits: { maxOutputBytes: 1024 * 1024, maxPngBytes: 1024 * 1024 }
    });
  }
};

function harness() {
  const messages = [];
  const runtime = new RenderWorkerRuntime({
    wasm: fakeWasm,
    send(message, transfer = []) {
      messages.push({ message, transfer });
    }
  });
  return { runtime, messages };
}

function request(requestId, operation, payload = {}) {
  return { protocol: PROTOCOL, type: "request", requestId, operation, payload };
}

async function settle(turns = 8) {
  for (let index = 0; index < turns; index += 1) {
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
}

function result(messages, requestId) {
  return messages.find(
    ({ message }) => message.type === "result" && message.requestId === requestId
  )?.message;
}

test("worker opens once and virtualizes only the requested tile and page", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();
  runtime.receive(
    request("open-1", "open", {
      documentId: "doc-1",
      bytes: Uint8Array.of(80, 75, 3, 4)
    })
  );
  await settle();
  assert.equal(result(messages, "open-1").ok, true);

  runtime.receive(
    request("tile-1", "render-tile", {
      documentId: "doc-1",
      sheetIndex: 0,
      range: { firstRow: 10, firstCol: 2, lastRow: 19, lastCol: 7 },
      options: { gridlines: false }
    })
  );
  runtime.receive(
    request("page-7", "render-page", {
      documentId: "doc-1",
      sheetIndex: 0,
      pageIndex: 7
    })
  );
  await settle();
  assert.equal(result(messages, "tile-1").result.range.firstRow, 10);
  assert.match(result(messages, "page-7").result.svg, /data-page="7"/);
  assert.deepEqual(
    FakeSession.calls.filter(([kind]) => kind === "page"),
    [["page", 0, 7, "{}"]]
  );
  assert.equal(FakeSession.calls.filter(([kind]) => kind === "open").length, 1);
  assert.deepEqual(
    messages
      .filter(({ message }) => message.type === "progress" && message.requestId === "tile-1")
      .map(({ message }) => [message.completed, message.total, message.stage]),
    [
      [0, 3, "accepted"],
      [1, 3, "rendering"],
      [2, 3, "finalizing"],
      [3, 3, "complete"]
    ]
  );
});

test("queued cancellation prevents wasm work and returns a typed result", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();
  runtime.receive(
    request("cancel-me", "open", {
      documentId: "doc-cancelled",
      bytes: Uint8Array.of(1)
    })
  );
  runtime.receive({ protocol: PROTOCOL, type: "cancel", requestId: "cancel-me" });
  await settle();
  const cancelled = result(messages, "cancel-me");
  assert.equal(cancelled.ok, false);
  assert.equal(cancelled.error.code, "cancelled");
  assert.equal(FakeSession.calls.length, 0);
});

test("worker rejects input and grid limits before wasm", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();
  runtime.receive(
    request("too-large", "open", {
      documentId: "doc-large",
      bytes: new Uint8Array(MAX_INPUT_BYTES + 1)
    })
  );
  await settle();
  assert.equal(result(messages, "too-large").error.resource, "inputBytes");
  assert.equal(FakeSession.calls.length, 0);

  runtime.receive(
    request("open-good", "open", {
      documentId: "doc-good",
      bytes: Uint8Array.of(1)
    })
  );
  await settle();
  runtime.receive(
    request("bad-range", "render-tile", {
      documentId: "doc-good",
      sheetIndex: 0,
      range: { firstRow: 0, firstCol: 0, lastRow: 1_048_576, lastCol: 0 }
    })
  );
  await settle();
  assert.equal(result(messages, "bad-range").error.code, "range_outside_grid");
  assert.equal(FakeSession.calls.filter(([kind]) => kind === "tile").length, 0);
});

test("PNG pages transfer one independent buffer", async () => {
  const { runtime, messages } = harness();
  runtime.receive(
    request("open-png", "open", {
      documentId: "doc-png",
      bytes: Uint8Array.of(1)
    })
  );
  await settle();
  runtime.receive(
    request("png-1", "render-page-png", {
      documentId: "doc-png",
      sheetIndex: 0,
      pageIndex: 1,
      dpi: 144
    })
  );
  await settle();
  const row = messages.find(({ message }) => message.requestId === "png-1" && message.ok);
  assert.deepEqual([...row.message.result.bytes], [137, 80, 78, 71, 13, 10, 26, 10]);
  assert.equal(row.transfer.length, 1);
  assert.equal(row.transfer[0], row.message.result.bytes.buffer);
});

test("unknown cancellations do not accumulate or poison a later request id", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();
  for (let index = 0; index < MAX_PENDING_REQUESTS * 4; index += 1) {
    runtime.receive({ protocol: PROTOCOL, type: "cancel", requestId: `unknown-${index}` });
  }
  runtime.receive(
    request("unknown-0", "open", { documentId: "doc-after-cancel", bytes: Uint8Array.of(1) })
  );
  await settle();
  assert.equal(result(messages, "unknown-0").ok, true);
  assert.equal(FakeSession.calls.filter(([kind]) => kind === "open").length, 1);
});

test("worker caps queued requests and rejects impossible page indexes before wasm", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();
  for (let index = 0; index <= MAX_PENDING_REQUESTS; index += 1) {
    runtime.receive(request(`queued-${index}`, "capabilities"));
  }
  await settle(MAX_PENDING_REQUESTS + 4);
  assert.equal(result(messages, `queued-${MAX_PENDING_REQUESTS}`).error.resource, "pendingRequests");
  assert.equal(
    messages.filter(({ message }) => message.type === "result" && message.ok).length,
    MAX_PENDING_REQUESTS
  );

  runtime.receive(
    request("open-index", "open", { documentId: "doc-index", bytes: Uint8Array.of(1) })
  );
  await settle();
  runtime.receive(
    request("bad-page", "render-page", {
      documentId: "doc-index",
      sheetIndex: 0,
      pageIndex: MAX_PAGES
    })
  );
  await settle();
  assert.equal(result(messages, "bad-page").error.resource, "pages");
  assert.equal(FakeSession.calls.filter(([kind]) => kind === "page").length, 0);
});

test("active asynchronous work observes cancellation before emitting output", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();
  runtime.receive(
    request("open-async", "open", { documentId: "doc-async", bytes: Uint8Array.of(1) })
  );
  await settle();
  let release;
  FakeSession.pageGate = new Promise((resolve) => (release = resolve));
  runtime.receive(
    request("page-async", "render-page", {
      documentId: "doc-async",
      sheetIndex: 0,
      pageIndex: 0
    })
  );
  await new Promise((resolve) => setImmediate(resolve));
  runtime.receive({ protocol: PROTOCOL, type: "cancel", requestId: "page-async" });
  release('<svg data-page="0"></svg>');
  await settle();
  FakeSession.pageGate = null;
  assert.equal(result(messages, "page-async").error.code, "cancelled");
  assert.equal(
    messages.some(({ message }) => message.type === "result" && message.requestId === "page-async" && message.ok),
    false
  );
});

test("worker rejects active or externally-loaded SVG before returning it", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();
  runtime.receive(
    request("open-svg", "open", { documentId: "doc-svg", bytes: Uint8Array.of(1) })
  );
  await settle();
  for (const [requestId, svg] of [
    ["script-svg", "<svg><script>bad()</script></svg>"],
    ["remote-svg", '<svg><image href="https://example.com/a.png"/></svg>']
  ]) {
    FakeSession.sheetSvg = svg;
    runtime.receive(
      request(requestId, "render-sheet", { documentId: "doc-svg", sheetIndex: 0 })
    );
    await settle();
    assert.match(result(messages, requestId).error.code, /unsafe_svg|external_svg_resource/);
  }
  FakeSession.sheetSvg = '<?xml version="1.0"?><svg><title>S</title></svg>';
});

test("worker rejects unbounded payload fields and caps queued transferable bytes", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();
  runtime.receive({
    protocol: PROTOCOL,
    type: "request",
    requestId: "/private/invalid",
    operation: "capabilities",
    payload: {}
  });
  assert.equal(result(messages, "invalid").error.code, "invalid_request_id");

  runtime.receive(
    request("unknown-field", "render-sheet", {
      documentId: "doc",
      sheetIndex: 0,
      options: {},
      retainedJunk: "not allowed"
    })
  );
  assert.equal(result(messages, "unknown-field").error.code, "invalid_payload");

  const sharedMaximumInput = new Uint8Array(MAX_INPUT_BYTES);
  for (let index = 0; index < 5; index += 1) {
    runtime.receive(
      request(`resource-${index}`, "open", {
        documentId: `resource-doc-${index}`,
        bytes: sharedMaximumInput
      })
    );
  }
  assert.equal(result(messages, "resource-4").error.resource, "pendingResourceBytes");
  await settle();
  assert.equal(
    FakeSession.calls.filter(([kind]) => kind === "open").length,
    4
  );
});
