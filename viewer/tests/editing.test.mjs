import test from "node:test";
import assert from "node:assert/strict";
import { createEditingController } from "../src/editing.js";

function control() {
  const listeners = new Map();
  const attributes = new Map();
  return {
    value: "",
    textContent: "",
    title: "",
    dataset: {},
    open: false,
    disabled: false,
    hidden: false,
    classList: { toggle() {} },
    addEventListener: (type, listener) => listeners.set(type, listener),
    emit: (type) => listeners.get(type)?.({ preventDefault() {} }),
    setAttribute: (name, value) => attributes.set(name, value),
    removeAttribute: (name) => attributes.delete(name),
    querySelectorAll: () => [],
    showModal() {
      this.open = true;
    },
    close() {
      this.open = false;
    },
    focus() {},
    select() {},
  };
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

function setup({
  readOnly = false,
  client = {},
  beforeCommand,
  renderCurrent,
} = {}) {
  const elements = new Proxy(
    {},
    {
      get(target, id) {
        return (target[id] ??= control());
      },
    },
  );
  const workbook = {
    sheetCount: 1,
    sheets: [{ index: 0, name: "Sheet 1" }],
    properties: { title: "Original title", company: "Example" },
  };
  const state = {
    client,
    workbook,
    editState: {
      capability: "read-write",
      dirty: false,
      canUndo: true,
      canRedo: true,
    },
    documentId: "document-one",
    sheetIndex: 0,
    pageIndex: 3,
    busy: false,
    manifests: new Map([[0, { pages: [1] }]]),
    file: { name: "Quarter.xlsm", size: 100, source: "Local file" },
  };
  const calls = {
    errors: [],
    busy: [],
    downloads: [],
    renders: [],
    updates: 0,
    confirms: [],
  };
  const controller = createEditingController({
    state,
    elements,
    readOnly,
    beforeCommand,
    setBusy(busy, label) {
      state.busy = busy;
      calls.busy.push({ busy, label });
    },
    showError: (error) => calls.errors.push(error),
    updateWorkbookUi: () => {
      calls.updates += 1;
    },
    renderCurrent: async (options) => {
      calls.renders.push(options);
      await renderCurrent?.(state);
      state.busy = false;
    },
    download: (blob, name) => calls.downloads.push({ blob, name }),
    confirm: (message) => {
      calls.confirms.push(message);
      return false;
    },
  });
  elements["cell-reference"].value = "A1";
  controller.bindEvents();
  return { controller, elements, state, calls, workbook };
}

const flush = () => new Promise((resolve) => setImmediate(resolve));

test("editing UI is safe before loading a workbook and stays read-only in VS Code", async () => {
  const fixture = setup({ readOnly: true });
  const { controller, state, elements, calls } = fixture;
  controller.updateEditUi();
  assert.equal(elements["meta-editing"].textContent, "Read-only");
  assert.equal(elements["edit-cell"].disabled, true);
  assert.equal(elements["save-document"].disabled, true);
  await controller.openCellEditor();
  controller.openPropertiesEditor();
  await controller.applyHistoryEdit("undo");
  await controller.saveWorkbookCopy();
  assert.equal(elements["cell-dialog"].open, false);
  assert.equal(elements["properties-dialog"].open, false);
  assert.deepEqual(calls.errors, []);
  assert.deepEqual(calls.downloads, []);
  state.workbook = null;
  state.editState = null;
  controller.updateEditUi();
  assert.equal(elements["meta-editing"].textContent, "-");
});

test("a changed reference invalidates an in-flight read without enabling stale edits", async () => {
  const pending = deferred();
  const { controller, elements } = setup({
    client: { readCell: () => pending.promise },
  });
  const opened = controller.openCellEditor();
  assert.equal(elements["apply-cell-edit"].disabled, true);
  elements["cell-reference"].value = "B2";
  elements["cell-reference"].emit("input");
  pending.resolve({
    value: { kind: "text", value: "stale" },
    formatted: "stale",
  });
  await opened;
  assert.equal(elements["cell-value"].value, "");
  assert.equal(elements["apply-cell-edit"].disabled, true);
  assert.match(elements["cell-current-value"].textContent, /Load this cell/);
});

test("closing the editor discards late reads and does not reopen the dialog", async () => {
  const pending = deferred();
  const { controller, elements } = setup({
    client: { readCell: () => pending.promise },
  });
  const opened = controller.openCellEditor();
  controller.closeCellEditor();
  pending.resolve({ value: { kind: "number", value: 42 }, formatted: "42" });
  await opened;
  assert.equal(elements["cell-dialog"].open, false);
  assert.equal(elements["cell-value"].value, "");
  assert.equal(elements["apply-cell-edit"].disabled, true);
});

test("document identity guards a read even when the reference remains unchanged", async () => {
  const pending = deferred();
  const { controller, state, elements } = setup({
    client: { readCell: () => pending.promise },
  });
  const opened = controller.openCellEditor();
  state.documentId = "document-two";
  pending.resolve({ value: { kind: "number", value: 42 }, formatted: "42" });
  await opened;
  assert.equal(elements["cell-value"].value, "");
  assert.equal(elements["apply-cell-edit"].disabled, true);
});

test("a loaded cell submits a typed edit and refreshes the rendered workbook", async () => {
  const edits = [];
  const fixture = setup({
    client: {
      readCell: async () => ({
        value: { kind: "number", value: 4 },
        formatted: "4",
      }),
      setCell: async (...args) => {
        edits.push(args);
        return {
          workbook: fixture.workbook,
          editState: { capability: "read-write", dirty: true },
        };
      },
    },
  });
  const { controller, elements, state, calls } = fixture;
  await controller.openCellEditor();
  assert.equal(elements["cell-value"].value, "4");
  assert.equal(elements["apply-cell-edit"].disabled, false);
  elements["cell-value"].value = "9.5";
  elements["cell-form"].emit("submit");
  await flush();
  assert.deepEqual(edits, [
    ["document-one", 0, 0, 0, { kind: "number", value: 9.5 }],
  ]);
  assert.equal(elements["cell-dialog"].open, false);
  assert.equal(state.editState.dirty, true);
  assert.equal(state.manifests.size, 0);
  assert.equal(state.pageIndex, 3);
  assert.deepEqual(calls.renders, [{ fit: false }]);
  assert.equal(elements["status-message"].textContent, "A1 updated");
});

test("a failed read reports the error and never permits an edit", async () => {
  const failure = new Error("read failed");
  const { controller, elements, calls } = setup({
    client: {
      readCell: async () => {
        throw failure;
      },
    },
  });
  await controller.openCellEditor();
  assert.deepEqual(calls.errors, [failure]);
  assert.equal(elements["apply-cell-edit"].disabled, true);
  assert.equal(elements["read-cell"].disabled, false);
});

test("properties controller prefills fields and submits absent properties as null", async () => {
  const submitted = [];
  const fixture = setup({
    client: {
      setDocumentProperties: async (...args) => {
        submitted.push(args);
        return {
          workbook: fixture.workbook,
          editState: fixture.state.editState,
        };
      },
    },
  });
  fixture.controller.openPropertiesEditor();
  assert.equal(fixture.elements["property-title"].value, "Original title");
  fixture.elements["property-title"].value = "Updated title";
  fixture.elements["property-company"].value = "";
  fixture.elements["properties-form"].emit("submit");
  await flush();
  assert.equal(submitted[0][0], "document-one");
  assert.equal(submitted[0][1].title, "Updated title");
  assert.equal(submitted[0][1].company, null);
  assert.equal(fixture.elements["properties-dialog"].open, false);
});

test("undo and redo use their distinct worker commands and recover from worker failure", async () => {
  const commands = [];
  const fixture = setup();
  const result = {
    workbook: fixture.workbook,
    editState: fixture.state.editState,
  };
  fixture.state.client = {
    undoEdit: async (id) => {
      commands.push(["undo", id]);
      return result;
    },
    redoEdit: async (id) => {
      commands.push(["redo", id]);
      return result;
    },
  };
  await fixture.controller.applyHistoryEdit("undo");
  await fixture.controller.applyHistoryEdit("redo");
  assert.deepEqual(commands, [
    ["undo", "document-one"],
    ["redo", "document-one"],
  ]);
  fixture.state.client.undoEdit = async () => {
    throw new Error("worker stopped");
  };
  await fixture.controller.applyHistoryEdit("undo");
  assert.equal(fixture.state.busy, false);
  assert.match(fixture.calls.errors[0].message, /worker stopped/);
});

test("preserved saves keep macro bytes, MIME, and extension and restore idle state", async () => {
  const bytes = new Uint8Array([1, 2, 3]);
  const { controller, calls, state } = setup({
    client: {
      saveDocument: async () => ({ bytes }),
    },
  });
  await controller.saveWorkbookCopy();
  assert.equal(calls.downloads[0].name, "Quarter-edited.xlsm");
  assert.equal(
    calls.downloads[0].blob.type,
    "application/vnd.ms-excel.sheet.macroenabled.12",
  );
  assert.deepEqual(
    new Uint8Array(await calls.downloads[0].blob.arrayBuffer()),
    bytes,
  );
  assert.equal(state.busy, false);
  assert.equal(controller.confirmDiscardChanges(), true);
  state.editState.dirty = true;
  assert.equal(controller.confirmDiscardChanges(), false);
  assert.deepEqual(calls.confirms, ["Discard unsaved workbook edits?"]);
});

test("a refused inline-draft command gate blocks dialog, properties, history, and save", async () => {
  let gates = 0;
  const { controller, calls, elements } = setup({
    beforeCommand: async () => {
      gates += 1;
      return false;
    },
  });
  await controller.openCellEditor();
  await controller.openPropertiesEditor();
  await controller.applyHistoryEdit("undo");
  await controller.saveWorkbookCopy();
  assert.equal(gates, 4);
  assert.equal(elements["cell-dialog"].open, false);
  assert.equal(elements["properties-dialog"].open, false);
  assert.deepEqual(calls.busy, []);
  assert.deepEqual(calls.downloads, []);
  assert.deepEqual(calls.errors, []);
});

test("shared cell commit requires the current bounded editable target", async () => {
  const { controller, state, calls } = setup();
  const target = {
    client: state.client,
    documentId: state.documentId,
    sheetIndex: 0,
    row: 0,
    col: 0,
  };
  await assert.rejects(
    controller.commitCellEdit(
      { ...target, documentId: "old" },
      { kind: "blank" },
    ),
    /changed/,
  );
  await assert.rejects(
    controller.commitCellEdit({ ...target, row: -1 }, { kind: "blank" }),
    /grid/,
  );
  state.busy = true;
  await assert.rejects(
    controller.commitCellEdit(target, { kind: "blank" }),
    /busy/,
  );
  state.busy = false;
  state.editState.capability = "read-only";
  await assert.rejects(
    controller.commitCellEdit(target, { kind: "blank" }),
    /read-only/,
  );
  assert.deepEqual(calls.renders, []);
});

test("shared commit uses the existing render path and does not recursively invoke toolbar gates", async () => {
  let gates = 0;
  const fixture = setup({
    beforeCommand: async () => {
      gates += 1;
      return true;
    },
  });
  fixture.state.client.setCell = async () => ({
    workbook: fixture.workbook,
    editState: { capability: "read-write", dirty: true },
  });
  const target = {
    client: fixture.state.client,
    documentId: "document-one",
    sheetIndex: 0,
    row: 3,
    col: 2,
  };
  assert.equal(
    await fixture.controller.commitCellEdit(target, {
      kind: "number",
      value: 7,
    }),
    true,
  );
  assert.equal(gates, 0);
  assert.equal(fixture.state.busy, false);
  assert.equal(fixture.state.editState.dirty, true);
  assert.deepEqual(fixture.calls.renders, [{ fit: false }]);
  assert.equal(fixture.elements["status-message"].textContent, "C4 updated");
});

test("shared commit ignores late results after client, document, or sheet changes", async () => {
  for (const field of ["client", "documentId", "sheetIndex"]) {
    const pending = deferred();
    const fixture = setup({ client: { setCell: () => pending.promise } });
    const target = {
      client: fixture.state.client,
      documentId: "document-one",
      sheetIndex: 0,
      row: 0,
      col: 0,
    };
    const operation = fixture.controller.commitCellEdit(target, {
      kind: "number",
      value: 7,
    });
    fixture.state[field] =
      field === "client" ? {} : field === "sheetIndex" ? 1 : "document-two";
    const replacement = {
      sheetCount: 1,
      sheets: [{ name: "Current" }],
      properties: {},
    };
    fixture.state.workbook = replacement;
    pending.resolve({
      workbook: fixture.workbook,
      editState: { capability: "read-write", dirty: true },
    });
    assert.equal(await operation, false);
    assert.equal(fixture.state.workbook, replacement);
    assert.deepEqual(fixture.calls.renders, []);
  }
});

test("shared commit rejects concurrent writes and releases its busy gate after failure", async () => {
  const pending = deferred();
  const fixture = setup({ client: { setCell: () => pending.promise } });
  const target = {
    client: fixture.state.client,
    documentId: "document-one",
    sheetIndex: 0,
    row: 0,
    col: 0,
  };
  const operation = fixture.controller.commitCellEdit(target, {
    kind: "number",
    value: 7,
  });
  await assert.rejects(
    fixture.controller.commitCellEdit(target, { kind: "blank" }),
    /busy/,
  );
  pending.reject(new Error("write failed"));
  await assert.rejects(operation, /write failed/);
  assert.equal(fixture.state.busy, false);
  assert.equal(fixture.state.editState.dirty, false);
});

test("shared commit prefers atomic recalculation and reports successful cached-value fallbacks", async () => {
  const fixture = setup();
  const operations = [];
  fixture.state.client.setCell = async () => {
    throw new Error("legacy path must not run");
  };
  fixture.state.client.setCellAndRecalculate = async function (...args) {
    assert.equal(
      this,
      fixture.state.client,
      "worker client receiver is preserved",
    );
    operations.push(args);
    return {
      workbook: fixture.workbook,
      editState: { capability: "read-write", dirty: true },
      recalculation: {
        computedCells: 4,
        unchangedCells: 2,
        unsupportedCells: 1,
        reasons: ["unsupported_function"],
      },
    };
  };
  const target = {
    client: fixture.state.client,
    documentId: "document-one",
    sheetIndex: 0,
    row: 3,
    col: 2,
  };
  assert.equal(
    await fixture.controller.commitCellEdit(target, {
      kind: "number",
      value: 7,
    }),
    true,
  );
  assert.deepEqual(operations, [
    ["document-one", 0, 3, 2, { kind: "number", value: 7 }],
  ]);
  assert.equal(fixture.state.editState.dirty, true);
  assert.equal(fixture.elements["error-banner"].hidden, false);
  assert.match(fixture.elements["error-message"].textContent, /^C4 updated\./);
  assert.match(
    fixture.elements["error-message"].textContent,
    /4 formula cells recalculated; 1 unsupported formula cell kept their cached values/,
  );
  assert.deepEqual(
    fixture.calls.errors,
    [],
    "successful fallback must not be reported as a failed mutation",
  );
  assert.deepEqual(fixture.calls.renders, [{ fit: false }]);
});

test("a fully recalculated later edit clears its previous unsupported warning", async () => {
  const fixture = setup();
  let unsupportedCells = 1;
  fixture.state.client.setCellAndRecalculate = async () => ({
    workbook: fixture.workbook,
    editState: fixture.state.editState,
    recalculation: {
      computedCells: 1,
      unchangedCells: 0,
      unsupportedCells,
      reasons: [],
    },
  });
  const target = {
    client: fixture.state.client,
    documentId: "document-one",
    sheetIndex: 0,
    row: 0,
    col: 0,
  };
  await fixture.controller.commitCellEdit(target, { kind: "number", value: 1 });
  assert.equal(fixture.elements["error-banner"].hidden, false);
  unsupportedCells = 0;
  await fixture.controller.commitCellEdit(target, { kind: "number", value: 2 });
  assert.equal(fixture.elements["error-banner"].hidden, true);
  assert.equal(fixture.elements["error-message"].textContent, "");
  assert.equal(
    fixture.elements["status-message"].textContent,
    "A1 updated. 1 formula cell recalculated.",
  );
});

function warningFixture() {
  const fixture = setup();
  fixture.state.client.setCellAndRecalculate = async () => ({
    workbook: fixture.workbook,
    editState: { ...fixture.state.editState, dirty: true },
    recalculation: {
      computedCells: 4,
      unchangedCells: 2,
      unsupportedCells: 1,
      reasons: ["unsupported_function"],
    },
  });
  fixture.state.client.setDocumentProperties = async () => ({
    workbook: fixture.workbook,
    editState: fixture.state.editState,
  });
  fixture.state.client.undoEdit = fixture.state.client.redoEdit = async () => ({
    workbook: fixture.workbook,
    editState: fixture.state.editState,
  });
  fixture.commit = () =>
    fixture.controller.commitCellEdit(
      {
        client: fixture.state.client,
        documentId: fixture.state.documentId,
        sheetIndex: fixture.state.sheetIndex,
        row: 0,
        col: 0,
      },
      { kind: "number", value: 7 },
    );
  fixture.properties = async () => {
    await fixture.controller.openPropertiesEditor();
    fixture.elements["property-title"].value = "Updated title";
    fixture.elements["properties-form"].emit("submit");
    await flush();
  };
  return fixture;
}

test("metadata edits retain the current page and existing cached-formula warning", async () => {
  const fixture = warningFixture();
  await fixture.commit();
  fixture.state.pageIndex = 3;
  await fixture.properties();
  assert.equal(fixture.state.pageIndex, 3);
  assert.equal(fixture.elements["error-banner"].hidden, false);
  assert.match(
    fixture.elements["error-message"].textContent,
    /1 unsupported formula cell/,
  );
  assert.doesNotMatch(
    fixture.elements["status-message"].textContent,
    /recalculated/,
  );
  assert.deepEqual(fixture.calls.errors, []);
});

test("undo and redo restore cached-formula warning state without changing pages", async () => {
  const fixture = warningFixture();
  await fixture.commit();
  await fixture.properties();
  for (const [direction, warning] of [
    ["undo", true],
    ["undo", false],
    ["redo", true],
    ["redo", true],
  ]) {
    fixture.state.pageIndex = 3;
    await fixture.controller.applyHistoryEdit(direction);
    assert.equal(fixture.state.pageIndex, 3);
    assert.equal(fixture.elements["error-banner"].hidden, !warning, direction);
    assert.doesNotMatch(
      fixture.elements["status-message"].textContent,
      /recalculated/,
    );
  }
});

test("undo restores a prior warning after a fully recalculated edit and redo clears it", async () => {
  const fixture = warningFixture();
  await fixture.commit();
  fixture.state.client.setCellAndRecalculate = async () => ({
    workbook: fixture.workbook,
    editState: fixture.state.editState,
    recalculation: {
      computedCells: 2,
      unchangedCells: 0,
      unsupportedCells: 0,
      reasons: [],
    },
  });
  await fixture.commit();
  assert.equal(fixture.elements["error-banner"].hidden, true);
  await fixture.controller.applyHistoryEdit("undo");
  assert.equal(fixture.elements["error-banner"].hidden, false);
  await fixture.controller.applyHistoryEdit("redo");
  assert.equal(fixture.elements["error-banner"].hidden, true);
});

test("warning history follows worker snapshot eviction and new edit branches", async () => {
  const fixture = warningFixture();
  await fixture.commit();
  fixture.state.client.setDocumentProperties = async () => ({
    workbook: fixture.workbook,
    editState: { ...fixture.state.editState, undoDepth: 1, redoDepth: 0 },
  });
  await fixture.properties();
  fixture.state.client.undoEdit = async () => ({
    workbook: fixture.workbook,
    editState: { ...fixture.state.editState, undoDepth: 0, redoDepth: 1 },
  });
  await fixture.controller.applyHistoryEdit("undo");
  assert.equal(fixture.elements["error-banner"].hidden, false);
  fixture.state.client.setCellAndRecalculate = async () => ({
    workbook: fixture.workbook,
    editState: { ...fixture.state.editState, undoDepth: 1, redoDepth: 0 },
    recalculation: {
      computedCells: 1,
      unchangedCells: 0,
      unsupportedCells: 0,
      reasons: [],
    },
  });
  await fixture.commit();
  assert.equal(fixture.elements["error-banner"].hidden, true);
  await fixture.controller.applyHistoryEdit("undo");
  assert.equal(fixture.elements["error-banner"].hidden, false);
  fixture.state.client.redoEdit = async () => ({
    workbook: fixture.workbook,
    editState: { ...fixture.state.editState, undoDepth: 1, redoDepth: 0 },
  });
  await fixture.controller.applyHistoryEdit("redo");
  assert.equal(fixture.elements["error-banner"].hidden, true);
});

test("the refreshed renderer may clamp a retained page after cell, metadata, and history edits", async () => {
  const pages = [];
  const fixture = setup({
    renderCurrent(state) {
      pages.push(state.pageIndex);
      state.pageIndex = Math.min(state.pageIndex, 1);
    },
  });
  const result = () => ({
    workbook: fixture.workbook,
    editState: fixture.state.editState,
  });
  fixture.state.client.setCell = async () => result();
  fixture.state.client.setDocumentProperties = async () => result();
  fixture.state.client.undoEdit = async () => result();
  await fixture.controller.commitCellEdit(
    {
      client: fixture.state.client,
      documentId: fixture.state.documentId,
      sheetIndex: 0,
      row: 0,
      col: 0,
    },
    { kind: "number", value: 1 },
  );
  assert.equal(fixture.state.pageIndex, 1);
  fixture.state.pageIndex = 3;
  await fixture.controller.openPropertiesEditor();
  fixture.elements["properties-form"].emit("submit");
  await flush();
  assert.equal(fixture.state.pageIndex, 1);
  fixture.state.pageIndex = 3;
  await fixture.controller.applyHistoryEdit("undo");
  assert.equal(fixture.state.pageIndex, 1);
  assert.deepEqual(pages, [3, 3, 3]);
});

test("new document identity clears warning history without clearing unrelated errors", async () => {
  for (const field of ["client", "documentId"]) {
    const fixture = warningFixture();
    await fixture.commit();
    fixture.state[field] = field === "client" ? {} : "document-two";
    fixture.controller.updateEditUi();
    assert.equal(fixture.elements["error-banner"].hidden, true);
    assert.equal(fixture.elements["error-message"].textContent, "");
    assert.doesNotMatch(
      fixture.elements["status-message"].textContent,
      /unsupported formula/,
    );
  }
  const fixture = warningFixture();
  await fixture.commit();
  fixture.elements["error-message"].textContent = "Current renderer error";
  fixture.state.documentId = "document-two";
  fixture.controller.updateEditUi();
  assert.equal(fixture.elements["error-banner"].hidden, false);
  assert.equal(
    fixture.elements["error-message"].textContent,
    "Current renderer error",
  );
});

test("failed and stale metadata/history results cannot change current warnings or pages", async () => {
  for (const operation of ["properties", "undo", "redo"]) {
    for (const outcome of ["failed", "stale-result", "stale-error"]) {
      const stale = outcome !== "failed";
      const fixture = warningFixture();
      await fixture.commit();
      const warning = fixture.elements["error-message"].textContent;
      const pending = deferred();
      const method =
        operation === "properties"
          ? "setDocumentProperties"
          : `${operation}Edit`;
      fixture.state.client[method] = () => pending.promise;
      const task =
        operation === "properties"
          ? fixture.properties()
          : fixture.controller.applyHistoryEdit(operation);
      await flush();
      const currentWorkbook = {
        ...fixture.workbook,
        properties: { title: "Current" },
      };
      if (stale) {
        fixture.state.documentId = "document-two";
        fixture.state.workbook = currentWorkbook;
        fixture.state.busy = false;
        fixture.controller.updateEditUi();
        fixture.elements["error-message"].textContent = "Current diagnostic";
        fixture.elements["status-message"].textContent = "Current status";
        fixture.elements["error-banner"].hidden = false;
        fixture.state.pageIndex = 5;
        if (outcome === "stale-error")
          pending.reject(new Error("Old document error"));
        else
          pending.resolve({
            workbook: fixture.workbook,
            editState: fixture.state.editState,
          });
      } else {
        pending.reject(new Error("Mutation rejected"));
      }
      await task;
      await flush();
      if (stale) {
        assert.equal(fixture.state.workbook, currentWorkbook);
        assert.equal(fixture.state.pageIndex, 5);
        assert.equal(
          fixture.elements["error-message"].textContent,
          "Current diagnostic",
        );
        assert.equal(
          fixture.elements["status-message"].textContent,
          "Current status",
        );
        assert.equal(fixture.state.busy, false);
        assert.deepEqual(fixture.calls.errors, []);
      } else {
        assert.equal(fixture.elements["error-message"].textContent, warning);
        assert.equal(fixture.elements["error-banner"].hidden, false);
      }
    }
  }
});

test("a stale metadata request releases form buttons for the next document", async () => {
  const fixture = warningFixture();
  const buttons = [control(), control()];
  fixture.elements["properties-form"].querySelectorAll = () => buttons;
  const pending = deferred();
  fixture.state.client.setDocumentProperties = () => pending.promise;
  await fixture.properties();
  assert.ok(buttons.every((button) => button.disabled));
  fixture.state.documentId = "document-two";
  fixture.state.busy = false;
  fixture.controller.closePropertiesEditor();
  pending.resolve({
    workbook: fixture.workbook,
    editState: fixture.state.editState,
  });
  await flush();
  assert.ok(buttons.every((button) => !button.disabled));
  await fixture.controller.openPropertiesEditor();
  assert.equal(fixture.elements["properties-dialog"].open, true);
});

test("a stale recalculation result cannot overwrite a new document or its visible diagnostic", async () => {
  const pending = deferred();
  const fixture = setup({
    client: { setCellAndRecalculate: () => pending.promise },
  });
  const target = {
    client: fixture.state.client,
    documentId: "document-one",
    sheetIndex: 0,
    row: 0,
    col: 0,
  };
  const operation = fixture.controller.commitCellEdit(target, {
    kind: "number",
    value: 1,
  });
  fixture.state.documentId = "document-two";
  fixture.elements["error-message"].textContent = "Current document notice";
  fixture.elements["status-message"].textContent = "Current document rendered";
  pending.resolve({
    workbook: fixture.workbook,
    editState: fixture.state.editState,
    recalculation: {
      computedCells: 2,
      unchangedCells: 0,
      unsupportedCells: 3,
      reasons: ["unsupported_function"],
    },
  });
  assert.equal(await operation, false);
  assert.equal(
    fixture.elements["error-message"].textContent,
    "Current document notice",
  );
  assert.equal(
    fixture.elements["status-message"].textContent,
    "Current document rendered",
  );
  assert.deepEqual(fixture.calls.renders, []);
});
