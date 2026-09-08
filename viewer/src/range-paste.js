import { MAX_PASTE_BYTES, rangePasteRequest } from "./clipboard.js";
import { describeError } from "./core.js";

/** Preview native clipboard events before submitting one atomic worker transaction. */
export function createRangePasteController({ grid, elements, showError }) {
  const dialog = elements["paste-dialog"];
  const status = elements["paste-status"];
  const summary = elements["paste-summary"];
  const preview = elements["paste-preview"];
  const applyButton = elements["apply-range-paste"];
  const textButton = elements["paste-as-text"];
  const cancelButton = elements["cancel-range-paste"];
  let pending = null;
  let applying = false;

  elements["document-surface"].addEventListener("paste", onPaste);
  applyButton.addEventListener("click", () => void apply(false));
  textButton.addEventListener("click", () => void apply(true));
  cancelButton.addEventListener("click", () => cancel());
  dialog.addEventListener("cancel", (event) => {
    event.preventDefault();
    if (!applying) cancel();
  });
  dialog.addEventListener("keydown", (event) => {
    // Keep Escape owned by this modal; the window shortcut cancels cell drafts.
    if (event.key === "Escape") event.stopPropagation();
    if (
      (event.ctrlKey || event.metaKey) &&
      ["o", "s", "z", "y"].includes(event.key.toLowerCase())
    ) {
      event.preventDefault();
      event.stopPropagation();
      ready();
    }
  });
  return { hasPending: () => Boolean(pending), ready, cancel };

  function ready() {
    if (!pending) return true;
    status.textContent = applying
      ? "Applying the range…"
      : "Apply or cancel the paste before using another command.";
    return false;
  }

  function close() {
    pending = null;
    preview.replaceChildren();
    if (dialog.open) dialog.close();
  }

  function cancel() {
    if (applying) return false;
    close();
    return true;
  }

  function onPaste(event) {
    if (pending || applying || event.defaultPrevented) return;
    const target = grid.getPasteTarget();
    if (!target || event.isComposing) return;
    const text = event.clipboardData?.getData("text/plain");
    if (typeof text !== "string" || !/[\t\r\n]/.test(text)) return;
    event.preventDefault();
    if (
      text.length > MAX_PASTE_BYTES ||
      new TextEncoder().encode(text).length > MAX_PASTE_BYTES
    ) {
      showError(
        new RangeError("Clipboard size exceeds the 1 MiB paste limit."),
      );
      return;
    }
    let request = null;
    let parseError = null;
    try {
      request = rangePasteRequest(text, target.row, target.col, target);
    } catch (error) {
      parseError = error;
    }
    pending = { target, text, request };
    applying = false;
    applyButton.disabled = !request;
    textButton.disabled = false;
    cancelButton.disabled = false;
    preview.replaceChildren();
    summary.textContent = request
      ? `${request.reference} · ${request.rows.length} rows × ${request.rows[0].length} columns · ${request.cellCount.toLocaleString("en-US")} cells`
      : "This clipboard cannot be pasted as a range.";
    status.textContent = parseError
      ? describeError(parseError)
      : "Applying replaces these cells and any current in-cell draft. Undo restores the whole range. Formulas use the pasted references as written.";
    if (request) {
      const document = preview.ownerDocument;
      for (const row of request.rows.slice(0, 5)) {
        const tr = document.createElement("tr");
        for (const value of row.slice(0, 5)) {
          const td = document.createElement("td");
          td.textContent =
            value.length > 80 ? `${value.slice(0, 80)}…` : value || "(blank)";
          tr.append(td);
        }
        preview.append(tr);
      }
    }
    dialog.showModal();
    // Enter must not apply an unreviewed overwrite immediately after paste.
    cancelButton.focus();
  }

  async function apply(asText) {
    if (!pending || applying) return;
    const current = pending;
    let request;
    try {
      request = asText
        ? rangePasteRequest(
            current.text,
            current.target.row,
            current.target.col,
            { ...current.target, asText: true },
          )
        : current.request;
      if (!request) return;
    } catch (error) {
      status.textContent = describeError(error);
      return;
    }
    applying = true;
    applyButton.disabled = textButton.disabled = cancelButton.disabled = true;
    status.textContent = "Applying the range…";
    try {
      if (await grid.applyRange(current.target, request.values)) close();
      else
        status.textContent =
          "The paste was not applied. Your draft is retained. Check the error or cancel and select the destination again.";
    } catch (error) {
      status.textContent = describeError(error);
      showError(error);
    } finally {
      applying = false;
      if (pending === current) {
        applyButton.disabled = !current.request;
        textButton.disabled = cancelButton.disabled = false;
        cancelButton.focus();
      }
    }
  }
}
