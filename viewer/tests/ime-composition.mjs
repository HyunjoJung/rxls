import assert from "node:assert/strict";
import { recordJourney } from "./journey-evidence.mjs";
import { assertViewerReopens } from "./reopen-journey.mjs";

/** Chromium input pipeline via CDP, not dispatchEvent and not an OS IME. */
export async function exerciseCdpComposition(page, helpers) {
  const text = "한글 입력 검증";
  const input = page.locator("#grid-input");
  await page.locator("#inspector-reference").fill("C4");
  await page.locator("#inspector-reference").press("Enter");
  await helpers.waitForCondition(async () =>
    (await page.locator("#grid-selection").getAttribute("data-reference")) === "C4" &&
    !(await input.isDisabled()), "CDP composition target");
  await input.press("F2");
  await input.press("ControlOrMeta+a");
  const oldLength = (await input.inputValue()).length;
  await page.evaluate(async () => {
    const { RenderWorkerClient } = await import(new URL("runtime/js/client.mjs", document.baseURI).href);
    const prototype = RenderWorkerClient.prototype;
    const originalEdit = prototype.setCellAndRecalculate;
    const originalSave = prototype.saveDocument;
    const input = document.getElementById("grid-input");
    const types = ["compositionstart", "compositionupdate", "compositionend", "beforeinput", "input", "keydown", "keyup"];
    const evidence = globalThis.__rxlsCdpIme = { edits: 0, saves: 0, events: [] };
    const capture = (event) => evidence.events.push({
      type: event.type, isTrusted: event.isTrusted, isComposing: event.isComposing ?? null,
      inputType: event.inputType ?? null, data: event.data ?? null,
      key: event.key ?? null, keyCode: event.keyCode ?? null, value: input.value,
    });
    for (const type of types) input.addEventListener(type, capture);
    prototype.setCellAndRecalculate = function (...args) {
      evidence.edits++;
      return originalEdit.apply(this, args);
    };
    prototype.saveDocument = function (...args) {
      evidence.saves++;
      return originalSave.apply(this, args);
    };
    evidence.restore = () => {
      for (const type of types) input.removeEventListener(type, capture);
      prototype.setCellAndRecalculate = originalEdit;
      prototype.saveDocument = originalSave;
    };
  });
  const protocol = await page.context().newCDPSession(page);
  const snapshot = () => page.evaluate(() => ({
    edits: globalThis.__rxlsCdpIme.edits, saves: globalThis.__rxlsCdpIme.saves,
    events: globalThis.__rxlsCdpIme.events, state: globalThis.__rxlsViewerState(),
    value: document.getElementById("grid-input").value,
  }));
  try {
    await protocol.send("Input.imeSetComposition", {
      text: "ㅎ", selectionStart: 1, selectionEnd: 1,
      replacementStart: 0, replacementEnd: oldLength,
    });
    await protocol.send("Input.imeSetComposition", {
      text, selectionStart: text.length, selectionEnd: text.length,
    });
    await protocol.send("Input.dispatchKeyEvent", {
      type: "rawKeyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13,
    });
    await protocol.send("Input.dispatchKeyEvent", {
      type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13,
    });
    const during = await snapshot();
    assert.equal(during.edits, 0, "Enter during composition cannot write a cell");
    assert.equal(during.state.dirty, false);
    assert.equal(during.value, text);
    assert.ok(during.events.some((e) => e.type === "keydown" && e.key === "Enter" && e.isComposing && e.isTrusted));
    await protocol.send("Input.insertText", { text });
    const committedInput = await snapshot();
    assert.equal(committedInput.value, text);
    assert.equal(committedInput.edits, 0, "compositionend alone is not a workbook edit");
    // Input.insertText emits compositionend with isTrusted=false in the tested
    // Chrome build. Record that boundary rather than claiming every event is trusted.
    assert.ok(committedInput.events.some((e) => e.type === "compositionend"));
    await input.press("Enter");
    await helpers.waitForViewerState(page, (s) => !s.busy && s.dirty, "CDP text cell committed");
    const committedCell = await snapshot();
    assert.equal(committedCell.edits, 1);
    assert.ok(committedCell.events.some((e) => e.type === "keydown" && e.key === "Enter" &&
      e.isTrusted && e.isComposing === false), "post-composition Enter uses the browser's ended composition state");
    for (const type of ["compositionstart", "compositionupdate", "beforeinput", "input"]) {
      const events = committedCell.events.filter((e) => e.type === type);
      assert.ok(events.length > 0, `${type} was observed`);
      assert.ok(events.every((e) => e.isTrusted), `${type} came from the browser input pipeline`);
    }
    const saved = await helpers.downloadWorkbook(page, ".xlsx");
    const afterSave = await snapshot();
    assert.equal(afterSave.edits, 1);
    assert.equal(afterSave.saves, 1);
    await assertViewerReopens(page, saved, {
      format: "xlsx", label: "CDP Korean input download reopened clean",
      cells: { C4: { kind: "text", value: text } },
    }, helpers);
    await recordJourney("CDP Korean composition", {
      status: "passed", inputMethods: ["Input.imeSetComposition", "Input.insertText"],
      duringCompositionEdits: during.edits, edits: afterSave.edits, saves: afterSave.saves,
      finalValue: text, events: afterSave.events,
      compositionEndIsTrusted: afterSave.events.filter((e) => e.type === "compositionend").map((e) => e.isTrusted),
      nativeOsKoreanIme: "not_verified",
    });
    console.log("PASS CDP Korean composition; premature_edits=0; committed_edits=1; saves=1; reopened=true");
  } catch (error) {
    await recordJourney("CDP Korean composition", {
      status: "failed", error: String(error), ...(await snapshot()), nativeOsKoreanIme: "not_verified",
    });
    throw error;
  } finally {
    await protocol.detach();
    await page.evaluate(() => { globalThis.__rxlsCdpIme.restore(); delete globalThis.__rxlsCdpIme; });
  }
  await page.locator("#undo-edit").click();
  await helpers.waitForViewerState(page, (s) => !s.busy && !s.dirty, "CDP composition one undo restores source");
}
