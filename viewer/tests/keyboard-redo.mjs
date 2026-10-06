import assert from "node:assert/strict";
import { recordJourney } from "./journey-evidence.mjs";

/** Real keyboard events and worker calls at a fresh sole-edit history boundary. */
export async function exerciseKeyboardRedo(originalPage, helpers) {
  const failures = [];
  for (const modifier of ["Control", "Meta"]) {
    const page = await originalPage.context().browser().newPage({ viewport: originalPage.viewportSize() });
    const errors = { pageErrors: [], consoleErrors: [], failedResponses: [] };
    page.on("pageerror", (error) => errors.pageErrors.push(String(error)));
    page.on("console", (message) => { if (message.type() === "error") errors.consoleErrors.push(message.text()); });
    page.on("response", (response) => { if (response.status() >= 400) errors.failedResponses.push({ status: response.status(), url: response.url() }); });
    let installed = false;
    const snapshot = () => page.evaluate(() => ({
      state: globalThis.__rxlsViewerState(),
      calls: globalThis.__rxlsKeyboardRedo?.calls ?? null,
      events: globalThis.__rxlsKeyboardRedo?.events ?? [],
      input: document.getElementById("grid-input")?.value ?? null,
    }));
    const stages = [];
    try {
      await page.goto(originalPage.url(), { waitUntil: "domcontentloaded" });
      await helpers.waitForViewerState(page, (s) => s.rendered && !s.busy && s.format === "xlsx" && s.mode === "sheet", `${modifier} fresh workbook`);
      const fresh = await snapshot();
      assert.equal(fresh.state.canUndo, false);
      assert.equal(fresh.state.canRedo, false);
      assert.equal(fresh.state.dirty, false);
      assert.equal(fresh.state.editCapability, "read-write");
      await page.evaluate(async () => {
        const { RenderWorkerClient } = await import(new URL("runtime/js/client.mjs", document.baseURI).href);
        const prototype = RenderWorkerClient.prototype;
        const originals = {};
        const evidence = globalThis.__rxlsKeyboardRedo = { calls: { edit: 0, undo: 0, redo: 0 }, events: [] };
        for (const [method, counter] of [["setCellAndRecalculate", "edit"], ["undoEdit", "undo"], ["redoEdit", "redo"]]) {
          originals[method] = prototype[method];
          prototype[method] = function (...args) { evidence.calls[counter]++; return originals[method].apply(this, args); };
        }
        const capture = (event) => {
          if ((event.ctrlKey || event.metaKey) && ["z", "y"].includes(event.key.toLowerCase())) {
            evidence.events.push({ key: event.key, ctrlKey: event.ctrlKey, metaKey: event.metaKey, shiftKey: event.shiftKey, isTrusted: event.isTrusted, isComposing: event.isComposing, keyCode: event.keyCode, defaultPrevented: event.defaultPrevented, target: event.target?.id ?? null });
          }
        };
        // Installed after the application's bubble listener, so prevention is observable.
        window.addEventListener("keydown", capture);
        evidence.restore = () => { for (const method of Object.keys(originals)) prototype[method] = originals[method]; window.removeEventListener("keydown", capture); };
      });
      installed = true;
      const input = page.locator("#grid-input");
      const selectA1 = async (expected) => {
        await page.locator("#inspector-reference").fill("A1");
        await page.locator("#inspector-reference").press("Enter");
        await helpers.waitForCondition(async () => (await page.locator("#grid-selection").getAttribute("data-reference")) === "A1" && !(await input.isDisabled()), `${modifier} A1 ready`);
        if (expected !== undefined) await helpers.waitForCondition(async () => (await page.locator("#inspector-value").inputValue()) === expected, `${modifier} A1 inspected`);
        await input.focus();
      };
      const value = async (expected) => {
        await selectA1(expected);
        assert.ok(await page.locator("svg text").filter({ hasText: expected }).count(), `${modifier} value actually rendered`);
      };
      const state = async (dirty, canUndo, canRedo, label) => {
        const observed = await helpers.waitForViewerState(page, (s) => !s.busy && s.rendered && s.dirty === dirty && s.canUndo === canUndo && s.canRedo === canRedo, `${modifier} ${label}`);
        stages.push({ label, state: observed, calls: (await snapshot()).calls });
        return observed;
      };
      const calls = async (edit, undo, redo) => assert.deepEqual((await snapshot()).calls, { edit, undo, redo });
      await selectA1();
      const sourceText = await page.locator("#inspector-value").inputValue();
      assert.ok(sourceText.length > 0);
      const editedText = `W24 keyboard redo ${modifier}`;
      await input.fill(editedText);
      await input.press("Enter");
      await state(true, true, false, "sole edit committed");
      await calls(1, 0, 0);
      await value(editedText);

      const undo = async (count) => {
        await input.focus();
        await input.press(`${modifier}+z`);
        await state(false, false, true, "sole undo exhausted undo history");
        await calls(1, count, count - 1);
        await value(sourceText);
      };
      await undo(1);
      const boundary = await snapshot();
      assert.equal(boundary.state.canUndo, false, "exact original-bug boundary");
      assert.equal(boundary.state.canRedo, true, "redo remains available");
      assert.equal(await page.locator("#undo-edit").isDisabled(), true);
      assert.equal(await page.locator("#redo-edit").isDisabled(), false);

      await input.press(`${modifier}+Shift+z`);
      await state(true, true, false, "Shift+Z redid sole edit");
      await calls(1, 1, 1);
      await value(editedText);
      const shiftEvent = (await snapshot()).events.filter((e) => e.key.toLowerCase() === "z" && e.shiftKey && e.target === "grid-input").at(-1);
      assert.ok(shiftEvent?.isTrusted && shiftEvent.defaultPrevented, "real browser Shift+Z was handled and prevented");
      assert.equal(modifier === "Control" ? shiftEvent.ctrlKey : shiftEvent.metaKey, true);

      await undo(2);
      await input.press(`${modifier}+y`);
      await state(true, true, false, "Y redo retained");
      await calls(1, 2, 2);
      await value(editedText);
      await undo(3);
      await page.locator("#redo-edit").click();
      await state(true, true, false, "toolbar redo retained");
      await calls(1, 3, 3);
      await value(editedText);
      await undo(4);

      const stable = async (before, label) => {
        await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
        const after = await snapshot();
        assert.deepEqual(after.calls, before.calls, `${label}: no worker mutation`);
        assert.deepEqual(after.state, before.state, `${label}: history/state unchanged`);
        return after;
      };
      const retainedDraft = `retained draft ${modifier}`;
      await input.fill(retainedDraft);
      const beforeDraft = await snapshot();
      await input.press(`${modifier}+Shift+z`);
      await stable(beforeDraft, "active inline draft");
      assert.equal(await input.inputValue(), retainedDraft);
      await input.press("Escape");
      await value(sourceText);

      const external = page.locator("#inspector-reference");
      await external.focus();
      const externalText = await external.inputValue();
      const beforeExternal = await snapshot();
      await external.press(`${modifier}+Shift+z`);
      await stable(beforeExternal, "external native input");
      assert.equal(await external.inputValue(), externalText);

      await page.locator("#edit-cell").click();
      await page.locator("#cell-dialog").waitFor({ state: "visible" });
      await helpers.waitForCondition(async () => !(await page.locator("#apply-cell-edit").isDisabled()), `${modifier} active dialog ready`);
      const field = page.locator("#cell-value");
      await field.fill(`unapplied dialog ${modifier}`);
      const beforeDialog = await snapshot();
      await field.press(`${modifier}+Shift+z`);
      await stable(beforeDialog, "active Cell Options editor");
      assert.equal(await field.inputValue(), `unapplied dialog ${modifier}`);
      await page.locator("#cancel-cell-edit").click();
      await page.locator("#cell-dialog").waitFor({ state: "hidden" });
      await selectA1(sourceText);

      const syntheticGuards = [];
      for (const guard of ["prevented", "composing", "ime229"]) {
        const before = await snapshot();
        const observed = await input.evaluate((element, { guard, modifier }) => {
          const event = new KeyboardEvent("keydown", { key: "Z", code: "KeyZ", bubbles: true, cancelable: true, shiftKey: true, ctrlKey: modifier === "Control", metaKey: modifier === "Meta", isComposing: guard === "composing", keyCode: guard === "ime229" ? 229 : 90 });
          if (guard === "prevented") event.preventDefault();
          if (guard === "ime229") assertKeyCode();
          function assertKeyCode() { if (event.keyCode !== 229) throw new Error("synthetic 229 probe was not admitted by the browser"); }
          element.dispatchEvent(event);
          return { guard, isTrusted: event.isTrusted, isComposing: event.isComposing, keyCode: event.keyCode, defaultPrevented: event.defaultPrevented };
        }, { guard, modifier });
        await stable(before, guard);
        assert.equal(observed.isTrusted, false);
        syntheticGuards.push(observed);
      }
      await calls(1, 4, 3);
      assert.deepEqual(errors, { pageErrors: [], consoleErrors: [], failedResponses: [] });
      await recordJourney(`keyboard-redo-${modifier}`, { status: "passed", modifier, exactBeforeShiftZ: boundary, stages, final: await snapshot(), syntheticGuards, nativeOsIme: "not_verified", errors });
    } catch (error) {
      const observed = await snapshot().catch(() => null);
      const failure = { modifier, error: String(error), observed, stages, errors };
      failures.push(failure);
      await recordJourney(`keyboard-redo-${modifier}`, { status: "failed", ...failure });
    } finally {
      if (installed) await page.evaluate(() => { globalThis.__rxlsKeyboardRedo.restore(); delete globalThis.__rxlsKeyboardRedo; }).catch(() => undefined);
      await page.close();
    }
  }
  if (failures.length) throw new AggregateError(failures.map((f) => new Error(`${f.modifier}: ${f.error}`)), "keyboard redo journey failed");
}
