// Bounded W04 scrolling journey using the reviewed main diagnostics contract.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { readZipEntries } from "../scripts/zip.mjs";
import { assertViewerReopens } from "./reopen-journey.mjs";
import { recordJourney, sha256 } from "./journey-evidence.mjs";

const SVG_LIMIT = 2 * 1024 * 1024;

export async function installScrollingProbe(page) {
  await page.addInitScript(() => {
    const OriginalWorker = globalThis.Worker;
    const records = new Map(), workers = new Map(), replays = new WeakSet();
    let workerId = 0, peakInFlight = 0, overflow = false;
    let gate = null, gateTimer = null, held = null, gateGeneration = 0;
    const isTile = (r) => r.operation === "render-viewport-tile";
    const pending = (r) => r.deliveredAtMs === null && r.terminalAtMs === null;
    const inFlight = () => [...records.values()].filter((r) => isTile(r) && pending(r)).length;
    function clearGate() { clearTimeout(gateTimer); gateTimer = null; gate = null; }
    function retireWorker(id, reason, error) {
      const owner = workers.get(id);
      if (owner) { owner.terminal = reason; if (error) owner.error = String(error).slice(0, 1024); }
      for (const r of records.values()) {
        if (r.workerId !== id || !pending(r)) continue;
        r.terminalAtMs = performance.now(); r.terminal = reason;
        // An observed error is not a native completion. terminate() is recorded separately.
        if (error) r.workerError = String(error).slice(0, 1024);
      }
      if (held?.record.workerId === id) { held = null; clearGate(); }
    }
    function releaseHeld(reason) {
      const item = held;
      held = null; clearGate();
      if (!item) return false;
      item.record.gateRelease = reason;
      if (!pending(item.record) || workers.get(item.record.workerId)?.terminated) return false;
      const event = new MessageEvent("message", { data: item.data, origin: item.origin,
        lastEventId: item.lastEventId, ports: item.ports });
      replays.add(event);
      // Replay the identical actual native result on its original live Worker.
      item.worker.dispatchEvent(event);
      return true;
    }
    globalThis.Worker = class extends OriginalWorker {
      constructor(...arguments_) {
        super(...arguments_);
        const id = ++workerId;
        if (workers.size >= 32) {
          this.terminate();
          throw new Error("Owned scrolling probe worker limit exceeded.");
        }
        workers.set(id, { id, terminated: false, terminal: null, error: null });
        this.addEventListener("message", (event) => {
          const data = event.data;
          if (data?.protocol !== "rxls.render-worker.v2" || data.type !== "result") return;
          const record = records.get(`${id}:${data.requestId}`);
          if (!record || !pending(record)) return;
          if (!replays.has(event)) {
            if (record.finishedAtMs !== null) return;
            record.finishedAtMs = performance.now();
            record.ok = data.ok;
            if (!data.ok) {
              const e = data.error;
              record.error = { code: e.code, resource: e.resource, actual: e.actual,
                limit: e.limit, message: String(e.message).slice(0, 1024) };
            } else if (record.operation === "prepare-viewport") {
              const d = data.result, r = d.preparationReport;
              record.descriptor = { geometryId: d.geometryId, revision: d.revision,
                widthRaw: d.widthRaw, heightRaw: d.heightRaw, sourceRange: d.sourceRange,
                sheetVisibility: d.sheetVisibility, resources: d.resources,
                preparationReport: r && { coordinateVisits: r.coordinateVisits,
                  sourceRawCells: r.sourceRawCells, sourceHyperlinks: r.sourceHyperlinks,
                  sourceIndexBuildPeakBytes: r.sourceIndexBuildPeakBytes,
                  geometryBytes: r.geometryBytes, textBytes: r.textBytes,
                  shapedGlyphs: r.shapedGlyphs, textWork: r.textWork, shapedRuns: r.shapedRuns,
                  textLines: r.textLines, pathCommands: r.pathCommands,
                  conditionalEvaluations: r.conditionalEvaluations, fontPackSha256: r.fontPackSha256,
                  fontFaceCount: r.fontFaces.length, warningCount: r.warnings.length,
                  fontFaces: r.fontFaces.slice(0, 16), warnings: r.warnings.slice(0, 16) } };
            } else if (isTile(record)) {
              const t = data.result;
              record.tile = { geometryId: t.geometryId, revision: t.revision,
                namespace: t.namespace, logicalRect: t.logicalRect,
                sceneNodes: t.report?.scene_nodes ?? null,
                svgBytes: t.report?.svg_bytes ?? null, metrics: t.metrics };
            }
            if (gate && !held && isTile(record) && data.ok &&
                record.documentId === gate.documentId && record.geometryId === gate.geometryId) {
              const bytes = data.result?.report?.svg_bytes;
              if (!Number.isSafeInteger(bytes) || bytes <= 0 || bytes > 2 * 1024 * 1024 ||
                  typeof data.result.svg !== "string" || data.result.svg.length > 2 * 1024 * 1024)
                throw new Error("Actual held tile exceeded its existing SVG limit.");
              record.gateGeneration = gate.generation;
              record.heldAtMs = performance.now();
              held = { worker: this, record, data, origin: event.origin,
                lastEventId: event.lastEventId, ports: event.ports };
              event.stopImmediatePropagation();
              return;
            }
          }
          record.deliveredAtMs = performance.now(); record.terminal = "result";
        }, { capture: true });
        for (const type of ["error", "messageerror"]) this.addEventListener(type, (event) => {
          retireWorker(id, type, event.message || type);
        }, { capture: true });
        const originalTerminate = this.terminate.bind(this);
        this.terminate = () => {
          const owner = workers.get(id);
          owner.terminateRequestedAtMs = performance.now();
          try {
            originalTerminate();
            owner.terminated = true; owner.terminatedAtMs = performance.now();
            retireWorker(id, "terminated");
          } catch (error) {
            owner.terminationError = String(error).slice(0, 1024);
            throw error;
          }
        };
        const originalPost = this.postMessage.bind(this);
        this.postMessage = (data, ...rest) => {
          if (data?.protocol === "rxls.render-worker.v2" && data.type === "request") {
            const key = `${id}:${data.requestId}`;
            if (records.size >= 512 || records.has(key)) {
              overflow = true;
              throw new Error("Owned scrolling probe request identity/record limit exceeded.");
            }
            const p = data.payload;
            const record = { workerId: id, requestId: data.requestId, operation: data.operation,
              documentId: p.documentId, sheetIndex: p.sheetIndex,
              geometryId: p.geometryId ?? null, revision: p.revision ?? null,
              namespace: p.namespace ?? null, rect: p.rect ?? null,
              startedAtMs: performance.now(), finishedAtMs: null, deliveredAtMs: null,
              terminalAtMs: null, terminal: null };
            records.set(key, record);
            if (isTile(record)) peakInFlight = Math.max(peakInFlight, inFlight());
            try { return originalPost(data, ...rest); }
            catch (error) {
              record.terminalAtMs = performance.now(); record.terminal = "post-error";
              record.workerError = String(error).slice(0, 1024);
              throw error;
            }
          }
          return originalPost(data, ...rest);
        };
      }
    };
    globalThis.__rxlsScrollingGate = {
      arm({ documentId, geometryId, timeoutMs = 10_000 }) {
        if (gate || held) throw new Error("Only one owned response gate is allowed.");
        if (typeof documentId !== "string" || typeof geometryId !== "string" ||
            documentId.length < 1 || geometryId.length < 1 || documentId.length > 64 || geometryId.length > 60 ||
            !Number.isSafeInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 10_000)
          throw new Error("Invalid owned response gate identity/deadline.");
        if (!Number.isSafeInteger(gateGeneration + 1)) throw new Error("Owned response gate counter exhausted.");
        gate = { documentId, geometryId, generation: ++gateGeneration };
        gateTimer = setTimeout(() => releaseHeld("deadline"), timeoutMs);
        return gate.generation;
      },
      release: () => releaseHeld("explicit"),
      restore: () => releaseHeld("finally"),
    };
    globalThis.__rxlsScrollingProbe = () => ({ overflow, inFlight: inFlight(), peakInFlight,
      trackedScope: "all posted v2 requests; payload/result bodies omitted; only tile delivery gated",
      nativePending: [...records.values()].filter((r) => isTile(r) && r.finishedAtMs === null &&
        r.terminal !== "post-error" && !workers.get(r.workerId)?.terminated).length,
      nativeUnresolvedAfterError: [...records.values()].filter((r) => isTile(r) &&
        r.finishedAtMs === null && ["error", "messageerror"].includes(r.terminal) &&
        !workers.get(r.workerId)?.terminated).length,
      pendingRequests: [...records.values()].filter(pending).length,
      gate: gate && { ...gate, held: Boolean(held), requestId: held?.record.requestId ?? null,
        workerId: held?.record.workerId ?? null },
      workers: [...workers.values()].map((owner) => ({ ...owner })),
      records: [...records.values()].map((record) => ({ ...record })) });
  });
}

async function sourceInputs() {
  const directory = process.env.RXLS_W04_FIXTURE_DIRECTORY;
  const manifestPath = process.env.RXLS_W04_FIXTURE_MANIFEST;
  assert.ok(directory && manifestPath, "pin W04 fixture directory and manifest explicitly");
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  assert.equal(manifest.schema, "rxls.W04.browser-fixtures.v1");
  assert.equal(manifest.files.length, 6);
  const files = new Map();
  for (const item of manifest.files) {
    assert.match(item.path, /^w04-[a-z-]+\.(?:xlsx|xlsm|ods)$/);
    const path = resolve(directory, item.path);
    const bytes = await readFile(path);
    assert.equal(bytes.length, item.bytes);
    assert.equal(sha256(bytes), item.sha256, `original ${item.path}`);
    assert.ok(bytes.length <= 32 * 1024 * 1024);
    files.set(item.case, { ...item, path, bytes });
  }
  return files;
}

async function diagnostic(page) {
  return page.evaluate(() => {
    const s = globalThis.__rxlsViewerState();
    const p = document.querySelector("#viewer-viewport");
    const surface = document.querySelector("#document-surface");
    const tile = surface.querySelector(".viewport-tile");
    const rect = (e) => {
      if (!e) return null;
      const b = e.getBoundingClientRect();
      return { left: b.left, top: b.top, right: b.right, bottom: b.bottom,
        width: b.width, height: b.height };
    };
    const box = p.getBoundingClientRect();
    const left = box.left + p.clientLeft, top = box.top + p.clientTop;
    return { state: s, viewport: { left, top, right: left + p.clientWidth,
      bottom: top + p.clientHeight }, surface: rect(surface), tile: rect(tile),
      attachedTiles: surface.querySelectorAll(".viewport-tile").length,
      domNodes: tile?.querySelectorAll("*").length ?? 0,
      jsUsedHeap: performance.memory?.usedJSHeapSize ?? null };
  });
}

function contains(outer, inner) {
  return outer && inner && outer.xRaw <= inner.xRaw && outer.yRaw <= inner.yRaw &&
    outer.xRaw + outer.widthRaw >= inner.xRaw + inner.widthRaw &&
    outer.yRaw + outer.heightRaw >= inner.yRaw + inner.heightRaw;
}

function assertCoverage(snapshot) {
  const { state: s, viewport: box, surface, tile } = snapshot;
  const v = s.viewport;
  assert.equal(v.schemaVersion, 1, "approved diagnostics must be installed");
  assert.equal(s.displayKind, "tiled");
  assert.equal(s.rendered, true);
  assert.equal(v.ready, true);
  assert.ok(contains(v.coverage.attached, v.coverage.visible));
  assert.equal(snapshot.attachedTiles, 1);
  // Independent DOM coverage, including surface/scrollport padding and borders.
  const visible = { left: Math.max(box.left, surface.left), top: Math.max(box.top, surface.top),
    right: Math.min(box.right, surface.right), bottom: Math.min(box.bottom, surface.bottom) };
  assert.ok(tile && tile.left <= visible.left + .5 && tile.top <= visible.top + .5 &&
    tile.right >= visible.right - .5 && tile.bottom >= visible.bottom - .5,
  "mounted tile covers actual scrollport/surface intersection");
  assert.ok(Math.abs(surface.width - v.widthRaw / 1024 * s.zoom) <= .5);
  assert.ok(Math.abs(surface.height - v.heightRaw / 1024 * s.zoom) <= .5);
  assert.ok(surface.width <= 8_000_000 && surface.height <= 8_000_000);
  assert.ok(v.peakActive <= 1 && v.peakDesired <= 1);
  assert.ok(v.cache.chargedRecords <= 8 && v.cache.peakRecords <= 8);
  assert.ok(v.cache.chargedBytes <= 32 * 1024 * 1024 && v.cache.peakBytes <= 32 * 1024 * 1024);
  assert.ok(Number.isSafeInteger(v.tile.sceneNodes) && v.tile.sceneNodes >= 0 && v.tile.sceneNodes <= 100_000);
  assert.ok(Number.isSafeInteger(v.tile.svgBytes) && v.tile.svgBytes > 0 && v.tile.svgBytes <= SVG_LIMIT);
  return v.coverage;
}

function viewportMemoryProof(snapshot, probe) {
  assertCoverage(snapshot);
  const v = snapshot.state.viewport, c = v.coverage;
  const record = probe.records.findLast((r) => r.operation === "render-viewport-tile" && r.ok === true &&
    r.deliveredAtMs !== null && r.terminal === "result" && r.documentId === c.documentId &&
    r.sheetIndex === c.sheetIndex && r.geometryId === c.geometryId && r.revision === c.revision && r.namespace === c.namespace);
  assert.ok(record, "memory sample requires an actual delivered native tile matching current coverage identity");
  const owners = probe.workers.filter((worker) => !worker.terminated);
  assert.equal(owners.length, 1, "one exact live observed worker owner");
  assert.equal(owners[0].id, record.workerId);
  assert.equal(probe.pendingRequests, 0); assert.equal(probe.inFlight, 0); assert.equal(probe.nativePending, 0);
  assert.equal(c.geometryId, v.geometryId); assert.equal(c.revision, v.revision);
  assert.equal(c.namespace, v.tile.namespace);
  assert.ok(record.tile && contains(record.tile.logicalRect, c.visible));
  assert.ok(["xRaw", "yRaw", "widthRaw", "heightRaw"].every((key) => record.tile.logicalRect[key] === c.attached[key]));
  assert.equal(record.tile.svgBytes, v.tile.svgBytes); assert.equal(record.tile.sceneNodes, v.tile.sceneNodes);
  return Object.freeze({ initialized: true, idle: true, currentCoverage: true,
    documentId: c.documentId, sheetIndex: c.sheetIndex, geometryId: c.geometryId,
    revision: c.revision, namespace: c.namespace, workerId: record.workerId });
}

async function sampleViewportMemory(page, helpers, phase) {
  let observation;
  if (typeof helpers.sampleWasmMemory !== "function") {
    observation = { schemaVersion: 1, phase, available: false, value: null,
      errors: [{ type: "Unavailable", message: "optional existing-worker sampler not provided" }] };
  } else {
    try {
      const probe = await page.evaluate(() => globalThis.__rxlsScrollingProbe());
      const proof = viewportMemoryProof(await diagnostic(page), probe);
      observation = await helpers.sampleWasmMemory(page, phase, proof);
      assert.ok(observation && typeof observation.available === "boolean" && observation.phase === phase);
      assert.ok(JSON.stringify(observation).length <= 8192, "bounded scalar memory receipt");
    } catch (error) {
      observation = { schemaVersion: 1, phase, available: false, value: null,
        errors: [{ type: error?.name ?? "Error", message: String(error?.message ?? error).slice(0, 1024) }] };
    }
  }
  // Separate from accepted-to-covered timestamps and primary sample classes.
  await recordJourney(`W04 WASM memory ${phase}`, observation);
  return observation;
}

async function readyTiled(page, helpers, sheetIndex, label) {
  await helpers.waitForViewerState(page, (s) => !s.busy && s.sheetIndex === sheetIndex &&
    s.displayKind === "tiled" && s.rendered, label);
  const snapshot = await diagnostic(page);
  assertCoverage(snapshot);
  return snapshot;
}

async function openFixture(page, file, helpers) {
  await page.locator("#file-input").setInputFiles(file.path);
  await helpers.waitForViewerState(page, (s) => s.fileName === file.path.split(/[\\/]/).at(-1) &&
    s.sheetIndex === 0 && s.rendered && !s.busy && !s.dirty, file.case);
}

async function selectSheet(page, index) {
  await page.locator(`#sheet-list button[data-index="${index}"]`).click();
}

async function editSmall(page, helpers) {
  await page.locator("#edit-cell").click();
  await page.locator("#cell-reference").fill("A2");
  await page.locator("#read-cell").click();
  await helpers.waitForCondition(async () => !(await page.locator("#apply-cell-edit").isDisabled()) &&
    (await page.locator("#cell-current-value").textContent()).startsWith("A2:"), "Small A2 loaded");
  assert.equal(await page.locator("#cell-kind").inputValue(), "number");
  await page.locator("#cell-value").fill("9");
  await page.locator("#apply-cell-edit").click();
  await page.locator("#cell-dialog").waitFor({ state: "hidden" });
  const s = await helpers.waitForViewerState(page, (s) => s.dirty && !s.busy && s.rendered, "Small dirty");
  assert.equal(s.canUndo, true);
  return s;
}

async function typedAdmission(page, expected, sheetIndex) {
  const probe = await page.evaluate(() => globalThis.__rxlsScrollingProbe());
  assert.equal(probe.overflow, false);
  const prepare = probe.records.findLast((r) => r.operation === "prepare-viewport" &&
    r.sheetIndex === sheetIndex && r.ok);
  assert.ok(prepare, "real successful preparation observed");
  const full = probe.records.findLast((r) => ["render-sheet", "render-sheet-interactive"].includes(r.operation) &&
    r.documentId === prepare.documentId && r.sheetIndex === sheetIndex && r.startedAtMs < prepare.startedAtMs && !r.ok);
  assert.ok(full, "typed full failure precedes preparation");
  for (const [key, value] of Object.entries(expected)) assert.equal(full.error[key], value, key);
  assert.ok(prepare.descriptor.resources.geometryReservationBytes <= 8 * 1024 * 1024);
  assert.ok(prepare.descriptor.resources.sourceIndexReservationBytes <= 8 * 1024 * 1024);
  assert.ok(prepare.descriptor.preparationReport.sourceRawCells <= 250_000);
  return { full, prepare };
}

async function scrollSample(page, fraction) {
  // Timestamp scope comes from accepted/mounted main scalars, not Node polling.
  const sample = await page.evaluate(async (fraction) => {
    const initial = globalThis.__rxlsViewerState().viewport;
    const before = initial.coverage.intent;
    const viewport = document.querySelector("#viewer-viewport");
    viewport.scrollTop = Math.floor((viewport.scrollHeight - viewport.clientHeight) * fraction);
    viewport.dispatchEvent(new Event("scroll"));
    const deadline = performance.now() + 30_000;
    while (performance.now() < deadline) {
      const s = globalThis.__rxlsViewerState();
      const c = s.viewport?.coverage;
      if (s.rendered && s.viewport.geometryId === initial.geometryId && s.viewport.revision === initial.revision &&
          c?.intent > before && c.coveredAtMs !== null) {
        return { geometryId: s.viewport.geometryId, revision: s.viewport.revision,
          ...c, ms: c.coveredAtMs - c.acceptedAtMs };
      }
      await new Promise(requestAnimationFrame);
    }
    throw new Error("Current accepted viewport did not mount within timeout.");
  }, fraction);
  assert.ok(Number.isFinite(sample.ms) && sample.ms >= 0);
  assertCoverage(await diagnostic(page));
  return sample;
}

async function exactZoom(page, value, helpers) {
  const result = await page.evaluate((value) => {
    if (typeof globalThis.__rxlsViewerSetZoomForTest !== "function") throw new Error("Exact zoom route pending.");
    return globalThis.__rxlsViewerSetZoomForTest(value);
  }, value);
  assert.equal(result.accepted, true);
  assert.equal(result.reason, "accepted");
  await helpers.waitForViewerState(page, (s) => s.rendered && s.zoom === value, `exact zoom ${value}`);
  assertCoverage(await diagnostic(page));
}

function primaryWarmSchedule() {
  return [
    ...Array.from({ length: 40 }, (_, i) => ({ lane: "distinct", fraction: .05 + i * .02 })),
    ...Array.from({ length: 20 }, (_, i) => ({ lane: "recent", fraction: i % 2 ? .83 : .81 })),
  ];
}

function manualRowTop(row, total) {
  let pixels = 0;
  for (let r = 1; r < row; r += 1) {
    if (total >= 4000 && ((r >= 2048 && r <= 2063) || (r >= 4090 && r <= 4094))) continue;
    pixels += [24, 32, 40][Math.floor((r - 1) / 8) % 3];
  }
  return pixels * 1024;
}

function manualColumnTop(column, total, physical) {
  let raw = 0;
  for (let c = 1; c < column; c += 1) {
    if (total >= 64 && c >= 18 && c <= 21) continue;
    // Pinned no-pack geometry: max digit7 + imported padding2. ODS declares points.
    const pixels = physical ? [88, 60, 72][c % 3] * 4 / 3 :
      Math.floor((([8.25, 12.5, 16.75][(c - 1) % 3] * 256 + Math.floor(128 / 7)) / 256) * 7) + 2;
    raw += Math.round(pixels * 1024);
  }
  return raw;
}

function intersectBoxes(...boxes) {
  if (boxes.some((b) => !b || ![b.left, b.top, b.right, b.bottom].every(Number.isFinite))) return null;
  const left = Math.max(...boxes.map((b) => b.left)), top = Math.max(...boxes.map((b) => b.top));
  const right = Math.min(...boxes.map((b) => b.right)), bottom = Math.min(...boxes.map((b) => b.bottom));
  return right > left && bottom > top ? { left, top, right, bottom, width: right - left, height: bottom - top } : null;
}

function sourceTextVisibility({ text }) {
  const svg = document.querySelector(".viewport-svg"), tile = document.querySelector(".viewport-tile");
  const p = document.querySelector("#viewer-viewport");
  if (!svg || !tile || !p) return null;
  const candidates = [...svg.querySelectorAll("text")].filter((node) => node.textContent.includes(text));
  const b = p.getBoundingClientRect();
  const scrollport = { left: b.left + p.clientLeft, top: b.top + p.clientTop,
    right: b.left + p.clientLeft + p.clientWidth, bottom: b.top + p.clientTop + p.clientHeight };
  const rect = (node) => {
    const r = node.getBoundingClientRect();
    return { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
  };
  const intersect = (...boxes) => {
    if (boxes.some((box) => ![box.left, box.top, box.right, box.bottom].every(Number.isFinite))) return null;
    const left = Math.max(...boxes.map((box) => box.left)), top = Math.max(...boxes.map((box) => box.top));
    const right = Math.min(...boxes.map((box) => box.right)), bottom = Math.min(...boxes.map((box) => box.bottom));
    return right > left && bottom > top ? { left, top, right, bottom, width: right - left, height: bottom - top } : null;
  };
  for (const node of candidates) {
    const bbox = node.getBBox();
    if (![bbox.x, bbox.y, bbox.width, bbox.height].every(Number.isFinite) || bbox.width <= 0 || bbox.height <= 0) continue;
    const clips = [];
    let ancestor = node, depth = 0;
    while (ancestor && ancestor !== svg) {
      if (++depth > 16) throw new Error("Unexpected generated SVG clip depth.");
      const value = ancestor.getAttribute("clip-path");
      if (value) {
        const id = /^url\(#([^\s)]+)\)$/.exec(value)?.[1];
        const clip = id && document.getElementById(id);
        const clipRect = clip?.querySelector("rect");
        if (!clipRect || !svg.contains(clip) || (clip.getAttribute("clipPathUnits") &&
            clip.getAttribute("clipPathUnits") !== "userSpaceOnUse"))
          throw new Error("Expected actual renderer user-space rectangular text clip.");
        const geometry = clipRect.getBBox(), matrix = clipRect.getScreenCTM() ?? svg.getScreenCTM();
        if (!matrix) throw new Error("Actual SVG clip has no screen transform.");
        const corners = [[geometry.x, geometry.y], [geometry.x + geometry.width, geometry.y],
          [geometry.x, geometry.y + geometry.height], [geometry.x + geometry.width, geometry.y + geometry.height]]
          .map(([x, y]) => new DOMPoint(x, y).matrixTransform(matrix));
        clips.push({ left: Math.min(...corners.map((point) => point.x)),
          top: Math.min(...corners.map((point) => point.y)), right: Math.max(...corners.map((point) => point.x)),
          bottom: Math.max(...corners.map((point) => point.y)) });
      }
      ancestor = ancestor.parentElement;
    }
    if (!clips.length) throw new Error("Actual fixture text is missing its source clip.");
    const textBox = rect(node), tileBox = rect(tile), surfaceBox = rect(document.querySelector("#document-surface"));
    const visible = intersect(textBox, scrollport, tileBox, surfaceBox, ...clips);
    if (visible) return { visible, textBox, scrollport, tileBox, surfaceBox, sourceClips: clips,
      localBBox: { x: bbox.x, y: bbox.y, width: bbox.width, height: bbox.height } };
  }
  return null;
}

async function inspectSourceRegions(page, sheet, helpers) {
  await exactZoom(page, 1, helpers);
  const initial = await diagnostic(page);
  const physical = initial.state.format === "ods";
  assert.equal(initial.state.viewport.preparationReport.fontPackSha256, null, "fixture anchor policy pins no font pack");
  assert.equal(initial.state.viewport.heightRaw, manualRowTop(sheet.rows + 1, sheet.rows),
    "manual heights and hidden bands retain complete global source units");
  const widthRaw = manualColumnTop(sheet.columns + 1, sheet.columns, physical);
  assert.equal(initial.state.viewport.widthRaw, widthRaw, "declared varying widths and hidden columns retain global units");
  const points = [
    { row: 40, column: 4, endRow: 48, endColumn: 24, text: "W04 MERGE D40:X48" },
    { row: 3200, column: 1, endRow: 3202, endColumn: sheet.columns, text: "W04 MERGE A3200:BL3202" },
    { row: Math.min(sheet.rows - 1, 5800), column: Math.min(sheet.columns, 42), text: "W04 DISTANT TEXT" },
    { row: sheet.rows, column: sheet.columns, text: `W04 FAR EDGE ${sheet.rows}/${sheet.columns}` },
  ];
  const regions = [];
  for (const point of points) {
    const start = manualColumnTop(point.column, sheet.columns, physical);
    const end = manualColumnTop((point.endColumn ?? point.column) + 1, sheet.columns, physical);
    const leftRaw = sheet.rtl ? widthRaw - end : start;
    const rightRaw = sheet.rtl ? widthRaw - start : end;
    const topRaw = manualRowTop(point.row, sheet.rows), bottomRaw = manualRowTop((point.endRow ?? point.row) + 1, sheet.rows);
    // Put the real source text anchor near the viewport center, rather than
    // choosing an arbitrary fraction of an unrelated global width. These
    // Latin markers use physical Start alignment even on the RTL sheet.
    await page.evaluate(({ xRaw, yRaw }) => {
      const s = globalThis.__rxlsViewerState(), p = document.querySelector("#viewer-viewport");
      const surface = document.querySelector("#document-surface").getBoundingClientRect(), box = p.getBoundingClientRect();
      p.scrollLeft += surface.left + xRaw / 1024 * s.zoom - (box.left + p.clientLeft + p.clientWidth / 2);
      p.scrollTop += surface.top + yRaw / 1024 * s.zoom - (box.top + p.clientTop + p.clientHeight / 2);
      p.dispatchEvent(new Event("scroll"));
    }, { xRaw: leftRaw + 8 * 1024, yRaw: bottomRaw - 8 * 1024 });
    let observed;
    await helpers.waitForCondition(async () => {
      const state = await page.evaluate(() => globalThis.__rxlsViewerState());
      if (!state.rendered || state.viewport?.geometryId !== initial.state.viewport.geometryId) return false;
      observed = await page.evaluate(sourceTextVisibility, { text: point.text });
      return Boolean(observed);
    }, `actual clipped visible bbox ${point.text}`);
    assertCoverage(await diagnostic(page));
    assert.ok(intersectBoxes(observed.textBox, observed.scrollport, observed.tileBox, ...observed.sourceClips),
      "actual text bbox intersects scrollport, mounted tile and every actual text clip");
    if (point.endColumn) {
      const expected = { left: observed.surfaceBox.left + leftRaw / 1024,
        top: observed.surfaceBox.top + topRaw / 1024,
        right: observed.surfaceBox.left + rightRaw / 1024,
        bottom: observed.surfaceBox.top + bottomRaw / 1024 };
      assert.ok(observed.sourceClips.some((clip) => Object.keys(expected).every((key) =>
        Math.abs(clip[key] - expected[key]) <= .5)), "actual complete crossing-merge clip retains its global source envelope");
    }
    regions.push({ ...point, expectedSourceEnvelope: { leftRaw, topRaw, rightRaw, bottomRaw }, ...observed });
  }
  const cdp = await page.context().newCDPSession(page);
  let physicalFonts;
  try {
    await cdp.send("DOM.enable");
    await cdp.send("CSS.enable");
    const { root } = await cdp.send("DOM.getDocument", { depth: 1 });
    const { nodeId } = await cdp.send("DOM.querySelector", { nodeId: root.nodeId, selector: ".viewport-svg text" });
    assert.ok(nodeId, "representative actual rendered text exists");
    physicalFonts = (await cdp.send("CSS.getPlatformFontsForNode", { nodeId })).fonts;
    assert.ok(physicalFonts.length <= 32);
  } catch (error) {
    physicalFonts = { unavailable: String(error).slice(0, 1024) };
  } finally {
    await cdp.detach();
  }
  await recordJourney(`W04 source regions ${sheet.name}`, { status: "passed", rtl: sheet.rtl,
    heightRaw: initial.state.viewport.heightRaw, widthRaw, regions, physicalFonts,
    physicalFontFileMapping: "candidate Arial file hashes in PIN-SET; actual face mapping explicit at run" });
}

async function saveDirtyLane(page, file, helpers, benchmark) {
  await openFixture(page, file, helpers);
  const edited = await editSmall(page, helpers);
  await selectSheet(page, 1);
  const first = await readyTiled(page, helpers, 1, `${file.case} LargeRows`);
  const admission = await typedAdmission(page, file.sheets[1].expected_full_rejection, 1);
  if (benchmark) await sampleViewportMemory(page, helpers, "primary-first-coverage");
  assert.equal(first.state.dirty, true);
  await page.getByRole("tab", { name: "Home", exact: true }).click();
  assert.equal(await page.locator("#editing-hint").isVisible(), false, "read-only tiled hint hidden");
  assert.equal(await page.locator("#edit-cell").isDisabled(), true);
  assert.equal(await page.locator("#quick-save").isDisabled(), false);
  assert.equal(await page.locator("#export-svg").isDisabled(), true);
  assert.equal(await page.locator("#export-png").isDisabled(), true);
  const saved = await helpers.downloadWorkbook(page, file.path.endsWith("xlsm") ? ".xlsm" : ".xlsx");
  const savedParts = readZipEntries(saved.bytes), originalParts = readZipEntries(file.bytes);
  for (const name of file.preserved_parts) assert.deepEqual(savedParts.get(name), originalParts.get(name), name);
  await helpers.assertOpenpyxlReopens(saved.path, { cell: "A2", expected: 9,
    cacheCell: "B2", expectedCache: 18, requireVba: file.path.endsWith("xlsm") });
  await assertViewerReopens(page, saved, { format: file.path.endsWith("xlsm") ? "xlsm" : "xlsx",
    label: `W04 ${file.case} saved Small reopen`, cells: {
      A2: { kind: "number", value: "9" },
      B2: { kind: "formula", formula: "A2*2", cachedKind: "number", cachedValue: "18" },
    } }, helpers);
  assert.equal((await diagnostic(page)).state.dirty, true, "save-copy does not clear dirty source");
  const samples = [];
  if (benchmark) {
    const v = first.state.viewport;
    assert.ok(v.preparation.finishedAtMs - v.preparation.startedAtMs <= 5000);
    assert.ok(v.firstCoverage.coveredAtMs - v.firstCoverage.acceptedAtMs <= 2000);
    for (const step of primaryWarmSchedule()) {
      samples.push({ lane: step.lane, ...await scrollSample(page, step.fraction) });
    }
    const sorted = samples.map((s) => s.ms).sort((a, b) => a - b);
    const p95 = sorted[Math.ceil(.95 * sorted.length) - 1], maximum = sorted.at(-1);
    assert.ok(p95 <= 250 && maximum <= 1000);
    assert.ok(samples.some((s) => s.cacheKind === "rendered"));
    assert.ok(samples.some((s) => s.cacheKind === "cached"));
    assert.ok(samples.filter((s) => s.lane === "distinct").every((s) => s.cacheKind === "rendered"));
    assert.ok(samples.filter((s) => s.lane === "recent").every((s) => s.cacheKind === "cached"));
    await sampleViewportMemory(page, helpers, "primary-post-60");
    await recordJourney("W04 primary warm scrolling", { status: "passed", samples,
      p95, maximum, byClass: Object.fromEntries(["rendered", "cached", "covered"].map((kind) =>
        [kind, samples.filter((s) => s.cacheKind === kind).map((s) => s.ms)])),
      preparationMs: v.preparation.finishedAtMs - v.preparation.startedAtMs,
      firstCoverageMs: v.firstCoverage.coveredAtMs - v.firstCoverage.acceptedAtMs,
      admission, memory: { jsUsedHeap: first.jsUsedHeap,
        wasmLinearHighWater: v.wasmLinearHighWater ?? null,
        ownedChromeWorkingSet: "outer native receipt; pending actual run" } });
  }
  await inspectSourceRegions(page, file.sheets[1], helpers);
  for (const zoom of [.25, 1, 2, 3]) await exactZoom(page, zoom, helpers);
  await page.locator("#zoom-out").click();
  await helpers.waitForViewerState(page, (s) => s.rendered && s.zoom < 3, "real minus");
  await page.locator("#zoom-in").click();
  await helpers.waitForViewerState(page, (s) => s.rendered && s.zoom === 3, "real plus");
  await page.getByRole("tab", { name: "View", exact: true }).click();
  await page.locator("#fit-view").click();
  await helpers.waitForViewerState(page, (s) => s.rendered && s.zoom >= .25 && s.zoom <= 3, "real fit");
  assertCoverage(await diagnostic(page));
  await page.locator("#reset-zoom").click();
  await helpers.waitForViewerState(page, (s) => s.rendered && s.zoom === 1, "real reset");
  await selectSheet(page, 2);
  await readyTiled(page, helpers, 2, "LargeCells");
  await typedAdmission(page, file.sheets[2].expected_full_rejection, 2);
  await inspectSourceRegions(page, file.sheets[2], helpers);
  await selectSheet(page, 0);
  const back = await helpers.waitForViewerState(page, (s) => s.sheetIndex === 0 && s.rendered && !s.busy, "Small restored");
  assert.equal(back.displayKind, "full");
  await page.getByRole("tab", { name: "Home", exact: true }).click();
  assert.equal(await page.locator("#editing-hint").isVisible(), true, "editable Small hint restored");
  assert.equal(back.dirty, true);
  assert.equal(back.canUndo, true);
  assert.deepEqual(back.editedParts, edited.editedParts);
  assert.equal(await page.locator("#edit-cell").isDisabled(), false);
  await page.keyboard.press("Control+z");
  await helpers.waitForViewerState(page, (s) => !s.busy && !s.dirty && s.canRedo, "original Small undo");
  await recordJourney(`W04 ${file.case} dirty tiled save`, { status: "passed",
    originalSha256: file.sha256, savedSha256: sha256(saved.bytes), preservedParts: file.preserved_parts });
}

async function boundedOwnedAction(action, milliseconds) {
  let timer;
  try {
    return await Promise.race([Promise.resolve().then(action).then(() => ({ status: "completed" }),
      (error) => ({ status: "failed", error: String(error).slice(0, 1024) })),
    new Promise((resolve) => { timer = setTimeout(() => resolve({ status: "timed-out" }), milliseconds); })]);
  } finally { clearTimeout(timer); }
}

async function triggerUncachedHeldTile(page, fraction, helpers) {
  const initial = await diagnostic(page);
  assertCoverage(initial);
  const v = initial.state.viewport;
  const gateGeneration = await page.evaluate(({ documentId, geometryId }) =>
    globalThis.__rxlsScrollingGate.arm({ documentId, geometryId }),
  { documentId: v.coverage.documentId, geometryId: v.geometryId });
  await page.evaluate((fraction) => {
    const p = document.querySelector("#viewer-viewport");
    p.scrollTop = Math.floor((p.scrollHeight - p.clientHeight) * fraction);
    p.dispatchEvent(new Event("scroll"));
  }, fraction);
  await helpers.waitForCondition(async () => {
    const probe = await page.evaluate(() => globalThis.__rxlsScrollingProbe());
    return probe.gate?.generation === gateGeneration && probe.gate.held;
  }, "actual native tile arrived and only its client delivery is held");
  const probe = await page.evaluate(() => globalThis.__rxlsScrollingProbe());
  const held = probe.records.find((r) => r.workerId === probe.gate.workerId && r.requestId === probe.gate.requestId);
  assert.ok(held?.ok && held.finishedAtMs !== null && held.deliveredAtMs === null && held.terminalAtMs === null);
  assert.equal(probe.inFlight, 1, "one original client promise is pending");
  assert.equal(probe.nativePending, 0, "held delivery is not labelled running native CPU");
  const state = await page.evaluate(() => globalThis.__rxlsViewerState());
  assert.equal(state.viewport.active, 1);
  assert.equal(state.viewport.cache.incomingBytes, 6 * 1024 * 1024);
  assert.equal(state.rendered, false, "old mounted SVG cannot make an uncovered current view ready");
  return { initial, held, gateGeneration };
}

async function exercisePendingCorrectness(sourcePage, files, helpers) {
  let page = null, primary = null, restore = { status: "not-created" }, close = { status: "not-created" };
  const observations = [];
  try {
    page = await sourcePage.context().browser().newPage({ viewport: sourcePage.viewportSize() });
    await installScrollingProbe(page);
    const errors = [];
    page.on("pageerror", (error) => { if (errors.length < 16) errors.push(String(error).slice(0, 1024)); });
    await page.goto(sourcePage.url(), { waitUntil: "domcontentloaded" });
    await helpers.waitForViewerState(page, (s) => s.rendered && !s.busy, "pending lane initial source");
    await openFixture(page, files.get("primary"), helpers);
    await selectSheet(page, 1);
    await readyTiled(page, helpers, 1, "pending lane initial LargeRows");
    await exactZoom(page, 1, helpers);
    const rapid = await triggerUncachedHeldTile(page, .13, helpers);
    const fractions = [.22, .34, .48, .62, .77];
    let desired;
    for (const fraction of fractions) {
      await page.evaluate(async (fraction) => {
        const p = document.querySelector("#viewer-viewport");
        p.scrollTop = Math.floor((p.scrollHeight - p.clientHeight) * fraction);
        p.dispatchEvent(new Event("scroll"));
        await new Promise(requestAnimationFrame); await new Promise(requestAnimationFrame);
      }, fraction);
      const probe = await page.evaluate(() => globalThis.__rxlsScrollingProbe());
      assert.equal(probe.gate?.generation, rapid.gateGeneration, "bounded gate has not expired");
      assert.equal(probe.gate.held, true);
      assert.equal(probe.inFlight, 1); assert.equal(probe.nativePending, 0);
      const posted = probe.records.filter((r) => r.operation === "render-viewport-tile" &&
        r.geometryId === rapid.held.geometryId && r.startedAtMs >= rapid.held.startedAtMs);
      assert.equal(posted.length, 1, "rapid intent changes do not post a native tile queue");
      const s = await page.evaluate(() => globalThis.__rxlsViewerState());
      assert.equal(s.viewport.active, 1); assert.equal(s.viewport.desired, 1);
      assert.equal(s.rendered, false);
      desired = s.viewport.coverage;
    }
    assert.equal(await page.evaluate(() => globalThis.__rxlsScrollingGate.release()), true);
    const final = await readyTiled(page, helpers, 1, "latest rapid cover after actual result release");
    assert.equal(final.state.viewport.coverage.intent, desired.intent);
    assert.deepEqual(final.state.viewport.coverage.visible, desired.visible);
    assert.notEqual(final.state.viewport.tile.namespace, rapid.held.namespace, "superseded native tile was not adopted");
    const completed = await page.evaluate(() => globalThis.__rxlsScrollingProbe());
    const rapidPosts = completed.records.filter((r) => r.operation === "render-viewport-tile" &&
      r.geometryId === rapid.held.geometryId && r.startedAtMs >= rapid.held.startedAtMs);
    assert.equal(rapidPosts.length, 2, "one held result then exactly one latest render");
    assert.equal(rapidPosts[1].namespace, final.state.viewport.tile.namespace);
    assert.ok(contains(rapidPosts[1].tile.logicalRect, desired.visible));
    observations.push({ lane: "rapid-latest", fractions, held: rapid.held, delivered: rapidPosts,
      latestCoverage: final.state.viewport.coverage });

    // Fresh geometry clears cache so the next distant request genuinely reaches native WASM.
    await selectSheet(page, 0);
    await helpers.waitForViewerState(page, (s) => s.sheetIndex === 0 && s.rendered && !s.busy, "Small before stale sheet");
    await selectSheet(page, 1);
    await readyTiled(page, helpers, 1, "fresh LargeRows before stale sheet");
    await exactZoom(page, 1, helpers);
    const staleSheet = await triggerUncachedHeldTile(page, .27, helpers);
    await selectSheet(page, 0);
    await helpers.waitForViewerState(page, (s) => s.sheetIndex === 0 && s.rendered && !s.busy && s.displayKind === "full",
      "Small while the actual old tile result remains undelivered");
    assert.equal(await page.locator(".viewport-tile").count(), 0);
    assert.equal((await page.evaluate(() => globalThis.__rxlsScrollingProbe())).inFlight, 1);
    await page.evaluate(() => {
      let addedTiles = 0;
      const observer = new MutationObserver((records) => {
        for (const record of records) for (const node of record.addedNodes) {
          if (node.nodeType === 1 && (node.matches(".viewport-tile") || node.querySelector(".viewport-tile"))) addedTiles += 1;
        }
      });
      observer.observe(document.querySelector("#document-surface"), { childList: true, subtree: true });
      globalThis.__rxlsStaleTileWatch = { inspect: () => addedTiles, stop: () => observer.disconnect() };
    });
    assert.equal(await page.evaluate(() => globalThis.__rxlsScrollingGate.release()), true);
    await helpers.waitForCondition(async () => (await page.evaluate(() => globalThis.__rxlsScrollingProbe())).inFlight === 0,
      "old original promise settled on actual delivery");
    await page.evaluate(async () => { await new Promise(requestAnimationFrame); await new Promise(requestAnimationFrame); });
    const sheetState = await page.evaluate(() => globalThis.__rxlsViewerState());
    assert.equal(sheetState.sheetIndex, 0); assert.equal(sheetState.displayKind, "full"); assert.equal(sheetState.rendered, true);
    assert.equal(await page.locator(".viewport-tile").count(), 0);
    assert.equal(await page.evaluate(() => globalThis.__rxlsStaleTileWatch.inspect()), 0, "no stale tile inserted even transiently");
    await page.evaluate(() => globalThis.__rxlsStaleTileWatch.stop());
    observations.push({ lane: "stale-sheet", held: staleSheet.held, currentSheet: sheetState.sheetIndex, staleTileInsertions: 0 });

    await selectSheet(page, 1);
    await readyTiled(page, helpers, 1, "fresh LargeRows before stale file");
    await exactZoom(page, 1, helpers);
    const staleFile = await triggerUncachedHeldTile(page, .41, helpers);
    await openFixture(page, files.get("rtl"), helpers);
    const fileState = await page.evaluate(() => globalThis.__rxlsViewerState());
    assert.equal(fileState.displayKind, "full"); assert.equal(fileState.sheetIndex, 0);
    assert.equal(await page.locator(".viewport-tile").count(), 0);
    const retired = await page.evaluate(() => globalThis.__rxlsScrollingProbe());
    const old = retired.records.find((r) => r.workerId === staleFile.held.workerId && r.requestId === staleFile.held.requestId);
    assert.equal(old.terminal, "terminated"); assert.equal(old.deliveredAtMs, null);
    assert.equal(old.ok, true); assert.ok(old.finishedAtMs !== null, "retain actual native result independently of retirement");
    assert.equal(retired.workers.find((w) => w.id === old.workerId).terminated, true);
    assert.equal(retired.inFlight, 0); assert.equal(retired.nativePending, 0);
    assert.equal(retired.gate, null);
    assert.equal(await page.evaluate(() => globalThis.__rxlsScrollingGate.release()), false, "terminated owner's held result is not replayed");
    assert.deepEqual(errors, []);
    assert.equal(retired.overflow, false); assert.equal(retired.pendingRequests, 0);
    assert.ok(retired.peakInFlight <= 1);
    observations.push({ lane: "stale-file", nativeResultThenRetirement: old, currentFile: fileState.fileName,
      oldOwner: retired.workers.find((w) => w.id === old.workerId) });
    if (typeof helpers.sampleWasmMemory === "function") {
      // The old owner's memory is retired, not sampled through a replacement.
      // This phase explicitly identifies the newly opened RTL source owner.
      await selectSheet(page, 1);
      await readyTiled(page, helpers, 1, "current new-source coverage for post-correctness memory");
    }
    await sampleViewportMemory(page, helpers, "post-pending-correctness");

  } catch (error) {
    primary = error;
  } finally {
    if (page) {
      restore = await boundedOwnedAction(() => page.evaluate(() => {
        globalThis.__rxlsStaleTileWatch?.stop(); globalThis.__rxlsScrollingGate?.restore();
      }), 2_000);
      close = await boundedOwnedAction(() => page.close(), 5_000);
    }
  }
  const cleanupFailed = [restore, close].some((r) => ["failed", "timed-out"].includes(r.status));
  let receiptError = null;
  try {
    await recordJourney("W04 pending-result correctness", { status: primary || cleanupFailed ? "failed" : "passed",
      timingExcluded: true, gateMeaning: "actual native result arrived; only original client delivery is held",
      originalSha256: files.get("primary").sha256, newFileSha256: files.get("rtl").sha256,
      observations, primaryError: primary ? String(primary).slice(0, 2048) : null, cleanup: { restore, close } });
  } catch (error) { receiptError = error; }
  if (primary) throw primary;
  assert.equal(cleanupFailed, false, "owned pending-lane page/gate cleanup completed");
  if (receiptError) throw receiptError;
}

export async function exerciseScrolling(page, helpers) {
  const files = await sourceInputs();
  await saveDirtyLane(page, files.get("primary"), helpers, true);
  await exercisePendingCorrectness(page, files, helpers);
  await saveDirtyLane(page, files.get("preservation"), helpers, false);
  for (const key of ["rtl", "readonly"]) {
    const file = files.get(key);
    await openFixture(page, file, helpers);
    for (const index of [1, 2]) {
      await selectSheet(page, index);
      const s = await readyTiled(page, helpers, index, `${key} ${index}`);
      await typedAdmission(page, file.sheets[index].expected_full_rejection, index);
      await inspectSourceRegions(page, file.sheets[index], helpers);
      if (key === "readonly") {
        assert.equal(s.state.editCapability, "read-only");
        assert.equal(await page.locator("#save-document").isDisabled(), true);
      }
    }
    await recordJourney(`W04 ${key}`, { status: "passed", originalSha256: file.sha256 });
  }
  await openFixture(page, files.get("below-limits"), helpers);
  for (const index of [1, 2]) {
    await selectSheet(page, index);
    await helpers.waitForViewerState(page, (s) => !s.busy && s.sheetIndex === index && s.rendered, `boundary ${index}`);
    const probe = await page.evaluate(() => globalThis.__rxlsScrollingProbe());
    const latest = probe.records.findLast((r) => ["render-sheet", "render-sheet-interactive"].includes(r.operation) && r.sheetIndex === index);
    assert.ok(latest);
    assert.ok(latest.ok || (latest.error.code === "limit_exceeded" &&
      !["rows", "cells"].includes(latest.error.resource)), "do not invent a row/cell boundary failure");
    await recordJourney(`W04 full boundary ${index}`, { status: "passed", actualFullResult: latest });
  }
  await openFixture(page, files.get("css-surface"), helpers);
  await selectSheet(page, 1);
  const tall = await readyTiled(page, helpers, 1, "TallCSS initial capped fit");
  await exactZoom(page, 1, helpers);
  const before = await diagnostic(page);
  const rejection = await page.evaluate(() => globalThis.__rxlsViewerSetZoomForTest(3));
  assert.equal(rejection.accepted, false);
  assert.equal(rejection.reason, "surface-limit");
  const after = await diagnostic(page);
  assert.equal(after.state.zoom, 1);
  assert.equal(after.state.viewport.geometryId, before.state.viewport.geometryId);
  assertCoverage(after);
  assert.match(await page.locator("#error-message").textContent(), /surface limit/i);
  await recordJourney("W04 tall CSS rejection", { status: "passed", initialZoom: tall.state.zoom,
    retainedZoom: after.state.zoom, heightRaw: after.state.viewport.heightRaw });
  const probe = await page.evaluate(() => globalThis.__rxlsScrollingProbe());
  assert.equal(probe.overflow, false);
  assert.ok(probe.peakInFlight <= 1, "no queued posted tile requests");
  assert.equal(probe.inFlight, 0);
  assert.equal(probe.nativePending, 0);
  assert.equal(probe.pendingRequests, 0);
  assert.equal(probe.gate, null);
  await recordJourney("W04 scrolling worker trace", { status: "passed", ...probe });
}
