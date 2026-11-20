import { createHash } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const evidenceDirectory = process.env.RXLS_VIEWER_EVIDENCE_DIR
  ? resolve(process.env.RXLS_VIEWER_EVIDENCE_DIR)
  : fileURLToPath(new URL("../../target/viewer-e2e/", import.meta.url));

const observations = [];
let downloadIndex = 0;
export const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

export async function recordJourney(name, details) {
  observations.push({ name, ...details });
  await mkdir(evidenceDirectory, { recursive: true });
  await writeFile(
    resolve(evidenceDirectory, "journeys.json"),
    `${JSON.stringify({ schema: "rxls.viewer-journeys.v1", observations }, null, 2)}\n`,
  );
}

export async function retainDownload(download, fileName) {
  const directory = resolve(evidenceDirectory, "downloads", String(++downloadIndex));
  await mkdir(directory, { recursive: true });
  // A suggested filename is untrusted; only retain its last path component.
  const name = fileName.split(/[\\/]/).at(-1);
  if (!name || name === "." || name === "..") throw new Error("invalid download name");
  const path = resolve(directory, name);
  await download.saveAs(path);
  return path;
}
