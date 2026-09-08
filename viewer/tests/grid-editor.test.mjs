import test from "node:test";
import assert from "node:assert/strict";
import { gridCells, gridPointer, inlineCellValue } from "../src/grid-editor.js";

import {
  setup,
  element,
  deferred,
  flush,
  interaction,
} from "./support/grid-editor.mjs";

test("range paste keeps the draft on failure and clears it only after one successful transaction", async () => {
  let fail = true;
  const env = setup({
    commitRangeEdit: async () => {
      if (fail) throw new Error("Merged cell interior");
      return true;
    },
  });
  await flush();
  await env.grid.beginEdit("original draft");
  const target = env.grid.getPasteTarget();
  const values = [[{ kind: "number", value: 2 }, { kind: "blank" }]];
  assert.equal(await env.grid.applyRange(target, values), false);
  assert.equal(env.input.value, "original draft");
  assert.equal(env.grid.hasChanges(), true);
  assert.equal(env.calls.commits.length, 0);
  fail = false;
  assert.equal(await env.grid.applyRange(target, values), true);
  assert.equal(env.grid.hasDraft(), false);
  assert.equal(env.calls.ranges.length, 2);
  assert.deepEqual(env.calls.ranges[1][1], values);
});

test("range paste is unavailable in read-only, busy, IME and stale-open contexts", async () => {
  assert.equal(setup({ readOnly: true }).grid.getPasteTarget(), null);
  const env = setup();
  await flush();
  const target = env.grid.getPasteTarget();
  env.state.openGeneration = 1;
  assert.equal(await env.grid.applyRange(target, [[{ kind: "blank" }]]), false);
  assert.equal(env.calls.ranges.length, 0);
  env.state.busy = true;
  assert.equal(env.grid.getPasteTarget(), null);
  env.state.busy = false;
  await env.input.fire("compositionstart");
  assert.equal(env.grid.getPasteTarget(), null);
});

test("a late range completion cannot clear a draft or focus a replacement workbook", async () => {
  const pending = deferred();
  const env = setup({ commitRangeEdit: () => pending.promise });
  await flush();
  await env.grid.beginEdit("keep me");
  const applying = env.grid.applyRange(env.grid.getPasteTarget(), [
    [{ kind: "blank" }],
  ]);
  env.state.openGeneration = 1;
  pending.resolve(true);
  assert.equal(await applying, false);
  assert.equal(env.input.value, "keep me");
  assert.equal(env.grid.hasChanges(), true);
});

test("large-grid selection and Tab avoid linear lookup and arrows avoid candidate sorting", async () => {
  const env = setup();
  const cells = Array.from({ length: 10_000 }, (_, i) => [
    Math.floor(i / 100),
    i % 100,
    (i % 100) * 50,
    Math.floor(i / 100) * 20,
    50,
    20,
  ]);
  env.grid.mount(
    { schemaVersion: 1, width: 5000, height: 2000, cells },
    env.svg,
  );
  await flush();
  assert.equal(await env.grid.select("50", 50), false);
  const originals = Object.fromEntries(
    ["find", "findIndex", "filter", "sort"].map((key) => [
      key,
      Array.prototype[key],
    ]),
  );
  const scans = [];
  try {
    for (const [key, original] of Object.entries(originals))
      Array.prototype[key] = function (...args) {
        if (this.length >= 100 && this[0]?.reference) scans.push(key);
        return Reflect.apply(original, this, args);
      };
    await env.grid.select(50, 50);
    await env.input.fire("keydown", { key: "Tab" });
    await env.input.fire("keydown", { key: "ArrowDown" });
    assert.equal(env.outline.dataset.reference, "AZ52");
    assert.deepEqual(scans, []);
  } finally {
    for (const [key, original] of Object.entries(originals))
      Array.prototype[key] = original;
  }
});

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
  const { input, outline, calls } = setup();
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
  assert.deepEqual(calls.exits, []);
});

test("Tab and Shift+Tab focus real outside targets at the last and first cell", async () => {
  for (const [direction, row, col, shiftKey] of [
    ["next", 2, 2, false],
    ["previous", 0, 0, true],
  ]) {
    const { grid, input, calls, exitTargets } = setup();
    await grid.select(row, col);
    const event = await input.fire("keydown", { key: "Tab", shiftKey });
    assert.equal(event.prevented, true);
    assert.equal(event.stopped, true);
    assert.deepEqual(calls.exits, [direction]);
    assert.equal(exitTargets[direction].focused, true);
    assert.deepEqual(calls.commits, []);
    assert.equal(grid.hasDraft(), false);
  }
});

test("edge Tab waits for its own async commit and remount before moving focus", async () => {
  for (const [direction, row, col, shiftKey] of [
    ["next", 2, 2, false],
    ["previous", 0, 0, true],
  ]) {
    const pending = deferred();
    const fixture = setup({
      commitCellEdit: () => {
        fixture.grid.mount(interaction, fixture.svg);
        return pending.promise;
      },
    });
    await fixture.grid.select(row, col);
    await fixture.grid.beginEdit("77");
    const event = await fixture.input.fire("keydown", { key: "Tab", shiftKey });
    assert.equal(
      event.prevented,
      true,
      "default is cancelled before the async write",
    );
    assert.deepEqual(fixture.calls.exits, []);
    assert.equal(fixture.grid.hasDraft(), true);
    pending.resolve(true);
    await flush();
    assert.deepEqual(fixture.calls.exits, [direction]);
    assert.equal(fixture.exitTargets[direction].focused, true);
    assert.equal(fixture.grid.hasDraft(), false);
    assert.equal(fixture.calls.commits.length, 1);
  }
});

test("edge Tab preserves failed or stale drafts without moving focus", async () => {
  for (const outcome of ["failed", "rejected", "stale"]) {
    const pending = deferred();
    const { grid, state, input, calls } = setup({
      commitCellEdit: () => pending.promise,
    });
    await grid.select(2, 2);
    await grid.beginEdit("77");
    await input.fire("keydown", { key: "Tab" });
    if (outcome === "failed") pending.reject(new Error("Save failed"));
    else if (outcome === "rejected") pending.resolve(false);
    else {
      state.documentId = "another workbook";
      pending.resolve(true);
    }
    await flush();
    assert.deepEqual(calls.exits, [], outcome);
    assert.equal(grid.hasDraft(), true, outcome);
    assert.equal(input.value, "77", outcome);
  }
});

test("failed keyboard commits restore the retained draft after disabling blurs the input", async () => {
  for (const key of ["Tab", "Enter"]) {
    for (const outcome of ["failed", "rejected"]) {
      const pending = deferred();
      const { grid, input, calls } = setup({
        commitCellEdit: () => pending.promise,
      });
      const document = { activeElement: null, body: element() };
      input.ownerDocument = document;
      await grid.select(2, 2);
      await grid.beginEdit("=UNKNOWNFUNCTION()");
      assert.equal(document.activeElement, input);
      await input.fire("keydown", { key });
      assert.equal(input.disabled, true);
      assert.equal(document.activeElement, document.body);
      if (outcome === "failed")
        pending.reject(new Error("Unsupported formula"));
      else pending.resolve(false);
      await flush();
      assert.equal(input.disabled, false);
      assert.equal(document.activeElement, input, `${key}: ${outcome}`);
      assert.equal(input.value, "=UNKNOWNFUNCTION()");
      assert.equal(grid.hasDraft(), true);
      assert.deepEqual(calls.exits, []);
    }
  }
});

test("failed commits never steal focus after a user focus move or workbook change", async () => {
  for (const outcome of ["other-control", "stale", "toolbar"]) {
    const pending = deferred();
    const { grid, state, input } = setup({
      commitCellEdit: () => pending.promise,
    });
    const document = { activeElement: null, body: element() };
    input.ownerDocument = document;
    await grid.select(2, 2);
    await grid.beginEdit("77");
    const otherControl = element();
    if (outcome === "toolbar") document.activeElement = otherControl;
    const committing = grid.commit();
    if (outcome === "other-control") document.activeElement = otherControl;
    if (outcome === "stale") state.documentId = "another workbook";
    pending.resolve(false);
    assert.equal(await committing, false);
    assert.equal(
      document.activeElement,
      outcome === "stale" ? document.body : otherControl,
    );
    assert.equal(grid.hasDraft(), true);
    assert.equal(input.value, "77");
  }
});

test("edge Tab can exit when its committed blank removes the last rendered cell", async () => {
  const fixture = setup({
    commitCellEdit: async () => {
      fixture.grid.mount(
        { ...interaction, cells: interaction.cells.slice(0, 1) },
        fixture.svg,
      );
      return true;
    },
  });
  await fixture.grid.select(2, 2);
  await fixture.grid.beginEdit("");
  await fixture.input.fire("keydown", { key: "Tab" });
  assert.equal(fixture.grid.hasDraft(), false);
  assert.deepEqual(fixture.calls.commits[0][1], { kind: "blank" });
  assert.deepEqual(fixture.calls.exits, ["next"]);
  assert.equal(fixture.exitTargets.next.focused, true);
});

test("edge Tab preserves selection when no outside target can accept focus", async () => {
  const { grid, input, outline, calls, exitTargets } = setup({
    focusOutsideGrid: () => false,
  });
  await grid.select(2, 2);
  await input.fire("keydown", { key: "Tab" });
  assert.deepEqual(calls.exits, ["next"]);
  assert.equal(exitTargets.next.focused, undefined);
  assert.equal(outline.dataset.reference, "C3");
  assert.equal(grid.hasDraft(), false);
  assert.deepEqual(calls.commits, []);
});

test("a successful edge commit does not steal focus from another control", async () => {
  const pending = deferred();
  const { grid, input, calls } = setup({
    commitCellEdit: () => pending.promise,
  });
  const document = { activeElement: input, body: element() };
  input.ownerDocument = document;
  await grid.select(2, 2);
  await grid.beginEdit("77");
  await input.fire("keydown", { key: "Tab" });
  const otherControl = element();
  document.activeElement = otherControl;
  pending.resolve(true);
  await flush();
  assert.equal(grid.hasDraft(), false);
  assert.deepEqual(calls.exits, []);
  assert.equal(document.activeElement, otherControl);
});

test("edge Tab leaves IME composition to the native input without committing", async () => {
  const { grid, input, calls } = setup();
  await grid.select(2, 2);
  await input.fire("compositionstart");
  input.value = "한글";
  await input.fire("input");
  const event = await input.fire("keydown", { key: "Tab", isComposing: true });
  assert.equal(event.prevented, false);
  assert.deepEqual(calls.exits, []);
  assert.deepEqual(calls.commits, []);
  assert.equal(grid.hasDraft(), true);
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
