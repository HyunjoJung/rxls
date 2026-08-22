import init, {
  extractText,
  maxInputBytes,
  reportJson,
  toCsv,
  toHtml,
} from "../web/rxls_wasm.js";

const fileInput = document.querySelector("#file");
const status = document.querySelector("#status");
const output = document.querySelector("#output");
const actions = {
  report: document.querySelector("#show-report"),
  text: document.querySelector("#show-text"),
  csv: document.querySelector("#show-csv"),
  html: document.querySelector("#show-html"),
};
let current = null;

function setActionsEnabled(enabled) {
  for (const button of Object.values(actions)) button.disabled = !enabled;
}

function show(kind) {
  if (!current) return;
  if (kind === "report") {
    output.textContent = JSON.stringify(JSON.parse(reportJson(current.bytes)), null, 2);
    status.textContent = `Showing report for ${current.name}.`;
  } else if (kind === "text") {
    output.textContent = extractText(current.bytes);
    status.textContent = `Exported text from ${current.name}.`;
  } else if (kind === "csv") {
    output.textContent = toCsv(current.bytes, 0);
    status.textContent = `Exported CSV from ${current.name}.`;
  } else {
    output.textContent = toHtml(current.bytes, 0);
    status.textContent = `Exported HTML from ${current.name}.`;
  }
}

await init();
status.textContent = `Ready. Maximum input: ${Math.floor(maxInputBytes() / 1048576)} MiB.`;

fileInput.addEventListener("change", async () => {
  const [file] = fileInput.files;
  if (!file) return;
  current = null;
  setActionsEnabled(false);
  output.textContent = "";
  if (file.size > maxInputBytes()) {
    status.textContent = `Rejected: ${file.name} exceeds the input limit.`;
    return;
  }
  try {
    const bytes = new Uint8Array(await file.arrayBuffer());
    const report = JSON.parse(reportJson(bytes));
    current = { bytes, name: file.name };
    output.textContent = JSON.stringify(report, null, 2);
    setActionsEnabled(true);
    status.textContent = `Parsed ${file.name}.`;
  } catch (error) {
    const details = error && typeof error === "object"
      ? `${error.kind ?? "unknown"} at ${error.location ?? "unknown"}: ${error.message}`
      : String(error);
    status.textContent = `Failed: ${details}`;
  }
});

for (const [kind, button] of Object.entries(actions)) {
  button.addEventListener("click", () => show(kind));
}
