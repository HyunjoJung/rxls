import { spawnSync } from "node:child_process";

// Cold process startup is not a DevTools HTTP request. Keep its allowance
// independent from the shorter deadlines used after Chromium is running.
export const CHROMIUM_VERSION_TIMEOUT_MS = 10_000;
const MAX_DIAGNOSTIC_CHARACTERS = 1_024;

export function probeChromiumVersion(executable, chromium, { run = spawnSync } = {}) {
  const version = run(executable, ["--version"], {
    encoding: "utf8",
    timeout: CHROMIUM_VERSION_TIMEOUT_MS,
    maxBuffer: 16 * 1024,
    killSignal: "SIGKILL"
  });
  if (version.error || version.status !== 0 || version.signal != null) {
    throw new Error(
      `Chromium version probe failed (timeoutMs=${CHROMIUM_VERSION_TIMEOUT_MS}, ` +
      `status=${version.status ?? "null"}, signal=${diagnostic(version.signal ?? "none")}, ` +
      `error=${diagnostic(version.error?.code ?? version.error?.name ?? "none")}); ` +
      `stdout=${diagnostic(version.stdout ?? "")}; stderr=${diagnostic(version.stderr ?? "")}`
    );
  }
  const acceptedProducts = [chromium.product, chromium.testingProduct].filter(Boolean);
  const actualVersion = (version.stdout ?? "").trim();
  if (!acceptedProducts.some((product) => actualVersion === `${product} ${chromium.version}`)) {
    throw new Error(
      `expected ${acceptedProducts.map((product) => `${product} ${chromium.version}`).join(" or ")}; ` +
      `got ${diagnostic(actualVersion || "unavailable")}`
    );
  }
  return actualVersion;
}

function diagnostic(value) {
  const text = String(value);
  const excerpt = text.slice(0, MAX_DIAGNOSTIC_CHARACTERS);
  return JSON.stringify(excerpt + (text.length > excerpt.length ? " [truncated]" : ""));
}
