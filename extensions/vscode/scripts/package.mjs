import { spawnSync } from "node:child_process";
import { copyFile, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const target = path.join(root, "target");
const manifest = JSON.parse(await readFile(path.join(root, "package.json"), "utf8"));
const baseName = `${manifest.name}-${manifest.version}`;
const candidates = [];

await rm(target, { recursive: true, force: true });
await mkdir(target, { recursive: true });

for (const index of [1, 2]) {
  const raw = path.join(target, `${baseName}-raw-${index}.vsix`);
  const normalized = path.join(target, `${baseName}-candidate-${index}.vsix`);
  run(vsceExecutable(), ["package", "--no-dependencies", "--out", raw], root);
  run(pythonExecutable(), [path.join(root, "scripts", "normalize_vsix.py"), raw, normalized], root);
  await rm(raw, { force: true });
  candidates.push(normalized);
}

const first = await readFile(candidates[0]);
const second = await readFile(candidates[1]);
const firstSha = sha256(first);
const secondSha = sha256(second);
if (firstSha !== secondSha || !first.equals(second)) {
  throw new Error(`VSIX build is not reproducible: ${firstSha} != ${secondSha}`);
}

const finalName = `${baseName}.vsix`;
const finalPath = path.join(target, finalName);
const checksumPath = `${finalPath}.sha256`;
await copyFile(candidates[0], finalPath);
await writeFile(checksumPath, `${firstSha}  ${finalName}\n`, "ascii");
for (const candidate of candidates) {
  await rm(candidate, { force: true });
}

run(
  pythonExecutable(),
  [
    path.join(root, "scripts", "verify_vsix.py"),
    finalPath,
    "--renderer-root",
    path.join(root, "node_modules", "@rxls", "render-worker"),
    "--checksum",
    checksumPath
  ],
  root
);
process.stdout.write(`${finalPath}\n${firstSha}\n`);

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function vsceExecutable() {
  return path.join(root, "node_modules", ".bin", process.platform === "win32" ? "vsce.cmd" : "vsce");
}

function pythonExecutable() {
  return process.env.PYTHON || (process.platform === "win32" ? "python" : "python3");
}

function run(command, args, cwd) {
  const result = spawnSync(command, args, {
    cwd,
    encoding: "utf8",
    stdio: "inherit",
    shell: process.platform === "win32" && command.toLowerCase().endsWith(".cmd")
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    throw new Error(`${command} exited with status ${result.status}`);
  }
}
