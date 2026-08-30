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
  createIcons
} from "lucide";
import {
  acceptsWorkbook,
  clampZoom,
  createLatestRequestGate,
  describeError,
  editableCell,
  editReasonLabel,
  extensionOf,
  fitZoom,
  formatBytes,
  formatLabel,
  parseCellReference,
  sameCellTarget,
  savedWorkbookName,
  safeBaseName,
  svgDimensions
} from "./core.js";
import "./styles.css";

const MAX_INPUT_BYTES = 32 * 1024 * 1024;
const MAX_PNG_PIXELS = 16 * 1024 * 1024;
const ZOOM_STEP = 0.15;
const BLOCKED_SVG_ELEMENTS = "script, foreignObject, iframe, object, embed";
const hostKind = document.querySelector('meta[name="rxls-host-kind"]')?.content ?? "browser";
const hostResourceBase = document.querySelector('meta[name="rxls-resource-base"]')?.content;
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
  ZoomOut
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
    "property-created"
  ].map((id) => [id, document.getElementById(id)])
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
  cellReadTarget: null,
  cellReadPending: false,
  cellEditPending: false,
  dragDepth: 0,
  hostGeneration: 0,
  hostWorker: null
};

const baseUrl = hostResourceBase
  ? new URL(hostResourceBase)
  : new URL(import.meta.env.BASE_URL, window.location.origin);
const openRequests = createLatestRequestGate();
const cellReads = createLatestRequestGate();
let samples = [];

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
    const sampleManifest = await fetchJson(new URL("samples/manifest.json", baseUrl));
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
    const sample = samples.find((entry) => entry.id === elements["sample-select"].value);
    if (sample) {
      void loadSample(sample);
    }
  });
  elements["sheet-view"].addEventListener("click", () => void setMode("sheet"));
  elements["page-view"].addEventListener("click", () => void setMode("page"));
  elements["previous-page"].addEventListener("click", () => void movePage(-1));
  elements["next-page"].addEventListener("click", () => void movePage(1));
  elements["edit-cell"].addEventListener("click", () => void openCellEditor());
  elements["undo-edit"].addEventListener("click", () => void applyHistoryEdit("undo"));
  elements["redo-edit"].addEventListener("click", () => void applyHistoryEdit("redo"));
  elements["document-properties"].addEventListener("click", openPropertiesEditor);
  elements["zoom-out"].addEventListener("click", () => setZoom(state.zoom - ZOOM_STEP));
  elements["zoom-in"].addEventListener("click", () => setZoom(state.zoom + ZOOM_STEP));
  elements["fit-view"].addEventListener("click", fitToWidth);
  elements["save-document"].addEventListener("click", () => void saveWorkbookCopy());
  elements["export-svg"].addEventListener("click", () => exportSvg());
  elements["export-png"].addEventListener("click", () => void exportPng());
  elements["dismiss-error"].addEventListener("click", dismissError);
  elements["sidebar-toggle"].addEventListener("click", toggleSidebar);
  elements["sidebar-scrim"].addEventListener("click", closeSidebar);
  elements["close-cell-dialog"].addEventListener("click", closeCellEditor);
  elements["cancel-cell-edit"].addEventListener("click", closeCellEditor);
  elements["read-cell"].addEventListener("click", () => void loadCellIntoEditor());
  elements["cell-reference"].addEventListener("input", invalidateCellRead);
  elements["cell-reference"].addEventListener("change", () => void loadCellIntoEditor());
  elements["cell-kind"].addEventListener("change", updateCellKindUi);
  elements["cell-form"].addEventListener("submit", (event) => void submitCellEdit(event));
  elements["close-properties-dialog"].addEventListener("click", closePropertiesEditor);
  elements["cancel-properties-edit"].addEventListener("click", closePropertiesEditor);
  elements["properties-form"].addEventListener("submit", (event) =>
    void submitPropertiesEdit(event)
  );

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
    if (state.editState?.dirty) {
      event.preventDefault();
      event.returnValue = "";
    }
  });
  window.addEventListener("resize", () => {
    if (state.svgElement && state.zoom <= 1) {
      fitToWidth();
    }
  });
}

function chooseFile() {
  elements["file-input"].click();
}

async function loadLocalFile(file) {
  if (!confirmDiscardChanges()) {
    return;
  }
  if (!acceptsWorkbook(file.name)) {
    showError(new Error("Choose an XLS, XLSX, XLSM, XLSB, or ODS file."));
    return;
  }
  if (file.size > MAX_INPUT_BYTES) {
    const error = new Error(
      `The browser viewer accepts files up to ${formatBytes(MAX_INPUT_BYTES)}.`
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
        sampleId: null
      },
      request
    );
  } catch (error) {
    failOpenRequest(request, error);
  }
}

async function loadSample(sample) {
  if (!confirmDiscardChanges()) {
    elements["sample-select"].value = state.file?.sampleId ?? "";
    return;
  }
  const request = beginOpenRequest(`Opening ${sample.name}`);
  try {
    const response = await fetch(new URL(`samples/${sample.file}`, baseUrl), {
      signal: request.abortController.signal
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
        sampleId: sample.id
      },
      request
    );
  } catch (error) {
    failOpenRequest(request, error);
  }
}

function beginOpenRequest(label) {
  state.openRequest?.abortController.abort();
  state.openRequest?.client?.terminate();
  const request = {
    token: openRequests.begin(),
    abortController: new AbortController(),
    client: null
  };
  state.openRequest = request;
  state.renderEpoch += 1;
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
    client = new state.runtime.RenderWorkerClient(workerTarget);
    request.client = client;
    const documentId = `viewer-${Date.now()}-${Math.random().toString(16).slice(2)}`;
    const opened = await client.open(bytes, { documentId });
    if (!isCurrentOpenRequest(request)) {
      client.terminate();
      return false;
    }

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
    await renderCurrent({ fit: true });
    if (isCurrentOpenRequest(request)) {
      state.openRequest = null;
      closeSidebar();
    }
    return true;
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
    fetch(wasmUrl)
  ]);
  if (!workerResponse.ok || !wasmResponse.ok) {
    throw new Error("The packaged VS Code renderer could not be loaded.");
  }
  const [workerBlob, wasmBytes] = await Promise.all([
    workerResponse.blob(),
    wasmResponse.arrayBuffer()
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
      wasm: wasmBytes
    },
    [wasmBytes]
  );
  return worker;
}

function handleHostedWorkerCrash(worker) {
  if (state.hostWorker !== worker || state.openRequest) {
    return;
  }
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
        `The VS Code preview accepts files up to ${formatBytes(MAX_INPUT_BYTES)}.`
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
        sampleId: null
      },
      request
    );
    if (opened && state.hostGeneration === message.generation) {
      postHostMessage({
        type: "loaded",
        generation: message.generation,
        preview: viewerStateForTest()
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
      const error = new Error(String(message.message ?? "The workbook could not be loaded."));
      error.code = String(message.code ?? "host_error");
      showError(error, false);
      break;
    }
    case "host-status":
      elements["status-message"].textContent = String(message.message ?? "Ready");
      break;
    case "host-command":
      if (message.command === "export-svg") {
        exportSvg(message.requestId);
      } else if (message.command === "export-png") {
        void exportPng(message.requestId);
      } else if (message.command === "test-crash-renderer" && state.hostWorker) {
        state.hostWorker.postMessage({ protocol: "rxls.vscode.worker.crash-test.v1" });
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
  state.mode = mode;
  state.pageIndex = 0;
  updateModeUi();
  await renderCurrent({ fit: true });
}

async function movePage(delta) {
  const manifest = state.manifests.get(state.sheetIndex);
  const pageCount = manifest?.pages?.length ?? 0;
  const next = Math.min(Math.max(state.pageIndex + delta, 0), Math.max(pageCount - 1, 0));
  if (next === state.pageIndex || state.busy) {
    return;
  }
  state.pageIndex = next;
  await renderCurrent({ fit: false });
}

async function renderCurrent({ fit }) {
  if (!state.client || !state.workbook) {
    return;
  }
  const epoch = ++state.renderEpoch;
  const client = state.client;
  const documentId = state.documentId;
  const sheetIndex = state.sheetIndex;
  const mode = state.mode;
  const sheet = state.workbook.sheets[sheetIndex];
  const isCurrent = () =>
    epoch === state.renderEpoch && client === state.client && documentId === state.documentId;
  setBusy(true, mode === "page" ? "Preparing pages" : `Rendering ${sheet.name}`);
  try {
    let rendered;
    if (mode === "page") {
      let manifest = state.manifests.get(sheetIndex);
      if (!manifest) {
        const prepared = await client.preparePages(documentId, sheetIndex);
        if (!isCurrent()) {
          return;
        }
        manifest = prepared.manifest;
        state.manifests.set(sheetIndex, manifest);
      }
      const pageCount = Math.max(1, manifest.pages.length);
      state.pageIndex = Math.min(state.pageIndex, pageCount - 1);
      const pageIndex = state.pageIndex;
      rendered = await client.renderPage(documentId, sheetIndex, pageIndex);
    } else {
      rendered = await client.renderSheet(documentId, sheetIndex);
    }
    if (!isCurrent()) {
      return;
    }
    showSvg(rendered.svg);
    if (fit) {
      requestAnimationFrame(fitToWidth);
    } else {
      applyZoom();
    }
    updatePageUi();
    setBusy(false);
    elements["status-message"].textContent = `${sheet.name} rendered`;
    elements["render-detail"].textContent =
      mode === "page" ? `Page ${state.pageIndex + 1}` : "Full sheet";
  } catch (error) {
    if (!isCurrent() || error?.name === "AbortError") {
      return;
    }
    setBusy(false);
    showError(error);
  }
}

function showSvg(svgText) {
  const parsed = new DOMParser().parseFromString(svgText, "image/svg+xml");
  if (parsed.querySelector("parsererror") || parsed.documentElement.localName !== "svg") {
    throw new Error("The renderer returned invalid SVG output.");
  }
  const svg = document.importNode(parsed.documentElement, true);
  sanitizeSvg(svg);
  svg.classList.add("rendered-svg");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", `${state.workbook.sheets[state.sheetIndex].name} spreadsheet`);
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
        const embeddedRaster = /^data:image\/(?:png|jpe?g|gif|webp);base64,/i.test(value);
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
  state.zoom = clampZoom(value);
  applyZoom();
}

function fitToWidth() {
  if (!state.svgElement) {
    return;
  }
  state.zoom = fitZoom(elements["viewer-viewport"].clientWidth, state.documentWidth);
  applyZoom();
}

function applyZoom() {
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
  elements["zoom-value"].textContent = `${Math.round(state.zoom * 100)}%`;
  elements["zoom-out"].disabled = state.zoom <= 0.25;
  elements["zoom-in"].disabled = state.zoom >= 3;
}

function updateWorkbookUi() {
  const { workbook, file } = state;
  elements["document-name"].textContent = file.name;
  elements["document-detail"].textContent = `${file.source} · ${formatBytes(file.size)}`;
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
    })
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

function updateEditUi() {
  const editState = state.editState;
  const editable = editState?.capability === "read-write";
  const hostReadOnly = Boolean(state.workbook && vscodeHost);
  const available = Boolean(state.workbook && editable && !state.busy && !vscodeHost);
  const readOnly = Boolean(state.workbook && !editable && !hostReadOnly);
  const reason = readOnly ? editReasonLabel(editState?.reason) : null;
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
    Boolean(editState?.dirty && !hostReadOnly)
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
    elements["save-document"]
  ]) {
    control.title = reason ?? control.dataset.commandTitle ?? control.title;
    if (reason) {
      control.setAttribute("aria-describedby", "editing-reason");
    } else {
      control.removeAttribute("aria-describedby");
    }
  }
  if (editable && !vscodeHost) {
    elements["edit-cell"].title = "Edit cell";
    elements["document-properties"].title = "Document properties";
    elements["save-document"].title = "Download preserved workbook";
  }
  if (state.file) {
    elements["document-detail"].textContent = `${state.file.source} · ${formatBytes(state.file.size)}${
      editState?.dirty ? " · Modified" : ""
    }`;
  }
  updateCellEditorControls();
}

async function openCellEditor() {
  if (!canEditWorkbook()) {
    return;
  }
  elements["cell-sheet-name"].textContent = state.workbook.sheets[state.sheetIndex].name;
  if (!elements["cell-dialog"].open) {
    elements["cell-dialog"].showModal();
  }
  invalidateCellRead();
  elements["cell-reference"].focus();
  elements["cell-reference"].select();
  await loadCellIntoEditor();
}

function closeCellEditor() {
  cellReads.invalidate();
  state.cellReadTarget = null;
  state.cellReadPending = false;
  state.cellEditPending = false;
  if (elements["cell-dialog"].open) {
    elements["cell-dialog"].close();
  }
  resetCellEditorFields();
  updateCellEditorControls();
}

function invalidateCellRead() {
  cellReads.invalidate();
  state.cellReadTarget = null;
  state.cellReadPending = false;
  resetCellEditorFields();
  if (elements["cell-dialog"].open) {
    elements["cell-current-value"].textContent = "Load this cell before editing it";
  }
  updateCellEditorControls();
}

async function loadCellIntoEditor() {
  if (!canEditWorkbook()) {
    return;
  }
  const token = cellReads.begin();
  const client = state.client;
  const documentId = state.documentId;
  const sheetIndex = state.sheetIndex;
  state.cellReadTarget = null;
  state.cellReadPending = true;
  resetCellEditorFields();
  updateCellEditorControls();
  let coordinate;
  try {
    coordinate = parseCellReference(elements["cell-reference"].value);
    elements["cell-reference"].value = coordinate.normalized;
    elements["cell-current-value"].textContent = `Loading ${coordinate.normalized}`;
    const result = await client.readCell(
      documentId,
      sheetIndex,
      coordinate.row,
      coordinate.col
    );
    if (
      !cellReads.isCurrent(token) ||
      client !== state.client ||
      documentId !== state.documentId ||
      sheetIndex !== state.sheetIndex ||
      !elements["cell-dialog"].open
    ) {
      return;
    }
    state.cellReadTarget = cellTarget(coordinate, client, documentId, sheetIndex);
    state.cellReadPending = false;
    populateCellEditor(result.value);
    elements["cell-current-value"].textContent = result.formatted
      ? `${coordinate.normalized}: ${result.formatted}`
      : `${coordinate.normalized}: ${describeCell(result.value)}`;
  } catch (error) {
    if (!cellReads.isCurrent(token)) {
      return;
    }
    state.cellReadTarget = null;
    state.cellReadPending = false;
    elements["cell-current-value"].textContent = describeError(error);
    showError(error);
  } finally {
    if (cellReads.isCurrent(token)) {
      state.cellReadPending = false;
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
  sheetIndex = state.sheetIndex
) {
  return {
    client,
    documentId,
    sheetIndex,
    row: coordinate.row,
    col: coordinate.col
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
  const loaded = sameCellTarget(state.cellReadTarget, currentCellTarget());
  const ready = dialogOpen && canEditWorkbook() && loaded && !state.cellReadPending;
  const valuesDisabled = !ready || state.cellEditPending;
  for (const control of [
    elements["cell-kind"],
    elements["cell-value"],
    elements["cell-formula"],
    elements["cell-cached-kind"],
    elements["cell-cached-value"]
  ]) {
    control.disabled = valuesDisabled;
  }
  elements["cell-reference"].disabled = state.cellEditPending;
  elements["read-cell"].disabled =
    !dialogOpen || !canEditWorkbook() || state.cellReadPending || state.cellEditPending;
  elements["apply-cell-edit"].disabled = !ready || state.cellEditPending;
  elements["close-cell-dialog"].disabled = state.cellEditPending;
  elements["cancel-cell-edit"].disabled = state.cellEditPending;
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
  try {
    const coordinate = parseCellReference(elements["cell-reference"].value);
    if (!sameCellTarget(state.cellReadTarget, cellTarget(coordinate))) {
      throw new Error(`Load ${coordinate.normalized} before applying an edit.`);
    }
    const value = editableCell(elements["cell-kind"].value, elements["cell-value"].value, {
      formula: elements["cell-formula"].value,
      cachedKind: elements["cell-cached-kind"].value,
      cachedValue: elements["cell-cached-value"].value
    });
    state.cellEditPending = true;
    updateCellEditorControls();
    elements["cell-current-value"].textContent = `Applying ${coordinate.normalized}`;
    const result = await state.client.setCell(
      state.documentId,
      state.sheetIndex,
      coordinate.row,
      coordinate.col,
      value
    );
    closeCellEditor();
    await applyMutationResult(result, `${coordinate.normalized} updated`);
  } catch (error) {
    elements["cell-current-value"].textContent = describeError(error);
    showError(error);
  } finally {
    state.cellEditPending = false;
    updateCellEditorControls();
    updateCellKindUi();
  }
}

function openPropertiesEditor() {
  if (!canEditWorkbook()) {
    return;
  }
  const properties = state.workbook.properties;
  for (const [property, elementId] of propertyFields()) {
    elements[elementId].value = properties[property] ?? "";
  }
  if (!elements["properties-dialog"].open) {
    elements["properties-dialog"].showModal();
  }
  elements["property-title"].focus();
}

function closePropertiesEditor() {
  if (elements["properties-dialog"].open) {
    elements["properties-dialog"].close();
  }
}

async function submitPropertiesEdit(event) {
  event.preventDefault();
  if (!canEditWorkbook()) {
    return;
  }
  try {
    setFormPending(elements["properties-form"], true);
    const properties = Object.fromEntries(
      propertyFields().map(([property, elementId]) => [
        property,
        elements[elementId].value || null
      ])
    );
    const result = await state.client.setDocumentProperties(state.documentId, properties);
    closePropertiesEditor();
    await applyMutationResult(result, "Document properties updated");
  } catch (error) {
    showError(error);
  } finally {
    setFormPending(elements["properties-form"], false);
  }
}

async function applyHistoryEdit(direction) {
  if (!canEditWorkbook()) {
    return;
  }
  try {
    setBusy(true, direction === "undo" ? "Undoing edit" : "Redoing edit");
    const result =
      direction === "undo"
        ? await state.client.undoEdit(state.documentId)
        : await state.client.redoEdit(state.documentId);
    await applyMutationResult(result, direction === "undo" ? "Edit undone" : "Edit redone");
  } catch (error) {
    setBusy(false);
    showError(error);
  }
}

async function applyMutationResult(result, message) {
  state.workbook = result.workbook;
  state.editState = result.editState;
  state.manifests.clear();
  state.pageIndex = 0;
  state.sheetIndex = Math.min(state.sheetIndex, Math.max(0, state.workbook.sheetCount - 1));
  updateWorkbookUi();
  await renderCurrent({ fit: false });
  elements["status-message"].textContent = message;
}

async function saveWorkbookCopy() {
  if (!canEditWorkbook()) {
    return;
  }
  try {
    setBusy(true, "Preparing preserved workbook");
    const saved = await state.client.saveDocument(state.documentId);
    const extension = extensionOf(state.file.name);
    const mimeType =
      extension === "xlsm"
        ? "application/vnd.ms-excel.sheet.macroEnabled.12"
        : "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
    downloadBlob(new Blob([saved.bytes], { type: mimeType }), savedWorkbookName(state.file.name));
    elements["status-message"].textContent = "Preserved workbook downloaded";
  } catch (error) {
    showError(error);
  } finally {
    setBusy(false);
    elements["export-menu"].removeAttribute("open");
  }
}

function canEditWorkbook() {
  return Boolean(
    !vscodeHost &&
      state.client &&
      state.workbook &&
      !state.busy &&
      state.editState?.capability === "read-write"
  );
}

function confirmDiscardChanges() {
  return !state.editState?.dirty || window.confirm("Discard unsaved workbook edits?");
}

function setFormPending(form, pending) {
  for (const button of form.querySelectorAll("button")) {
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
    ["created", "property-created"]
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
    })
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
    elements["export-png"]
  ]) {
    control.disabled = busy || !state.workbook;
  }
  if (!busy) {
    updatePageUi();
  }
  updateEditUi();
}

function showEmpty() {
  elements["document-stage"].hidden = true;
  elements["empty-state"].hidden = false;
  elements["document-name"].textContent = "No workbook open";
  elements["document-detail"].textContent = "Rust + WebAssembly";
  updateEditUi();
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
      message
    });
  }
}

function dismissError() {
  elements["error-banner"].hidden = true;
}

function exportSvg(requestId = null) {
  if (!state.svgText) {
    return;
  }
  const fileName = `${exportBaseName()}.svg`;
  if (vscodeHost) {
    postHostMessage({
      type: "export",
      requestId,
      kind: "svg",
      fileName,
      bytes: new TextEncoder().encode(state.svgText)
    });
  } else {
    downloadBlob(new Blob([state.svgText], { type: "image/svg+xml;charset=utf-8" }), fileName);
  }
  elements["status-message"].textContent = "SVG exported";
  elements["export-menu"].removeAttribute("open");
}

async function exportPng(requestId = null) {
  if (!state.svgElement) {
    return;
  }
  try {
    setBusy(true, "Creating PNG");
    const serialized = new XMLSerializer().serializeToString(state.svgElement);
    const source = new Blob([serialized], { type: "image/svg+xml;charset=utf-8" });
    const sourceUrl = URL.createObjectURL(source);
    try {
      const image = await loadImage(sourceUrl);
      const pixelScale = Math.min(
        2,
        Math.sqrt(MAX_PNG_PIXELS / (state.documentWidth * state.documentHeight))
      );
      const width = Math.max(1, Math.round(state.documentWidth * pixelScale));
      const height = Math.max(1, Math.round(state.documentHeight * pixelScale));
      const canvas = document.createElement("canvas");
      canvas.width = width;
      canvas.height = height;
      const context = canvas.getContext("2d", { alpha: false });
      context.fillStyle = "#ffffff";
      context.fillRect(0, 0, width, height);
      context.drawImage(image, 0, 0, width, height);
      const png = await new Promise((resolve, reject) => {
        canvas.toBlob(
          (blob) => (blob ? resolve(blob) : reject(new Error("PNG encoding failed."))),
          "image/png"
        );
      });
      const fileName = `${exportBaseName()}.png`;
      if (vscodeHost) {
        postHostMessage({
          type: "export",
          requestId,
          kind: "png",
          fileName,
          bytes: new Uint8Array(await png.arrayBuffer())
        });
      } else {
        downloadBlob(png, fileName);
      }
      elements["status-message"].textContent = "PNG exported";
    } finally {
      URL.revokeObjectURL(sourceUrl);
    }
  } catch (error) {
    showError(error);
  } finally {
    setBusy(false);
    elements["export-menu"].removeAttribute("open");
  }
}

function exportBaseName() {
  const sheetName = state.workbook?.sheets?.[state.sheetIndex]?.name ?? "sheet";
  const suffix = state.mode === "page" ? `page-${state.pageIndex + 1}` : "sheet";
  return `${safeBaseName(state.file?.name)}-${safeBaseName(sheetName)}-${suffix}`;
}

function downloadBlob(blob, fileName) {
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = fileName;
  anchor.click();
  setTimeout(() => URL.revokeObjectURL(url), 1_000);
}

function loadImage(url) {
  return new Promise((resolve, reject) => {
    const image = new Image();
    image.addEventListener("load", () => resolve(image), { once: true });
    image.addEventListener("error", () => reject(new Error("SVG rasterization failed.")), {
      once: true
    });
    image.src = url;
  });
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
  const modified = event.ctrlKey || event.metaKey;
  const key = event.key.toLowerCase();
  const editingText = event.target instanceof Element && event.target.matches("input, textarea, select");
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
  if (!editingText && key === "s" && state.editState?.capability === "read-write") {
    event.preventDefault();
    void saveWorkbookCopy();
    return;
  }
  if (!editingText && key === "e" && state.editState?.capability === "read-write") {
    event.preventDefault();
    void openCellEditor();
    return;
  }
  if (!editingText && key === "z" && state.editState?.canUndo) {
    event.preventDefault();
    void applyHistoryEdit(event.shiftKey ? "redo" : "undo");
    return;
  }
  if (!editingText && key === "y" && state.editState?.canRedo) {
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
  const open = !document.body.classList.contains("sidebar-open");
  document.body.classList.toggle("sidebar-open", open);
  elements["sidebar-toggle"].setAttribute("aria-expanded", String(open));
}

function closeSidebar() {
  document.body.classList.remove("sidebar-open");
  elements["sidebar-toggle"].setAttribute("aria-expanded", "false");
}

async function fetchJson(url) {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`Viewer manifest request failed with HTTP ${response.status}.`);
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
    rendered: Boolean(state.svgElement),
    editCapability: state.editState?.capability ?? null,
    editReason: state.editState?.reason ?? null,
    dirty: state.editState?.dirty ?? false,
    canUndo: state.editState?.canUndo ?? false,
    canRedo: state.editState?.canRedo ?? false,
    editedParts: [...(state.editState?.editedParts ?? [])]
  };
}

globalThis.__rxlsViewerState = viewerStateForTest;
