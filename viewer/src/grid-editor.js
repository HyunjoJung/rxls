import { describeError } from "./core.js";

const MAX_CELLS = 250_000;
const DECIMAL = /^[+-]?(?:(?:0|[1-9]\d*)(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?$/;

/** Parse only unambiguous scalar input; formula caches are calculated by the worker. */
export function inlineCellValue(text, previous = { kind: "blank" }) {
  if (text === "") return { kind: "blank" };
  if (text.startsWith("'")) return { kind: "text", value: text.slice(1) };
  if (text.startsWith("=")) {
    const formula = text.slice(1).trim();
    if (!formula) throw new Error("Enter a formula after '='.");
    return { kind: "formula-auto", formula };
  }
  if (/^(?:TRUE|FALSE)$/i.test(text))
    return { kind: "boolean", value: /^true$/i.test(text) };
  if (DECIMAL.test(text)) {
    const value = Number(text);
    if (!Number.isFinite(value)) throw new RangeError("Enter a finite number.");
    return { kind: previous.kind === "date" ? "date" : "number", value };
  }
  return { kind: "text", value: text };
}

export function gridCellReference(row, col) {
  let letters = "";
  for (let n = col + 1; n > 0; n = Math.floor((n - 1) / 26)) {
    letters = String.fromCharCode(65 + ((n - 1) % 26)) + letters;
  }
  return `${letters}${row + 1}`;
}

/** Validate bounded worker geometry before making it interactive. */
export function gridCells(interaction) {
  if (
    interaction?.schemaVersion !== 1 ||
    !Number.isFinite(interaction.width) ||
    !Number.isFinite(interaction.height) ||
    interaction.width <= 0 ||
    interaction.height <= 0 ||
    !Array.isArray(interaction.cells) ||
    interaction.cells.length > MAX_CELLS
  ) {
    throw new Error(
      "The renderer returned invalid interactive sheet geometry.",
    );
  }
  const seen = new Set();
  return interaction.cells
    .map((cell) => {
      if (!Array.isArray(cell) || cell.length !== 6)
        throw new Error("Invalid interactive cell geometry.");
      const [row, col, x, y, width, height] = cell;
      const key = `${row}:${col}`;
      if (
        !Number.isInteger(row) ||
        row < 0 ||
        row > 1_048_575 ||
        !Number.isInteger(col) ||
        col < 0 ||
        col > 16_383 ||
        ![x, y, width, height].every(Number.isFinite) ||
        x < 0 ||
        y < 0 ||
        width <= 0 ||
        height <= 0 ||
        x + width > interaction.width ||
        y + height > interaction.height ||
        seen.has(key)
      ) {
        throw new Error("Invalid interactive cell geometry.");
      }
      seen.add(key);
      return {
        row,
        col,
        x,
        y,
        width,
        height,
        key,
        reference: gridCellReference(row, col),
      };
    })
    .sort((a, b) => a.row - b.row || a.col - b.col);
}

/** Convert a viewport pointer to the exact SVG coordinate space, including CSS zoom/scroll. */
export function gridPointer(svg, clientX, clientY) {
  try {
    const inverse = svg?.getScreenCTM()?.inverse();
    if (
      !inverse ||
      ![
        inverse.a,
        inverse.b,
        inverse.c,
        inverse.d,
        inverse.e,
        inverse.f,
        clientX,
        clientY,
      ].every(Number.isFinite)
    )
      return null;
    const x = inverse.a * clientX + inverse.c * clientY + inverse.e;
    const y = inverse.b * clientX + inverse.d * clientY + inverse.f;
    return Number.isFinite(x) && Number.isFinite(y) ? { x, y } : null;
  } catch {
    return null;
  }
}

/** A single native textarea handles selection, keyboard entry, and IME over rendered cell boxes. */
export function createGridEditor({
  state,
  editing,
  elements,
  onSelection = () => {},
  onDraft = () => {},
  focusOutsideGrid = () => false,
  showError,
  readOnly = false,
}) {
  const surface = elements["document-surface"];
  const layer = elements["grid-layer"];
  const outline = elements["grid-selection"];
  const input = elements["grid-input"];
  const status = elements["grid-status"];
  let cells = [];
  const cellIndices = new Map();
  let svg = null;
  let context = null;
  let selected = null;
  let loaded = null;
  let request = 0;
  let readPromise = null;
  let draft = null;
  let composing = false;
  let committing = false;

  surface.addEventListener("pointerdown", (event) => void pointer(event));
  surface.addEventListener("keydown", (event) => {
    if (event.target !== input) void keydown(event);
  });
  input.addEventListener("keydown", (event) => void keydown(event));
  input.addEventListener("input", () => {
    if (!draft && available()) startDraft(input.value);
    if (draft) draft.touched = true;
    notifyDraft();
  });
  input.addEventListener("compositionstart", () => {
    if (!available()) return;
    if (!draft) startDraft("");
    composing = true;
  });
  input.addEventListener("compositionend", () => {
    composing = false;
    if (draft) draft.touched = true;
    notifyDraft();
  });
  // Blur is deliberately not a save action: drafts survive toolbar focus and failures.
  input.addEventListener("blur", () => {
    if (draft)
      announce(
        "Uncommitted cell edit. Press Enter to apply or Escape to cancel.",
      );
  });

  return {
    mount,
    invalidate,
    update,
    reposition,
    hasDraft: () => Boolean(draft),
    hasChanges,
    cancel,
    commit,
    select,
    beginEdit,
    getPasteTarget,
    applyRange,
  };

  function snapshot() {
    return {
      client: state.client,
      documentId: state.documentId,
      sheetIndex: state.sheetIndex,
    };
  }

  function sameContext(left, right = snapshot()) {
    return Boolean(
      left &&
        right &&
        left.client === right.client &&
        left.documentId === right.documentId &&
        left.sheetIndex === right.sheetIndex,
    );
  }

  function available() {
    return Boolean(
      !readOnly &&
        !state.busy &&
        !committing &&
        state.client &&
        state.workbook &&
        state.mode === "sheet" &&
        state.editState?.capability === "read-write" &&
        svg &&
        sameContext(context),
    );
  }

  function announce(message) {
    status.textContent = message;
  }

  function hasChanges() {
    return Boolean(
      draft &&
        (composing ||
          (draft.initial === null
            ? draft.replace || draft.touched
            : input.value !== draft.initial)),
    );
  }

  function notifyDraft() {
    onDraft(
      draft
        ? {
            ...draft.target,
            reference: gridCellReference(draft.target.row, draft.target.col),
            text: input.value,
          }
        : null,
    );
  }

  function mount(interaction, nextSvg) {
    const nextContext = snapshot();
    try {
      const nextCells = gridCells(interaction);
      const same = sameContext(context, nextContext);
      if (!same && draft) {
        invalidate();
        announce("Finish or cancel the cell draft before changing workbooks.");
        return false;
      }
      cells = nextCells;
      cellIndices.clear();
      for (let index = 0; index < cells.length; index++)
        cellIndices.set(cells[index].key, index);
      context = nextContext;
      svg = nextSvg;
      if (layer.parentElement !== surface) surface.append(layer);
      if (!same) {
        selected = null;
        loaded = null;
        request += 1;
      }
      selected = cells[cellIndices.get(selected?.key)] ?? null;
      if (committing || draft) {
        update();
        reposition();
        return true;
      }
      if (!available()) {
        update();
        return true;
      }
      const next = selected ?? cells[0];
      selected = null;
      if (next) void select(next.row, next.col, { focus: false });
      else update();
      return true;
    } catch (error) {
      invalidate({ preserveSelection: true });
      showError(error);
      return false;
    }
  }

  function invalidate({ preserveSelection = false } = {}) {
    request += 1;
    loaded = null;
    readPromise = null;
    svg = null;
    cells = [];
    cellIndices.clear();
    if (!preserveSelection) selected = null;
    layer.hidden = true;
    outline.hidden = true;
    input.hidden = true;
    input.disabled = true;
    surface.classList.toggle("grid-active", false);
  }

  function update() {
    const visible = Boolean(
      svg &&
        selected &&
        sameContext(context) &&
        !readOnly &&
        state.mode === "sheet" &&
        state.editState?.capability === "read-write",
    );
    layer.hidden = !visible;
    surface.classList.toggle("grid-active", visible);
    outline.hidden = !visible;
    input.disabled =
      !available() || Boolean(draft && !draft.original && !draft.replace);
    input.hidden = !visible;
    layer.classList.toggle("is-editing", Boolean(draft));
    layer.classList.toggle("is-pending", committing);
    outline.dataset.reference = selected?.reference ?? "";
    if (selected)
      input.setAttribute(
        "aria-label",
        `${selected.reference}: ${draft ? "Edit cell" : "Selected cell. Click to edit or type to replace"}`,
      );
    reposition();
  }

  function reposition() {
    if (!svg || !selected) return;
    try {
      const matrix = svg.getScreenCTM();
      if (
        !matrix ||
        ![matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f].every(
          Number.isFinite,
        )
      )
        return;
      const fontScale = Math.hypot(matrix.c, matrix.d);
      if (Number.isFinite(fontScale) && fontScale > 0)
        input.style.fontSize = `${14 * fontScale}px`;
      const parent = surface.getBoundingClientRect();
      const points = [
        [selected.x, selected.y],
        [selected.x + selected.width, selected.y],
        [selected.x, selected.y + selected.height],
        [selected.x + selected.width, selected.y + selected.height],
      ].map(([x, y]) => ({
        x: matrix.a * x + matrix.c * y + matrix.e,
        y: matrix.b * x + matrix.d * y + matrix.f,
      }));
      const left = Math.min(...points.map((point) => point.x));
      const top = Math.min(...points.map((point) => point.y));
      outline.style.left = `${left - parent.left + (surface.scrollLeft || 0) - (surface.clientLeft || 0)}px`;
      outline.style.top = `${top - parent.top + (surface.scrollTop || 0) - (surface.clientTop || 0)}px`;
      outline.style.width = `${Math.max(...points.map((point) => point.x)) - left}px`;
      outline.style.height = `${Math.max(...points.map((point) => point.y)) - top}px`;
    } catch {
      /* A detached SVG has no usable screen transform. */
    }
  }

  async function select(row, col, { focus = true } = {}) {
    if (!available()) return false;
    if (!Number.isInteger(row) || !Number.isInteger(col)) return false;
    const next = cells[cellIndices.get(`${row}:${col}`)];
    if (!next) return false;
    if (selected?.key === next.key) {
      if (focus) {
        outline.scrollIntoView?.({ block: "nearest", inline: "nearest" });
        input.focus({ preventScroll: true });
      }
      return true;
    }
    if (draft && !hasChanges()) cancel();
    if (draft) {
      announce("Press Enter to apply the current edit or Escape to cancel it.");
      input.focus({ preventScroll: true });
      return false;
    }
    selected = next;
    loaded = null;
    input.value = "";
    update();
    if (focus) {
      outline.scrollIntoView?.({ block: "nearest", inline: "nearest" });
      input.focus({ preventScroll: true });
    }
    onSelection({ row, col, reference: next.reference });
    await readSelected();
    return true;
  }

  async function readSelected() {
    if (!selected || !available()) return null;
    const token = ++request;
    const target = {
      ...snapshot(),
      row: selected.row,
      col: selected.col,
      key: selected.key,
      openGeneration: state.openGeneration,
    };
    const promise = (async () => {
      try {
        const result = await target.client.readCell(
          target.documentId,
          target.sheetIndex,
          target.row,
          target.col,
        );
        if (
          token !== request ||
          !sameContext(target) ||
          target.openGeneration !== state.openGeneration ||
          target.key !== selected?.key
        )
          return null;
        loaded = { ...target, value: result.value };
        if (
          draft &&
          draft.target.key === target.key &&
          sameContext(draft.target)
        ) {
          draft.original = result.value;
          draft.initial = editText(result.value);
          if (!draft.replace && !draft.touched) input.value = draft.initial;
          update();
          notifyDraft();
        }
        announce(
          `${selected.reference}: ${result.formatted || editText(result.value) || "Blank cell"}`,
        );
        return loaded;
      } catch (error) {
        if (
          token === request &&
          sameContext(target) &&
          target.openGeneration === state.openGeneration
        ) {
          announce(describeError(error));
          showError(error);
        }
        return null;
      }
    })();
    readPromise = promise;
    return promise;
  }

  function editText(cell) {
    const text =
      cell.kind === "blank"
        ? ""
        : cell.kind === "formula"
          ? `=${cell.formula}`
          : cell.kind === "boolean"
            ? cell.value
              ? "TRUE"
              : "FALSE"
            : String(cell.value ?? "");
    // Match native textarea line endings for comparisons, without rewriting source cells.
    return text.replace(/\r\n?/g, "\n");
  }

  function startDraft(replacement) {
    if (!available() || !selected || draft) return;
    const current =
      loaded && loaded.key === selected.key && sameContext(loaded)
        ? loaded.value
        : null;
    draft = {
      target: {
        ...snapshot(),
        row: selected.row,
        col: selected.col,
        key: selected.key,
      },
      original: current,
      initial: current ? editText(current) : null,
      replace: replacement !== undefined,
      touched: false,
    };
    input.value = replacement ?? draft.initial ?? "";
    update();
    notifyDraft();
  }

  async function beginEdit(replacement) {
    if (!available() || !selected) return false;
    startDraft(replacement);
    if (!draft) return false;
    const current = draft;
    const generation = state.openGeneration;
    input.focus({ preventScroll: true });
    if (!current.original) await (readPromise ?? readSelected());
    if (
      draft !== current ||
      !sameContext(current.target) ||
      generation !== state.openGeneration
    )
      return false;
    if (!current.original) {
      announce(
        "The cell could not be read. Your draft is retained; retry or press Escape.",
      );
      return false;
    }
    if (replacement === undefined && !current.touched)
      input.setSelectionRange(input.value.length, input.value.length);
    input.focus({ preventScroll: true });
    return true;
  }

  function cancel() {
    if (committing) return false;
    draft = null;
    composing = false;
    input.value = "";
    update();
    notifyDraft();
    if (selected) announce(`${selected.reference} selected. Edit cancelled.`);
    return true;
  }

  function getPasteTarget() {
    if (!available() || !selected || composing) return null;
    return {
      ...snapshot(),
      row: selected.row,
      col: selected.col,
      key: selected.key,
      openGeneration: state.openGeneration,
    };
  }

  async function applyRange(target, values) {
    const isCurrent = () =>
      sameContext(target) &&
      target.openGeneration === state.openGeneration &&
      selected?.key === target.key;
    if (!available() || composing || !isCurrent()) return false;
    try {
      committing = true;
      update();
      const applied = await editing.commitRangeEdit(target, values);
      if (
        !applied ||
        !sameContext(target) ||
        target.openGeneration !== state.openGeneration
      )
        return false;
      draft = null;
      input.value = "";
      loaded = null;
      notifyDraft();
      announce("Pasted range updated. Undo restores the entire range.");
      return true;
    } catch (error) {
      if (
        sameContext(target) &&
        target.openGeneration === state.openGeneration
      ) {
        announce(
          `${describeError(error)} The range was not applied; your draft is retained.`,
        );
        showError(error);
      }
      return false;
    } finally {
      committing = false;
      update();
      if (
        !draft &&
        available() &&
        sameContext(target) &&
        target.openGeneration === state.openGeneration
      )
        void readSelected();
    }
  }

  async function commit() {
    if (committing || composing) return false;
    if (!draft) return true;
    if (!available() || !sameContext(draft.target)) {
      announce(
        "The workbook changed or is busy. Your cell draft has not been applied.",
      );
      return false;
    }
    const current = draft;
    const generation = state.openGeneration;
    if (!current.original) {
      await readSelected();
      if (
        draft !== current ||
        !current.original ||
        generation !== state.openGeneration ||
        !sameContext(current.target)
      )
        return false;
    }
    if (input.value === current.initial) {
      cancel();
      return true;
    }
    const ownerDocument = input.ownerDocument;
    const hadInputFocus = ownerDocument?.activeElement === input;
    try {
      const value = inlineCellValue(input.value, current.original);
      committing = true;
      update();
      const applied = await editing.commitCellEdit(current.target, value);
      if (
        !applied ||
        !sameContext(current.target) ||
        generation !== state.openGeneration
      ) {
        if (generation !== state.openGeneration) return false;
        announce(
          "The workbook changed before the edit completed. The draft was retained.",
        );
        return false;
      }
      draft = null;
      input.value = "";
      loaded = null;
      notifyDraft();
      announce(`${selected?.reference ?? "Cell"} updated.`);
      return true;
    } catch (error) {
      if (sameContext(current.target) && generation === state.openGeneration) {
        announce(`${describeError(error)} Your draft is retained.`);
        showError(error);
      }
      return false;
    } finally {
      committing = false;
      update();
      // Disabling a focused native textarea blurs it. Keep failed edits keyboard-retryable,
      // without stealing focus from a toolbar command or a newer document/control.
      if (
        draft === current &&
        sameContext(current.target) &&
        generation === state.openGeneration &&
        hadInputFocus &&
        available() &&
        !input.disabled &&
        !input.hidden &&
        (ownerDocument.activeElement === ownerDocument.body ||
          ownerDocument.activeElement === input)
      )
        input.focus({ preventScroll: true });
      if (!draft && available() && generation === state.openGeneration)
        void readSelected();
    }
  }

  async function pointer(event) {
    if (event.button !== undefined && event.button !== 0) return;
    if (!available()) return;
    if (event.target === input) {
      // Once editing, native clicks position the caret and double-clicks select words.
      if (!draft) {
        event.preventDefault();
        await beginEdit();
      }
      return;
    }
    const point = gridPointer(svg, event.clientX, event.clientY);
    if (!point) return;
    const cell = cells.find(
      (candidate) =>
        point.x >= candidate.x &&
        point.x < candidate.x + candidate.width &&
        point.y >= candidate.y &&
        point.y < candidate.y + candidate.height,
    );
    if (!cell) return;
    event.preventDefault();
    // select changes the target synchronously; create its draft before awaiting a read.
    // An older click must never resume later and open an editor on a newer target.
    const selecting = select(cell.row, cell.col);
    if (selected?.key === cell.key && !draft) await beginEdit();
    await selecting;
  }

  function adjacent(direction) {
    if (!selected) return null;
    if (direction === "next" || direction === "previous") {
      const index = cellIndices.get(selected.key);
      return cells[index + (direction === "next" ? 1 : -1)] ?? null;
    }
    const x = selected.x + Math.min(1, selected.width / 2);
    const y = selected.y + Math.min(1, selected.height / 2);
    const eligible = (cell) => {
      if (direction === "right")
        return (
          cell.x >= selected.x + selected.width - 0.001 &&
          y >= cell.y &&
          y < cell.y + cell.height
        );
      if (direction === "left")
        return (
          cell.x + cell.width <= selected.x + 0.001 &&
          y >= cell.y &&
          y < cell.y + cell.height
        );
      if (direction === "down")
        return (
          cell.y >= selected.y + selected.height - 0.001 &&
          x >= cell.x &&
          x < cell.x + cell.width
        );
      return (
        cell.y + cell.height <= selected.y + 0.001 &&
        x >= cell.x &&
        x < cell.x + cell.width
      );
    };
    const distance = (cell) =>
      direction === "right"
        ? cell.x - selected.x
        : direction === "left"
          ? selected.x - cell.x - cell.width
          : direction === "down"
            ? cell.y - selected.y
            : selected.y - cell.y - cell.height;
    let nearest = selected;
    let nearestDistance = Infinity;
    for (const cell of cells) {
      if (!eligible(cell)) continue;
      const nextDistance = distance(cell);
      if (nextDistance < nearestDistance) {
        nearest = cell;
        nearestDistance = nextDistance;
      }
    }
    return nearest;
  }

  async function keydown(event) {
    if (!available() || !selected) return;
    if (composing || event.isComposing || event.keyCode === 229) return;
    if (draft && !draft.original && !draft.replace && event.key !== "Escape")
      return;
    const modified = event.ctrlKey || event.metaKey || event.altKey;
    if (modified) return;
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      cancel();
      return;
    }
    if (event.key === "Enter" || event.key === "Tab") {
      event.preventDefault();
      event.stopPropagation();
      if (!draft && event.key === "Enter") {
        await beginEdit();
        return;
      }
      const direction =
        event.key === "Tab"
          ? event.shiftKey
            ? "previous"
            : "next"
          : event.shiftKey
            ? "up"
            : "down";
      const target = { ...snapshot(), key: selected.key };
      const exit = event.key === "Tab" && !adjacent(direction);
      const ownerDocument = input.ownerDocument;
      const activeElement = ownerDocument?.activeElement;
      if (
        (await commit()) &&
        available() &&
        sameContext(target) &&
        (selected?.key === target.key || (exit && !selected))
      ) {
        if (exit) {
          // The native Tab action cannot resume after an asynchronous commit.
          // Move to an actual outside control, unless the user moved focus meanwhile.
          const currentFocus = ownerDocument?.activeElement;
          if (
            !ownerDocument ||
            currentFocus === activeElement ||
            currentFocus === input ||
            currentFocus === surface ||
            currentFocus === ownerDocument.body
          )
            focusOutsideGrid(direction);
          return;
        }
        const next = adjacent(direction);
        if (next) await select(next.row, next.col);
      }
      return;
    }
    if (!draft && event.key === "F2") {
      event.preventDefault();
      event.stopPropagation();
      await beginEdit();
      return;
    }
    if (!draft && event.key.startsWith("Arrow")) {
      event.preventDefault();
      event.stopPropagation();
      const next = adjacent(event.key.slice(5).toLowerCase());
      if (next) await select(next.row, next.col);
      return;
    }
    if (
      !draft &&
      (event.key === "Backspace" ||
        event.key === "Delete" ||
        event.key.length === 1)
    ) {
      event.preventDefault();
      event.stopPropagation();
      await beginEdit(event.key.length === 1 ? event.key : "");
    }
  }
}
