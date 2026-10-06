import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import { setup, flush } from "./support/grid-editor.mjs";

const source = await readFile(new URL("../src/main.js", import.meta.url), "utf8");
const start = source.indexOf("function onKeyDown(event)");
const end = source.indexOf("function toggleSidebar()", start);
assert.ok(start >= 0 && end > start, "actual main keyboard-handler boundary exists");
const handler = source.slice(start, end);

class Element {
  matches() { return true; }
}

async function fixture(canUndo, canRedo) {
  const env = setup();
  await flush();
  Object.setPrototypeOf(env.input, Element.prototype);
  Object.assign(env.state.editState, { canUndo, canRedo });
  const calls = [];
  const sandbox = {
    state: env.state, grid: env.grid, elements: env.elements, Element,
    vscodeHost: null,
    applyHistoryEdit(direction) {
      calls.push(direction);
      env.state.editState.canUndo = direction === "redo";
      env.state.editState.canRedo = direction === "undo";
    },
  };
  vm.createContext(sandbox);
  vm.runInContext(handler, sandbox);
  return { ...env, calls, sandbox };
}

async function dispatch(env, details) {
  const event = await env.input.fire("keydown", details);
  event.defaultPrevented = Boolean(event.defaultPrevented || event.prevented);
  if (!event.stopped) env.sandbox.onKeyDown(event);
  return event;
}

test("Ctrl and Meta Shift+Z redo the sole undone edit at the selected grid input", async () => {
  for (const modifier of ["ctrlKey", "metaKey"]) {
    const env = await fixture(true, false);
    assert.equal(env.outline.dataset.reference, "A1");
    assert.equal(env.grid.hasDraft(), false);
    await dispatch(env, { key: "z", [modifier]: true });
    assert.equal(env.state.editState.canUndo, false);
    assert.equal(env.state.editState.canRedo, true);
    const redo = await dispatch(env, { key: "Z", shiftKey: true, [modifier]: true });
    assert.deepEqual(env.calls, ["undo", "redo"]);
    assert.equal(redo.prevented, true);
  }
});

test("Ctrl and Meta history shortcuts cannot mutate a tiled read-only display", async () => {
  for (const modifier of ["ctrlKey", "metaKey"]) {
    for (const [key, shiftKey] of [["z", false], ["z", true], ["y", false]]) {
      const env = await fixture(true, true);
      env.state.displayKind = "tiled";
      const event = await dispatch(env, { key, shiftKey, [modifier]: true });
      assert.deepEqual(env.calls, []);
      assert.equal(event.prevented, false);
    }
  }
});

test("Z history shortcuts use availability of the requested direction", async () => {
  for (const modifier of ["ctrlKey", "metaKey"]) {
    for (const canUndo of [false, true]) {
      for (const canRedo of [false, true]) {
        for (const shiftKey of [false, true]) {
          const env = await fixture(canUndo, canRedo);
          const event = await dispatch(env, { key: "z", shiftKey, [modifier]: true });
          const direction = shiftKey ? "redo" : "undo";
          const available = shiftKey ? canRedo : canUndo;
          assert.deepEqual(env.calls, available ? [direction] : []);
          assert.equal(event.prevented, available);
        }
      }
    }
  }
});

test("Y redo still works at empty undo history and guards retain native text behavior", async () => {
  for (const modifier of ["ctrlKey", "metaKey"]) {
    const env = await fixture(false, true);
    const event = await dispatch(env, { key: "y", [modifier]: true });
    assert.deepEqual(env.calls, ["redo"]);
    assert.equal(event.prevented, true);
  }
  for (const guard of ["draft", "external-text", "prevented", "composing", "ime229"]) {
    const env = await fixture(false, true);
    const details = { key: "z", shiftKey: true, ctrlKey: true };
    if (guard === "draft") await env.grid.beginEdit("retained draft");
    if (guard === "external-text") details.target = new Element();
    if (guard === "prevented") details.defaultPrevented = true;
    if (guard === "composing") details.isComposing = true;
    if (guard === "ime229") details.keyCode = 229;
    await dispatch(env, details);
    assert.deepEqual(env.calls, [], guard);
    if (guard === "draft") {
      assert.equal(env.grid.hasDraft(), true);
      assert.equal(env.input.value, "retained draft");
    }
  }
});
