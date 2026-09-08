import test from "node:test";
import assert from "node:assert/strict";
import { setup, deferred, flush } from "./support/grid-editor.mjs";

for (const outcome of ["resolve", "reject"]) {
  test(`a stale one-click read ${outcome} cannot announce over a newer open`, async () => {
    const pending = deferred();
    const { grid, state, calls, elements } = setup({
      readCell: () => pending.promise,
    });
    const editing = grid.beginEdit();
    state.openGeneration = 1;
    state.busy = true;
    elements["grid-status"].textContent = "Opening replacement";
    if (outcome === "resolve")
      pending.resolve({ value: { kind: "number", value: 10 } });
    else pending.reject(new Error("Old read failed"));
    assert.equal(await editing, false);
    assert.equal(elements["grid-status"].textContent, "Opening replacement");
    assert.deepEqual(calls.errors, []);
  });

  test(`a stale grid mutation ${outcome} cannot clear the draft or report an obsolete failure`, async () => {
    const pending = deferred();
    const { grid, state, input, calls, elements } = setup({
      commitCellEdit: () => pending.promise,
    });
    await flush();
    await grid.beginEdit();
    input.value = "7";
    await input.fire("input");
    const committing = grid.commit();
    state.openGeneration = 1;
    state.busy = true;
    elements["grid-status"].textContent = "Opening replacement";
    if (outcome === "resolve") pending.resolve(true);
    else pending.reject(new Error("Old write failed"));
    assert.equal(await committing, false);
    assert.equal(input.value, "7");
    assert.equal(grid.hasChanges(), true);
    assert.equal(elements["grid-status"].textContent, "Opening replacement");
    assert.deepEqual(calls.errors, []);
  });
}
