import assert from "node:assert/strict";
import { mkdir, readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { performance } from "node:perf_hooks";
import { createStoredZip, readZipEntries } from "../scripts/zip.mjs";

export async function exerciseRangePaste(
  page,
  {
    waitForCondition,
    waitForViewerState,
    downloadWorkbook,
    assertOpenpyxlReopens,
  },
) {
  const input = page.locator("#grid-input");
  const dialog = page.locator("#paste-dialog");
  const select = async (reference, expected) => {
    await page.locator("#inspector-reference").fill(reference);
    await page.locator("#inspector-reference").press("Enter");
    await waitForCondition(
      async () =>
        (await page
          .locator("#grid-selection")
          .getAttribute("data-reference")) === reference &&
        !(await input.isDisabled()) &&
        (expected === undefined ||
          (await page.locator("#inspector-value").inputValue()) === expected),
      `paste target ${reference}`,
    );
  };
  const paste = async (text) => {
    await input.focus();
    return input.evaluate((element, value) => {
      const data = new DataTransfer();
      data.setData("text/plain", value);
      const event = new ClipboardEvent("paste", {
        clipboardData: data,
        bubbles: true,
        cancelable: true,
      });
      element.dispatchEvent(event);
      return event.defaultPrevented;
    }, text);
  };
  const undo = async () => {
    await page.locator("#undo-edit").click();
    await waitForViewerState(
      page,
      (state) => !state.busy && !state.dirty,
      "one range undo restores source",
    );
  };

  await select("C4", "420000");
  await input.press("F2");
  await input.fill("retained draft");
  assert.equal(await paste("1\t2"), true);
  await dialog.waitFor({ state: "visible" });
  await page.keyboard.press("Escape");
  await dialog.waitFor({ state: "hidden" });
  assert.equal(
    await input.inputValue(),
    "retained draft",
    "Escape only cancels the preview",
  );
  await input.press("Escape");

  const clipboard = '125000\t=C4*2\r\n200000\t"two\tparts\r\nnext line"\r\n';
  assert.equal(await paste(clipboard), true);
  assert.match(
    await page.locator("#paste-summary").textContent(),
    /C4:D5.*2 rows.*2 columns.*4 cells/,
  );
  assert.equal(await page.locator("#paste-preview td").count(), 4);
  assert.equal(
    await page.locator("#paste-preview td").last().textContent(),
    "two\tparts\nnext line",
  );
  if (process.env.RXLS_VIEWER_SCREENSHOTS === "1") {
    await mkdir(new URL("../../target/viewer-e2e/", import.meta.url), {
      recursive: true,
    });
    await page.screenshot({
      path: fileURLToPath(
        new URL("../../target/viewer-e2e/range-paste.png", import.meta.url),
      ),
    });
  }
  await page.setViewportSize({ width: 390, height: 844 });
  const mobile = await dialog.boundingBox();
  assert.ok(
    mobile &&
      mobile.x >= 0 &&
      mobile.y >= 0 &&
      mobile.x + mobile.width <= 391 &&
      mobile.y + mobile.height <= 845,
    "range preview fits a mobile viewport",
  );
  for (const id of ["cancel-range-paste", "paste-as-text", "apply-range-paste"])
    assert.equal(await page.locator(`#${id}`).isVisible(), true);
  if (process.env.RXLS_VIEWER_SCREENSHOTS === "1")
    await page.screenshot({
      path: fileURLToPath(
        new URL(
          "../../target/viewer-e2e/mobile-range-paste.png",
          import.meta.url,
        ),
      ),
    });
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.keyboard.press(
    process.platform === "darwin" ? "Meta+s" : "Control+s",
  );
  assert.match(
    await page.locator("#paste-status").textContent(),
    /Apply or cancel/,
  );
  assert.equal(
    (await page.evaluate(() => globalThis.__rxlsViewerState())).dirty,
    false,
  );
  await page.locator("#apply-range-paste").click();
  await dialog.waitFor({ state: "hidden" });
  await waitForViewerState(
    page,
    (state) => !state.busy && state.dirty,
    "rectangular paste applied",
  );
  await select("C4", "125000");
  await select("D4", "=C4*2");
  await select("C5", "200000");
  await select("D5", "two\tparts\nnext line");
  const saved = await downloadWorkbook(page, ".xlsx");
  await assertOpenpyxlReopens(saved.path, {
    expected: "Q3 Operations Snapshot",
    cacheCell: "C10",
    expectedCache: 1094000,
  });
  await undo();
  await select("C4", "420000");
  await select("C5", "185000");
  await page.locator("#redo-edit").click();
  await waitForViewerState(
    page,
    (state) => !state.busy && state.dirty,
    "range redo",
  );
  await select("C4", "125000");
  await select("D5", "two\tparts\nnext line");
  await undo();

  // Merged interiors reject the whole batch, including the valid anchor.
  await select("A1", "Q3 Operations Snapshot");
  await input.press("F2");
  await input.fill("keep this draft");
  await paste("new heading\tmerged interior");
  await page.locator("#apply-range-paste").click();
  await waitForCondition(
    async () => !(await page.locator("#cancel-range-paste").isDisabled()),
    "failed paste settled",
  );
  assert.match(
    await page.locator("#paste-status").textContent(),
    /not applied/,
  );
  assert.equal(
    (await page.evaluate(() => globalThis.__rxlsViewerState())).dirty,
    false,
  );
  await page.locator("#cancel-range-paste").click();
  assert.equal(await input.inputValue(), "keep this draft");
  await input.press("Escape");
  await page.locator("#dismiss-error").click();
  await select("C4", "420000");

  // Invalid rectangles cannot partially write; users can intentionally keep multiline text.
  await paste("a\tb\nc");
  assert.equal(await page.locator("#apply-range-paste").isDisabled(), true);
  await page.locator("#paste-as-text").click();
  await dialog.waitFor({ state: "hidden" });
  await waitForViewerState(
    page,
    (state) => !state.busy && state.dirty,
    "explicit one-cell text paste",
  );
  await select("C4", "a\tb\nc");
  await undo();
  await select("C4", "420000");
  await paste("=UNKNOWNFUNCTION(1)\t2");
  await page.locator("#apply-range-paste").click();
  await waitForCondition(
    async () => !(await page.locator("#cancel-range-paste").isDisabled()),
    "unsupported formula paste settled",
  );
  assert.equal(
    (await page.evaluate(() => globalThis.__rxlsViewerState())).dirty,
    false,
  );
  await page.locator("#cancel-range-paste").click();
  await page.locator("#dismiss-error").click();

  await page.locator("#page-view").click();
  await waitForViewerState(
    page,
    (state) => !state.busy && state.mode === "page",
    "paste disabled in page view",
  );
  const pagePastePrevented = await page
    .locator("#document-surface")
    .evaluate((element) => {
      const data = new DataTransfer();
      data.setData("text/plain", "1\t2");
      const event = new ClipboardEvent("paste", {
        clipboardData: data,
        bubbles: true,
        cancelable: true,
      });
      element.dispatchEvent(event);
      return event.defaultPrevented;
    });
  assert.equal(pagePastePrevented, false);
  assert.equal(await dialog.isHidden(), true);
  await page.locator("#sheet-view").click();
  await waitForViewerState(
    page,
    (state) => !state.busy && state.mode === "sheet",
    "paste sheet restored",
  );
  await select("A1", "Q3 Operations Snapshot");
}

export async function exerciseLargeRangePaste(
  page,
  {
    waitForCondition,
    waitForViewerState,
    downloadWorkbook,
    assertOpenpyxlReopens,
  },
) {
  const sample = new URL("../samples/operations-report.xlsx", import.meta.url);
  const entries = readZipEntries(await readFile(sample));
  const data = Array.from(
    { length: 1000 },
    (_, row) =>
      `<row r="${row + 1}">${Array.from({ length: 16 }, (_, col) => `<c r="${String.fromCharCode(65 + col)}${row + 1}"><v>1</v></c>`).join("")}</row>`,
  ).join("");
  entries.set(
    "xl/worksheets/sheet1.xml",
    Buffer.from(
      `<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>${data}<row r="1001"><c r="P1001"><f>SUM(A1:A1000)</f><v>1000</v></c></row></sheetData></worksheet>`,
    ),
  );
  await page.locator("#file-input").setInputFiles({
    name: "large-range.xlsx",
    mimeType:
      "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    buffer: createStoredZip(
      [...entries].map(([name, data]) => ({ name, data })),
    ),
  });
  await waitForViewerState(
    page,
    (state) =>
      state.fileName === "large-range.xlsx" && !state.busy && state.rendered,
    "16,001-cell interactive workbook",
  );
  await page.locator("#inspector-reference").fill("A1");
  await page.locator("#inspector-reference").press("Enter");
  await waitForCondition(
    async () =>
      (await page.locator("#grid-selection").getAttribute("data-reference")) ===
        "A1" && !(await page.locator("#grid-input").isDisabled()),
    "large range anchor",
  );
  await page.evaluate(async () => {
    const { RenderWorkerClient } = await import(
      new URL("runtime/js/client.mjs", document.baseURI).href
    );
    const original = RenderWorkerClient.prototype.setRangeAndRecalculate;
    globalThis.__rangeTiming = {
      calls: 0,
      workerMs: 0,
      restore: () => {
        RenderWorkerClient.prototype.setRangeAndRecalculate = original;
      },
    };
    RenderWorkerClient.prototype.setRangeAndRecalculate = async function (
      ...args
    ) {
      globalThis.__rangeTiming.calls++;
      const start = performance.now();
      try {
        return await original.apply(this, args);
      } finally {
        globalThis.__rangeTiming.workerMs += performance.now() - start;
      }
    };
  });
  try {
    await page.locator("#grid-input").evaluate((element) => {
      const data = new DataTransfer();
      data.setData(
        "text/plain",
        Array.from({ length: 100 }, (_, row) =>
          Array.from({ length: 10 }, (_, col) =>
            row === 0 && col === 0 ? "'large range" : "2",
          ).join("\t"),
        ).join("\n"),
      );
      element.dispatchEvent(
        new ClipboardEvent("paste", {
          clipboardData: data,
          bubbles: true,
          cancelable: true,
        }),
      );
    });
    assert.match(
      await page.locator("#paste-summary").textContent(),
      /A1:J100.*1,000 cells/,
    );
    const start = performance.now();
    await page.locator("#apply-range-paste").click();
    await page.locator("#paste-dialog").waitFor({ state: "hidden" });
    await waitForViewerState(
      page,
      (state) => !state.busy && state.dirty,
      "large range committed and rendered",
    );
    const elapsedMs = performance.now() - start;
    const timing = await page.evaluate(() => ({
      calls: globalThis.__rangeTiming.calls,
      workerMs: globalThis.__rangeTiming.workerMs,
    }));
    assert.equal(
      timing.calls,
      1,
      "a thousand-cell paste is one worker transaction",
    );
    const saved = await downloadWorkbook(page, ".xlsx");
    await assertOpenpyxlReopens(saved.path, {
      expected: "large range",
      cacheCell: "P1001",
      expectedCache: 1098,
    });
    await page.locator("#undo-edit").click();
    await waitForViewerState(
      page,
      (state) => !state.busy && !state.dirty,
      "single large-range undo",
    );
    console.log(
      JSON.stringify({
        scenario: "large-range-paste",
        sourceCells: 16001,
        pastedCells: 1000,
        ...timing,
        applyAndRenderMs: elapsedMs,
      }),
    );
  } finally {
    await page.evaluate(() => {
      globalThis.__rangeTiming?.restore();
      delete globalThis.__rangeTiming;
    });
  }
  await page.locator("#file-input").setInputFiles(fileURLToPath(sample));
  await waitForViewerState(
    page,
    (state) =>
      state.fileName === "operations-report.xlsx" &&
      !state.busy &&
      !state.dirty,
    "original sample restored after scale check",
  );
}
