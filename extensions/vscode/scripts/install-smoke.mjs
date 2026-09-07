import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const result = spawnSync(process.execPath, [path.join(root, "scripts", "run-e2e.mjs"), "--installed"], {
  cwd: root,
  env: process.env,
  stdio: "inherit",
  windowsHide: true
});
if (result.error) {
  throw result.error;
}
if (result.status !== 0) {
  throw new Error(`installed VSIX E2E exited with status ${result.status}`);
}
