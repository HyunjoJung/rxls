import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const samplePath = fileURLToPath(
  new URL("../samples/operations-report.xlsx", import.meta.url),
);

/** Run on a clean, editable operations-report sample, and leave a fresh local copy. */
export async function exerciseEditingGuards(
  page,
  { waitForCondition, waitForViewerState },
) {
  const idle = (label) =>
    waitForViewerState(
      page,
      (state) =>
        state.fileName === "operations-report.xlsx" &&
        state.rendered &&
        !state.busy &&
        !state.dirty,
      label,
    );
  const downloads = [];
  const onDownload = (download) => downloads.push(download.suggestedFilename());
  page.on("download", onDownload);
  try {
    await page.getByRole("tab", { name: "Home", exact: true }).click();
    for (const kind of ["cell", "properties"]) {
      await page
        .locator(kind === "cell" ? "#edit-cell" : "#document-properties")
        .click();
      await page.locator(`#${kind}-dialog`).waitFor({ state: "visible" });
      if (kind === "cell") {
        await waitForCondition(
          async () => !(await page.locator("#apply-cell-edit").isDisabled()),
          "dialog cell read",
        );
        await page.locator("#cell-kind").selectOption("number");
        await page.locator("#cell-value").fill("invalid number draft");
      } else {
        await page.locator("#property-title").fill("Unapplied metadata draft");
        await page.locator("#property-created").fill("2026-09-08T15:30");
      }
      assert.equal(
        await page.evaluate(() => {
          const event = new Event("beforeunload", { cancelable: true });
          window.dispatchEvent(event);
          return event.defaultPrevented;
        }),
        true,
        `${kind} draft must guard reload/unload`,
      );
      await page.keyboard.press("ControlOrMeta+s");
      await waitForCondition(
        async () =>
          /Apply or Cancel/.test(
            await page.locator("#error-message").textContent(),
          ),
        "unapplied save warning",
      );
      assert.equal(
        downloads.length,
        0,
        "save must not omit unapplied dialog fields",
      );
      const draftField = kind === "cell" ? "#cell-value" : "#property-title";
      const draftValue = await page.locator(draftField).inputValue();
      await Promise.all([
        page.waitForEvent("dialog").then(async (dialog) => {
          assert.equal(dialog.type(), "confirm");
          assert.match(dialog.message(), /unapplied dialog changes/);
          await dialog.dismiss();
        }),
        page.locator("#file-input").setInputFiles(samplePath),
      ]);
      assert.equal(await page.locator(draftField).inputValue(), draftValue);
      assert.equal(await page.locator(`#${kind}-dialog`).isVisible(), true);
      await page.locator(`#cancel-${kind}-edit`).click();
      assert.equal(
        await page.evaluate(() => {
          const event = new Event("beforeunload", { cancelable: true });
          window.dispatchEvent(event);
          return event.defaultPrevented;
        }),
        false,
        "explicit Cancel clears only the unapplied draft",
      );
      if (await page.locator("#error-banner").isVisible())
        await page.locator("#dismiss-error").click();
    }

    // Accepted discard goes through the same real file-open event as a picker selection.
    await page.locator("#document-properties").click();
    await page
      .locator("#property-description")
      .fill("Discard this unapplied draft");
    await Promise.all([
      page.waitForEvent("dialog").then((dialog) => dialog.accept()),
      page.locator("#file-input").setInputFiles(samplePath),
    ]);
    await page.locator("#properties-dialog").waitFor({ state: "hidden" });
    await idle("accepted unapplied-dialog discard");

    for (const command of ["save"]) {
      for (const outcome of ["resolve", "reject"]) {
        for (const replacement of ["pending", "complete"]) {
          await installDelays(page, command, outcome);
          try {
            await page.locator("#quick-save").click();
            await waitForCondition(
              () =>
                page.evaluate(() => globalThis.__rxlsEditingGuards.captured),
              "worker result captured before replacement",
            );
            await page.locator("#file-input").setInputFiles(samplePath);
            await waitForCondition(
              () =>
                page.evaluate(() => globalThis.__rxlsEditingGuards.openStarted),
              "new open began before document identity changes",
            );
            if (replacement === "complete") {
              await page.evaluate(() =>
                globalThis.__rxlsEditingGuards.releaseOpen(),
              );
              await idle("replacement completes before old command response");
            }
            const before = await page.evaluate(() => ({
              state: globalThis.__rxlsViewerState(),
              status: document.getElementById("status-message").textContent,
            }));
            await page.evaluate(() =>
              globalThis.__rxlsEditingGuards.releaseOperation(),
            );
            await waitForCondition(
              () =>
                page.evaluate(() => globalThis.__rxlsEditingGuards.finished),
              "old command settles",
            );
            await page.evaluate(
              () =>
                new Promise((resolve) =>
                  requestAnimationFrame(() => requestAnimationFrame(resolve)),
                ),
            );
            const after = await page.evaluate(() => ({
              state: globalThis.__rxlsViewerState(),
              status: document.getElementById("status-message").textContent,
            }));
            assert.equal(
              after.state.busy,
              before.state.busy,
              `${command}/${outcome}/${replacement}: busy state`,
            );
            assert.equal(
              after.status,
              before.status,
              `${command}/${outcome}/${replacement}: status`,
            );
            assert.equal(after.state.dirty, false);
            assert.equal(await page.locator("#error-banner").isHidden(), true);
            assert.equal(
              downloads.length,
              0,
              "an obsolete save must never download",
            );
            if (replacement === "pending") {
              assert.equal(
                after.state.busy,
                true,
                "the held replacement open must remain busy",
              );
              await page.evaluate(() =>
                globalThis.__rxlsEditingGuards.releaseOpen(),
              );
              await idle("replacement completes after ignored old response");
            }
          } finally {
            await page.evaluate(() =>
              globalThis.__rxlsEditingGuards?.restore(),
            );
          }
        }
      }
    }
    for (const outcome of ["resolve", "reject"]) {
      await installDelays(page, "cell", outcome);
      try {
        await page.locator("#edit-cell").click();
        await page.locator("#cell-reference").fill("C4");
        await page.locator("#read-cell").click();
        await waitForCondition(
          async () =>
            !(await page.locator("#apply-cell-edit").isDisabled()) &&
            (
              await page.locator("#cell-current-value").textContent()
            ).startsWith("C4:"),
          "pending-submit source loaded",
        );
        await page.locator("#cell-value").fill("7");
        await page.locator("#apply-cell-edit").click();
        await waitForCondition(
          () => page.evaluate(() => globalThis.__rxlsEditingGuards.captured),
          "authoritative cell response held",
        );
        await page.locator("#file-input").setInputFiles(samplePath);
        await waitForCondition(
          async () =>
            /wait.*edit|edit.*finish/i.test(
              await page.locator("#error-message").textContent(),
            ),
          "replacement blocked while edit pending",
        );
        assert.equal(
          await page.evaluate(() => globalThis.__rxlsEditingGuards.openStarted),
          false,
        );
        assert.equal(
          (await page.evaluate(() => globalThis.__rxlsViewerState())).busy,
          true,
        );
        assert.equal(await page.locator("#cell-dialog").isVisible(), true);
        assert.equal(await page.locator("#cell-value").inputValue(), "7");
        await page.evaluate(() =>
          globalThis.__rxlsEditingGuards.releaseOperation(),
        );
        await waitForViewerState(
          page,
          (state) => !state.busy && state.dirty === (outcome === "resolve"),
          "authoritative edit settles before replacement",
        );
        if (outcome === "reject") {
          assert.equal(await page.locator("#cell-dialog").isVisible(), true);
          assert.equal(await page.locator("#cell-value").inputValue(), "7");
        }
      } finally {
        await page.evaluate(() => globalThis.__rxlsEditingGuards?.restore());
      }
      await Promise.all([
        page.waitForEvent("dialog").then((dialog) => dialog.accept()),
        page.locator("#file-input").setInputFiles(samplePath),
      ]);
      await page.locator("#cell-dialog").waitFor({ state: "hidden" });
      await idle("replacement allowed after authoritative edit settles");
      assert.equal(await page.locator("#error-banner").isHidden(), true);
    }
    console.log(
      "PASS dialog drafts/discard/save guard, 4 stale save responses, and 2 authoritative edit replacement guards",
    );
  } finally {
    page.off("download", onDownload);
  }
}

async function installDelays(page, command, outcome) {
  await page.evaluate(
    async ({ command, outcome }) => {
      const { RenderWorkerClient } = await import(
        new URL("runtime/js/client.mjs", document.baseURI).href
      );
      const prototype = RenderWorkerClient.prototype;
      const method =
        command === "save" ? "saveDocument" : "setCellAndRecalculate";
      const originalOperation = prototype[method];
      const originalOpen = prototype.open;
      let releaseOperation;
      let releaseOpen;
      const operationGate = new Promise((resolve) => {
        releaseOperation = resolve;
      });
      const openGate = new Promise((resolve) => {
        releaseOpen = resolve;
      });
      const guard = (globalThis.__rxlsEditingGuards = {
        captured: false,
        openStarted: false,
        finished: false,
        releaseOperation,
        releaseOpen,
        restore() {
          prototype[method] = originalOperation;
          prototype.open = originalOpen;
          releaseOperation();
          releaseOpen();
          delete globalThis.__rxlsEditingGuards;
        },
      });
      prototype[method] = async function (...args) {
        const result =
          command === "cell" && outcome === "reject"
            ? null
            : await originalOperation.apply(this, args);
        guard.captured = true;
        await operationGate;
        try {
          if (outcome === "reject")
            throw new Error("Obsolete editing guard response");
          return result;
        } finally {
          guard.finished = true;
        }
      };
      prototype.open = async function (...args) {
        guard.openStarted = true;
        await openGate;
        return originalOpen.apply(this, args);
      };
    },
    { command, outcome },
  );
}
