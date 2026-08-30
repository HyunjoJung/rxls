import {
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  CircleAlert,
  Download,
  ExternalLink,
  FileCode2,
  FileSpreadsheet,
  Files,
  FileUp,
  FolderOpen,
  ImageDown,
  PanelLeft,
  Scan,
  ShieldCheck,
  Table2,
  X,
  ZoomIn,
  ZoomOut,
  createIcons
} from "lucide";
import {
  acceptsWorkbook,
  clampZoom,
  describeError,
  extensionOf,
  fitZoom,
  formatBytes,
  formatLabel,
  safeBaseName,
  svgDimensions
} from "./core.js";
import "./styles.css";

const MAX_INPUT_BYTES = 32 * 1024 * 1024;
const MAX_PNG_PIXELS = 16 * 1024 * 1024;
const ZOOM_STEP = 0.15;
const BLOCKED_SVG_ELEMENTS = "script, foreignObject, iframe, object, embed";

const icons = {
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  CircleAlert,
  Download,
  ExternalLink,
  FileCode2,
  FileSpreadsheet,
  Files,
  FileUp,
  FolderOpen,
  ImageDown,
  PanelLeft,
  Scan,
  ShieldCheck,
  Table2,
  X,
  ZoomIn,
  ZoomOut
};

createIcons({ icons });

const elements = Object.fromEntries(
  [
    "document-name",
    "document-detail",
    "open-button",
    "empty-open-button",
    "file-input",
    "sample-select",
    "sheet-list",
    "sheet-count",
    "meta-format",
    "meta-size",
    "meta-images",
    "sheet-view",
    "page-view",
    "page-controls",
    "previous-page",
    "next-page",
    "page-position",
    "zoom-out",
    "zoom-in",
    "zoom-value",
    "fit-view",
    "export-menu",
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
    "sidebar-scrim"
  ].map((id) => [id, document.getElementById(id)])
);

const state = {
  runtime: null,
  client: null,
  documentId: null,
  workbook: null,
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
  dragDepth: 0
};

const baseUrl = new URL(import.meta.env.BASE_URL, window.location.origin);
let samples = [];

bindEvents();
setBusy(true, "Starting renderer");
void initialize();

async function initialize() {
  try {
    const [runtime, sampleManifest] = await Promise.all([
      import(/* @vite-ignore */ new URL("runtime/js/client.mjs", baseUrl).href),
      fetchJson(new URL("samples/manifest.json", baseUrl))
    ]);
    state.runtime = runtime;
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
  elements["zoom-out"].addEventListener("click", () => setZoom(state.zoom - ZOOM_STEP));
  elements["zoom-in"].addEventListener("click", () => setZoom(state.zoom + ZOOM_STEP));
  elements["fit-view"].addEventListener("click", fitToWidth);
  elements["export-svg"].addEventListener("click", exportSvg);
  elements["export-png"].addEventListener("click", () => void exportPng());
  elements["dismiss-error"].addEventListener("click", dismissError);
  elements["sidebar-toggle"].addEventListener("click", toggleSidebar);
  elements["sidebar-scrim"].addEventListener("click", closeSidebar);

  const viewport = elements["viewer-viewport"];
  viewport.addEventListener("dragenter", onDragEnter);
  viewport.addEventListener("dragover", onDragOver);
  viewport.addEventListener("dragleave", onDragLeave);
  viewport.addEventListener("drop", onDrop);
  window.addEventListener("keydown", onKeyDown);
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
  if (!acceptsWorkbook(file.name)) {
    showError(new Error("Choose an XLS, XLSX, XLSM, XLSB, or ODS file."));
    return;
  }
  if (file.size > MAX_INPUT_BYTES) {
    const error = new Error(`The browser viewer accepts files up to ${formatBytes(MAX_INPUT_BYTES)}.`);
    error.code = "limit_exceeded";
    showError(error);
    return;
  }
  const bytes = new Uint8Array(await file.arrayBuffer());
  await openWorkbook(bytes, {
    name: file.name,
    size: file.size,
    source: "Local file"
  });
}

async function loadSample(sample) {
  setBusy(true, `Opening ${sample.name}`);
  try {
    const response = await fetch(new URL(`samples/${sample.file}`, baseUrl));
    if (!response.ok) {
      throw new Error(`Sample request failed with HTTP ${response.status}.`);
    }
    const bytes = new Uint8Array(await response.arrayBuffer());
    await openWorkbook(bytes, {
      name: sample.name,
      size: bytes.byteLength,
      source: "Project sample"
    });
  } catch (error) {
    setBusy(false);
    showError(error);
  }
}

async function openWorkbook(bytes, file) {
  dismissError();
  setBusy(true, `Opening ${file.name}`);
  state.renderEpoch += 1;
  state.client?.terminate();
  const workerUrl = new URL("runtime/js/worker.mjs", baseUrl);
  state.client = new state.runtime.RenderWorkerClient(workerUrl);
  state.documentId = `viewer-${Date.now()}-${Math.random().toString(16).slice(2)}`;
  state.manifests.clear();
  state.pageIndex = 0;
  state.sheetIndex = 0;
  state.file = file;
  try {
    const opened = await state.client.open(bytes, { documentId: state.documentId });
    state.workbook = opened.workbook;
    updateWorkbookUi();
    await renderCurrent({ fit: true });
    closeSidebar();
  } catch (error) {
    state.client?.terminate();
    state.client = null;
    state.workbook = null;
    setBusy(false);
    showEmpty();
    showError(error);
  }
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
  const sheet = state.workbook.sheets[state.sheetIndex];
  setBusy(true, state.mode === "page" ? "Preparing pages" : `Rendering ${sheet.name}`);
  try {
    let rendered;
    if (state.mode === "page") {
      let manifest = state.manifests.get(state.sheetIndex);
      if (!manifest) {
        const prepared = await state.client.preparePages(state.documentId, state.sheetIndex);
        manifest = prepared.manifest;
        state.manifests.set(state.sheetIndex, manifest);
      }
      const pageCount = Math.max(1, manifest.pages.length);
      state.pageIndex = Math.min(state.pageIndex, pageCount - 1);
      rendered = await state.client.renderPage(
        state.documentId,
        state.sheetIndex,
        state.pageIndex
      );
    } else {
      rendered = await state.client.renderSheet(state.documentId, state.sheetIndex);
    }
    if (epoch !== state.renderEpoch) {
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
      state.mode === "page" ? `Page ${state.pageIndex + 1}` : "Full sheet";
  } catch (error) {
    if (epoch !== state.renderEpoch || error?.name === "AbortError") {
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
  elements["sample-select"].replaceChildren(
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
}

function showEmpty() {
  elements["document-stage"].hidden = true;
  elements["empty-state"].hidden = false;
  elements["document-name"].textContent = "No workbook open";
  elements["document-detail"].textContent = "Rust + WebAssembly";
}

function showError(error) {
  elements["error-message"].textContent = describeError(error);
  elements["error-banner"].hidden = false;
}

function dismissError() {
  elements["error-banner"].hidden = true;
}

function exportSvg() {
  if (!state.svgText) {
    return;
  }
  downloadBlob(
    new Blob([state.svgText], { type: "image/svg+xml;charset=utf-8" }),
    `${exportBaseName()}.svg`
  );
  elements["export-menu"].removeAttribute("open");
}

async function exportPng() {
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
      downloadBlob(png, `${exportBaseName()}.png`);
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
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "o") {
    event.preventDefault();
    chooseFile();
    return;
  }
  if ((event.ctrlKey || event.metaKey) && ["+", "="].includes(event.key)) {
    event.preventDefault();
    setZoom(state.zoom + ZOOM_STEP);
    return;
  }
  if ((event.ctrlKey || event.metaKey) && event.key === "-") {
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
    fileName: state.file?.name ?? null,
    source: state.file?.source ?? null,
    format: state.file ? extensionOf(state.file.name) : null,
    sheetCount: state.workbook?.sheetCount ?? 0,
    sheetIndex: state.sheetIndex,
    mode: state.mode,
    pageIndex: state.pageIndex,
    zoom: state.zoom,
    busy: state.busy,
    rendered: Boolean(state.svgElement)
  };
}

globalThis.__rxlsViewerState = viewerStateForTest;
