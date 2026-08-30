import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  copyFile,
  cp,
  mkdir,
  readFile,
  readdir,
  rm,
  stat,
  writeFile
} from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const extensionRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = path.resolve(extensionRoot, "../..");
const viewerRoot = path.join(repositoryRoot, "viewer");
const viewerDist = path.join(viewerRoot, "dist");
const stage = path.join(extensionRoot, "media", "viewer");
const renderPackage = path.join(extensionRoot, "node_modules", "@rxls", "render-worker");
const npm = "npm";

run(npm, ["--prefix", viewerRoot, "run", "build"], {
  ...process.env,
  RXLS_BASE_PATH: "./",
  RXLS_SOURCEMAP: "0"
});

await assertDirectory(viewerDist, "viewer build output");
await assertDirectory(renderPackage, "installed @rxls/render-worker package");
await safeRemove(stage);
await mkdir(path.dirname(stage), { recursive: true });
await cp(viewerDist, stage, { recursive: true, force: true, dereference: true });

await safeRemove(path.join(stage, "runtime"));
await safeRemove(path.join(stage, "samples"));
await removeSourceMaps(stage);

const runtime = path.join(stage, "runtime");
await mkdir(runtime, { recursive: true });
for (const relative of [
  "js",
  "pkg",
  "LICENSE",
  "README.md",
  "THIRD_PARTY_NOTICES.txt",
  "package.json"
]) {
  const source = path.join(renderPackage, relative);
  const destination = path.join(runtime, relative);
  const metadata = await stat(source);
  if (metadata.isDirectory()) {
    await cp(source, destination, { recursive: true, force: true, dereference: true });
  } else {
    await mkdir(path.dirname(destination), { recursive: true });
    await copyFile(source, destination);
  }
}

const workerBundle = path.join(runtime, "vscode-worker.js");
await build({
  absWorkingDir: extensionRoot,
  entryPoints: [path.join(extensionRoot, "scripts", "vscode-worker-entry.mjs")],
  outfile: workerBundle,
  bundle: true,
  charset: "utf8",
  format: "iife",
  legalComments: "none",
  minify: true,
  platform: "browser",
  sourcemap: false,
  target: ["chrome128"]
});

await copyFile(path.join(repositoryRoot, "LICENSE"), path.join(stage, "LICENSE.txt"));
const viewerNotices = await readFile(path.join(viewerRoot, "THIRD_PARTY_NOTICES.txt"), "utf8");
await writeFile(
  path.join(stage, "THIRD_PARTY_NOTICES.txt"),
  `${viewerNotices.trimEnd()}\n\nRenderer notices are packaged separately at runtime/THIRD_PARTY_NOTICES.txt.\n`,
  "utf8"
);

const extensionLock = JSON.parse(
  await readFile(path.join(extensionRoot, "package-lock.json"), "utf8")
);
const lockedRenderer = extensionLock.packages?.["node_modules/@rxls/render-worker"];
const lockedBundler = extensionLock.packages?.["node_modules/esbuild"];
const rendererManifest = JSON.parse(
  await readFile(path.join(renderPackage, "package.json"), "utf8")
);
if (
  rendererManifest.name !== "@rxls/render-worker" ||
  rendererManifest.version !== "0.2.0" ||
  lockedRenderer?.version !== rendererManifest.version ||
    typeof lockedRenderer?.integrity !== "string"
  ) {
  throw new Error("installed renderer does not match the locked 0.2.0 package");
}
if (lockedBundler?.version !== "0.28.2" || typeof lockedBundler.integrity !== "string") {
  throw new Error("installed worker bundler does not match the locked esbuild 0.28.2 package");
}
const workerBundleBytes = await readFile(workerBundle);
const viewerManifest = JSON.parse(await readFile(path.join(viewerRoot, "package.json"), "utf8"));
await writeFile(
  path.join(stage, "build-manifest.json"),
  `${JSON.stringify(
    {
      schema: "rxls.vscode-viewer.v1",
      renderer: {
        name: rendererManifest.name,
        version: rendererManifest.version,
        integrity: lockedRenderer.integrity
      },
      viewer: {
        name: viewerManifest.name,
        version: viewerManifest.version
      },
      workerBundle: {
        path: "runtime/vscode-worker.js",
        format: "classic-single-file",
        sha256: createHash("sha256").update(workerBundleBytes).digest("hex"),
        bundler: {
          name: "esbuild",
          version: lockedBundler.version,
          integrity: lockedBundler.integrity
        }
      }
    },
    null,
    2
  )}\n`,
  "utf8"
);

await assertFile(path.join(stage, "index.html"), "staged viewer index");
await assertFile(
  path.join(runtime, "pkg", "rxls_render_wasm_bg.wasm"),
  "staged renderer WASM"
);
await assertFile(workerBundle, "staged single-file VS Code worker");

function run(command, args, env) {
  const result = spawnSync(command, args, {
    cwd: repositoryRoot,
    env,
    encoding: "utf8",
    stdio: "inherit",
    shell: process.platform === "win32"
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    throw new Error(`${command} exited with status ${result.status}`);
  }
}

async function safeRemove(target) {
  const resolved = path.resolve(target);
  if (!resolved.startsWith(`${extensionRoot}${path.sep}`)) {
    throw new Error(`refusing unsafe stage removal: ${resolved}`);
  }
  await rm(resolved, { recursive: true, force: true });
}

async function removeSourceMaps(directory) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const target = path.join(directory, entry.name);
    if (entry.isDirectory()) {
      await removeSourceMaps(target);
    } else if (entry.isSymbolicLink()) {
      throw new Error(`staged viewer contains a symlink: ${target}`);
    } else if (entry.name.endsWith(".map")) {
      await rm(target);
    }
  }
}

async function assertDirectory(target, label) {
  const metadata = await stat(target).catch(() => undefined);
  if (!metadata?.isDirectory()) {
    throw new Error(`${label} is missing: ${target}`);
  }
}

async function assertFile(target, label) {
  const metadata = await stat(target).catch(() => undefined);
  if (!metadata?.isFile()) {
    throw new Error(`${label} is missing: ${target}`);
  }
}
