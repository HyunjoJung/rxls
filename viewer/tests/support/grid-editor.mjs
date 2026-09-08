import { createGridEditor } from "../../src/grid-editor.js";

const interaction = {
  schemaVersion: 1,
  width: 150,
  height: 40,
  cells: [
    [0, 0, 0, 0, 100, 20],
    [0, 2, 100, 0, 50, 20],
    [2, 0, 0, 20, 50, 20],
    [2, 2, 100, 20, 50, 20],
  ],
};
const flush = () => new Promise((resolve) => setImmediate(resolve));
function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
function element() {
  const listeners = new Map();
  const classes = new Set();
  const attrs = new Map();
  let value = "";
  let disabled = false;
  return {
    get value() {
      return value;
    },
    set value(next) {
      value = String(next).replace(/\r\n?/g, "\n");
    },
    hidden: true,
    get disabled() {
      return disabled;
    },
    set disabled(next) {
      disabled = Boolean(next);
      if (disabled && this.ownerDocument?.activeElement === this)
        this.ownerDocument.activeElement = this.ownerDocument.body;
    },
    style: {},
    dataset: {},
    parentElement: null,
    classList: {
      contains: (name) => classes.has(name),
      toggle(name, value) {
        if (value) classes.add(name);
        else classes.delete(name);
      },
    },
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    async fire(type, details = {}) {
      const event = {
        target: this,
        prevented: false,
        stopped: false,
        preventDefault() {
          this.prevented = true;
        },
        stopPropagation() {
          this.stopped = true;
        },
        ...details,
      };
      await listeners.get(type)?.(event);
      await flush();
      return event;
    },
    append(child) {
      child.parentElement = this;
    },
    setAttribute(name, value) {
      attrs.set(name, value);
    },
    getAttribute: (name) => attrs.get(name),
    focus() {
      if (this.disabled) return;
      this.focused = true;
      if (this.ownerDocument) this.ownerDocument.activeElement = this;
    },
    setSelectionRange(start, end) {
      this.selectionStart = start;
      this.selectionEnd = end;
    },
    getBoundingClientRect: () => ({ left: 100, top: 50 }),
  };
}
function setup({
  createEditor = createGridEditor,
  readOnly = false,
  readCell,
  commitCellEdit,
  commitRangeEdit,
  focusOutsideGrid,
} = {}) {
  const calls = {
    reads: [],
    commits: [],
    ranges: [],
    selections: [],
    errors: [],
    exits: [],
  };
  const exitTargets = { previous: element(), next: element() };
  const elements = Object.fromEntries(
    [
      "document-surface",
      "grid-layer",
      "grid-selection",
      "grid-input",
      "grid-status",
    ].map((id) => [id, element()]),
  );
  const svg = {
    getScreenCTM: () => ({
      a: 2,
      b: 0,
      c: 0,
      d: 2,
      e: 100,
      f: 50,
      inverse: () => ({ a: 0.5, b: 0, c: 0, d: 0.5, e: -50, f: -25 }),
    }),
  };
  const state = {
    mode: "sheet",
    busy: false,
    documentId: "one",
    sheetIndex: 0,
    workbook: { sheetCount: 1 },
    editState: { capability: "read-write" },
    client: {
      readCell: async (...args) => {
        calls.reads.push(args);
        return readCell
          ? readCell(...args)
          : { value: { kind: "number", value: 10 }, formatted: "10" };
      },
    },
  };
  const grid = createEditor({
    state,
    elements,
    readOnly,
    editing: {
      commitCellEdit: async (...args) => {
        calls.commits.push(args);
        return commitCellEdit ? commitCellEdit(...args) : true;
      },
      commitRangeEdit: async (...args) => {
        calls.ranges.push(args);
        return commitRangeEdit ? commitRangeEdit(...args) : true;
      },
    },
    onSelection: (value) => calls.selections.push(value),
    focusOutsideGrid: (direction) => {
      calls.exits.push(direction);
      if (focusOutsideGrid) return focusOutsideGrid(direction);
      exitTargets[direction].focus();
      return true;
    },
    showError: (error) => calls.errors.push(error),
  });
  grid.mount(interaction, svg);
  return {
    grid,
    state,
    calls,
    elements,
    svg,
    exitTargets,
    input: elements["grid-input"],
    outline: elements["grid-selection"],
    layer: elements["grid-layer"],
    surface: elements["document-surface"],
  };
}

export { setup, element, deferred, flush, interaction };
