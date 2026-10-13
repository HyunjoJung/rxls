import { build } from "vite";
import { createHash, randomUUID } from "node:crypto";
import { copyFile, lstat, mkdir, readFile, readdir, realpath, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const viewer = fileURLToPath(new URL("..", import.meta.url));
const root = path.resolve(viewer, "..");
const scaffold = path.join(root, "packages/editor-embed");
const MAX_FILES = 256;
const MAX_ENTRIES = 512;
const MAX_BYTES = 64 * 1024 * 1024;
const MAX_FILE = 32 * 1024 * 1024;
const metadataReserve = 32 * 1024;
const argumentsList = process.argv.slice(2);
if (argumentsList.length && (argumentsList.length !== 2 || argumentsList[0] !== "--output-dir")) {
  throw new Error("Usage: node viewer/scripts/build-editor-embed.mjs [--output-dir target/fresh-directory]");
}
const output = path.resolve(root, argumentsList[1] ?? path.join("target/editor-embed", randomUUID()));
await validateOutput(output);
const metadata = JSON.parse(await readFile(path.join(scaffold, "package.json"), "utf8"));
if (!metadata.private || metadata.version !== "0.0.0-dev") throw new Error("Use the private development package identity.");
const parentProtocol = await readFile(path.join(scaffold, "client/protocol.mjs"));
const childProtocol = await readFile(path.join(viewer, "src/embed/protocol.mjs"));
if (!parentProtocol.equals(childProtocol)) throw new Error("Parent and editor protocol schemas differ.");

// Every build owns a fresh output tree; tracked package sources stay unchanged.
await mkdir(path.dirname(output), { recursive: true });
await validateOutput(output);
await mkdir(output); // Atomically claim the leaf before any Vite output is written.
const scratchUi = path.join(output, "ui");
const packageRoot = path.join(output, "package");
const assets = path.join(packageRoot, "assets");
await build({ configFile: false, root: viewer, base: "./", publicDir: false,
  plugins: [{ name: "rxls-empty-editor-embed", transformIndexHtml() {
    return [{ tag: "meta", attrs: { name: "rxls-host-kind", content: "embed" }, injectTo: "head" }];
  } }], build: { outDir: scratchUi, emptyOutDir: false, sourcemap: false, target: "es2022" } });

// Inventory all generated/runtime/scaffold inputs before copying any package file.
// One cumulative counter bounds nested directories, file count and copied bytes.
const plan = [];
let entries = 0;
let total = metadataReserve;
async function add(source, destination) {
  if (++entries > MAX_ENTRIES) throw new Error("Editor package entry limit exceeded.");
  const info = await lstat(source);
  if (!info.isFile() || info.isSymbolicLink()) throw new Error("Editor package inputs must be regular files.");
  if (info.size > MAX_FILE || plan.length + 1 >= MAX_FILES || total + info.size > MAX_BYTES) {
    throw new Error("Editor package file/byte limit exceeded.");
  }
  total += info.size;
  plan.push({ source, destination, bytes: info.size });
}
async function tree(source, destination) {
  if (++entries > MAX_ENTRIES) throw new Error("Editor package entry limit exceeded.");
  const info = await lstat(source);
  if (!info.isDirectory() || info.isSymbolicLink()) throw new Error("Editor package inputs must be ordinary directories.");
  for (const name of await readdir(source)) {
    const input = path.join(source, name);
    const child = await lstat(input);
    if (child.isDirectory() && !child.isSymbolicLink()) await tree(input, path.posix.join(destination, name));
    else await add(input, path.posix.join(destination, name));
  }
}
await tree(path.join(scratchUi, "assets"), "assets/assets");
await add(path.join(scratchUi, "index.html"), "assets/embed.html");
await tree(path.join(root, "bindings/render-wasm/js"), "assets/runtime/js");
await tree(path.join(root, "bindings/render-wasm/pkg"), "assets/runtime/pkg");
await tree(path.join(scaffold, "client"), "client");
for (const name of ["package.json", "README.md"]) await add(path.join(scaffold, name), name);
await add(path.join(root, "LICENSE"), "LICENSE");
await add(path.join(root, "LICENSE"), "assets/LICENSE.txt");
const noticeInputs = [path.join(viewer, "THIRD_PARTY_NOTICES.txt"), path.join(root, "bindings/render-wasm/THIRD_PARTY_NOTICES.txt")];
for (const input of noticeInputs) {
  const info = await lstat(input);
  if (!info.isFile() || info.isSymbolicLink() || info.size > 512 * 1024) throw new Error("Invalid editor legal notice input.");
}
const notices = ["RXLS EDITOR EMBED THIRD-PARTY NOTICES", "", "Viewer UI", "",
  await readFile(noticeInputs[0], "utf8"), "", "Renderer / WASM", "", await readFile(noticeInputs[1], "utf8")].join("\n");
const noticeBytes = Buffer.byteLength(notices);
if (plan.length + 3 > MAX_FILES || total + noticeBytes * 2 > MAX_BYTES) throw new Error("Editor package legal/metadata budget exceeded.");
total += noticeBytes * 2;

const files = [];
for (const item of plan) {
  const info = await lstat(item.source);
  if (!info.isFile() || info.isSymbolicLink() || info.size !== item.bytes) throw new Error("Editor package input changed after preflight.");
  const destination = path.join(packageRoot, item.destination);
  await mkdir(path.dirname(destination), { recursive: true });
  await copyFile(item.source, destination, 1); // COPYFILE_EXCL
  const data = await readFile(destination);
  if (data.length !== item.bytes) throw new Error("Editor package copy size changed after preflight.");
  if (item.destination.startsWith("assets/")) files.push(record(item.destination.slice(7), data));
}
for (const name of ["THIRD_PARTY_NOTICES.txt", "assets/THIRD_PARTY_NOTICES.txt"]) {
  await writeFile(path.join(packageRoot, name), notices, { flag: "wx" });
}
files.push(record("THIRD_PARTY_NOTICES.txt", Buffer.from(notices)));
files.sort((a, b) => a.path < b.path ? -1 : a.path > b.path ? 1 : 0);
const bundleId = createHash("sha256").update(JSON.stringify(files)).digest("hex");
const runtime = { bundleId, packageVersion: metadata.version, workerProtocol: "rxls.render-worker.v2" };
const manifest = JSON.stringify({ schemaVersion: 1, runtime, files }, null, 2) + "\n";
if (Buffer.byteLength(manifest) > metadataReserve) throw new Error("Editor asset manifest exceeds its reserved budget.");
await writeFile(path.join(assets, "embed-manifest.json"), manifest, { flag: "wx" });
await writeFile(path.join(output, "build-result.json"), JSON.stringify({ status: "built", packageRoot, assets,
  runtime, files, packageBytesUpperBound: total, node: process.version, verifiedConsumer: false }, null, 2) + "\n", { flag: "wx" });
console.log(JSON.stringify({ packageRoot, output, runtime, files: files.length }));

function record(relative, data) {
  return { path: relative.replaceAll(path.sep, "/"), bytes: data.length, sha256: createHash("sha256").update(data).digest("hex") };
}
async function validateOutput(candidate) {
  const relative = path.relative(root, candidate);
  const components = relative.split(path.sep);
  if (path.isAbsolute(relative) || components.includes("..") || components.length < 2 || !["target", "local"].includes(components[0])) {
    throw new Error("Build output must be a fresh directory below target/ or local/.");
  }
  const canonicalRoot = await realpath(root);
  let checked = root;
  for (const component of components.slice(0, -1)) {
    checked = path.join(checked, component);
    const info = await lstat(checked).catch((error) => { if (error.code === "ENOENT") return null; throw error; });
    if (!info) break;
    if (!info.isDirectory() || info.isSymbolicLink()) throw new Error("Build output ancestors must be ordinary directories.");
  }
  let ancestor = path.dirname(candidate);
  while (!(await lstat(ancestor).catch((error) => { if (error.code === "ENOENT") return null; throw error; }))) ancestor = path.dirname(ancestor);
  const canonicalAncestor = await realpath(ancestor);
  const canonicalRelative = path.relative(canonicalRoot, canonicalAncestor);
  if (path.isAbsolute(canonicalRelative) || canonicalRelative.split(path.sep).includes("..")) throw new Error("Build output escapes the repository.");
  if (await lstat(candidate).catch((error) => { if (error.code === "ENOENT") return null; throw error; })) throw new Error("Build output already exists; preserve it and choose a fresh directory.");
}
