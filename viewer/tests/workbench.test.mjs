import test from "node:test";
import assert from "node:assert/strict";
import { createWorkbench, inspectorText } from "../src/workbench.js";

function classList() {
  const values = new Set();
  return {
    contains: (name) => values.has(name),
    toggle(name, enabled = !values.has(name)) {
      if (enabled) values.add(name);
      else values.delete(name);
      return enabled;
    },
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

function setup({ readOnly = false, readCell, onInspect } = {}) {
  const controls = new Map();
  const calls = { reads: [], edits: [], saves: 0, files: 0, zooms: [] };
  const dom = {
    activeElement: null,
    body: { classList: classList() },
    getElementById(id) {
      if (!controls.has(id)) {
        const listeners = new Map();
        const attributes = new Map();
        const element = {
          value: "",
          textContent: "",
          disabled: false,
          hidden: false,
          inert: false,
          tabIndex: 0,
          classList: classList(),
          addEventListener: (type, listener) => listeners.set(type, listener),
          emit(type, details = {}) {
            const event = {
              prevented: false,
              preventDefault() {
                this.prevented = true;
              },
              ...details,
            };
            return { event, result: listeners.get(type)?.(event) };
          },
          setAttribute: (name, value) => attributes.set(name, value),
          getAttribute: (name) => attributes.get(name),
          focus() {
            dom.activeElement = element;
          },
          contains: (candidate) =>
            candidate === element || candidate?.parent === element,
        };
        controls.set(id, element);
      }
      return controls.get(id);
    },
  };
  const ui = new Proxy({}, { get: (_target, id) => dom.getElementById(id) });
  const state = {
    documentId: "one",
    sheetIndex: 0,
    busy: false,
    workbook: { sheetCount: 2 },
    editState: { capability: "read-write", dirty: false },
    client: {
      readCell: async (...args) => {
        calls.reads.push(args);
        return readCell
          ? readCell(...args)
          : { value: { kind: "text", value: "Hello" }, formatted: "Hello" };
      },
    },
  };
  ui["inspector-reference"].value = "A1";
  ui["sidebar"].inert = true;
  const workbench = createWorkbench({
    state,
    readOnly,
    dom,
    editing: {
      openCellEditor: async () => calls.edits.push(ui["cell-reference"].value),
      saveWorkbookCopy: async () => {
        calls.saves += 1;
      },
    },
    openFile: () => {
      calls.files += 1;
    },
    setZoom: (value) => calls.zooms.push(value),
    onInspect,
  });
  workbench.update();
  return { workbench, state, ui, calls, dom };
}

test("inspector presentation preserves formulas, date serials, boolean meaning, and blanks", () => {
  const cases = [
    [{ kind: "blank" }, ""],
    [
      {
        kind: "formula",
        formula: "SUM(A1:A2)",
        cached: { kind: "number", value: 7 },
      },
      "=SUM(A1:A2)",
    ],
    [{ kind: "date", value: 45200.5 }, "45200.5"],
    [{ kind: "boolean", value: true }, "TRUE"],
    [{ kind: "boolean", value: false }, "FALSE"],
    [{ kind: "number", value: 0 }, "0"],
    [{ kind: "text", value: "00123" }, "00123"],
    [{ kind: "text", value: "<script>text</script>" }, "<script>text</script>"],
    [{ kind: "error", value: "#DIV/0!" }, "#DIV/0!"],
  ];
  for (const [cell, expected] of cases)
    assert.equal(inspectorText(cell), expected);
});

test("inspection normalizes a bounded reference and passes zero-based coordinates", async () => {
  const { workbench, ui, calls } = setup();
  ui["inspector-reference"].value = " $b$12 ";
  await workbench.inspect();
  assert.deepEqual(calls.reads, [["one", 0, 11, 1]]);
  assert.equal(ui["inspector-reference"].value, "B12");
  assert.equal(ui["inspector-value"].value, "Hello");
  assert.equal(ui["inspector-status"].textContent, "B12: Hello");
  assert.equal(ui["inspector-edit"].disabled, false);
  await ui["inspector-edit"].emit("click").result;
  assert.deepEqual(calls.edits, ["B12"]);
});

test("sheet selection synchronizes both address fields without inspection feedback loops", async () => {
  const notifications = [];
  const { workbench, ui, calls } = setup({
    onInspect: (cell) => notifications.push(cell),
  });
  await workbench.selectCell({ reference: "C4" });
  assert.equal(ui["inspector-reference"].value, "C4");
  assert.equal(ui["cell-reference"].value, "C4");
  assert.deepEqual(calls.reads, [["one", 0, 3, 2]]);
  assert.deepEqual(notifications, []);
  await workbench.selectCell({ reference: "C4" });
  assert.equal(calls.reads.length, 1);
  ui["inspector-reference"].value = "B2";
  await workbench.inspect();
  assert.equal(notifications.length, 1);
  assert.equal(notifications[0].row, 1);
  assert.equal(notifications[0].col, 1);
});

test("invalid addresses fail locally and never enable inspected-cell editing", async () => {
  const { workbench, ui, calls } = setup();
  for (const reference of ["", "A0", "XFE1", "A1048577", "A1:B2", "1A"]) {
    ui["inspector-reference"].value = reference;
    ui["inspector-reference"].emit("input");
    await workbench.inspect();
    assert.equal(ui["inspector-edit"].disabled, true, reference);
    assert.match(
      ui["inspector-value"].value,
      /cell reference|worksheet grid/,
      reference,
    );
    await ui["inspector-edit"].emit("click").result;
  }
  assert.deepEqual(calls.reads, []);
  assert.deepEqual(calls.edits, []);
});

test("the formula bar mirrors drafts and delayed reads cannot overwrite typing", async () => {
  const delayed = deferred();
  let first = true;
  const { workbench, state, ui } = setup({
    readCell: () => {
      if (first) {
        first = false;
        return delayed.promise;
      }
      return { value: { kind: "number", value: 7 }, formatted: "7" };
    },
  });
  const reading = workbench.inspect();
  workbench.setDraft({
    client: state.client,
    documentId: state.documentId,
    sheetIndex: state.sheetIndex,
    reference: "A1",
    text: "=SUM(B1:B3)",
  });
  delayed.resolve({ value: { kind: "text", value: "old" }, formatted: "old" });
  await reading;
  assert.equal(ui["inspector-value"].value, "=SUM(B1:B3)");
  assert.equal(ui["inspector-kind"].textContent, "Editing");
  assert.equal(ui["inspector-reference"].disabled, true);
  await workbench.refresh();
  assert.equal(ui["inspector-value"].value, "=SUM(B1:B3)");
  workbench.setDraft(null);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(ui["inspector-value"].value, "7");
  assert.equal(ui["inspector-reference"].disabled, false);
});

test("typing another address clears a loaded value and blocks its edit route", async () => {
  const { workbench, ui, calls } = setup();
  await workbench.inspect();
  ui["inspector-reference"].value = "C9";
  ui["inspector-reference"].emit("input");
  assert.equal(ui["inspector-value"].value, "");
  assert.equal(ui["inspector-edit"].disabled, true);
  await ui["inspector-edit"].emit("click").result;
  assert.deepEqual(calls.edits, []);
});

test("typing while a read is pending rejects its delayed response", async () => {
  const pending = deferred();
  const { workbench, ui } = setup({ readCell: () => pending.promise });
  const inspection = workbench.inspect();
  ui["inspector-reference"].value = "B2";
  ui["inspector-reference"].emit("input");
  pending.resolve({ value: { kind: "text", value: "old" }, formatted: "old" });
  await inspection;
  assert.equal(ui["inspector-value"].value, "");
  assert.equal(
    ui["inspector-status"].textContent,
    "Press Enter to read the cell.",
  );
  assert.equal(ui["inspector-edit"].disabled, true);
});

for (const changed of ["documentId", "sheetIndex", "client"]) {
  test(`a ${changed} change rejects a delayed inspection without opening the editor`, async () => {
    const pending = deferred();
    const { workbench, ui, state, calls } = setup({
      readCell: () => pending.promise,
    });
    const inspection = workbench.inspect();
    state[changed] =
      changed === "sheetIndex" ? 1 : changed === "client" ? {} : "two";
    pending.resolve({
      value: { kind: "text", value: "old" },
      formatted: "old",
    });
    await inspection;
    assert.equal(ui["inspector-value"].value, "");
    assert.equal(ui["inspector-edit"].disabled, true);
    await ui["inspector-edit"].emit("click").result;
    assert.deepEqual(calls.edits, []);
  });
}

test("a later inspection wins even when the previous read fails afterward", async () => {
  const old = deferred();
  const current = deferred();
  let count = 0;
  const { workbench, ui } = setup({
    readCell: () => (++count === 1 ? old.promise : current.promise),
  });
  const first = workbench.inspect();
  ui["inspector-reference"].value = "B2";
  ui["inspector-reference"].emit("input");
  const second = workbench.inspect();
  current.resolve({ value: { kind: "number", value: 42 }, formatted: "42" });
  await second;
  old.reject(new Error("obsolete failure"));
  await first;
  assert.equal(ui["inspector-value"].value, "42");
  assert.equal(ui["inspector-status"].textContent, "B2: 42");
  assert.equal(ui["inspector-edit"].disabled, false);
});

test("refresh keeps the reference for the same workbook but resets it on a sheet change", async () => {
  const { workbench, ui, state, calls } = setup();
  await workbench.refresh();
  ui["inspector-reference"].value = "D4";
  await workbench.refresh();
  assert.equal(ui["inspector-reference"].value, "D4");
  state.sheetIndex = 1;
  await workbench.refresh();
  assert.equal(ui["inspector-reference"].value, "A1");
  assert.deepEqual(calls.reads, [
    ["one", 0, 0, 0],
    ["one", 0, 3, 3],
    ["one", 1, 0, 0],
  ]);
});

test("read-only hosts can inspect values but never route inspected cells into editing", async () => {
  const { workbench, ui, calls } = setup({ readOnly: true });
  await workbench.inspect();
  assert.equal(ui["inspector-value"].value, "Hello");
  assert.equal(ui["inspector-edit"].disabled, true);
  assert.equal(ui["quick-save"].disabled, true);
  assert.equal(ui["workbook-mode"].textContent, "Read-only");
  await ui["inspector-edit"].emit("click").result;
  assert.deepEqual(calls.edits, []);
});

test("read-only formats and busy or absent workbooks disable unavailable commands", async () => {
  const { workbench, ui, state, calls } = setup();
  state.editState.capability = "read-only";
  await workbench.inspect();
  assert.equal(ui["inspector-edit"].disabled, true);
  assert.equal(ui["workbook-mode"].textContent, "Read-only");
  state.busy = true;
  workbench.update();
  assert.equal(ui["inspect-cell"].disabled, true);
  assert.equal(ui["inspector-reference"].disabled, true);
  assert.equal(ui["reset-zoom"].disabled, true);
  await workbench.inspect();
  assert.equal(calls.reads.length, 1);
  state.busy = false;
  state.workbook = null;
  await workbench.refresh();
  assert.equal(ui["inspector-value"].value, "");
  assert.equal(ui["workbook-mode"].textContent, "No workbook");
});

test("date inspection exposes the source serial and separately reports formatted text", async () => {
  const { workbench, ui } = setup({
    readCell: async () => ({
      value: { kind: "date", value: 45200.5 },
      formatted: "2023-10-01 12:00",
    }),
  });
  await workbench.inspect();
  assert.equal(ui["inspector-value"].value, "45200.5");
  assert.equal(ui["inspector-kind"].textContent, "Date serial");
  assert.match(ui["inspector-status"].textContent, /2023-10-01 12:00/);
});

test("ribbon tabs support arrow/Home/End keyboard navigation with a single tab stop", () => {
  const { ui, dom } = setup();
  ui["tab-view"].emit("click");
  for (const [from, key, selected] of [
    ["view", "ArrowRight", "home"],
    ["home", "ArrowLeft", "view"],
    ["view", "Home", "home"],
    ["home", "End", "view"],
  ]) {
    const { event } = ui[`tab-${from}`].emit("keydown", { key });
    assert.equal(event.prevented, true);
    for (const name of ["home", "view"]) {
      const active = name === selected;
      assert.equal(
        ui[`tab-${name}`].getAttribute("aria-selected"),
        String(active),
      );
      assert.equal(ui[`tab-${name}`].tabIndex, active ? 0 : -1);
      assert.equal(ui[`ribbon-${name}`].hidden, !active);
    }
    assert.equal(dom.activeElement, ui[`tab-${selected}`]);
  }
  assert.equal(
    ui["tab-view"].emit("keydown", { key: "Tab" }).event.prevented,
    false,
  );
});

test("inspector visibility toggles together with the ribbon button's pressed state", () => {
  const { ui } = setup();
  ui["toggle-inspector"].emit("click");
  assert.equal(ui["cell-inspector"].hidden, true);
  assert.equal(ui["toggle-inspector"].getAttribute("aria-pressed"), "false");
  ui["toggle-inspector"].emit("click");
  assert.equal(ui["cell-inspector"].hidden, false);
  assert.equal(ui["toggle-inspector"].getAttribute("aria-pressed"), "true");
});

test("workbook panel class, inert state, and toggle ARIA remain synchronized", () => {
  const { workbench, ui, dom } = setup();
  ui["ribbon-file"].emit("click");
  assert.equal(dom.body.classList.contains("sidebar-open"), true);
  assert.equal(ui.sidebar.inert, false);
  for (const id of ["sidebar-toggle", "ribbon-file"]) {
    assert.equal(ui[id].getAttribute("aria-expanded"), "true");
  }
  dom.activeElement = { parent: ui.sidebar };
  workbench.closePanel();
  assert.equal(dom.body.classList.contains("sidebar-open"), false);
  assert.equal(ui.sidebar.inert, true);
  assert.equal(dom.activeElement, ui["sidebar-toggle"]);
  for (const id of ["sidebar-toggle", "ribbon-file"]) {
    assert.equal(ui[id].getAttribute("aria-expanded"), "false");
  }
  ui["view-workbook-panel"].emit("click");
  assert.equal(ui.sidebar.inert, false);
  workbench.togglePanel();
  assert.equal(ui.sidebar.inert, true);
});

test("ribbon commands delegate open/save/reset without mutating workbook data", () => {
  const { ui, state, calls, workbench } = setup();
  ui["panel-open"].emit("click");
  ui["quick-save"].emit("click");
  ui["reset-zoom"].emit("click");
  assert.equal(calls.files, 1);
  assert.equal(calls.saves, 1);
  assert.deepEqual(calls.zooms, [1]);
  state.editState.dirty = true;
  workbench.update();
  assert.equal(
    ui["workbook-mode"].textContent,
    "Unsaved changes · save a copy",
  );
});
