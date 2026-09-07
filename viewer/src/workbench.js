import { describeError, parseCellReference } from "./core.js";

/** Show source values without coercing dates, formulas, or blank cells. */
export function inspectorText(cell) {
  if (cell.kind === "blank") return "";
  if (cell.kind === "formula") return `=${cell.formula}`;
  if (cell.kind === "boolean") return cell.value ? "TRUE" : "FALSE";
  return String(cell.value ?? "");
}

/** Ribbon and cell inspection own presentation, never workbook mutation. */
export function createWorkbench({
  state,
  editing,
  readOnly,
  openFile,
  setZoom,
  onInspect = () => {},
  onEditCell,
  dom = document,
}) {
  const ids = [
    "ribbon-file",
    "tab-home",
    "tab-view",
    "ribbon-home",
    "ribbon-view",
    "workbook-mode",
    "panel-open",
    "quick-save",
    "view-workbook-panel",
    "toggle-inspector",
    "reset-zoom",
    "cell-inspector",
    "inspector-reference",
    "inspect-cell",
    "inspector-value",
    "inspector-kind",
    "inspector-edit",
    "inspector-status",
    "cell-reference",
    "sidebar",
    "sidebar-toggle",
  ];
  const ui = Object.fromEntries(ids.map((id) => [id, dom.getElementById(id)]));
  let request = 0;
  let loaded = null;
  let context = null;
  let pending = false;
  let cellDraft = null;

  ui["ribbon-file"].addEventListener("click", () => togglePanel());
  ui["view-workbook-panel"].addEventListener("click", () => togglePanel());
  ui["panel-open"].addEventListener("click", openFile);
  ui["quick-save"].addEventListener(
    "click",
    () => void editing.saveWorkbookCopy(),
  );
  ui["reset-zoom"].addEventListener("click", () => setZoom(1));
  ui["toggle-inspector"].addEventListener("click", () => {
    const hidden = !ui["cell-inspector"].hidden;
    ui["cell-inspector"].hidden = hidden;
    ui["toggle-inspector"].setAttribute("aria-pressed", String(!hidden));
  });
  for (const name of ["home", "view"]) {
    ui[`tab-${name}`].addEventListener("click", () => selectTab(name));
    ui[`tab-${name}`].addEventListener("keydown", (event) => {
      if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key))
        return;
      event.preventDefault();
      const next =
        event.key === "Home"
          ? "home"
          : event.key === "End"
            ? "view"
            : name === "home"
              ? "view"
              : "home";
      selectTab(next);
      ui[`tab-${next}`].focus();
    });
  }
  ui["cell-inspector"].addEventListener("submit", (event) => {
    event.preventDefault();
    void inspect();
  });
  ui["inspector-reference"].addEventListener("input", invalidate);
  ui["inspector-edit"].addEventListener("click", async () => {
    if (!canEdit() || !loaded || !sameContext(loaded, currentContext())) return;
    if (onEditCell && (await onEditCell(loaded))) return;
    ui["cell-reference"].value = loaded.reference;
    await editing.openCellEditor();
  });

  return {
    update,
    refresh,
    togglePanel,
    closePanel,
    inspect,
    selectCell,
    setDraft,
  };

  function setDraft(draft) {
    if (!draft && !cellDraft) return;
    if (draft && !sameContext(draft, currentContext())) return;
    const previous = cellDraft;
    cellDraft = draft;
    request += 1;
    pending = false;
    loaded = null;
    if (draft) {
      ui["inspector-reference"].value = draft.reference;
      ui["inspector-value"].value = draft.text;
      ui["inspector-kind"].textContent = "Editing";
      ui["inspector-status"].textContent =
        `${draft.reference}: uncommitted cell edit`;
    } else if (previous) {
      void inspect(false);
    }
    update();
  }

  async function selectCell({ reference }) {
    ui["cell-reference"].value = reference;
    if (
      ui["inspector-reference"].value === reference &&
      loaded?.reference === reference &&
      sameContext(loaded, currentContext())
    )
      return;
    ui["inspector-reference"].value = reference;
    await inspect(false);
  }

  function currentContext() {
    return {
      client: state.client,
      documentId: state.documentId,
      sheetIndex: state.sheetIndex,
    };
  }

  function sameContext(left, right) {
    return (
      left &&
      right &&
      left.client === right.client &&
      left.documentId === right.documentId &&
      left.sheetIndex === right.sheetIndex
    );
  }

  function canEdit() {
    return Boolean(
      state.workbook &&
        state.editState?.capability === "read-write" &&
        !readOnly &&
        !state.busy &&
        !pending,
    );
  }

  function selectTab(name) {
    for (const tab of ["home", "view"]) {
      const selected = tab === name;
      ui[`tab-${tab}`].setAttribute("aria-selected", String(selected));
      ui[`tab-${tab}`].tabIndex = selected ? 0 : -1;
      ui[`ribbon-${tab}`].hidden = !selected;
    }
  }

  function setPanel(open) {
    dom.body.classList.toggle("sidebar-open", open);
    ui.sidebar.inert = !open;
    for (const id of ["sidebar-toggle", "ribbon-file", "view-workbook-panel"]) {
      ui[id].setAttribute("aria-expanded", String(open));
    }
    if (!open && ui.sidebar.contains(dom.activeElement))
      ui["sidebar-toggle"].focus();
  }

  function togglePanel() {
    setPanel(!dom.body.classList.contains("sidebar-open"));
  }

  function closePanel() {
    setPanel(false);
  }

  function invalidate() {
    request += 1;
    pending = false;
    loaded = null;
    if (cellDraft) {
      setDraft(cellDraft);
      return;
    }
    ui["inspector-value"].value = "";
    ui["inspector-kind"].textContent = "";
    ui["inspector-status"].textContent = "Press Enter to read the cell.";
    update();
  }

  function update() {
    const available = Boolean(state.workbook && !state.busy && !cellDraft);
    ui["inspect-cell"].disabled = !available;
    ui["inspector-reference"].disabled = !available;
    ui["inspector-edit"].disabled =
      !canEdit() || !loaded || !sameContext(loaded, currentContext());
    ui["quick-save"].disabled = !canEdit();
    ui["reset-zoom"].disabled = !available;
    const editable = state.editState?.capability === "read-write" && !readOnly;
    ui["workbook-mode"].textContent = !state.workbook
      ? "No workbook"
      : !editable
        ? "Read-only"
        : state.editState.dirty
          ? "Unsaved changes · save a copy"
          : "Editing · local file";
    ui["inspector-edit"].title = editable
      ? "Edit inspected cell"
      : "This workbook is read-only";
  }

  async function refresh() {
    const next = currentContext();
    const changed = !sameContext(context, next);
    context = next;
    invalidate();
    if (changed) ui["inspector-reference"].value = "A1";
    if (state.workbook && !state.busy) await inspect();
  }

  async function inspect(notifySelection = true) {
    if (!state.workbook || state.busy || !state.client || cellDraft) return;
    const token = ++request;
    const target = currentContext();
    loaded = null;
    pending = true;
    ui["inspector-value"].value = "";
    ui["inspector-kind"].textContent = "";
    update();
    try {
      const coordinate = parseCellReference(ui["inspector-reference"].value);
      ui["inspector-reference"].value = coordinate.normalized;
      ui["inspector-status"].textContent = `Reading ${coordinate.normalized}`;
      const result = await target.client.readCell(
        target.documentId,
        target.sheetIndex,
        coordinate.row,
        coordinate.col,
      );
      if (token !== request || !sameContext(target, currentContext())) return;
      loaded = { ...target, reference: coordinate.normalized };
      ui["cell-reference"].value = coordinate.normalized;
      ui["inspector-value"].value = inspectorText(result.value);
      ui["inspector-kind"].textContent =
        result.value.kind === "date" ? "Date serial" : result.value.kind;
      ui["inspector-status"].textContent =
        `${coordinate.normalized}: ${result.formatted || inspectorText(result.value) || "Blank cell"}`;
      if (notifySelection) onInspect(coordinate);
    } catch (error) {
      if (token !== request || !sameContext(target, currentContext())) return;
      ui["inspector-value"].value = describeError(error);
      ui["inspector-status"].textContent = describeError(error);
    } finally {
      if (token === request) {
        pending = false;
        update();
      }
    }
  }
}
