import { execFileSync } from "node:child_process";
import { performance } from "node:perf_hooks";
import { fileURLToPath } from "node:url";
import { createGridEditor } from "../src/grid-editor.js";
import { setup, flush } from "../tests/support/grid-editor.mjs";

// Local diagnostic, not a wall-clock CI assertion. Worker/DOM rendering time is excluded.
const args = process.argv.slice(2);
if (
  args.length &&
  (args.length !== 2 ||
    args[0] !== "--baseline-ref" ||
    !/^[a-zA-Z0-9._/-]+$/.test(args[1]))
)
  throw new Error(
    "Usage: node scripts/benchmark-grid.mjs [--baseline-ref <git-ref>]",
  );
let createEditor = createGridEditor;
if (args.length) {
  const source = execFileSync(
    "git",
    ["show", `${args[1]}:viewer/src/grid-editor.js`],
    {
      cwd: fileURLToPath(new URL("../../", import.meta.url)),
      encoding: "utf8",
    },
  ).replace(
    '"./core.js"',
    JSON.stringify(new URL("../src/core.js", import.meta.url).href),
  );
  ({ createGridEditor: createEditor } = await import(
    `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`
  ));
}
const results = [];
const percentile = (values, proportion) =>
  [...values].sort((a, b) => a - b)[
    Math.min(values.length - 1, Math.floor(values.length * proportion))
  ];
for (const count of [1_000, 10_000, 100_000, 250_000]) {
  const cells = Array.from({ length: count }, (_, i) => [
    Math.floor(i / 100),
    i % 100,
    (i % 100) * 50,
    Math.floor(i / 100) * 20,
    50,
    20,
  ]);
  const env = setup({ createEditor });
  const start = performance.now();
  env.grid.mount(
    {
      schemaVersion: 1,
      width: 5000,
      height: Math.ceil(count / 100) * 20,
      cells,
    },
    env.svg,
  );
  const mountMs = performance.now() - start;
  await flush();
  await env.grid.select(Math.floor(count / 200), 50);
  const arrowMs = [];
  const tabMs = [];
  for (let i = 0; i < 70; i++) {
    let began = performance.now();
    await env.input.fire("keydown", { key: i % 2 ? "ArrowUp" : "ArrowDown" });
    if (i >= 10) arrowMs.push(performance.now() - began);
    began = performance.now();
    await env.input.fire("keydown", { key: "Tab", shiftKey: Boolean(i % 2) });
    if (i >= 10) tabMs.push(performance.now() - began);
  }
  results.push({
    cells: count,
    mountMs,
    arrowMedianMs: percentile(arrowMs, 0.5),
    arrowP95Ms: percentile(arrowMs, 0.95),
    tabMedianMs: percentile(tabMs, 0.5),
    tabP95Ms: percentile(tabMs, 0.95),
  });
}
console.log(
  JSON.stringify(
    {
      schemaVersion: 1,
      node: process.version,
      source: args[1] ?? "working-tree",
      samples: 60,
      results,
    },
    null,
    2,
  ),
);
