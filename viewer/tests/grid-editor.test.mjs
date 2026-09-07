import test from "node:test";
import assert from "node:assert/strict";
import {
  createGridEditor,
  gridCells,
  gridPointer,
  inlineCellValue,
} from "../src/grid-editor.js";

const interaction = {
  schemaVersion: 1,
  width: 150,
  height: 40,
  cells: [
    [0, 0, 0, 0, 100, 20],
    [0, 2, 100, 0, 50, 20],
    [2, 0, 0, 20, 50, 20],
    [2, 2, 100, 20, 50, 20],
  ],
};
const flush = () => new Promise((resolve) => setImmediate(resolve));
function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
function element() {
  const listeners = new Map();
  const classes = new Set();
  const attrs = new Map();
  let value = "";
  return {
    get value() {
      return value;
    },
    set value(next) {
      value = String(next).replace(/\r\n?/g, "\n");
    },
    hidden: true,
    disabled: false,
    style: {},
    dataset: {},
    parentElement: null,
    classList: {
      contains: (name) => classes.has(name),
      toggle(name, value) {
        if (value) classes.add(name);
        else classes.delete(name);
      },
    },
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    async fire(type, details = {}) {
      const event = {
        target: this,
        prevented: false,
        stopped: false,
        preventDefault() {
          this.prevented = true;
        },
        stopPropagation() {
          this.stopped = true;
        },
        ...details,
      };
      await listeners.get(type)?.(event);
      await flush();
      return event;
    },
    append(child) {
      child.parentElement = this;
    },
    setAttribute(name, value) {
      attrs.set(name, value);
    },
    getAttribute: (name) => attrs.get(name),
    focus() {
      this.focused = true;
    },
    setSelectionRange(start, end) {
      this.selectionStart = start;
      this.selectionEnd = end;
    },
    getBoundingClientRect: () => ({ left: 100, top: 50 }),
  };
}
function setup({ readOnly = false, readCell, commitCellEdit } = {}) {
  const calls = { reads: [], commits: [], selections: [], errors: [] };
  const elements = Object.fromEntries(
    [
      "document-surface",
      "grid-layer",
      "grid-selection",
      "grid-input",
      "grid-status",
    ].map((id) => [id, element()]),
  );
  const svg = {
    getScreenCTM: () => ({
      a: 2,
      b: 0,
      c: 0,
      d: 2,
      e: 100,
      f: 50,
      inverse: () => ({ a: 0.5, b: 0, c: 0, d: 0.5, e: -50, f: -25 }),
    }),
  };
  const state = {
    mode: "sheet",
    busy: false,
    documentId: "one",
    sheetIndex: 0,
    workbook: { sheetCount: 1 },
    editState: { capability: "read-write" },
    client: {
      readCell: async (...args) => {
        calls.reads.push(args);
        return readCell
          ? readCell(...args)
          : { value: { kind: "number", value: 10 }, formatted: "10" };
      },
    },
  };
  const grid = createGridEditor({
    state,
    elements,
    readOnly,
    editing: {
      commitCellEdit: async (...args) => {
        calls.commits.push(args);
        return commitCellEdit ? commitCellEdit(...args) : true;
      },
    },
    onSelection: (value) => calls.selections.push(value),
    showError: (error) => calls.errors.push(error),
  });
  grid.mount(interaction, svg);
  return {
    grid,
    state,
    calls,
    elements,
    svg,
    input: elements["grid-input"],
    outline: elements["grid-selection"],
    layer: elements["grid-layer"],
    surface: elements["document-surface"],
  };
}

test("inline values preserve text intent and date serials without fabricating formula caches", () => {
  for (const [text, expected] of [
    ["", { kind: "blank" }],
    ["'00123", { kind: "text", value: "00123" }],
    ["00123", { kind: "text", value: "00123" }],
    ["12.5", { kind: "number", value: 12.5 }],
    ["-.5", { kind: "number", value: -0.5 }],
    ["1e3", { kind: "number", value: 1000 }],
    ["TRUE", { kind: "boolean", value: true }],
    ["false", { kind: "boolean", value: false }],
    ["=SUM(A1:A2)", { kind: "formula-auto", formula: "SUM(A1:A2)" }],
    ["'=1", { kind: "text", value: "=1" }],
    [" 12 ", { kind: "text", value: " 12 " }],
    ["9/7/2026", { kind: "text", value: "9/7/2026" }],
  ])
    assert.deepEqual(inlineCellValue(text), expected);
  assert.deepEqual(inlineCellValue("45200.5", { kind: "date", value: 1 }), {
    kind: "date",
    value: 45200.5,
  });
  assert.throws(() => inlineCellValue("="), /formula/);
  assert.throws(() => inlineCellValue("1e999"), /finite/);
});

test("interactive geometry rejects invalid bounds, duplicates, and non-finite boxes", () => {
  assert.deepEqual(
    gridCells(interaction).map((cell) => cell.reference),
    ["A1", "C1", "A3", "C3"],
  );
  for (const cell of [
    [-1, 0, 0, 0, 1, 1],
    [0, 16384, 0, 0, 1, 1],
    [0, 0, 0, 0, 0, 1],
    [0, 0, NaN, 0, 1, 1],
    [0, 0, -1, 0, 10, 10],
    [0, 0, 149, 0, 2, 10],
    [0, 0, 0, 39, 1, 2],
  ]) {
    assert.throws(
      () => gridCells({ ...interaction, cells: [cell] }),
      /geometry/,
    );
  }
  assert.throws(
    () =>
      gridCells({
        ...interaction,
        cells: [interaction.cells[0], interaction.cells[0]],
      }),
    /geometry/,
  );
  assert.throws(
    () => gridCells({ ...interaction, schemaVersion: 2 }),
    /geometry/,
  );
  assert.throws(
    () => gridCells({ ...interaction, cells: new Array(250_001) }),
    /geometry/,
  );
});

test("pointer conversion includes SVG CSS scale and safely rejects detached or singular transforms", () => {
  const { svg } = setup();
  assert.deepEqual(gridPointer(svg, 320, 60), { x: 110, y: 5 });
  assert.equal(gridPointer({ getScreenCTM: () => null }, 1, 1), null);
  assert.equal(
    gridPointer(
      {
        getScreenCTM: () => ({
          inverse() {
            throw new Error("singular");
          },
        }),
      },
      1,
      1,
    ),
    null,
  );
  assert.equal(gridPointer(svg, Infinity, 1), null);
});

test("mount creates a single transparent native input over the exact merged anchor box", async () => {
  const { grid, input, outline, layer, surface, calls } = setup();
  await flush();
  assert.equal(outline.dataset.reference, "A1");
  assert.equal(outline.style.width, "200px");
  assert.equal(outline.style.height, "40px");
  assert.equal(
    input.style.fontSize,
    "28px",
    "inline text follows the SVG screen scale",
  );
  assert.equal(input.hidden, false);
  assert.equal(layer.parentElement, surface);
  assert.equal(layer.classList.contains("is-editing"), false);
  assert.equal(grid.hasDraft(), false);
  assert.deepEqual(calls.reads, [["one", 0, 0, 0]]);
});

test("pointer hits retain source coordinates and inspector same-cell selection cannot recurse", async () => {
  const { surface, outline, grid, calls } = setup();
  await surface.fire("pointerdown", { clientX: 320, clientY: 60, button: 0 });
  assert.equal(outline.dataset.reference, "C1");
  assert.equal(outline.style.left, "200px");
  const count = calls.selections.length;
  await grid.select(0, 2, { focus: false });
  assert.equal(calls.selections.length, count);
  assert.equal(
    await grid.select(0, 1),
    false,
    "covered merged-cell coordinate must not exist",
  );
});

test("one painted-cell click opens the existing value without changing the workbook", async () => {
  const { surface, input, grid, calls } = setup();
  await surface.fire("pointerdown", { clientX: 320, clientY: 60, button: 0 });
  assert.equal(grid.hasDraft(), true);
  assert.equal(input.value, "10");
  assert.equal(input.selectionStart, 2);
  assert.equal(grid.hasChanges(), false);
  assert.deepEqual(calls.commits, []);
});

test("one selected-input click edits, then native caret and word selection remain untouched", async () => {
  const { surface, input, grid } = setup();
  await flush();
  await surface.fire("pointerdown", { target: input, button: 0 });
  assert.equal(grid.hasDraft(), true);
  assert.equal(input.value, "10");
  input.setSelectionRange(1, 1);
  const click = await surface.fire("pointerdown", { target: input, button: 0 });
  assert.equal(click.prevented, false);
  assert.equal(input.selectionStart, 1);
  const doubleClick = await surface.fire("dblclick", { target: input });
  assert.equal(doubleClick.prevented, false);
  assert.equal(input.selectionStart, 1);
});

test("clicking through unchanged cells never coerces source values or adds history", async () => {
  for (const value of [
    { kind: "text", value: "TRUE" },
    { kind: "text", value: "123" },
    { kind: "text", value: "a\r\nb\rc" },
    {
      kind: "formula",
      formula: "UNKNOWN(1)",
      cached: { kind: "number", value: 8 },
    },
  ]) {
    const { surface, input, outline, grid, calls } = setup({
      readCell: async () => ({ value }),
    });
    await flush();
    await surface.fire("pointerdown", { target: input, button: 0 });
    await surface.fire("pointerdown", { clientX: 320, clientY: 60, button: 0 });
    assert.equal(outline.dataset.reference, "C1");
    assert.equal(grid.hasDraft(), true);
    assert.equal(grid.hasChanges(), false);
    await input.fire("keydown", { key: "Tab" });
    assert.deepEqual(calls.commits, []);
  }
});

test("one-click reads guard input until the existing value arrives and cannot reopen a cancelled draft", async () => {
  const pending = deferred();
  const { surface, input, grid } = setup({ readCell: () => pending.promise });
  await surface.fire("pointerdown", { target: input, button: 0 });
  assert.equal(grid.hasDraft(), true);
  assert.equal(input.disabled, true);
  await input.fire("keydown", { key: "7" });
  assert.equal(input.value, "");
  grid.cancel();
  pending.resolve({ value: { kind: "number", value: 42 } });
  await flush();
  assert.equal(grid.hasDraft(), false);
  assert.equal(input.value, "");
});

test("a delayed one-click read cannot edit a more recently clicked cell", async () => {
  const pending = deferred();
  const { surface, input, outline, grid } = setup({
    readCell: (_id, _sheet, row, col) =>
      row === 0 && col === 2
        ? pending.promise
        : { value: { kind: "text", value: "Current" } },
  });
  await surface.fire("pointerdown", { clientX: 320, clientY: 60, button: 0 });
  await surface.fire("pointerdown", { clientX: 320, clientY: 100, button: 0 });
  assert.equal(outline.dataset.reference, "C3");
  assert.equal(input.value, "Current");
  grid.cancel();
  pending.resolve({ value: { kind: "text", value: "Stale" } });
  await flush();
  assert.equal(grid.hasDraft(), false);
  assert.equal(outline.dataset.reference, "C3");
});

test("an unchanged composition remains protected when another cell is clicked", async () => {
  const { surface, input, outline, grid, calls } = setup();
  await flush();
  await surface.fire("pointerdown", { target: input, button: 0 });
  await input.fire("compositionstart");
  await surface.fire("pointerdown", { clientX: 320, clientY: 60, button: 0 });
  assert.equal(outline.dataset.reference, "A1");
  assert.equal(input.value, "10");
  assert.equal(grid.hasChanges(), true);
  assert.deepEqual(calls.commits, []);
});

test("a pending one-click editor becomes editable with its source value after loading", async () => {
  const pending = deferred();
  const { surface, input, grid } = setup({ readCell: () => pending.promise });
  await surface.fire("pointerdown", { target: input, button: 0 });
  assert.equal(input.disabled, true);
  pending.resolve({ value: { kind: "number", value: 42 } });
  await flush();
  assert.equal(input.disabled, false);
  assert.equal(input.value, "42");
  assert.equal(input.selectionStart, 2);
  assert.equal(grid.hasChanges(), false);
});

test("focused navigation reveals the cell while inspector synchronization never steals scroll", async () => {
  const { grid, outline } = setup();
  const reveals = [];
  outline.scrollIntoView = (options) => reveals.push(options);
  await grid.select(0, 2, { focus: false });
  assert.equal(reveals.length, 0);
  await grid.select(2, 0);
  assert.deepEqual(reveals, [{ block: "nearest", inline: "nearest" }]);
});

test("keyboard navigation skips hidden/covered cells and stays bounded to rendered anchors", async () => {
  const { input, outline } = setup();
  await input.fire("keydown", { key: "ArrowRight" });
  assert.equal(outline.dataset.reference, "C1");
  await input.fire("keydown", { key: "Tab" });
  assert.equal(outline.dataset.reference, "A3");
  await input.fire("keydown", { key: "Tab", shiftKey: true });
  assert.equal(outline.dataset.reference, "C1");
  await input.fire("keydown", { key: "ArrowLeft" });
  assert.equal(outline.dataset.reference, "A1");
  await input.fire("keydown", { key: "ArrowLeft" });
  assert.equal(outline.dataset.reference, "A1");
});

test("F2/Enter edit the selected source value; Escape and blur never write", async () => {
  const { grid, input, calls } = setup();
  await input.fire("keydown", { key: "F2" });
  assert.equal(input.value, "10");
  input.value = "draft";
  await input.fire("input");
  await input.fire("blur");
  assert.equal(grid.hasDraft(), true);
  assert.deepEqual(calls.commits, []);
  await input.fire("keydown", { key: "Escape" });
  assert.equal(grid.hasDraft(), false);
  await input.fire("keydown", { key: "Enter" });
  assert.equal(grid.hasDraft(), true);
  assert.equal(input.value, "10");
});

test("typing replaces a selected cell and Enter commits before navigating down", async () => {
  const { grid, input, calls, outline } = setup();
  await input.fire("keydown", { key: "7" });
  assert.equal(input.value, "7");
  await input.fire("keydown", { key: "Enter" });
  assert.equal(grid.hasDraft(), false);
  assert.equal(calls.commits.length, 1);
  assert.deepEqual(calls.commits[0][1], { kind: "number", value: 7 });
  assert.equal(calls.commits[0][0].row, 0);
  assert.equal(calls.commits[0][0].col, 0);
  assert.equal(outline.dataset.reference, "A3");
});

test("native IME Enter confirms composition without committing the cell", async () => {
  const { grid, input, calls } = setup();
  await flush();
  await input.fire("compositionstart");
  input.value = "한글";
  await input.fire("input");
  await input.fire("keydown", {
    key: "Enter",
    isComposing: true,
    keyCode: 229,
  });
  assert.equal(await grid.commit(), false);
  assert.deepEqual(calls.commits, []);
  await input.fire("compositionend");
  assert.equal(grid.hasDraft(), true);
  await input.fire("keydown", { key: "Enter" });
  assert.deepEqual(calls.commits[0][1], { kind: "text", value: "한글" });
});

test("an out-of-order read never replaces the selected cell or typed draft", async () => {
  const pending = deferred();
  const { grid, input, outline } = setup({
    readCell: async (_id, _sheet, _row, col) =>
      col === 0
        ? pending.promise
        : { value: { kind: "text", value: "Current" } },
  });
  await grid.select(0, 2);
  await grid.beginEdit("Keep me");
  pending.resolve({ value: { kind: "text", value: "Stale" } });
  await flush();
  assert.equal(outline.dataset.reference, "C1");
  assert.equal(input.value, "Keep me");
});

test("a stale document or sheet cannot receive an inline draft commit", async () => {
  const { grid, state, calls, input } = setup();
  await grid.beginEdit("new");
  state.documentId = "two";
  assert.equal(await grid.commit(), false);
  assert.equal(grid.hasDraft(), true);
  assert.equal(input.value, "new");
  assert.deepEqual(calls.commits, []);
});

test("worker formula errors preserve the complete draft and permit a corrected retry", async () => {
  let fail = true;
  const { grid, input, calls } = setup({
    commitCellEdit: async () => {
      if (fail)
        throw new Error(
          "Unsupported formula: no deterministic cache available",
        );
      return true;
    },
  });
  await grid.beginEdit("=UNSUPPORTED(A1)");
  assert.equal(await grid.commit(), false);
  assert.equal(input.value, "=UNSUPPORTED(A1)");
  assert.equal(grid.hasDraft(), true);
  fail = false;
  input.value = "=1+2";
  assert.equal(await grid.commit(), true);
  assert.deepEqual(calls.commits[1][1], {
    kind: "formula-auto",
    formula: "1+2",
  });
});

test("own-commit remount preserves draft until success and blocks simultaneous commands", async () => {
  const pending = deferred();
  const fixture = setup({
    commitCellEdit: async () => {
      fixture.state.busy = true;
      fixture.grid.update();
      fixture.state.busy = false;
      fixture.grid.mount(interaction, fixture.svg);
      return pending.promise;
    },
  });
  await fixture.grid.beginEdit("77");
  const commit = fixture.grid.commit();
  assert.equal(fixture.input.value, "77");
  assert.equal(fixture.grid.hasDraft(), true);
  assert.equal(await fixture.grid.commit(), false);
  assert.equal(fixture.grid.cancel(), false);
  pending.resolve(true);
  assert.equal(await commit, true);
  assert.equal(fixture.grid.hasDraft(), false);
  assert.equal(fixture.outline.dataset.reference, "A1");
});

test("a late successful write cannot clear a draft for a now-different document", async () => {
  const pending = deferred();
  const { grid, state, input } = setup({
    commitCellEdit: () => pending.promise,
  });
  await grid.beginEdit("77");
  const commit = grid.commit();
  state.documentId = "two";
  pending.resolve(true);
  assert.equal(await commit, false);
  assert.equal(grid.hasDraft(), true);
  assert.equal(input.value, "77");
});

test("read-only, page, and busy states cannot start inline editing", async () => {
  for (const mode of ["host", "format", "page", "busy"]) {
    const { grid, state, calls } = setup({ readOnly: mode === "host" });
    if (mode === "format") state.editState.capability = "read-only";
    if (mode === "page") state.mode = "page";
    if (mode === "busy") state.busy = true;
    grid.update();
    assert.equal(await grid.beginEdit("99"), false, mode);
    assert.equal(
      await grid.commit(),
      true,
      "no draft must not block latest-open requests",
    );
    assert.deepEqual(calls.commits, []);
  }
});

test("clicking another cell while drafting does not save or discard that draft", async () => {
  const { grid, surface, outline, input, calls } = setup();
  await grid.beginEdit("do not lose");
  await surface.fire("pointerdown", { clientX: 320, clientY: 60, button: 0 });
  assert.equal(outline.dataset.reference, "A1");
  assert.equal(input.value, "do not lose");
  assert.deepEqual(calls.commits, []);
});
