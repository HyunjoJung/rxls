import { mkdir, readFile, rm } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const lock = JSON.parse(await readFile(new URL("../toolchain-lock.json", import.meta.url), "utf8"));
const cargoPath = rustupBinary("cargo", lock.rust);
const rustcPath = rustupBinary("rustc", lock.rust);
const buildEnvironment = {
  ...process.env,
  CARGO: cargoPath,
  RUSTC: rustcPath,
  RUSTUP_TOOLCHAIN: lock.rust
};
const probe = spawnSync("wasm-pack", ["--version"], { encoding: "utf8" });
if (probe.error?.code === "ENOENT") {
  const bindgen = spawnSync("wasm-bindgen", ["--version"], { encoding: "utf8" });
  if (
    bindgen.status !== 0 ||
    bindgen.stdout.trim() !== `wasm-bindgen ${lock.wasmBindgen.version}`
  ) {
    console.error(
      `expected wasm-pack ${lock.wasmPack.version} or wasm-bindgen ${lock.wasmBindgen.version}`
    );
    process.exit(2);
  }
  const cargo = spawnSync(
    cargoPath,
    ["build", "--target", "wasm32-unknown-unknown", "--release", "--locked"],
    { cwd: root, stdio: "inherit", env: buildEnvironment }
  );
  if (cargo.status !== 0) {
    process.exit(cargo.status ?? 1);
  }
  await rm(new URL("../pkg", import.meta.url), { recursive: true, force: true });
  await mkdir(new URL("../pkg", import.meta.url), { recursive: true });
  const generated = spawnSync(
    "wasm-bindgen",
    [
      "--target",
      "web",
      "--out-dir",
      "pkg",
      "--out-name",
      "rxls_render_wasm",
      "target/wasm32-unknown-unknown/release/rxls_render_wasm.wasm"
    ],
    { cwd: root, stdio: "inherit" }
  );
  process.exit(generated.status ?? 1);
}
if (probe.status !== 0 || probe.stdout.trim() !== `wasm-pack ${lock.wasmPack.version}`) {
  console.error(`expected wasm-pack ${lock.wasmPack.version}; got ${probe.stdout.trim() || "unknown"}`);
  process.exit(2);
}
await rm(new URL("../pkg", import.meta.url), { recursive: true, force: true });
const build = spawnSync(
  "wasm-pack",
  ["build", "--target", "web", "--release", "--out-dir", "pkg", "--locked"],
  { cwd: root, stdio: "inherit", env: buildEnvironment }
);
process.exit(build.status ?? 1);

function rustupBinary(name, toolchain) {
  const located = spawnSync(
    "rustup",
    ["which", name, "--toolchain", toolchain],
    { encoding: "utf8" }
  );
  if (located.status !== 0 || located.stdout.trim() === "") {
    console.error(`Rust ${toolchain} ${name} is unavailable`);
    process.exit(2);
  }
  return located.stdout.trim();
}
