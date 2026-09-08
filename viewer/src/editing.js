import {
  createLatestRequestGate,
  describeError,
  editableCell,
  editReasonLabel,
  extensionOf,
  formatBytes,
  parseCellReference,
  sameCellTarget,
  savedWorkbookName,
} from "./core.js";
import { downloadBlob } from "./browser-files.js";

/** Own the editing dialogs and commands without owning workbook loading or rendering. */
export function createEditingController({
  state,
  elements,
  readOnly = false,
  setBusy,
  showError,
  updateWorkbookUi,
  renderCurrent,
  beforeCommand,
  download = downloadBlob,
  confirm = (message) => globalThis.confirm(message),
}) {
  const cellReads = createLatestRequestGate();
  let mutationPending = null;
  let cellBaseline = null;
  let propertiesBaseline = null;
  let cellDialogGeneration = 0;
  let propertiesDialogGeneration = 0;
  let recalculationWarning = "";
  let warningContext = null;
  let warningSummary = null;
  const warningUndo = [];
  const warningRedo = [];
  // Match the worker's bounded edit history; retain counts, never workbook text.
  const maxWarningHistory = 20;
  const editing = {
    cellReadTarget: null,
    cellReadPending: false,
    cellEditPending: false,
  };

  return {
    bindEvents,
    updateEditUi,
    openCellEditor,
    closeCellEditor,
    openPropertiesEditor,
    closePropertiesEditor,
    applyHistoryEdit,
    saveWorkbookCopy,
    commitCellEdit,
    commitRangeEdit,
    hasDraftChanges,
    hasPendingMutation,
    confirmDiscardChanges,
  };

  function bindEvents() {
    elements["edit-cell"].addEventListener(
      "click",
      () => void openCellEditor(),
    );
    elements["undo-edit"].addEventListener(
      "click",
      () => void applyHistoryEdit("undo"),
    );
    elements["redo-edit"].addEventListener(
      "click",
      () => void applyHistoryEdit("redo"),
    );
    elements["document-properties"].addEventListener(
      "click",
      openPropertiesEditor,
    );
    elements["save-document"].addEventListener(
      "click",
      () => void saveWorkbookCopy(),
    );
    elements["close-cell-dialog"].addEventListener("click", closeCellEditor);
    elements["cancel-cell-edit"].addEventListener("click", closeCellEditor);
    elements["cell-dialog"].addEventListener("cancel", (event) => {
      event.preventDefault();
      if (!editing.cellEditPending) closeCellEditor();
    });
    elements["read-cell"].addEventListener(
      "click",
      () => void loadCellIntoEditor(),
    );
    elements["cell-reference"].addEventListener("input", invalidateCellRead);
    elements["cell-reference"].addEventListener(
      "change",
      () => void loadCellIntoEditor(),
    );
    elements["cell-kind"].addEventListener("change", updateCellKindUi);
    elements["cell-form"].addEventListener(
      "submit",
      (event) => void submitCellEdit(event),
    );
    elements["close-properties-dialog"].addEventListener(
      "click",
      closePropertiesEditor,
    );
    elements["cancel-properties-edit"].addEventListener(
      "click",
      closePropertiesEditor,
    );
    elements["properties-dialog"].addEventListener("cancel", (event) => {
      event.preventDefault();
      if (!hasPendingMutation()) closePropertiesEditor();
    });
    elements["properties-form"].addEventListener(
      "submit",
      (event) => void submitPropertiesEdit(event),
    );
  }

  function updateEditUi() {
    syncWarningContext();
    const editState = state.editState;
    const editable = editState?.capability === "read-write";
    const hostReadOnly = Boolean(state.workbook && readOnly);
    const available = Boolean(
      state.workbook && editable && !state.busy && !readOnly,
    );
    const sourceReadOnly = Boolean(
      state.workbook && !editable && !hostReadOnly,
    );
    const reason = sourceReadOnly ? editReasonLabel(editState?.reason) : null;
    const status = hostReadOnly
      ? "Read-only"
      : editable
        ? editState.dirty
          ? "Modified"
          : "Available"
        : "Read-only";
    elements["meta-editing"].textContent = state.workbook ? status : "-";
    elements["meta-editing"].title = hostReadOnly
      ? "VS Code previews do not modify source workbooks."
      : (reason ?? status);
    elements["meta-editing"].classList.toggle(
      "dirty",
      Boolean(editState?.dirty && !hostReadOnly),
    );
    elements["editing-reason"].hidden = !reason;
    elements["editing-reason"].textContent = reason ?? "";
    elements["edit-cell"].disabled = !available;
    elements["document-properties"].disabled = !available;
    elements["undo-edit"].disabled = !available || !editState.canUndo;
    elements["redo-edit"].disabled = !available || !editState.canRedo;
    elements["save-document"].disabled = !available;
    for (const control of [
      elements["edit-cell"],
      elements["document-properties"],
      elements["save-document"],
    ]) {
      control.title = reason ?? control.dataset.commandTitle ?? control.title;
      if (reason) {
        control.setAttribute("aria-describedby", "editing-reason");
      } else {
        control.removeAttribute("aria-describedby");
      }
    }
    if (editable && !readOnly) {
      elements["edit-cell"].title = "Advanced typed cell options";
      elements["document-properties"].title = "Document properties";
      elements["save-document"].title = "Download preserved workbook";
    }
    if (state.file) {
      elements["document-detail"].textContent =
        `${state.file.source} · ${formatBytes(state.file.size)}${
          editState?.dirty ? " · Modified" : ""
        }`;
    }
    updateCellEditorControls();
  }

  async function openCellEditor() {
    if (state.busy || hasPendingMutation()) return;
    const target = currentTarget();
    if (beforeCommand && !(await beforeCommand())) return;
    if (!isCurrentTarget(target) || !canEditWorkbook()) {
      return;
    }
    if (elements["cell-dialog"].open) return;
    elements["cell-sheet-name"].textContent =
      state.workbook.sheets[state.sheetIndex].name;
    if (!elements["cell-dialog"].open) {
      elements["cell-dialog"].showModal();
    }
    cellDialogGeneration++;
    resetCellEditorFields();
    cellBaseline = snapshot(cellFields());
    invalidateCellRead();
    elements["cell-reference"].focus();
    elements["cell-reference"].select();
    await loadCellIntoEditor();
  }

  function closeCellEditor() {
    cellDialogGeneration++;
    cellBaseline = null;
    cellReads.invalidate();
    editing.cellReadTarget = null;
    editing.cellReadPending = false;
    editing.cellEditPending = false;
    if (elements["cell-dialog"].open) {
      elements["cell-dialog"].close();
    }
    resetCellEditorFields();
    updateCellEditorControls();
  }

  function invalidateCellRead() {
    cellReads.invalidate();
    editing.cellReadPending = false;
    if (elements["cell-dialog"].open) {
      elements["cell-current-value"].textContent =
        "Load this cell before editing it";
    }
    updateCellEditorControls();
  }

  async function loadCellIntoEditor() {
    if (!canEditWorkbook()) {
      return;
    }
    if (
      changedSnapshot(cellBaseline, cellFields().slice(1)) &&
      !confirm(
        "Discard unapplied Cell Options changes before loading this cell?",
      )
    )
      return;
    const target = currentTarget();
    const token = cellReads.begin();
    const client = state.client;
    const documentId = state.documentId;
    const sheetIndex = state.sheetIndex;
    editing.cellReadTarget = null;
    editing.cellReadPending = true;
    resetCellEditorFields();
    updateCellEditorControls();
    let coordinate;
    try {
      coordinate = parseCellReference(elements["cell-reference"].value);
      elements["cell-reference"].value = coordinate.normalized;
      cellBaseline = snapshot(cellFields());
      elements["cell-current-value"].textContent =
        `Loading ${coordinate.normalized}`;
      const result = await client.readCell(
        documentId,
        sheetIndex,
        coordinate.row,
        coordinate.col,
      );
      if (
        !cellReads.isCurrent(token) ||
        !isCurrentTarget(target) ||
        client !== state.client ||
        documentId !== state.documentId ||
        sheetIndex !== state.sheetIndex ||
        !elements["cell-dialog"].open
      ) {
        return;
      }
      editing.cellReadTarget = cellTarget(
        coordinate,
        client,
        documentId,
        sheetIndex,
      );
      editing.cellReadPending = false;
      populateCellEditor(result.value);
      cellBaseline = snapshot(cellFields());
      elements["cell-current-value"].textContent = result.formatted
        ? `${coordinate.normalized}: ${result.formatted}`
        : `${coordinate.normalized}: ${describeCell(result.value)}`;
    } catch (error) {
      if (!cellReads.isCurrent(token) || !isCurrentTarget(target)) {
        return;
      }
      editing.cellReadTarget = null;
      editing.cellReadPending = false;
      elements["cell-current-value"].textContent = describeError(error);
      showError(error);
    } finally {
      if (cellReads.isCurrent(token) && isCurrentTarget(target)) {
        editing.cellReadPending = false;
        updateCellEditorControls();
      }
    }
  }

  function populateCellEditor(cell) {
    resetCellEditorFields();
    if (cell.kind === "formula") {
      elements["cell-kind"].value = "formula";
      elements["cell-formula"].value = cell.formula;
      elements["cell-cached-kind"].value = cell.cached.kind;
      elements["cell-cached-value"].value = scalarInputValue(cell.cached);
    } else {
      elements["cell-kind"].value = cell.kind;
      elements["cell-value"].value = scalarInputValue(cell);
    }
    updateCellKindUi();
  }

  function resetCellEditorFields() {
    elements["cell-kind"].value = "text";
    elements["cell-value"].value = "";
    elements["cell-formula"].value = "";
    elements["cell-cached-kind"].value = "number";
    elements["cell-cached-value"].value = "";
    updateCellKindUi();
  }

  function cellTarget(
    coordinate,
    client = state.client,
    documentId = state.documentId,
    sheetIndex = state.sheetIndex,
  ) {
    return {
      client,
      documentId,
      sheetIndex,
      row: coordinate.row,
      col: coordinate.col,
    };
  }

  function currentCellTarget() {
    try {
      return cellTarget(parseCellReference(elements["cell-reference"].value));
    } catch {
      return null;
    }
  }

  function updateCellEditorControls() {
    const dialogOpen = elements["cell-dialog"].open;
    const loaded = sameCellTarget(editing.cellReadTarget, currentCellTarget());
    const ready =
      dialogOpen && canEditWorkbook() && loaded && !editing.cellReadPending;
    const valuesDisabled = !ready || editing.cellEditPending;
    for (const control of [
      elements["cell-kind"],
      elements["cell-value"],
      elements["cell-formula"],
      elements["cell-cached-kind"],
      elements["cell-cached-value"],
    ]) {
      control.disabled = valuesDisabled;
    }
    elements["cell-reference"].disabled = editing.cellEditPending;
    elements["read-cell"].disabled =
      !dialogOpen ||
      !canEditWorkbook() ||
      editing.cellReadPending ||
      editing.cellEditPending;
    elements["apply-cell-edit"].disabled = !ready || editing.cellEditPending;
    elements["close-cell-dialog"].disabled = editing.cellEditPending;
    elements["cancel-cell-edit"].disabled = editing.cellEditPending;
  }

  function updateCellKindUi() {
    const kind = elements["cell-kind"].value;
    const formula = kind === "formula";
    elements["cell-value-field"].hidden = formula || kind === "blank";
    elements["cell-formula-fields"].hidden = !formula;
    elements["cell-formula"].required = formula;
    elements["cell-cached-value"].required = formula;
    elements["cell-value"].required = !formula && kind !== "blank";
  }

  async function submitCellEdit(event) {
    event.preventDefault();
    if (!canEditWorkbook()) {
      return;
    }
    const target = currentTarget();
    const dialogGeneration = cellDialogGeneration;
    const isCurrent = () =>
      isCurrentTarget(target) && dialogGeneration === cellDialogGeneration;
    try {
      const coordinate = parseCellReference(elements["cell-reference"].value);
      if (!sameCellTarget(editing.cellReadTarget, cellTarget(coordinate))) {
        throw new Error(
          `Load ${coordinate.normalized} before applying an edit.`,
        );
      }
      const value = editableCell(
        elements["cell-kind"].value,
        elements["cell-value"].value,
        {
          formula: elements["cell-formula"].value,
          cachedKind: elements["cell-cached-kind"].value,
          cachedValue: elements["cell-cached-value"].value,
        },
      );
      editing.cellEditPending = true;
      updateCellEditorControls();
      elements["cell-current-value"].textContent =
        `Applying ${coordinate.normalized}`;
      if ((await commitCellEdit(cellTarget(coordinate), value)) && isCurrent())
        closeCellEditor();
    } catch (error) {
      if (isCurrent()) {
        elements["cell-current-value"].textContent = describeError(error);
        showError(error);
      }
    } finally {
      if (isCurrent()) {
        editing.cellEditPending = false;
        updateCellEditorControls();
        updateCellKindUi();
      }
    }
  }

  async function openPropertiesEditor() {
    if (state.busy || hasPendingMutation()) return;
    const target = currentTarget();
    if (beforeCommand && !(await beforeCommand())) return;
    if (!isCurrentTarget(target) || !canEditWorkbook()) {
      return;
    }
    if (elements["properties-dialog"].open) return;
    const properties = state.workbook.properties;
    for (const [property, elementId] of propertyFields()) {
      elements[elementId].value = properties[property] ?? "";
    }
    propertiesDialogGeneration++;
    propertiesBaseline = snapshot(propertyFields().map(([, id]) => id));
    setFormPending(elements["properties-form"], false);
    if (!elements["properties-dialog"].open) {
      elements["properties-dialog"].showModal();
    }
    elements["property-title"].focus();
  }

  function closePropertiesEditor() {
    propertiesDialogGeneration++;
    propertiesBaseline = null;
    setFormPending(elements["properties-form"], false);
    if (elements["properties-dialog"].open) {
      elements["properties-dialog"].close();
    }
  }

  async function submitPropertiesEdit(event) {
    event.preventDefault();
    if (!canEditWorkbook()) {
      return;
    }
    const target = currentTarget();
    const dialogGeneration = propertiesDialogGeneration;
    mutationPending = target;
    try {
      setFormPending(elements["properties-form"], true);
      setBusy(true, "Updating document properties");
      const properties = Object.fromEntries(
        propertyFields().map(([property, elementId]) => [
          property,
          elements[elementId].value || null,
        ]),
      );
      const result = await target.client.setDocumentProperties(
        target.documentId,
        properties,
      );
      if (!isCurrentTarget(target)) return;
      closePropertiesEditor();
      await applyMutationResult(result, "Document properties updated", target);
    } catch (error) {
      if (isCurrentTarget(target)) showError(error);
    } finally {
      if (mutationPending === target) mutationPending = null;
      if (dialogGeneration === propertiesDialogGeneration) {
        setFormPending(elements["properties-form"], false);
      }
      if (isCurrentTarget(target)) {
        setBusy(false);
      }
    }
  }

  async function applyHistoryEdit(direction) {
    if (state.busy || hasPendingMutation() || rejectUnappliedDrafts()) return;
    const target = currentTarget();
    if (beforeCommand && !(await beforeCommand())) return;
    if (!isCurrentTarget(target) || !canEditWorkbook()) {
      return;
    }
    mutationPending = target;
    try {
      setBusy(true, direction === "undo" ? "Undoing edit" : "Redoing edit");
      const result =
        direction === "undo"
          ? await target.client.undoEdit(target.documentId)
          : await target.client.redoEdit(target.documentId);
      await applyMutationResult(
        result,
        direction === "undo" ? "Edit undone" : "Edit redone",
        target,
        direction,
      );
    } catch (error) {
      if (isCurrentTarget(target)) {
        showError(error);
      }
    } finally {
      if (mutationPending === target) mutationPending = null;
      if (isCurrentTarget(target)) setBusy(false);
    }
  }

  async function applyMutationResult(
    result,
    message,
    target,
    history = "edit",
  ) {
    if (!isCurrentTarget(target)) return false;
    syncWarningContext();
    const report = updateWarningHistory(result, history);
    state.workbook = result.workbook;
    state.editState = result.editState;
    state.manifests.clear();
    // Pagination may change; renderCurrent clamps against the refreshed manifest.
    state.sheetIndex = Math.min(
      state.sheetIndex,
      Math.max(0, state.workbook.sheetCount - 1),
    );
    updateWorkbookUi();
    await renderCurrent({ fit: false });
    if (isCurrentTarget(target)) {
      elements["status-message"].textContent = summarizeRecalculation(
        report,
        message,
      );
    }
    return isCurrentTarget(target);
  }

  function clearWarning() {
    if (
      recalculationWarning &&
      elements["error-message"].textContent === recalculationWarning
    ) {
      elements["error-banner"].hidden = true;
      elements["error-message"].textContent = "";
    }
    if (
      recalculationWarning &&
      elements["status-message"].textContent === recalculationWarning
    ) {
      elements["status-message"].textContent = "";
    }
    recalculationWarning = "";
  }

  function syncWarningContext() {
    if (
      state.workbook &&
      warningContext &&
      warningContext.client === state.client &&
      warningContext.documentId === state.documentId
    )
      return;
    clearWarning();
    warningSummary = null;
    warningUndo.length = warningRedo.length = 0;
    warningContext = state.workbook ? currentTarget() : null;
  }

  function updateWarningHistory(result, direction) {
    if (direction === "undo" || direction === "redo") {
      const source = direction === "undo" ? warningUndo : warningRedo;
      const destination = direction === "undo" ? warningRedo : warningUndo;
      destination.push(warningSummary);
      // If an older snapshot was evicted, retain uncertainty rather than claiming clean caches.
      if (source.length) warningSummary = source.pop();
    } else {
      warningUndo.push(warningSummary);
      warningRedo.length = 0;
    }
    for (const [entries, depth] of [
      [warningUndo, result.editState?.undoDepth],
      [warningRedo, result.editState?.redoDepth],
    ]) {
      const limit =
        Number.isSafeInteger(depth) && depth >= 0
          ? Math.min(depth, maxWarningHistory)
          : maxWarningHistory;
      entries.splice(0, Math.max(0, entries.length - limit));
    }
    const report = result.recalculation;
    if (
      !report ||
      !Number.isSafeInteger(report.computedCells) ||
      report.computedCells < 0 ||
      !Number.isSafeInteger(report.unsupportedCells) ||
      report.unsupportedCells < 0
    )
      return null;
    const summary = {
      computedCells: report.computedCells,
      unsupportedCells: report.unsupportedCells,
    };
    warningSummary = summary.unsupportedCells > 0 ? summary : null;
    return summary;
  }

  function summarizeRecalculation(report, message) {
    // Metadata/history do not recalculate; an absent report must not claim clean caches.
    clearWarning();
    if (!report && !warningSummary) return message;
    if (!report) {
      const count = warningSummary.unsupportedCells;
      recalculationWarning = `${message}. ${count} unsupported formula cell${count === 1 ? " still uses" : "s still use"} cached values.`;
      elements["error-message"].textContent = recalculationWarning;
      elements["error-banner"].hidden = false;
      return recalculationWarning;
    }
    const computed = `${report.computedCells} formula cell${report.computedCells === 1 ? "" : "s"} recalculated`;
    if (report.unsupportedCells === 0) return `${message}. ${computed}.`;
    const unsupported = `${report.unsupportedCells} unsupported formula cell${report.unsupportedCells === 1 ? "" : "s"}`;
    recalculationWarning = `${message}. ${computed}; ${unsupported} kept their cached values.`;
    elements["error-message"].textContent = recalculationWarning;
    elements["error-banner"].hidden = false;
    return recalculationWarning;
  }

  /** Commit a captured cell target through the same retained-package/render path as the dialog. */
  async function commitCellEdit(target, value) {
    return commitMutation(
      target,
      (operation) => {
        const setCell =
          typeof operation.client.setCellAndRecalculate === "function"
            ? operation.client.setCellAndRecalculate
            : operation.client.setCell;
        return setCell.call(
          operation.client,
          operation.documentId,
          operation.sheetIndex,
          target.row,
          target.col,
          value,
        );
      },
      () => `${cellReference(target.row, target.col)} updated`,
    );
  }

  /** Apply a bounded rectangular paste as one worker mutation and one undo entry. */
  async function commitRangeEdit(target, values) {
    if (typeof target?.client?.setRangeAndRecalculate !== "function") {
      throw new Error("This rendering runtime does not support range editing.");
    }
    return commitMutation(
      target,
      (operation) =>
        operation.client.setRangeAndRecalculate(
          operation.documentId,
          operation.sheetIndex,
          target.row,
          target.col,
          values,
        ),
      "Pasted cells updated",
    );
  }

  async function commitMutation(target, apply, message) {
    if (!canEditWorkbook() || hasPendingMutation()) {
      throw new Error(
        "Cell editing is unavailable while the workbook is read-only or busy.",
      );
    }
    if (!isCurrentTarget(target)) {
      throw new Error(
        "The workbook or sheet changed. Select the cell again before editing.",
      );
    }
    if (
      !Number.isInteger(target.row) ||
      target.row < 0 ||
      target.row > 1_048_575 ||
      !Number.isInteger(target.col) ||
      target.col < 0 ||
      target.col > 16_383
    ) {
      throw new RangeError(
        "The cell reference is outside the XLSX worksheet grid.",
      );
    }
    const operation = currentTarget();
    mutationPending = operation;
    setBusy(true, "Applying cell edit");
    try {
      const result = await apply(operation);
      if (!isCurrentTarget(operation)) return false;
      return await applyMutationResult(
        result,
        typeof message === "function" ? message() : message,
        operation,
      );
    } catch (error) {
      if (!isCurrentTarget(operation)) return false;
      throw error;
    } finally {
      if (mutationPending === operation) mutationPending = null;
      if (isCurrentTarget(operation)) setBusy(false);
    }
  }

  function isCurrentTarget(target) {
    return Boolean(
      target &&
        target.client === state.client &&
        target.documentId === state.documentId &&
        target.sheetIndex === state.sheetIndex &&
        (target.openGeneration === undefined ||
          target.openGeneration === (state.openGeneration ?? 0)),
    );
  }

  function currentTarget() {
    return {
      client: state.client,
      documentId: state.documentId,
      sheetIndex: state.sheetIndex,
      openGeneration: state.openGeneration ?? 0,
    };
  }

  function cellReference(row, col) {
    let letters = "";
    for (let n = col + 1; n > 0; n = Math.floor((n - 1) / 26)) {
      letters = String.fromCharCode(65 + ((n - 1) % 26)) + letters;
    }
    return `${letters}${row + 1}`;
  }

  async function saveWorkbookCopy() {
    if (state.busy || hasPendingMutation() || rejectUnappliedDrafts()) return;
    const target = currentTarget();
    if (beforeCommand && !(await beforeCommand())) return;
    if (
      !isCurrentTarget(target) ||
      !canEditWorkbook() ||
      rejectUnappliedDrafts()
    ) {
      return;
    }
    const fileName = state.file.name;
    try {
      setBusy(true, "Preparing preserved workbook");
      const saved = await target.client.saveDocument(target.documentId);
      if (!isCurrentTarget(target)) return;
      const extension = extensionOf(fileName);
      const mimeType =
        extension === "xlsm"
          ? "application/vnd.ms-excel.sheet.macroEnabled.12"
          : "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
      download(
        new Blob([saved.bytes], { type: mimeType }),
        savedWorkbookName(fileName),
      );
      elements["status-message"].textContent = "Preserved workbook downloaded";
    } catch (error) {
      if (isCurrentTarget(target)) showError(error);
    } finally {
      if (isCurrentTarget(target)) {
        setBusy(false);
        elements["export-menu"].removeAttribute("open");
      }
    }
  }

  function canEditWorkbook() {
    return Boolean(
      !readOnly &&
        !hasPendingMutation() &&
        state.client &&
        state.workbook &&
        !state.busy &&
        state.editState?.capability === "read-write",
    );
  }

  function confirmDiscardChanges() {
    const draft = hasDraftChanges();
    return (
      !(state.editState?.dirty || draft) ||
      confirm(
        draft
          ? "Discard unsaved workbook edits and unapplied dialog changes?"
          : "Discard unsaved workbook edits?",
      )
    );
  }

  function hasPendingMutation() {
    return Boolean(mutationPending && isCurrentTarget(mutationPending));
  }

  function cellFields() {
    return [
      "cell-reference",
      "cell-kind",
      "cell-value",
      "cell-formula",
      "cell-cached-kind",
      "cell-cached-value",
    ];
  }

  function snapshot(fields) {
    return {
      target: currentTarget(),
      values: Object.fromEntries(fields.map((id) => [id, elements[id].value])),
    };
  }

  function changedSnapshot(baseline, fields) {
    return Boolean(
      baseline &&
        isCurrentTarget(baseline.target) &&
        fields.some((id) => baseline.values[id] !== elements[id].value),
    );
  }

  function hasDraftChanges() {
    return Boolean(
      (elements["cell-dialog"].open &&
        changedSnapshot(cellBaseline, cellFields())) ||
        (elements["properties-dialog"].open &&
          changedSnapshot(
            propertiesBaseline,
            propertyFields().map(([, id]) => id),
          )),
    );
  }

  function rejectUnappliedDrafts() {
    if (!hasDraftChanges()) return false;
    showError(
      new Error(
        "Apply or Cancel the unapplied dialog changes before continuing.",
      ),
    );
    return true;
  }

  function setFormPending(form, pending) {
    for (const button of form.querySelectorAll(
      "button, input, textarea, select",
    )) {
      button.disabled = pending;
    }
  }

  function propertyFields() {
    return [
      ["title", "property-title"],
      ["subject", "property-subject"],
      ["creator", "property-creator"],
      ["keywords", "property-keywords"],
      ["description", "property-description"],
      ["lastModifiedBy", "property-last-modified-by"],
      ["company", "property-company"],
      ["created", "property-created"],
    ];
  }

  function scalarInputValue(cell) {
    return cell.kind === "blank" ? "" : String(cell.value);
  }

  function describeCell(cell) {
    if (cell.kind === "blank") {
      return "Blank";
    }
    if (cell.kind === "formula") {
      return `=${cell.formula}`;
    }
    return String(cell.value);
  }
}
