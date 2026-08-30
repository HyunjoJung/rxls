import { readdir, readFile, rm } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { runVSCodeCommand } from "@vscode/test-electron";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const manifest = JSON.parse(await readFile(path.join(root, "package.json"), "utf8"));
const vsix = path.join(root, "target", `${manifest.name}-${manifest.version}.vsix`);
const smoke = path.join(root, "target", "install-smoke");
const userData = path.join(smoke, "user");
const extensions = path.join(smoke, "extensions");

await rm(smoke, { recursive: true, force: true });
await runVSCodeCommand(
  [
    "--user-data-dir",
    userData,
    "--extensions-dir",
    extensions,
    "--install-extension",
    vsix,
    "--force"
  ],
  { version: process.env.RXLS_VSCODE_VERSION || "1.134.0" }
);

const installed = (await readdir(extensions, { withFileTypes: true })).filter((entry) =>
  entry.isDirectory()
);
const prefix = `${manifest.publisher}.${manifest.name}-`.toLowerCase();
const match = installed.find((entry) => entry.name.toLowerCase().startsWith(prefix));
if (!match) {
  throw new Error(`clean VSIX install did not create ${prefix}*`);
}
const installedManifest = JSON.parse(
  await readFile(path.join(extensions, match.name, "package.json"), "utf8")
);
if (
  installedManifest.name !== manifest.name ||
  installedManifest.publisher !== manifest.publisher ||
  installedManifest.version !== manifest.version
) {
  throw new Error("clean VSIX install produced the wrong extension identity");
}
process.stdout.write(`${match.name}\n`);
