import test from "node:test";
import assert from "node:assert/strict";

import {
  MAX_INPUT_BYTES,
  MAX_MANUAL_PAGE_BREAKS,
  MAX_OPEN_DOCUMENTS,
  MAX_OPEN_RESOURCE_BYTES,
  MAX_PAGES,
  MAX_PENDING_REQUESTS,
  PROTOCOL
} from "../js/protocol.mjs";
import { RenderWorkerRuntime } from "../js/worker-runtime.mjs";

function fakeWorkbook() {
  return {
    schemaVersion: 1,
    sheetCount: 1,
    sheets: [{ index: 0, name: "S", embeddedImages: 0 }],
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

class FakeSession {
  static calls = [];
  static inspectionHook = null;
  static sheetSvg = '<?xml version="1.0"?><svg><title>S</title></svg>';
  static pageGate = null;
  static cellInspection = null;
  static mutationGate = null;
  static workbookInspection = null;
  static editCapability = "read-write";
  static printManifest = null;
  static savedDocument = null;

  constructor(bytes, fontBundle) {
    FakeSession.calls.push(["open", bytes.byteLength, fontBundle.byteLength]);
    this.editState = {
      schemaVersion: 1,
      capability: FakeSession.editCapability,
      reason: FakeSession.editCapability === "read-write" ? null : "legacy-biff",
      dirty: false,
      canUndo: false,
      canRedo: false,
      undoDepth: 0,
      redoDepth: 0,
      historyBytes: 0,
      editedParts: []
    };
  }

  inspectionJson() {
    const inspection = JSON.stringify(FakeSession.workbookInspection ?? fakeWorkbook());
    FakeSession.inspectionHook?.();
    return inspection;
  }

  editStateJson() {
    return JSON.stringify(this.editState);
  }

  readCellJson(sheetIndex, row, col) {
    FakeSession.calls.push(["read-cell", sheetIndex, row, col]);
    return JSON.stringify(
      FakeSession.cellInspection ?? {
        schemaVersion: 1,
        sheetIndex,
        row,
        col,
        value: { kind: "text", value: "before" },
        formatted: "before"
      }
    );
  }

  setCellJson(requestJson) {
    FakeSession.calls.push(["set-cell", JSON.parse(requestJson)]);
    const mutate = () => {
      this.editState = {
        ...this.editState,
        dirty: true,
        canUndo: true,
        undoDepth: 1,
        historyBytes: 4,
        editedParts: ["xl/worksheets/sheet1.xml"]
      };
      return this.mutationJson();
    };
    return FakeSession.mutationGate ? FakeSession.mutationGate.then(mutate) : mutate();
  }

  setDocumentPropertiesJson(requestJson) {
    FakeSession.calls.push(["set-document-properties", JSON.parse(requestJson)]);
    this.editState = {
      ...this.editState,
      dirty: true,
      canUndo: true,
      undoDepth: 1,
      historyBytes: 4,
      editedParts: ["docProps/core.xml"]
    };
    return this.mutationJson();
  }

  undoEditJson() {
    FakeSession.calls.push(["undo-edit"]);
    this.editState = {
      ...this.editState,
      dirty: false,
      canUndo: false,
      canRedo: true,
      undoDepth: 0,
      redoDepth: 1,
      historyBytes: 4,
      editedParts: []
    };
    return this.mutationJson();
  }

  redoEditJson() {
    FakeSession.calls.push(["redo-edit"]);
    this.editState = {
      ...this.editState,
      dirty: true,
      canUndo: true,
      canRedo: false,
      undoDepth: 1,
      redoDepth: 0,
      historyBytes: 4,
      editedParts: ["xl/worksheets/sheet1.xml"]
    };
    return this.mutationJson();
  }

  saveDocumentBytes() {
    FakeSession.calls.push(["save-document"]);
    return FakeSession.savedDocument ?? Uint8Array.of(80, 75, 3, 4);
  }

  mutationJson() {
    return JSON.stringify({
      workbook: FakeSession.workbookInspection ?? fakeWorkbook(),
      editState: this.editState
    });
  }

  printManifestJson(sheetIndex, options) {
    FakeSession.calls.push(["prepare", sheetIndex, options]);
    return JSON.stringify(FakeSession.printManifest ?? fakePrintManifest(sheetIndex));
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
      limits: { maxOutputBytes: 1024 * 1024, maxPngBytes: 1024 * 1024 },
      editing: { maxHistoryEntries: 20, maxHistoryBytes: 32 * 1024 * 1024 }
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

async function assertReopenedDocumentBalancesResources({
  runtime,
  messages,
  cancelledRequestId,
  reopenedRequestId,
  documentId,
  bytes
}) {
  assert.equal(result(messages, reopenedRequestId).ok, true);
  assert.equal(result(messages, reopenedRequestId).result.documentId, documentId);
  assert.equal(bytes.byteLength * MAX_OPEN_DOCUMENTS, MAX_OPEN_RESOURCE_BYTES);

  runtime.receive({
    protocol: PROTOCOL,
    type: "cancel",
    requestId: cancelledRequestId
  });
  const documentIds = [documentId];
  for (let index = 1; index < MAX_OPEN_DOCUMENTS; index += 1) {
    const extraDocumentId = `${documentId}-extra-${index}`;
    const extraRequestId = `${reopenedRequestId}-extra-${index}`;
    documentIds.push(extraDocumentId);
    runtime.receive(
      request(extraRequestId, "open", {
        documentId: extraDocumentId,
        bytes
      })
    );
  }
  await settle(MAX_OPEN_DOCUMENTS + 4);
  for (let index = 1; index < MAX_OPEN_DOCUMENTS; index += 1) {
    assert.equal(result(messages, `${reopenedRequestId}-extra-${index}`).ok, true);
  }

  for (const [index, openDocumentId] of documentIds.entries()) {
    runtime.receive(
      request(`${reopenedRequestId}-close-${index}`, "close", {
        documentId: openDocumentId
      })
    );
  }
  await settle(MAX_OPEN_DOCUMENTS + 4);
  for (const index of documentIds.keys()) {
    const closed = result(messages, `${reopenedRequestId}-close-${index}`);
    assert.equal(closed.ok, true);
    assert.equal(closed.result.closed, true);
  }
  assert.equal(
    FakeSession.calls.filter(([kind]) => kind === "free").length,
    FakeSession.calls.filter(([kind]) => kind === "open").length
  );
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

test("worker rejects an incomplete print manifest returned by wasm", async (t) => {
  FakeSession.calls = [];
  FakeSession.printManifest = null;
  t.after(() => {
    FakeSession.printManifest = null;
  });
  const { runtime, messages } = harness();
  runtime.receive(
    request("manifest-contract-open", "open", {
      documentId: "manifest-contract",
      bytes: Uint8Array.of(80, 75, 3, 4)
    })
  );
  await settle();
  assert.equal(result(messages, "manifest-contract-open").ok, true);

  FakeSession.printManifest = {
    schema_version: 2,
    sheet_index: 0,
    pages: [{ output_index: 0 }]
  };
  runtime.receive(
    request("manifest-contract-prepare", "prepare-pages", {
      documentId: "manifest-contract",
      sheetIndex: 0
    })
  );
  await settle();
  assert.equal(result(messages, "manifest-contract-prepare").ok, false);
  assert.equal(
    result(messages, "manifest-contract-prepare").error.code,
    "wasm_api_mismatch"
  );
});

test("worker accepts the core manual page-break boundary", async (t) => {
  FakeSession.calls = [];
  FakeSession.printManifest = fakePrintManifest();
  FakeSession.printManifest.manual_row_breaks = Array.from(
    { length: MAX_MANUAL_PAGE_BREAKS },
    (_, index) => index
  );
  t.after(() => {
    FakeSession.printManifest = null;
  });
  const { runtime, messages } = harness();
  runtime.receive(
    request("break-boundary-open", "open", {
      documentId: "break-boundary",
      bytes: Uint8Array.of(80, 75, 3, 4)
    })
  );
  await settle();
  runtime.receive(
    request("break-boundary-prepare", "prepare-pages", {
      documentId: "break-boundary",
      sheetIndex: 0
    })
  );
  await settle();
  assert.equal(result(messages, "break-boundary-prepare").ok, true);
  assert.equal(
    result(messages, "break-boundary-prepare").result.manifest.manual_row_breaks.length,
    MAX_MANUAL_PAGE_BREAKS
  );
});

test("worker keeps preservation edits, history, and save bytes inside the session", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();
  runtime.receive(
    request("edit-open", "open", {
      documentId: "editable",
      bytes: Uint8Array.of(80, 75, 3, 4)
    })
  );
  await settle();
  assert.equal(result(messages, "edit-open").result.editState.capability, "read-write");

  runtime.receive(
    request("read-a1", "read-cell", {
      documentId: "editable",
      sheetIndex: 0,
      row: 0,
      col: 0
    })
  );
  runtime.receive(
    request("set-a1", "set-cell", {
      documentId: "editable",
      sheetIndex: 0,
      row: 0,
      col: 0,
      value: { kind: "text", value: "after" }
    })
  );
  await settle();
  assert.equal(result(messages, "read-a1").result.value.value, "before");
  assert.equal(result(messages, "set-a1").result.editState.dirty, true);
  assert.deepEqual(result(messages, "set-a1").result.editState.editedParts, [
    "xl/worksheets/sheet1.xml"
  ]);

  runtime.receive(request("undo-a1", "undo-edit", { documentId: "editable" }));
  runtime.receive(request("redo-a1", "redo-edit", { documentId: "editable" }));
  runtime.receive(request("save-a1", "save-document", { documentId: "editable" }));
  await settle();
  assert.equal(result(messages, "undo-a1").result.editState.dirty, false);
  assert.equal(result(messages, "redo-a1").result.editState.dirty, true);
  assert.equal(result(messages, "save-a1").result.mimeType, "application/octet-stream");
  assert.deepEqual([...result(messages, "save-a1").result.bytes], [80, 75, 3, 4]);
  assert.equal(
    messages.find(({ message }) => message.requestId === "save-a1" && message.ok).transfer.length,
    1
  );
});

test("worker rejects a saved workbook without an OOXML ZIP header", async (t) => {
  FakeSession.calls = [];
  FakeSession.savedDocument = new Uint8Array();
  t.after(() => {
    FakeSession.savedDocument = null;
  });
  const { runtime, messages } = harness();
  runtime.receive(
    request("invalid-save-open", "open", {
      documentId: "invalid-save",
      bytes: Uint8Array.of(80, 75, 3, 4)
    })
  );
  await settle();
  runtime.receive(request("invalid-save-result", "save-document", { documentId: "invalid-save" }));
  await settle();
  assert.equal(result(messages, "invalid-save-result").ok, false);
  assert.equal(result(messages, "invalid-save-result").error.code, "wasm_api_mismatch");
});

test("worker rejects malformed cell inspection output from wasm", async (t) => {
  FakeSession.calls = [];
  FakeSession.cellInspection = null;
  t.after(() => {
    FakeSession.cellInspection = null;
  });
  const { runtime, messages } = harness();
  runtime.receive(
    request("cell-contract-open", "open", {
      documentId: "cell-contract",
      bytes: Uint8Array.of(80, 75, 3, 4)
    })
  );
  await settle();
  assert.equal(result(messages, "cell-contract-open").ok, true);

  const invalidInspections = [
    {
      schemaVersion: 1,
      sheetIndex: 0,
      row: 0,
      col: 0,
      value: { kind: "text", value: "before", extra: true },
      formatted: "before"
    },
    {
      schemaVersion: 1,
      sheetIndex: 0,
      row: 0,
      col: 0,
      value: { kind: "formula", formula: "1+1", cached: { kind: "number", value: "2" } },
      formatted: "2"
    },
    {
      schemaVersion: 1,
      sheetIndex: 0,
      row: 0,
      col: 0,
      value: { kind: "blank" },
      formatted: 7
    }
  ];
  for (const [index, inspection] of invalidInspections.entries()) {
    FakeSession.cellInspection = inspection;
    const requestId = `cell-contract-${index}`;
    runtime.receive(
      request(requestId, "read-cell", {
        documentId: "cell-contract",
        sheetIndex: 0,
        row: 0,
        col: 0
      })
    );
    await settle();
    assert.equal(result(messages, requestId).ok, false);
    assert.equal(result(messages, requestId).error.code, "wasm_api_mismatch");
  }
});

test("worker rejects a coercible non-string workbook hash", async (t) => {
  FakeSession.calls = [];
  FakeSession.workbookInspection = fakeWorkbook();
  FakeSession.workbookInspection.fontPackSha256 = ["a".repeat(64)];
  t.after(() => {
    FakeSession.workbookInspection = null;
  });
  const { runtime, messages } = harness();
  runtime.receive(
    request("hash-contract-open", "open", {
      documentId: "hash-contract",
      bytes: Uint8Array.of(80, 75, 3, 4)
    })
  );
  await settle();
  assert.equal(result(messages, "hash-contract-open").ok, false);
  assert.equal(result(messages, "hash-contract-open").error.code, "wasm_api_mismatch");
});

test("progress never reaches completion on a failing request and stays monotonic", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();
  // "documentId" format is checked during the synchronous protocol preflight
  // (before the request is even queued, so it never reaches #run), but
  // whether that document is actually open is only known once #execute runs.
  // Requesting an unopened, well-formed documentId therefore fails from
  // inside #execute, after the pre-work progress steps have already been
  // sent -- exactly the "failure mid-flight" case this test targets.
  runtime.receive(
    request("page-not-open", "render-page", {
      documentId: "doc-never-opened",
      sheetIndex: 0,
      pageIndex: 0
    })
  );
  await settle();
  const failure = result(messages, "page-not-open");
  assert.equal(failure.ok, false);
  assert.equal(failure.error.code, "document_not_open");

  const progress = messages
    .filter(
      ({ message }) => message.type === "progress" && message.requestId === "page-not-open"
    )
    .map(({ message }) => [message.completed, message.total, message.stage]);

  // The document-existence check runs inside #execute, before any wasm
  // call, so only the pre-execution steps are ever observed for this
  // failure.
  assert.deepEqual(progress, [
    [0, 3, "accepted"],
    [1, 3, "rendering"]
  ]);
  // A failed request must never fabricate a jump to completion: no observed
  // step may report completed === total, and "complete" must never appear.
  assert.equal(
    progress.some(([completed, total]) => completed === total),
    false
  );
  assert.equal(
    progress.some(([, , stage]) => stage === "complete"),
    false
  );
  for (let index = 1; index < progress.length; index += 1) {
    assert.ok(progress[index][0] > progress[index - 1][0], "progress must strictly increase");
    assert.equal(progress[index][1], progress[0][1], "total must stay constant");
  }
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

test("an active mutation ignores cancellation and returns its committed result", async (t) => {
  FakeSession.calls = [];
  let releaseMutation;
  FakeSession.mutationGate = new Promise((resolve) => {
    releaseMutation = resolve;
  });
  t.after(() => {
    FakeSession.mutationGate = null;
  });
  const { runtime, messages } = harness();
  runtime.receive(
    request("mutation-open", "open", {
      documentId: "mutation-document",
      bytes: Uint8Array.of(1)
    })
  );
  await settle();
  runtime.receive(
    request("mutation-active", "set-cell", {
      documentId: "mutation-document",
      sheetIndex: 0,
      row: 0,
      col: 0,
      value: { kind: "text", value: "committed" }
    })
  );
  await settle(3);
  assert.equal(FakeSession.calls.some(([kind]) => kind === "set-cell"), true);
  runtime.receive({ protocol: PROTOCOL, type: "cancel", requestId: "mutation-active" });
  releaseMutation();
  await settle();
  assert.equal(result(messages, "mutation-active").ok, true);
  assert.equal(result(messages, "mutation-active").result.editState.dirty, true);
});

test("editable documents reserve their bounded history inside the global resource cap", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();
  for (let index = 0; index < 3; index += 1) {
    runtime.receive(
      request(`reserved-open-${index}`, "open", {
        documentId: `reserved-document-${index}`,
        bytes: Uint8Array.of(index)
      })
    );
    await settle();
    assert.equal(result(messages, `reserved-open-${index}`).ok, true);
  }
  runtime.receive(
    request("reserved-overflow", "open", {
      documentId: "reserved-overflow-document",
      bytes: Uint8Array.of(4)
    })
  );
  await settle();
  assert.equal(result(messages, "reserved-overflow").ok, false);
  assert.equal(result(messages, "reserved-overflow").error.resource, "openResourceBytes");

  runtime.receive(
    request("reserved-close", "close", { documentId: "reserved-document-0" })
  );
  await settle();
  runtime.receive(
    request("reserved-reopen", "open", {
      documentId: "reserved-reopened-document",
      bytes: Uint8Array.of(5)
    })
  );
  await settle();
  assert.equal(result(messages, "reserved-reopen").ok, true);
});

test("worker rejects incomplete workbook inspection in a mutation result", async (t) => {
  FakeSession.calls = [];
  FakeSession.workbookInspection = null;
  t.after(() => {
    FakeSession.workbookInspection = null;
  });
  const { runtime, messages } = harness();
  runtime.receive(
    request("mutation-contract-open", "open", {
      documentId: "mutation-contract",
      bytes: Uint8Array.of(1)
    })
  );
  await settle();
  FakeSession.workbookInspection = { schemaVersion: 1, sheetCount: 1, sheets: [] };
  runtime.receive(
    request("mutation-contract-edit", "set-cell", {
      documentId: "mutation-contract",
      sheetIndex: 0,
      row: 0,
      col: 0,
      value: { kind: "text", value: "invalid response" }
    })
  );
  await settle();
  assert.equal(result(messages, "mutation-contract-edit").ok, false);
  assert.equal(result(messages, "mutation-contract-edit").error.code, "wasm_api_mismatch");
});

test("active open cancellation rolls back before a same-document reopen", async (t) => {
  FakeSession.calls = [];
  FakeSession.editCapability = "read-only";
  t.after(() => {
    FakeSession.inspectionHook = null;
    FakeSession.editCapability = "read-write";
  });
  const bytes = new Uint8Array(MAX_INPUT_BYTES);
  const { runtime, messages } = harness();
  FakeSession.inspectionHook = () => {
    FakeSession.inspectionHook = null;
    setTimeout(() => {
      runtime.receive({ protocol: PROTOCOL, type: "cancel", requestId: "active-open" });
      runtime.receive(
        request("active-reopen", "open", {
          documentId: "active-document",
          bytes
        })
      );
    }, 0);
  };

  runtime.receive(
    request("active-open", "open", {
      documentId: "active-document",
      bytes
    })
  );
  await settle();

  const cancelled = result(messages, "active-open");
  assert.equal(cancelled.ok, false);
  assert.equal(cancelled.error.code, "cancelled");
  assert.equal(
    messages.some(
      ({ message }) =>
        message.type === "result" && message.requestId === "active-open" && message.ok
    ),
    false
  );
  assert.equal(FakeSession.calls.filter(([kind]) => kind === "free").length, 1);
  await assertReopenedDocumentBalancesResources({
    runtime,
    messages,
    cancelledRequestId: "active-open",
    reopenedRequestId: "active-reopen",
    documentId: "active-document",
    bytes
  });
});

test("completed open remains live after its request id is reused and cancelled", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();

  runtime.receive(
    request("reused", "open", {
      documentId: "persistent-document",
      bytes: Uint8Array.of(1)
    })
  );
  await settle();
  assert.equal(result(messages, "reused").ok, true);

  runtime.receive(request("reused", "capabilities"));
  await settle();
  assert.equal(
    messages.filter(
      ({ message }) => message.type === "result" && message.requestId === "reused" && message.ok
    ).length,
    2
  );
  runtime.receive({ protocol: PROTOCOL, type: "cancel", requestId: "reused" });
  runtime.receive(
    request("close-persistent", "close", {
      documentId: "persistent-document"
    })
  );
  await settle();
  assert.equal(result(messages, "close-persistent").ok, true);
  assert.equal(result(messages, "close-persistent").result.closed, true);
  assert.equal(FakeSession.calls.filter(([kind]) => kind === "free").length, 1);
});

test("failed open with a reused request id cannot roll back the original document", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();
  runtime.receive(
    request("reused-open", "open", {
      documentId: "original-document",
      bytes: Uint8Array.of(1)
    })
  );
  await settle();
  runtime.receive(
    request("reused-open", "open", {
      documentId: "original-document",
      bytes: Uint8Array.of(2)
    })
  );
  await settle();
  const reusedResults = messages
    .filter(
      ({ message }) => message.type === "result" && message.requestId === "reused-open"
    )
    .map(({ message }) => message);
  assert.equal(reusedResults.length, 2);
  assert.equal(reusedResults[0].ok, true);
  assert.equal(reusedResults[1].ok, false);
  assert.equal(reusedResults[1].error.code, "document_exists");
  assert.equal(FakeSession.calls.filter(([kind]) => kind === "free").length, 0);

  runtime.receive(
    request("close-original", "close", {
      documentId: "original-document"
    })
  );
  await settle();
  assert.equal(result(messages, "close-original").result.closed, true);
  assert.equal(FakeSession.calls.filter(([kind]) => kind === "free").length, 1);
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

test("worker stays reusable and the document stays open after cancelling an active render", async () => {
  FakeSession.calls = [];
  const { runtime, messages } = harness();
  runtime.receive(
    request("open-reuse", "open", { documentId: "doc-reuse", bytes: Uint8Array.of(1) })
  );
  await settle();
  let release;
  FakeSession.pageGate = new Promise((resolve) => (release = resolve));
  runtime.receive(
    request("page-cancelled", "render-page", {
      documentId: "doc-reuse",
      sheetIndex: 0,
      pageIndex: 0
    })
  );
  await new Promise((resolve) => setImmediate(resolve));
  runtime.receive({ protocol: PROTOCOL, type: "cancel", requestId: "page-cancelled" });
  release('<svg data-page="0"></svg>');
  await settle();
  FakeSession.pageGate = null;
  assert.equal(result(messages, "page-cancelled").error.code, "cancelled");

  // Only "open" transactions roll back on cancellation; a cancelled render
  // must not free the document's session out from under later requests.
  assert.equal(FakeSession.calls.filter(([kind]) => kind === "free").length, 0);

  // A fresh request against the same document, on the same runtime instance,
  // must still succeed -- proving the earlier cancellation left neither the
  // document nor the worker's queue wedged.
  runtime.receive(
    request("page-after-cancel", "render-page", {
      documentId: "doc-reuse",
      sheetIndex: 0,
      pageIndex: 1
    })
  );
  await settle();
  const followUp = result(messages, "page-after-cancel");
  assert.equal(followUp.ok, true);
  assert.match(followUp.result.svg, /data-page="1"/);

  runtime.receive(request("close-reuse", "close", { documentId: "doc-reuse" }));
  await settle();
  assert.equal(result(messages, "close-reuse").result.closed, true);
  assert.equal(FakeSession.calls.filter(([kind]) => kind === "free").length, 1);
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
  assert.equal(result(messages, "resource-0").ok, true);
  assert.equal(result(messages, "resource-1").ok, true);
  assert.equal(result(messages, "resource-2").error.resource, "openResourceBytes");
  assert.equal(result(messages, "resource-3").error.resource, "openResourceBytes");
  assert.equal(
    FakeSession.calls.filter(([kind]) => kind === "open").length,
    2
  );
});
