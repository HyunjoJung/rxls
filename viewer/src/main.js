import {
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  CircleAlert,
  Download,
  ExternalLink,
  FileCode2,
  FileSpreadsheet,
  FileText,
  Files,
  FileUp,
  FolderOpen,
  ImageDown,
  PanelLeft,
  Pencil,
  Redo2,
  RefreshCw,
  Scan,
  Save,
  ShieldCheck,
  Table2,
  Undo2,
  X,
  ZoomIn,
  ZoomOut,
  createIcons,
} from "lucide";
import {
  acceptsWorkbook,
  clampZoom,
  createLatestRequestGate,
  describeError,
  extensionOf,
  fitZoom,
  formatBytes,
  formatLabel,
  parseCellReference,
  svgDimensions,
} from "./core.js";
import { createEditingController } from "./editing.js";
import { createExportController } from "./exports.js";
import { createWorkbench } from "./workbench.js";
import { createGridEditor } from "./grid-editor.js";
import { createRangePasteController } from "./range-paste.js";
import {
  VIEWPORT_POLICY, createViewportScheduler, viewportCover,
  viewportFallbackError, viewportSurfaceSize, viewportContains,
} from "./viewport.js";

const MAX_INPUT_BYTES = 32 * 1024 * 1024;
const ZOOM_STEP = 0.15;
const BLOCKED_SVG_ELEMENTS = "script, foreignObject, iframe, object, embed";
const hostKind =
  document.querySelector('meta[name="rxls-host-kind"]')?.content ?? "browser";
const hostResourceBase = document.querySelector(
  'meta[name="rxls-resource-base"]',
)?.content;
const vscodeHost =
  hostKind === "vscode" && typeof globalThis.acquireVsCodeApi === "function"
    ? globalThis.acquireVsCodeApi()
    : null;

document.body.classList.toggle("host-vscode", Boolean(vscodeHost));

const icons = {
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  CircleAlert,
  Download,
  ExternalLink,
  FileCode2,
  FileSpreadsheet,
  FileText,
  Files,
  FileUp,
  FolderOpen,
  ImageDown,
  PanelLeft,
  Pencil,
  Redo2,
  RefreshCw,
  Scan,
  Save,
  ShieldCheck,
  Table2,
  Undo2,
  X,
  ZoomIn,
  ZoomOut,
};

createIcons({ icons });

const elements = Object.fromEntries(
  [
    "document-name",
    "document-detail",
    "reload-document",
    "open-button",
    "empty-open-button",
    "file-input",
    "sample-select",
    "sheet-list",
    "sheet-count",
    "meta-format",
    "meta-size",
    "meta-images",
    "meta-editing",
    "editing-reason",
    "sheet-view",
    "page-view",
    "page-controls",
    "previous-page",
    "next-page",
    "page-position",
    "edit-cell",
    "undo-edit",
    "redo-edit",
    "document-properties",
    "zoom-out",
    "zoom-in",
    "zoom-value",
    "fit-view",
    "export-menu",
    "save-document",
    "export-svg",
    "export-png",
    "viewer-viewport",
    "drop-overlay",
    "empty-state",
    "loading-state",
    "loading-label",
    "document-stage",
    "document-surface",
    "grid-layer",
    "grid-selection",
    "grid-input",
    "grid-status",
    "status-message",
    "render-detail",
    "error-banner",
    "error-message",
    "dismiss-error",
    "sidebar",
    "sidebar-toggle",
    "sidebar-scrim",
    "cell-dialog",
    "cell-form",
    "cell-sheet-name",
    "close-cell-dialog",
    "cancel-cell-edit",
    "cell-reference",
    "read-cell",
    "cell-kind",
    "cell-value-field",
    "cell-value",
    "cell-formula-fields",
    "cell-formula",
    "cell-cached-kind",
    "cell-cached-value",
    "cell-current-value",
    "apply-cell-edit",
    "properties-dialog",
    "properties-form",
    "close-properties-dialog",
    "cancel-properties-edit",
    "property-title",
    "property-subject",
    "property-creator",
    "property-keywords",
    "property-description",
    "property-last-modified-by",
    "property-company",
    "property-created",
    "paste-dialog",
    "paste-status",
    "paste-summary",
    "paste-preview",
    "apply-range-paste",
    "paste-as-text",
    "cancel-range-paste",
  ].map((id) => [id, document.getElementById(id)]),
);

const state = {
  runtime: null,
  client: null,
  documentId: null,
  workbook: null,
  editState: null,
  file: null,
  sheetIndex: 0,
  mode: "sheet",
  manifests: new Map(),
  pageIndex: 0,
  zoom: 1,
  svgText: "",
  svgElement: null,
  documentWidth: 1024,
  documentHeight: 768,
  busy: false,
  renderEpoch: 0,
  openRequest: null,
  openGeneration: 0,
  dragDepth: 0,
  hostGeneration: 0,
  hostWorker: null,
  displayKind: "none",
  renderOutcome: "empty",
  viewportContext: null,
  viewportSvg: null,
  viewportReady: false,
  viewportBarrier: null,
  viewportFrame: null,
  viewportInitialFit: false,
  viewportDisposed: false,
  viewportDiagnostics: null,
  viewportDelivery: null,
};

const baseUrl = hostResourceBase
  ? new URL(hostResourceBase)
  : new URL(import.meta.env.BASE_URL, window.location.origin);
const openRequests = createLatestRequestGate();
let samples = [];
let grid = null;
let rangePaste = null;
let viewportObserver = null;
const viewportScheduler = createViewportScheduler({
  render: async ({ context, rect, namespace }) => {
    // Keep this original request promise. A UI deadline never frees WASM CPU.
    const tile = await context.client.renderViewportTile(
      context.documentId, context.sheetIndex, context.geometryId,
      context.revision, rect, namespace,
    );
    if (tile.documentId !== context.documentId || tile.sheetIndex !== context.sheetIndex ||
        tile.geometryId !== context.geometryId || tile.revision !== context.revision ||
        tile.namespace !== namespace) throw new Error("Viewport tile identity changed.");
    if (currentViewport(context)) state.viewportDelivery = { namespace, consumed: false };
    return tile;
  },
  adopt: adoptViewportTile,
  onState: updateViewportState,
  onError: failViewport,
});

const editing = createEditingController({
  state,
  elements,
  readOnly: Boolean(vscodeHost),
  setBusy,
  showError,
  updateWorkbookUi,
  renderCurrent,
  beforeCommand: commitGridDraft,
});
const {
  updateEditUi,
  openCellEditor,
  closeCellEditor,
  closePropertiesEditor,
  applyHistoryEdit,
  saveWorkbookCopy,
  confirmDiscardChanges,
} = editing;
const { exportSvg, exportPng } = createExportController({
  state,
  elements,
  host: Boolean(vscodeHost),
  setBusy,
  showError,
  postHostMessage,
});

const workbench = createWorkbench({
  state,
  editing,
  readOnly: Boolean(vscodeHost),
  openFile: chooseFile,
  setZoom,
  onInspect: ({ row, col }) => {
    void grid?.select(row, col, { focus: false });
  },
  onEditCell: async ({ reference }) => {
    const { row, col } = parseCellReference(reference);
    if (state.mode !== "sheet" || !editing.canMutateDisplayedSheet() ||
        !(await grid.select(row, col)) || !editing.canMutateDisplayedSheet()) return false;
    await grid.beginEdit();
    return true;
  },
});

grid = createGridEditor({
  state,
  editing,
  elements,
  readOnly: Boolean(vscodeHost),
  onSelection: (selection) => {
    void workbench.selectCell(selection);
  },
  onDraft: (draft) => workbench.setDraft(draft),
  focusOutsideGrid,
  showError,
});

rangePaste = createRangePasteController({ grid, elements, showError });

async function commitGridDraft() {
  if (rangePaste && !rangePaste.ready()) return false;
  return grid ? grid.commit() : true;
}

function focusOutsideGrid(direction) {
  const surface = elements["document-surface"];
  // Resolve the current tab order after a commit: rendering may replace controls.
  const stops = [
    ...document.querySelectorAll(
      "a[href], button, input, select, textarea, summary, [tabindex]",
    ),
  ]
    .filter(
      (element) =>
        (element === surface || !surface.contains(element)) &&
        element.tabIndex >= 0 &&
        !element.matches(":disabled") &&
        !element.closest("[hidden], [inert]") &&
        element.getClientRects().length > 0 &&
        getComputedStyle(element).visibility === "visible",
    )
    .sort((a, b) => {
      if (a.tabIndex === b.tabIndex) return 0;
      if (a.tabIndex === 0) return 1;
      if (b.tabIndex === 0) return -1;
      return a.tabIndex - b.tabIndex;
    });
  const index = stops.indexOf(surface);
  if (index < 0 || !["next", "previous"].includes(direction)) return false;
  const target = stops[index + (direction === "next" ? 1 : -1)];
  if (!target) return false;
  target.focus();
  return document.activeElement === target;
}

bindEvents();
setBusy(true, "Starting renderer");
void initialize();

async function initialize() {
  try {
    const runtime = await import(
      /* @vite-ignore */ new URL("runtime/js/client.mjs", baseUrl).href
    );
    state.runtime = runtime;
    if (vscodeHost) {
      setBusy(false);
      showEmpty();
      elements["document-name"].textContent = "Spreadsheet preview";
      elements["document-detail"].textContent = "Waiting for VS Code";
      elements["status-message"].textContent = "Waiting for workbook";
      postHostMessage({ type: "ready" });
      return;
    }
    const sampleManifest = await fetchJson(
      new URL("samples/manifest.json", baseUrl),
    );
    samples = sampleManifest.samples ?? [];
    populateSamples();
    if (samples.length > 0) {
      await loadSample(samples[0]);
    } else {
      setBusy(false);
      showEmpty();
    }
  } catch (error) {
    setBusy(false);
    showEmpty();
    showError(error);
  }
}

function bindEvents() {
  elements["reload-document"].addEventListener("click", requestHostReload);
  elements["open-button"].addEventListener("click", chooseFile);
  elements["empty-open-button"].addEventListener("click", chooseFile);
  elements["file-input"].addEventListener("change", (event) => {
    const [file] = event.target.files ?? [];
    if (file) {
      void loadLocalFile(file);
    }
    event.target.value = "";
  });
  elements["sample-select"].addEventListener("change", () => {
    const sample = samples.find(
      (entry) => entry.id === elements["sample-select"].value,
    );
    if (sample) {
      void loadSample(sample);
    }
  });
  elements["sheet-view"].addEventListener("click", () => void setMode("sheet"));
  elements["page-view"].addEventListener("click", () => void setMode("page"));
  elements["previous-page"].addEventListener("click", () => void movePage(-1));
  elements["next-page"].addEventListener("click", () => void movePage(1));
  elements["zoom-out"].addEventListener("click", () =>
    setZoom(state.zoom - ZOOM_STEP),
  );
  elements["zoom-in"].addEventListener("click", () =>
    setZoom(state.zoom + ZOOM_STEP),
  );
  elements["fit-view"].addEventListener("click", fitToWidth);
  elements["export-svg"].addEventListener(
    "click",
    () => void exportWithDraft("svg"),
  );
  elements["export-png"].addEventListener(
    "click",
    () => void exportWithDraft("png"),
  );
  elements["dismiss-error"].addEventListener("click", dismissError);
  elements["sidebar-toggle"].addEventListener("click", toggleSidebar);
  elements["sidebar-scrim"].addEventListener("click", closeSidebar);
  editing.bindEvents();

  if (vscodeHost) {
    window.addEventListener("message", onHostMessage);
  } else {
    const viewport = elements["viewer-viewport"];
    viewport.addEventListener("dragenter", onDragEnter);
    viewport.addEventListener("dragover", onDragOver);
    viewport.addEventListener("dragleave", onDragLeave);
    viewport.addEventListener("drop", onDrop);
  }
  window.addEventListener("keydown", onKeyDown);
  window.addEventListener("beforeunload", (event) => {
    if (
      state.editState?.dirty ||
      grid.hasChanges() ||
      editing.hasDraftChanges() ||
      rangePaste.hasPending()
    ) {
      event.preventDefault();
      event.returnValue = "";
    }
  });
  elements["viewer-viewport"].addEventListener("scroll", scheduleViewport, { passive: true });
  if (typeof ResizeObserver === "function") {
    viewportObserver = new ResizeObserver(scheduleViewport);
    viewportObserver.observe(elements["viewer-viewport"]);
  }
  window.addEventListener("resize", () => {
    scheduleViewport();
    if (state.svgElement && state.zoom <= 1) {
      fitToWidth();
    }
  });
}

function chooseFile() {
  elements["file-input"].click();
}

async function loadLocalFile(file) {
  if (!canReplaceWorkbook()) return;
  if (!(await commitGridDraft())) return;
  if (!confirmDiscardChanges()) {
    return;
  }
  if (!acceptsWorkbook(file.name)) {
    showError(new Error("Choose an XLS, XLSX, XLSM, XLSB, or ODS file."));
    return;
  }
  if (file.size > MAX_INPUT_BYTES) {
    const error = new Error(
      `The browser viewer accepts files up to ${formatBytes(MAX_INPUT_BYTES)}.`,
    );
    error.code = "limit_exceeded";
    showError(error);
    return;
  }
  const request = beginOpenRequest(`Opening ${file.name}`);
  try {
    const bytes = new Uint8Array(await file.arrayBuffer());
    if (!isCurrentOpenRequest(request)) {
      return;
    }
    await openWorkbook(
      bytes,
      {
        name: file.name,
        size: file.size,
        source: "Local file",
        sampleId: null,
      },
      request,
    );
  } catch (error) {
    failOpenRequest(request, error);
  }
}

async function loadSample(sample) {
  if (!canReplaceWorkbook()) {
    elements["sample-select"].value = state.file?.sampleId ?? "";
    return;
  }
  if (!(await commitGridDraft())) {
    elements["sample-select"].value = state.file?.sampleId ?? "";
    return;
  }
  if (!confirmDiscardChanges()) {
    elements["sample-select"].value = state.file?.sampleId ?? "";
    return;
  }
  const request = beginOpenRequest(`Opening ${sample.name}`);
  try {
    const response = await fetch(new URL(`samples/${sample.file}`, baseUrl), {
      signal: request.abortController.signal,
    });
    if (!response.ok) {
      throw new Error(`Sample request failed with HTTP ${response.status}.`);
    }
    const bytes = new Uint8Array(await response.arrayBuffer());
    if (!isCurrentOpenRequest(request)) {
      return;
    }
    await openWorkbook(
      bytes,
      {
        name: sample.name,
        size: bytes.byteLength,
        source: "Project sample",
        sampleId: sample.id,
      },
      request,
    );
  } catch (error) {
    failOpenRequest(request, error);
  }
}

function canReplaceWorkbook() {
  if (!editing.hasPendingMutation()) return true;
  showError(
    new Error(
      "Wait for the current edit to finish before opening another workbook.",
    ),
  );
  return false;
}

function beginOpenRequest(label) {
  rangePaste.cancel();
  state.openGeneration += 1;
  state.openRequest?.abortController.abort();
  state.openRequest?.client?.terminate();
  const request = {
    token: openRequests.begin(),
    abortController: new AbortController(),
    client: null,
  };
  state.openRequest = request;
  state.renderEpoch += 1;
  resetViewport();
  state.renderOutcome = "pending";
  closeCellEditor();
  closePropertiesEditor();
  dismissError();
  setBusy(true, label);
  return request;
}

function isCurrentOpenRequest(request) {
  return state.openRequest === request && openRequests.isCurrent(request.token);
}

function failOpenRequest(request, error) {
  request.client?.terminate();
  request.client = null;
  if (!isCurrentOpenRequest(request)) {
    return;
  }
  state.openRequest = null;
  elements["sample-select"].value = state.file?.sampleId ?? "";
  setBusy(false);
  if (!state.workbook) {
    showEmpty();
  }
  showError(error);
}

async function openWorkbook(bytes, file, request) {
  if (!isCurrentOpenRequest(request)) {
    return false;
  }
  let client = null;
  try {
    const workerUrl = new URL("runtime/js/worker.mjs", baseUrl);
    const workerTarget = await createWorkerTarget(workerUrl);
    if (!isCurrentOpenRequest(request)) {
      if (workerTarget instanceof Worker) workerTarget.terminate();
      return false;
    }
    client = new state.runtime.RenderWorkerClient(workerTarget);
    request.client = client;
    const documentId = `viewer-${Date.now()}-${Math.random().toString(16).slice(2)}`;
    const opened = await client.open(bytes, { documentId });
    if (!isCurrentOpenRequest(request)) {
      client.terminate();
      return false;
    }

    grid.invalidate();
    const previousClient = state.client;
    state.client = client;
    state.hostWorker = workerTarget instanceof Worker ? workerTarget : null;
    state.documentId = documentId;
    state.workbook = opened.workbook;
    state.editState = opened.editState;
    state.file = file;
    state.manifests.clear();
    state.pageIndex = 0;
    state.sheetIndex = 0;
    request.client = null;
    previousClient?.terminate();
    updateWorkbookUi();
    const outcome = await renderCurrent({ fit: true });
    if (isCurrentOpenRequest(request)) {
      state.openRequest = null;
      closeSidebar();
    }
    return !openRequests.isCurrent(request.token) ? false :
      (outcome.status === "ready" || outcome.status === "empty") &&
      state.client === client && state.documentId === documentId;
  } catch (error) {
    client?.terminate();
    if (state.client === client) {
      state.client = null;
      state.hostWorker = null;
      state.documentId = null;
      state.workbook = null;
      state.editState = null;
      state.file = null;
    }
    failOpenRequest(request, error);
    return false;
  }
}

async function createWorkerTarget(workerUrl) {
  if (!vscodeHost) {
    return workerUrl;
  }
  const workerBundleUrl = new URL("runtime/vscode-worker.js", baseUrl);
  const wasmUrl = new URL("runtime/pkg/rxls_render_wasm_bg.wasm", baseUrl);
  const [workerResponse, wasmResponse] = await Promise.all([
    fetch(workerBundleUrl),
    fetch(wasmUrl),
  ]);
  if (!workerResponse.ok || !wasmResponse.ok) {
    throw new Error("The packaged VS Code renderer could not be loaded.");
  }
  const [workerBlob, wasmBytes] = await Promise.all([
    workerResponse.blob(),
    wasmResponse.arrayBuffer(),
  ]);
  const bootstrapUrl = URL.createObjectURL(workerBlob);
  const worker = new Worker(bootstrapUrl, { name: "rxls-render-worker" });
  let released = false;
  const releaseBootstrap = () => {
    if (!released) {
      released = true;
      URL.revokeObjectURL(bootstrapUrl);
    }
  };
  worker.addEventListener("message", releaseBootstrap, { once: true });
  worker.addEventListener("error", releaseBootstrap, { once: true });
  worker.addEventListener("error", () => handleHostedWorkerCrash(worker));
  worker.postMessage(
    {
      protocol: "rxls.vscode.worker.bootstrap.v1",
      wasm: wasmBytes,
    },
    [wasmBytes],
  );
  return worker;
}

function handleHostedWorkerCrash(worker) {
  if (state.hostWorker !== worker || state.openRequest) {
    return;
  }
  resetViewport();
  state.renderOutcome = "failed";
  state.client = null;
  state.hostWorker = null;
  setBusy(false);
  const error = new Error("The isolated renderer stopped unexpectedly.");
  error.code = "worker_crashed";
  showError(error);
}

async function loadHostWorkbook(message) {
  try {
    if (!Number.isSafeInteger(message?.generation) || message.generation <= 0) {
      throw new TypeError("The VS Code workbook generation is invalid.");
    }
    const name = String(message?.file?.name ?? "");
    if (!acceptsWorkbook(name)) {
      throw new TypeError("VS Code provided an unsupported workbook name.");
    }
    const bytes = hostMessageBytes(message.bytes);
    if (bytes.byteLength > MAX_INPUT_BYTES) {
      const error = new Error(
        `The VS Code preview accepts files up to ${formatBytes(MAX_INPUT_BYTES)}.`,
      );
      error.code = "limit_exceeded";
      throw error;
    }
    state.hostGeneration = message.generation;
    const request = beginOpenRequest(`Opening ${name}`);
    const opened = await openWorkbook(
      bytes,
      {
        name,
        size: bytes.byteLength,
        source: "VS Code",
        sampleId: null,
      },
      request,
    );
    if (opened && state.hostGeneration === message.generation) {
      postHostMessage({
        type: "loaded",
        generation: message.generation,
        preview: viewerStateForTest(),
      });
    }
  } catch (error) {
    setBusy(false);
    if (!state.workbook) {
      showEmpty();
    }
    showError(error);
  }
}

function hostMessageBytes(value) {
  if (value instanceof Uint8Array) {
    return value;
  }
  if (value instanceof ArrayBuffer) {
    return new Uint8Array(value);
  }
  throw new TypeError("VS Code did not provide binary workbook bytes.");
}

function onHostMessage(event) {
  const message = event.data;
  if (!message || typeof message !== "object") {
    return;
  }
  switch (message.type) {
    case "load":
      void loadHostWorkbook(message);
      break;
    case "host-error": {
      if (
        Number.isSafeInteger(message.generation) &&
        message.generation < state.hostGeneration
      ) {
        return;
      }
      if (Number.isSafeInteger(message.generation) && message.generation >= 0) {
        state.hostGeneration = message.generation;
      }
      setBusy(false);
      if (!state.workbook) {
        showEmpty();
      }
      const error = new Error(
        String(message.message ?? "The workbook could not be loaded."),
      );
      error.code = String(message.code ?? "host_error");
      showError(error, false);
      break;
    }
    case "host-status":
      elements["status-message"].textContent = String(
        message.message ?? "Ready",
      );
      break;
    case "host-command":
      if (message.command === "export-svg") {
        exportSvg(message.requestId);
      } else if (message.command === "export-png") {
        void exportPng(message.requestId);
      } else if (
        message.command === "test-crash-renderer" &&
        state.hostWorker
      ) {
        state.hostWorker.postMessage({
          protocol: "rxls.vscode.worker.crash-test.v1",
        });
      }
      break;
    default:
      break;
  }
}

function requestHostReload() {
  if (!vscodeHost || state.busy) {
    return;
  }
  postHostMessage({ type: "reload" });
}

function postHostMessage(message) {
  vscodeHost?.postMessage(message);
}

async function selectSheet(index) {
  if (state.busy || index === state.sheetIndex || !state.workbook) {
    return;
  }
  if (!readyForViewChange() || !(await commitGridDraft())) return;
  if (!readyForViewChange() || state.busy || !state.workbook || index === state.sheetIndex) return;
  grid.invalidate();
  state.sheetIndex = index;
  state.pageIndex = 0;
  updateSheetSelection();
  await renderCurrent({ fit: true });
  closeSidebar();
}

async function setMode(mode) {
  if (!state.workbook || state.busy || state.mode === mode) {
    return;
  }
  if (!readyForViewChange() || !(await commitGridDraft())) return;
  if (!readyForViewChange() || state.busy || !state.workbook || mode === state.mode) return;
  grid.invalidate();
  state.mode = mode;
  state.pageIndex = 0;
  updateModeUi();
  await renderCurrent({ fit: true });
}

async function movePage(delta) {
  const manifest = state.manifests.get(state.sheetIndex);
  const pageCount = manifest?.pages?.length ?? 0;
  const next = Math.min(
    Math.max(state.pageIndex + delta, 0),
    Math.max(pageCount - 1, 0),
  );
  if (next === state.pageIndex || state.busy) {
    return;
  }
  state.pageIndex = next;
  await renderCurrent({ fit: false });
}

function readyForViewChange() {
  if (!editing.readyForViewChange()) return false;
  if (rangePaste.hasPending() || grid.hasDraft()) {
    showError(new Error("Apply or Cancel the cell or paste draft before changing views."));
    return false;
  }
  return true;
}

async function renderCurrent({ fit }) {
  if (!state.client || !state.workbook) return { status: "empty" };
  resetViewport();
  const target = Object.freeze({
    client: state.client, documentId: state.documentId,
    openGeneration: state.openGeneration, renderEpoch: ++state.renderEpoch,
    sheetIndex: state.sheetIndex, mode: state.mode,
  });
  const { client, documentId, sheetIndex, mode } = target;
  const sheet = state.workbook.sheets[sheetIndex];
  const isCurrent = () => currentRenderTarget(target);
  state.renderOutcome = "pending";
  setBusy(true, mode === "page" ? "Preparing pages" : `Rendering ${sheet.name}`);
  try {
    let rendered;
    if (mode === "page") {
      let manifest = state.manifests.get(sheetIndex);
      if (!manifest) {
        const prepared = await client.preparePages(documentId, sheetIndex);
        if (!isCurrent()) return { status: "stale" };
        manifest = prepared.manifest;
        state.manifests.set(sheetIndex, manifest);
      }
      const pageCount = Math.max(1, manifest.pages.length);
      state.pageIndex = Math.min(state.pageIndex, pageCount - 1);
      rendered = await client.renderPage(documentId, sheetIndex, state.pageIndex);
    } else {
      try {
        rendered = !vscodeHost && state.editState?.capability === "read-write" &&
          typeof client.renderSheetInteractive === "function"
          ? await client.renderSheetInteractive(documentId, sheetIndex)
          : await client.renderSheet(documentId, sheetIndex);
      } catch (error) {
        if (!isCurrent()) return { status: "stale" };
        if (!viewportFallbackError(error) ||
            !["viewportCapabilities", "prepareViewport", "renderViewportTile", "releaseViewport"]
              .every((name) => typeof client[name] === "function")) throw error;
        const outcome = await prepareTiledView(target, fit);
        return isCurrent() ? outcome : { status: "stale" };
      }
    }
    if (!isCurrent()) return { status: "stale" };
    showSvg(rendered.svg);
    state.displayKind = mode === "page" ? "page" : "full";
    state.renderOutcome = "ready";
    if (fit) requestAnimationFrame(() => { if (isCurrent()) fitToWidth(); });
    else applyZoom();
    updatePageUi();
    setBusy(false);
    if (rendered.interaction) grid.mount(rendered.interaction, state.svgElement);
    else grid.invalidate();
    void workbench.refresh();
    elements["status-message"].textContent = `${sheet.name} rendered`;
    elements["render-detail"].textContent = mode === "page" ? `Page ${state.pageIndex + 1}` : "Full sheet";
    return { status: "ready" };
  } catch (error) {
    if (!isCurrent() || error?.name === "AbortError") return { status: "stale" };
    grid.invalidate({ preserveSelection: true });
    state.renderOutcome = "failed";
    setBusy(false);
    showError(error);
    return { status: "failed", error };
  }
}

function currentRenderTarget(target) {
  return Boolean(target && target.client === state.client &&
    target.documentId === state.documentId && target.openGeneration === state.openGeneration &&
    target.renderEpoch === state.renderEpoch && target.sheetIndex === state.sheetIndex &&
    target.mode === state.mode);
}

function currentViewport(context) {
  const active = state.viewportContext;
  return currentRenderTarget(context) && state.displayKind === "tiled" && active &&
    context.geometryId === active.geometryId && context.revision === active.revision;
}

function settleViewport(status, error) {
  const barrier = state.viewportBarrier;
  if (!barrier) return;
  state.viewportBarrier = null;
  clearTimeout(barrier.timer);
  barrier.resolve({ status, ...(error ? { error } : {}) });
}

function releaseViewport(context) {
  if (!context) return;
  // Release only this immutable handle; an old completion cannot drop a newer one.
  void context.client.releaseViewport(context.documentId, context.sheetIndex,
    context.geometryId, context.revision).catch(() => {});
}

function resetViewport() {
  const previous = state.viewportContext;
  const previousSvg = state.viewportSvg;
  if (previousSvg?.wrapper.parentElement === elements["document-surface"])
    previousSvg.wrapper.remove();
  if (state.displayKind === "tiled") state.displayKind = "none";
  state.viewportContext = null;
  state.viewportReady = false;
  state.viewportSvg = null;
  state.viewportDiagnostics = null;
  state.viewportDelivery = null;
  settleViewport("stale");
  if (state.viewportFrame !== null) cancelAnimationFrame(state.viewportFrame);
  state.viewportFrame = null;
  if (!state.viewportDisposed) viewportScheduler.reset();
  elements["document-surface"].classList.remove("is-tiled");
  releaseViewport(previous);
}

async function prepareTiledView(target, fit) {
  const startedAtMs = performance.now();
  const capabilities = await target.client.viewportCapabilities();
  if (!currentRenderTarget(target)) return { status: "stale" };
  if (capabilities?.schemaVersion !== 1) throw new Error("Prepared viewport rendering is unavailable.");
  grid.invalidate();
  state.displayKind = "tiled";
  state.svgText = "";
  state.svgElement = null;
  elements["document-surface"].replaceChildren();
  elements["document-surface"].classList.add("is-tiled");
  const prepared = await target.client.prepareViewport(target.documentId, target.sheetIndex);
  const context = Object.freeze({ ...target,
    geometryId: prepared.geometryId, revision: prepared.revision,
    widthRaw: prepared.widthRaw, heightRaw: prepared.heightRaw,
    sheetVisibility: prepared.sheetVisibility,
    preparationReport: prepared.preparationReport,
  });
  if (!currentRenderTarget(target)) {
    releaseViewport(context);
    return { status: "stale" };
  }
  state.viewportContext = context;
  const finishedAtMs = performance.now();
  state.viewportDiagnostics = {
    preparation: Object.freeze({ startedAtMs, finishedAtMs, elapsedMs: finishedAtMs - startedAtMs }),
    coverage: null,
    firstCoverage: null,
    tile: null,
  };
  state.documentWidth = context.widthRaw / VIEWPORT_POLICY.unitsPerPixel;
  state.documentHeight = context.heightRaw / VIEWPORT_POLICY.unitsPerPixel;
  state.viewportInitialFit = fit;
  try {
    const maximum = VIEWPORT_POLICY.maxSurfacePixels;
    const fitted = Math.min(fitZoom(elements["viewer-viewport"].clientWidth, state.documentWidth || 1),
      state.documentWidth ? maximum / state.documentWidth : 3,
      state.documentHeight ? maximum / state.documentHeight : 3, 3);
    const zoom = fit ? clampZoom(fitted) : state.zoom;
    viewportSurfaceSize(context.widthRaw, context.heightRaw, zoom);
    state.zoom = zoom;
    applyZoom();
  } catch (error) {
    state.viewportContext = null;
    releaseViewport(context);
    throw error;
  }
  elements["document-stage"].hidden = false;
  elements["empty-state"].hidden = true;
  const outcome = new Promise((resolve) => {
    const timer = setTimeout(() => failViewport(new Error("The first visible viewport did not become ready within 30 seconds.")),
      VIEWPORT_POLICY.deadlineMs);
    state.viewportBarrier = { context, resolve, timer };
  });
  viewportScheduler.reset(context);
  if (context.widthRaw === 0 || context.heightRaw === 0) {
    state.renderOutcome = "empty";
    state.viewportReady = true;
    setBusy(false);
    elements["status-message"].textContent = "No visible sheet geometry";
    elements["render-detail"].textContent = "Tiled view · empty";
    settleViewport("empty");
    return outcome;
  }
  elements["render-detail"].textContent = "Preparing tiled view · read-only display";
  scheduleViewport();
  return outcome;
}

function scheduleViewport() {
  if (state.viewportDisposed || !state.viewportContext || state.displayKind !== "tiled" || state.viewportFrame !== null) return;
  state.viewportFrame = requestAnimationFrame(() => {
    state.viewportFrame = null;
    const context = state.viewportContext;
    if (!currentViewport(context) || context.widthRaw === 0 || context.heightRaw === 0) return;
    try {
      const cover = currentViewportCover(context);
      // A positive sheet inside a collapsed host waits for real nonzero bounds.
      if (cover) {
        const previous = state.viewportDiagnostics.coverage;
        const intent = (previous?.intent ?? 0) + 1;
        if (!Number.isSafeInteger(intent)) throw new RangeError("Viewport diagnostic intent exhausted.");
        state.viewportDiagnostics.coverage = Object.freeze({
          intent, acceptedAtMs: performance.now(), coveredAtMs: null,
          cacheKind: viewportContains(state.viewportSvg?.rect, cover.visible) ? "covered" : null,
          visible: Object.freeze({ ...cover.visible }),
          requested: Object.freeze({ ...cover.rect }),
          attached: state.viewportSvg ? Object.freeze({ ...state.viewportSvg.rect }) : null,
          documentId: context.documentId, sheetIndex: context.sheetIndex,
          geometryId: context.geometryId, revision: context.revision,
          namespace: state.viewportDiagnostics.tile?.namespace ?? null,
        });
        viewportScheduler.request(cover);
      }
    } catch (error) { if (currentViewport(context)) failViewport(error); }
  });
}

function currentViewportCover(context) {
  const viewport = elements["viewer-viewport"];
  if (viewport.clientWidth === 0 || viewport.clientHeight === 0) return null;
  const box = viewport.getBoundingClientRect();
  const surface = elements["document-surface"].getBoundingClientRect();
  const expected = viewportSurfaceSize(context.widthRaw, context.heightRaw, state.zoom);
  // CSS fractional extent tolerance 0.5px; integer scroll extent 1px.
  if (Math.abs(surface.width - expected.width) > 0.5 ||
      Math.abs(surface.height - expected.height) > 0.5 ||
      (viewport.clientWidth > 0 && viewport.scrollWidth + 1 < expected.width) ||
      (viewport.clientHeight > 0 && viewport.scrollHeight + 1 < expected.height)) {
    throw new Error("The browser clamped the prepared sheet surface.");
  }
  const left = box.left + viewport.clientLeft;
  const top = box.top + viewport.clientTop;
  return viewportCover({ ...context, zoom: state.zoom, surface,
    viewport: { left, top, right: left + viewport.clientWidth, bottom: top + viewport.clientHeight } });
}

function adoptViewportTile(tile, context) {
  if (!currentViewport(context)) throw new Error("Stale viewport output.");
  const svg = parsedSvg(tile.svg);
  const rect = tile.logicalRect;
  if (!rect) throw new Error("A visible viewport returned no geometry.");
  const wrapper = document.createElement("div");
  wrapper.className = "viewport-tile";
  wrapper.setAttribute("aria-label", "Read-only sheet viewport");
  svg.classList.add("viewport-svg");
  svg.setAttribute("role", "img");
  wrapper.append(svg);
  positionViewportTile(wrapper, svg, rect);
  if (!currentViewport(context)) throw new Error("Stale viewport output.");
  elements["document-surface"].replaceChildren(wrapper);
  state.viewportSvg = { wrapper, svg, rect };
  const diagnostic = state.viewportDiagnostics;
  const delivery = state.viewportDelivery;
  const cacheKind = delivery?.namespace === tile.namespace && !delivery.consumed ? "rendered" : "cached";
  if (delivery?.namespace === tile.namespace) delivery.consumed = true;
  diagnostic.tile = Object.freeze({ namespace: tile.namespace, mountedAtMs: performance.now(),
    svgBytes: tile.report?.svg_bytes ?? null, sceneNodes: tile.report?.scene_nodes ?? null,
    metrics: tile.metrics ? Object.freeze({ ...tile.metrics }) : null });
  if (diagnostic.coverage) diagnostic.coverage = Object.freeze({ ...diagnostic.coverage,
    cacheKind, attached: Object.freeze({ ...rect }), namespace: tile.namespace });
}

function positionViewportTile(wrapper, svg, rect) {
  const unit = VIEWPORT_POLICY.unitsPerPixel;
  const width = rect.widthRaw / unit;
  const height = rect.heightRaw / unit;
  Object.assign(wrapper.style, { left: `${rect.xRaw / unit * state.zoom}px`,
    top: `${rect.yRaw / unit * state.zoom}px`, width: `${width * state.zoom}px`, height: `${height * state.zoom}px` });
  Object.assign(svg.style, { width: `${width}px`, height: `${height}px`, transform: `scale(${state.zoom})` });
}

function updateViewportState(value) {
  const context = state.viewportContext;
  if (!currentViewport(context) || context.widthRaw === 0 || context.heightRaw === 0) return;
  // A scheduler callback can precede the next resize/zoom animation frame.
  // Recheck actual DOM coverage rather than trusting its preceding rectangle.
  let cover;
  try { cover = currentViewportCover(context); }
  catch (error) { failViewport(error); return; }
  state.viewportReady = Boolean(value.ready && cover &&
    viewportContains(state.viewportSvg?.rect, cover.visible));
  if (!state.viewportReady) return;
  const diagnostic = state.viewportDiagnostics;
  if (diagnostic.coverage && diagnostic.coverage.coveredAtMs === null) {
    diagnostic.coverage = Object.freeze({ ...diagnostic.coverage, coveredAtMs: performance.now() });
    if (!diagnostic.firstCoverage) diagnostic.firstCoverage = Object.freeze({
      acceptedAtMs: diagnostic.coverage.acceptedAtMs, coveredAtMs: diagnostic.coverage.coveredAtMs,
    });
  }
  state.renderOutcome = "ready";
  if (state.viewportBarrier) {
    setBusy(false);
    void workbench.refresh();
    settleViewport("ready");
  }
  elements["status-message"].textContent = "Tiled view · read-only display";
  elements["render-detail"].textContent = state.viewportInitialFit
    ? "Tiled view · fitted to viewport surface limit" : "Tiled view";
}

function failViewport(error) {
  if (!currentViewport(state.viewportContext)) return;
  const firstFailure = Boolean(state.viewportBarrier);
  state.viewportReady = false;
  state.renderOutcome = "failed";
  settleViewport("failed", error);
  if (firstFailure) {
    // Failed initial readiness cannot later be resurrected by an old delivery.
    // reset preserves ownership of the original CPU promise until settlement.
    resetViewport();
    state.displayKind = "tiled";
  }
  setBusy(false);
  showError(error);
}

/** Final embed disposal; the embedding owner separately terminates its worker. */
export function disposeViewport() {
  if (state.viewportDisposed) return;
  resetViewport();
  state.viewportDisposed = true;
  viewportScheduler.dispose();
  viewportObserver?.disconnect();
  elements["viewer-viewport"].removeEventListener("scroll", scheduleViewport);
}

function parsedSvg(svgText) {
  const parsed = new DOMParser().parseFromString(svgText, "image/svg+xml");
  if (
    parsed.querySelector("parsererror") ||
    parsed.documentElement.localName !== "svg"
  ) {
    throw new Error("The renderer returned invalid SVG output.");
  }
  const svg = document.importNode(parsed.documentElement, true);
  sanitizeSvg(svg);
  return svg;
}

function showSvg(svgText) {
  const svg = parsedSvg(svgText);
  svg.classList.add("rendered-svg");
  svg.setAttribute("role", "img");
  svg.setAttribute(
    "aria-label",
    `${state.workbook.sheets[state.sheetIndex].name} spreadsheet`,
  );
  const dimensions = svgDimensions(svg);
  state.svgText = svgText;
  state.svgElement = svg;
  state.documentWidth = dimensions.width;
  state.documentHeight = dimensions.height;
  elements["document-surface"].replaceChildren(svg);
  elements["document-stage"].hidden = false;
  elements["empty-state"].hidden = true;
}

function sanitizeSvg(svg) {
  for (const blocked of svg.querySelectorAll(BLOCKED_SVG_ELEMENTS)) {
    blocked.remove();
  }
  for (const element of [svg, ...svg.querySelectorAll("*")]) {
    for (const attribute of [...element.attributes]) {
      const name = attribute.name.toLowerCase();
      const value = attribute.value.trim();
      if (name.startsWith("on")) {
        element.removeAttribute(attribute.name);
        continue;
      }
      if (name === "href" || name.endsWith(":href")) {
        const internalReference = value.startsWith("#");
        const embeddedRaster =
          /^data:image\/(?:png|jpe?g|gif|webp);base64,/i.test(value);
        if (!internalReference && !embeddedRaster) {
          element.removeAttribute(attribute.name);
        }
      }
      if (name === "style" && /url\s*\((?!\s*['\"]?#)/i.test(value)) {
        element.removeAttribute(attribute.name);
      }
    }
  }
}

function setZoom(value) {
  const zoom = clampZoom(value);
  if (state.displayKind === "tiled" && state.viewportContext) {
    try { viewportSurfaceSize(state.viewportContext.widthRaw, state.viewportContext.heightRaw, zoom); }
    catch (error) { showError(error); return; }
  }
  state.zoom = zoom;
  state.viewportInitialFit = false;
  if (state.displayKind === "tiled") state.viewportReady = false;
  applyZoom();
}

function fitToWidth() {
  if (state.displayKind === "tiled" && state.viewportContext) {
    const maximum = VIEWPORT_POLICY.maxSurfacePixels;
    setZoom(Math.min(fitZoom(elements["viewer-viewport"].clientWidth, state.documentWidth || 1),
      state.documentWidth ? maximum / state.documentWidth : 3,
      state.documentHeight ? maximum / state.documentHeight : 3));
    state.viewportInitialFit = true;
    return;
  }
  if (!state.svgElement) {
    return;
  }
  state.zoom = fitZoom(
    elements["viewer-viewport"].clientWidth,
    state.documentWidth,
  );
  applyZoom();
}

function applyZoom() {
  if (state.displayKind === "tiled" && state.viewportContext) {
    const size = viewportSurfaceSize(state.viewportContext.widthRaw, state.viewportContext.heightRaw, state.zoom);
    Object.assign(elements["document-surface"].style, { width: `${size.width}px`, height: `${size.height}px` });
    if (state.viewportSvg) positionViewportTile(state.viewportSvg.wrapper, state.viewportSvg.svg, state.viewportSvg.rect);
    updateZoomUi();
    scheduleViewport();
    return;
  }
  if (!state.svgElement) {
    return;
  }
  const width = state.documentWidth * state.zoom;
  const height = state.documentHeight * state.zoom;
  const surface = elements["document-surface"];
  surface.style.width = `${width}px`;
  surface.style.height = `${height}px`;
  state.svgElement.style.width = `${state.documentWidth}px`;
  state.svgElement.style.height = `${state.documentHeight}px`;
  state.svgElement.style.transform = `scale(${state.zoom})`;
  updateZoomUi();
  grid.reposition();
}

function updateZoomUi() {
  elements["zoom-value"].textContent = `${Math.round(state.zoom * 100)}%`;
  elements["zoom-out"].disabled = state.zoom <= 0.25;
  elements["zoom-in"].disabled = state.zoom >= 3;
}

async function exportWithDraft(kind) {
  if (!(await commitGridDraft())) return;
  if (kind === "svg") exportSvg();
  else await exportPng();
}

function updateWorkbookUi() {
  const { workbook, file } = state;
  elements["document-name"].textContent = file.name;
  elements["document-detail"].textContent =
    `${file.source} · ${formatBytes(file.size)}`;
  elements["sheet-count"].textContent = String(workbook.sheetCount);
  elements["meta-format"].textContent = formatLabel(file.name);
  elements["meta-size"].textContent = formatBytes(file.size);
  elements["meta-images"].textContent = String(workbook.embeddedImages ?? 0);
  elements["sample-select"].value = file.sampleId ?? "";
  elements["sheet-list"].replaceChildren(
    ...workbook.sheets.map((sheet) => {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "sheet-button";
      button.dataset.index = String(sheet.index);
      button.textContent = sheet.name;
      button.addEventListener("click", () => void selectSheet(sheet.index));
      return button;
    }),
  );
  updateSheetSelection();
  updateModeUi();
  updateEditUi();
}

function updateSheetSelection() {
  for (const button of elements["sheet-list"].querySelectorAll("button")) {
    const active = Number(button.dataset.index) === state.sheetIndex;
    button.classList.toggle("active", active);
    button.setAttribute("aria-current", active ? "true" : "false");
  }
}

function updateModeUi() {
  const pageMode = state.mode === "page";
  elements["sheet-view"].classList.toggle("active", !pageMode);
  elements["sheet-view"].setAttribute("aria-pressed", String(!pageMode));
  elements["page-view"].classList.toggle("active", pageMode);
  elements["page-view"].setAttribute("aria-pressed", String(pageMode));
  elements["page-controls"].hidden = !pageMode;
  updatePageUi();
}

function updatePageUi() {
  const manifest = state.manifests.get(state.sheetIndex);
  const count = Math.max(1, manifest?.pages?.length ?? 1);
  elements["page-position"].textContent = `${state.pageIndex + 1} / ${count}`;
  elements["previous-page"].disabled = state.pageIndex <= 0;
  elements["next-page"].disabled = state.pageIndex >= count - 1;
}

function populateSamples() {
  const localOption = document.createElement("option");
  localOption.value = "";
  localOption.textContent = "Local file";
  localOption.disabled = true;
  elements["sample-select"].replaceChildren(
    localOption,
    ...samples.map((sample) => {
      const option = document.createElement("option");
      option.value = sample.id;
      option.textContent = `${sample.label} (${sample.format})`;
      return option;
    }),
  );
}

function setBusy(busy, label = "") {
  state.busy = busy;
  elements["loading-state"].hidden = !busy;
  if (label) {
    elements["loading-label"].textContent = label;
    elements["status-message"].textContent = label;
  }
  for (const control of [
    elements["sheet-view"],
    elements["page-view"],
    elements["previous-page"],
    elements["next-page"],
    elements["export-svg"],
    elements["export-png"],
  ]) {
    control.disabled = busy || !state.workbook ||
      (state.displayKind === "tiled" && [elements["export-svg"], elements["export-png"]].includes(control));
  }
  if (!busy) {
    updatePageUi();
  }
  updateEditUi();
  workbench.update();
  grid?.update();
}

function showEmpty() {
  resetViewport();
  state.displayKind = "none";
  state.renderOutcome = "empty";
  grid?.invalidate();
  elements["document-stage"].hidden = true;
  elements["empty-state"].hidden = false;
  elements["document-name"].textContent = "No workbook open";
  elements["document-detail"].textContent = "Rust + WebAssembly";
  updateEditUi();
  void workbench.refresh();
}

function showError(error, reportToHost = true) {
  const message = describeError(error);
  elements["error-message"].textContent = message;
  elements["error-banner"].hidden = false;
  if (reportToHost) {
    postHostMessage({
      type: "preview-error",
      generation: state.hostGeneration,
      code: String(error?.code ?? error?.name ?? "error"),
      message,
    });
  }
}

function dismissError() {
  elements["error-banner"].hidden = true;
}

function onDragEnter(event) {
  event.preventDefault();
  state.dragDepth += 1;
  elements["drop-overlay"].hidden = false;
}

function onDragOver(event) {
  event.preventDefault();
  event.dataTransfer.dropEffect = "copy";
}

function onDragLeave(event) {
  event.preventDefault();
  state.dragDepth = Math.max(0, state.dragDepth - 1);
  if (state.dragDepth === 0) {
    elements["drop-overlay"].hidden = true;
  }
}

function onDrop(event) {
  event.preventDefault();
  state.dragDepth = 0;
  elements["drop-overlay"].hidden = true;
  const [file] = event.dataTransfer.files ?? [];
  if (file) {
    void loadLocalFile(file);
  }
}

function onKeyDown(event) {
  if (event.defaultPrevented || event.isComposing || event.keyCode === 229)
    return;
  if (event.key === "Escape" && grid.hasDraft()) {
    event.preventDefault();
    if (grid.cancel()) elements["grid-input"].focus();
    return;
  }
  const modified = event.ctrlKey || event.metaKey;
  const key = event.key.toLowerCase();
  const editingText =
    event.target instanceof Element &&
    event.target.matches("input, textarea, select") &&
    (event.target !== elements["grid-input"] || grid.hasDraft());
  if (!modified) {
    return;
  }
  if (key === "o") {
    if (vscodeHost) {
      return;
    }
    event.preventDefault();
    chooseFile();
    return;
  }
  if (key === "s" && state.editState?.capability === "read-write") {
    event.preventDefault();
    void saveWorkbookCopy();
    return;
  }
  if (
    !editingText &&
    key === "e" &&
    state.displayKind !== "tiled" &&
    state.editState?.capability === "read-write"
  ) {
    event.preventDefault();
    void openCellEditor();
    return;
  }
  if (
    !editingText &&
    key === "z" &&
    state.displayKind !== "tiled" &&
    (event.shiftKey ? state.editState?.canRedo : state.editState?.canUndo)
  ) {
    event.preventDefault();
    void applyHistoryEdit(event.shiftKey ? "redo" : "undo");
    return;
  }
  if (!editingText && state.displayKind !== "tiled" && key === "y" && state.editState?.canRedo) {
    event.preventDefault();
    void applyHistoryEdit("redo");
    return;
  }
  if (["+", "="].includes(event.key)) {
    event.preventDefault();
    setZoom(state.zoom + ZOOM_STEP);
    return;
  }
  if (event.key === "-") {
    event.preventDefault();
    setZoom(state.zoom - ZOOM_STEP);
  }
}

function toggleSidebar() {
  workbench.togglePanel();
}

function closeSidebar() {
  workbench.closePanel();
}

async function fetchJson(url) {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(
      `Viewer manifest request failed with HTTP ${response.status}.`,
    );
  }
  return response.json();
}

export function viewerStateForTest() {
  return {
    host: hostKind,
    hostGeneration: state.hostGeneration,
    fileName: state.file?.name ?? null,
    source: state.file?.source ?? null,
    format: state.file ? extensionOf(state.file.name) : null,
    sheetCount: state.workbook?.sheetCount ?? 0,
    sheetIndex: state.sheetIndex,
    mode: state.mode,
    pageIndex: state.pageIndex,
    zoom: state.zoom,
    busy: state.busy,
    rendered: state.renderOutcome === "ready" &&
      (state.displayKind === "tiled" ? state.viewportReady : Boolean(state.svgElement)),
    displayKind: state.displayKind,
    renderOutcome: state.renderOutcome,
    viewport: state.displayKind === "tiled" ? {
      schemaVersion: 1,
      geometryId: state.viewportContext?.geometryId ?? null,
      revision: state.viewportContext?.revision ?? null,
      widthRaw: state.viewportContext?.widthRaw ?? null,
      heightRaw: state.viewportContext?.heightRaw ?? null,
      preparationReport: viewportPreparationDiagnostics(state.viewportContext?.preparationReport),
      ...(state.viewportDiagnostics ?? {}),
      ...viewportScheduler.inspect(),
    } : null,
    editCapability: state.editState?.capability ?? null,
    editReason: state.editState?.reason ?? null,
    dirty: state.editState?.dirty ?? false,
    canUndo: state.editState?.canUndo ?? false,
    canRedo: state.editState?.canRedo ?? false,
    editedParts: [...(state.editState?.editedParts ?? [])],
  };
}

function viewportPreparationDiagnostics(report) {
  if (!report) return null;
  const { fontFaces, warnings, ...counters } = report;
  return Object.freeze({ ...counters, fontFaceCount: fontFaces?.length ?? 0,
    warningCount: warnings?.length ?? 0 });
}

/** Bounded diagnostic zoom route; it exposes no editing, export or worker calls. */
export function viewerSetZoomForTest(value) {
  const context = state.viewportContext;
  let reason = "invalid-zoom";
  let accepted = false;
  if ([0.25, 1, 2, 3].includes(value)) {
    reason = "no-tiled-view";
    if (currentViewport(context)) {
      setZoom(value);
      accepted = currentViewport(context) && state.zoom === value;
      reason = accepted ? "accepted" : "surface-limit";
    }
  }
  return Object.freeze({ schemaVersion: 1, accepted, reason, zoom: state.zoom,
    geometryId: context?.geometryId ?? null, revision: context?.revision ?? null });
}

globalThis.__rxlsViewerState = viewerStateForTest;
globalThis.__rxlsViewerSetZoomForTest = viewerSetZoomForTest;
