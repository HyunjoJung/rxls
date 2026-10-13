import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { PRESERVED_PARTS } from "../scripts/preservation-fixture.mjs";
import { createStoredZip, readZipEntries } from "../scripts/zip.mjs";
import { recordJourney, sha256 } from "./journey-evidence.mjs";

async function inspectCell(page, reference, { waitForCondition }) {
  await page.locator("#edit-cell").click();
  await page.locator("#cell-dialog").waitFor({ state: "visible" });
  await page.locator("#cell-reference").fill(reference);
  await page.locator("#read-cell").click();
  await waitForCondition(
    async () => !(await page.locator("#apply-cell-edit").isDisabled()) &&
      (await page.locator("#cell-current-value").textContent()).startsWith(`${reference}:`),
    `reopened cell ${reference}`,
  );
  const kind = await page.locator("#cell-kind").inputValue();
  const value = kind === "formula"
    ? { kind, formula: await page.locator("#cell-formula").inputValue(),
        cachedKind: await page.locator("#cell-cached-kind").inputValue(),
        cachedValue: await page.locator("#cell-cached-value").inputValue() }
    : { kind, value: await page.locator("#cell-value").inputValue() };
  await page.locator("#cancel-cell-edit").click();
  return value;
}

async function assertCells(page, cells, helpers) {
  const actual = {};
  for (const [reference, expected] of Object.entries(cells)) {
    actual[reference] = await inspectCell(page, reference, helpers);
    assert.deepEqual(actual[reference], expected, `viewer cell ${reference}`);
  }
  return actual;
}

export async function assertViewerReopens(page, saved, expected, helpers) {
  // browser.newPage owns a single-page context; allocate an independent one
  // rather than trying to add a page to the caller's owned context.
  const target = expected.samePage ? page : await page.context().browser().newPage({
    viewport: page.viewportSize(),
  });
  const errors = [];
  const onError = (error) => errors.push(String(error));
  target.on("pageerror", onError);
  try {
    if (target !== page) {
      await target.goto(page.url(), { waitUntil: "domcontentloaded" });
      await helpers.waitForViewerState(target, (s) => s.rendered && !s.busy, "fresh viewer");
    }
    const before = await target.evaluate(() => globalThis.__rxlsViewerState());
    const open = () => target.locator("#file-input").setInputFiles(saved.path);
    if (before.dirty) {
      await Promise.all([
        target.waitForEvent("dialog", { timeout: 10_000 }).then(async (dialog) => {
          assert.equal(dialog.type(), "confirm");
          assert.match(dialog.message(), /Discard unsaved workbook edits/);
          await dialog.accept();
        }),
        open(),
      ]);
    } else {
      await open();
    }
    const state = await helpers.waitForViewerState(
      target,
      (s) => s.fileName === saved.fileName && s.source === "Local file" &&
        s.format === expected.format && s.rendered && !s.busy && !s.dirty,
      expected.label,
    );
    assert.equal(state.editCapability, "read-write");
    assert.equal(state.canUndo, false);
    assert.equal(state.canRedo, false);
    assert.deepEqual(state.editedParts, []);
    const cells = await assertCells(target, expected.cells, helpers);
    assert.deepEqual(errors, []);
    const after = await target.evaluate(() => globalThis.__rxlsViewerState());
    assert.equal(after.dirty, false, "inspection must not dirty a reopened file");
    await recordJourney(expected.label, { status: "passed", fileName: saved.fileName,
      sha256: sha256(saved.bytes), state: after, cells });
  } finally {
    target.off("pageerror", onError);
    if (target !== page) await target.close();
  }
}

export async function assertReloadCancellation(page, fields, label) {
  const values = {};
  for (const selector of fields) values[selector] = await page.locator(selector).inputValue();
  const before = await page.evaluate(() => {
    globalThis.__rxlsReloadSentinel = crypto.randomUUID();
    return { sentinel: globalThis.__rxlsReloadSentinel, state: globalThis.__rxlsViewerState() };
  });
  // Chromium's actual reload command triggers the browser-owned dialog.
  // Do not wait for a new DOMContentLoaded: cancelling deliberately prevents it.
  const protocol = await page.context().newCDPSession(page);
  let dialogType;
  try {
    [dialogType] = await Promise.all([
      page.waitForEvent("dialog", { timeout: 10_000 }).then(async (dialog) => {
        const type = dialog.type();
        await dialog.dismiss();
        assert.equal(type, "beforeunload", "real reload must reach the browser guard");
        return type;
      }),
      protocol.send("Page.reload"),
    ]);
    await page.evaluate(() => new Promise((resolve) =>
      requestAnimationFrame(() => requestAnimationFrame(resolve))));
  } finally {
    await protocol.detach();
  }
  const after = await page.evaluate(() => ({
    sentinel: globalThis.__rxlsReloadSentinel, state: globalThis.__rxlsViewerState(),
  }));
  assert.deepEqual(after, before, "cancel keeps the same document and workbook state");
  for (const selector of fields) {
    assert.equal(await page.locator(selector).isVisible(), true);
    assert.equal(await page.locator(selector).inputValue(), values[selector]);
  }
  await recordJourney(label, { status: "passed", dialogType,
    reloadCommand: "CDP Page.reload", dialogAction: "dismiss", fields: values,
    sameDocument: after.sentinel === before.sentinel, state: after.state });
  await page.evaluate(() => { delete globalThis.__rxlsReloadSentinel; });
}

export async function exerciseMultilineCellOptions(page, helpers) {
  const target = await page.context().browser().newPage({ viewport: page.viewportSize() });
  const text = "첫 줄\n둘째 줄";
  const entries = readZipEntries(await readFile(new URL("../samples/operations-report.xlsx", import.meta.url)));
  entries.set("xl/worksheets/sheet1.xml", Buffer.from(
    `<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t xml:space="preserve">${text}</t></is></c><c r="B1" t="str"><f>A1</f><v>${text}</v></c></row></sheetData></worksheet>`,
  ));
  const buffer = createStoredZip([...entries].map(([name, data]) => ({ name, data })));
  try {
    await target.goto(page.url());
    await helpers.waitForViewerState(target, (s) => s.rendered && !s.busy, "multiline fresh viewer");
    await target.locator("#file-input").setInputFiles({
      name: "multiline.xlsx", mimeType: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", buffer,
    });
    await helpers.waitForViewerState(target, (s) => s.fileName === "multiline.xlsx" && !s.busy && s.rendered, "multiline source");
    const cells = {
      A1: { kind: "text", value: text },
      B1: { kind: "formula", formula: "A1", cachedKind: "text", cachedValue: text },
    };
    await assertCells(target, cells, helpers);
    // Apply unchanged values through each real control; line breaks must survive
    // both UI population and the subsequent mutation/save path.
    for (const reference of ["A1", "B1"]) {
      await target.locator("#edit-cell").click();
      await target.locator("#cell-reference").fill(reference);
      await target.locator("#read-cell").click();
      await helpers.waitForCondition(async () => !(await target.locator("#apply-cell-edit").isDisabled()) &&
        (await target.locator("#cell-current-value").textContent()).startsWith(`${reference}:`), `multiline apply ${reference}`);
      const selector = reference === "A1" ? "#cell-value" : "#cell-cached-value";
      assert.equal(await target.locator(selector).inputValue(), text);
      await target.locator("#apply-cell-edit").click();
      await target.locator("#cell-dialog").waitFor({ state: "hidden" });
      await helpers.waitForViewerState(target, (s) => !s.busy, `multiline ${reference} applied`);
    }
    const saved = await helpers.downloadWorkbook(target, ".xlsx");
    await helpers.assertOpenpyxlReopens(saved.path, { expected: text });
    await assertViewerReopens(target, saved, {
      samePage: true, format: "xlsx", cells, label: "multiline Cell Options download reopened clean",
    }, helpers);
    await recordJourney("multiline Cell Options", {
      status: "passed", sourceSha256: sha256(buffer), cells, appliedControls: ["cell-value", "cell-cached-value"],
    });
    console.log("PASS multiline Cell Options; scalar_text=1; formula_text_cache=1; apply_save_reopen=true");
  } finally {
    await target.close();
  }
}

export async function exerciseMacroRoundtrip(page, sourceBytes, helpers) {
  const original = {};
  for (const ref of ["A1", "B1", "A2", "B2"]) original[ref] = await inspectCell(page, ref, helpers);
  const idle = (label, predicate) => helpers.waitForViewerState(page, (s) => !s.busy && predicate(s), label);
  const number = (value) => ({ kind: "number", value: String(value) });
  const formula = (text, value) => ({ kind: "formula", formula: text,
    cachedKind: "number", cachedValue: String(value) });
  await page.locator("#edit-cell").click();
  await page.locator("#cell-reference").fill("B1");
  await page.locator("#read-cell").click();
  await helpers.waitForCondition(async () => !(await page.locator("#apply-cell-edit").isDisabled()) &&
    (await page.locator("#cell-current-value").textContent()).startsWith("B1:"), "XLSM formula target");
  await page.locator("#cell-kind").selectOption("formula");
  await page.locator("#cell-formula").fill("2+3");
  await page.locator("#cell-cached-kind").selectOption("number");
  await page.locator("#cell-cached-value").fill("5");
  await page.locator("#apply-cell-edit").click();
  await idle("XLSM formula committed", (s) => s.dirty && s.canUndo);
  await assertCells(page, { B1: formula("2+3", 5) }, helpers);

  await page.locator("#inspector-reference").fill("A1");
  await page.locator("#inspector-reference").press("Enter");
  await helpers.waitForCondition(async () =>
    (await page.locator("#grid-selection").getAttribute("data-reference")) === "A1" &&
    !(await page.locator("#grid-input").isDisabled()), "XLSM range target");
  await page.locator("#grid-input").focus();
  const text = "보존\t한글\n다음 줄";
  await page.locator("#grid-input").evaluate((element, quoted) => {
    const data = new DataTransfer();
    data.setData("text/plain", `7\t=A1*3\r\n11\t"${quoted}"\r\n`);
    element.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true, cancelable: true }));
  }, text);
  await page.locator("#paste-dialog").waitFor({ state: "visible" });
  await page.locator("#apply-range-paste").click();
  await page.locator("#paste-dialog").waitFor({ state: "hidden" });
  await idle("XLSM range committed", (s) => s.dirty && s.canUndo);
  const edited = { A1: number(7), B1: formula("A1*3", 21), A2: number(11), B2: { kind: "text", value: text } };
  await assertCells(page, edited, helpers);
  await page.locator("#undo-edit").click();
  await idle("XLSM range undo retains formula edit", (s) => s.dirty && s.canUndo && s.canRedo);
  await assertCells(page, { ...original, B1: formula("2+3", 5) }, helpers);
  await page.locator("#undo-edit").click();
  await idle("XLSM formula undo restores clean source", (s) => !s.dirty && !s.canUndo && s.canRedo);
  await assertCells(page, original, helpers);
  await page.locator("#redo-edit").click();
  await idle("XLSM formula redo", (s) => s.dirty && s.canRedo);
  await assertCells(page, { B1: formula("2+3", 5) }, helpers);
  await page.locator("#redo-edit").click();
  const state = await idle("XLSM range redo", (s) => s.dirty && s.canUndo && !s.canRedo);
  await assertCells(page, edited, helpers);
  const saved = await helpers.downloadWorkbook(page, ".xlsm");
  helpers.assertZipPartsEqual(sourceBytes, saved.bytes, PRESERVED_PARTS);
  const source = readZipEntries(sourceBytes);
  const output = readZipEntries(saved.bytes);
  assert.deepEqual([...output.keys()].sort(), [...source.keys()].sort());
  const untouched = [...source.keys()].filter((name) => !state.editedParts.includes(name));
  helpers.assertZipPartsEqual(sourceBytes, saved.bytes, untouched);
  helpers.assertZipPartChanged(sourceBytes, saved.bytes, "xl/worksheets/sheet1.xml");
  const vba = output.get("xl/vbaProject.bin");
  assert.ok(vba.length >= 512);
  assert.equal(vba.subarray(0, 8).toString("hex"), "d0cf11e0a1b11ae1");
  await helpers.assertOpenpyxlReopens(saved.path, { cell: "B2", expected: text, requireVba: true, cacheCell: "B1", expectedCache: 21 });
  await assertViewerReopens(page, saved, { samePage: true, format: "xlsm",
    cells: edited, label: "XLSM formula-range download reopened clean" }, helpers);
  const cleanSaved = await helpers.downloadWorkbook(page, ".xlsm");
  helpers.assertZipPartsEqual(saved.bytes, cleanSaved.bytes, [...output.keys()]);
  await recordJourney("XLSM formula-range preservation", { status: "passed", sourceSha256: sha256(sourceBytes),
    savedSha256: sha256(saved.bytes), vbaSha256: sha256(vba), editedParts: state.editedParts,
    untouchedParts: untouched.map((name) => ({ name, sha256: sha256(source.get(name)) })),
    verifiedTransitions: ["formula", "range", "undo-range", "undo-formula-clean", "redo-formula", "redo-range", "save", "viewer-reopen-clean", "no-op-save"],
    cells: edited });
  console.log(`PASS XLSM formula/range undo-redo-save-reopen; untouched_parts=${untouched.length}`);
}
