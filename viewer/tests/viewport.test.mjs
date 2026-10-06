import test from "node:test";
import assert from "node:assert/strict";
import {
  VIEWPORT_POLICY, viewportContains, viewportSurfaceSize, viewportCover,
  viewportFallbackError, createViewportTileCache, createViewportScheduler,
} from "../src/viewport.js";

const tick = () => new Promise((resolve) => setImmediate(resolve));
const rect = (x = 0, width = 1024) => ({ xRaw: x, yRaw: 0, widthRaw: width, heightRaw: 1024 });
const cover = (x = 0) => ({ rect: rect(x), visible: rect(x, 512) });
function fixture() {
  const calls = [], adopted = [], errors = [], states = [], timers = new Map();
  let timerId = 0;
  const scheduler = createViewportScheduler({
    render(input) { return new Promise((resolve, reject) => calls.push({ input, resolve, reject })); },
    adopt(tile, context) { adopted.push({ tile, context }); },
    onError(error) { errors.push(error); }, onState(state) { states.push(state); },
    setTimer(fn, ms) { assert.equal(ms, 30000); timers.set(++timerId, fn); return timerId; },
    clearTimer(id) { timers.delete(id); },
  });
  const succeed = (index) => calls[index].resolve({ logicalRect: { ...calls[index].input.rect }, svg: `<svg id="n${calls[index].input.namespace}"/>` });
  return { scheduler, calls, adopted, errors, states, timers, succeed };
}

test("fractional zoom and actual surface bounds cover both visible axes", () => {
  for (const zoom of [0.25, 0.7, 1, 2, 3]) {
    const result = viewportCover({ widthRaw: 1_000_000_000, heightRaw: 1_000_000_000, zoom,
      viewport: { left: 25.25, top: 35.75, right: 901.5, bottom: 701.25 },
      surface: { left: -302.125, top: -405.875, right: 10000, bottom: 10000 } });
    assert.equal(viewportContains(result.rect, result.visible), true);
    assert.equal(result.visible.xRaw, Math.floor((25.25 + 302.125) * 1024 / zoom));
    assert.equal(result.visible.yRaw, Math.floor((35.75 + 405.875) * 1024 / zoom));
    assert.equal(result.visible.xRaw + result.visible.widthRaw, Math.ceil((901.5 + 302.125) * 1024 / zoom));
    assert.equal(result.visible.yRaw + result.visible.heightRaw, Math.ceil((701.25 + 405.875) * 1024 / zoom));
  }
});

test("large raw coordinates retain precision without 32-bit truncation", () => {
  const result = viewportCover({ widthRaw: 6_000_000_000, heightRaw: 4096, zoom: 1,
    viewport: { left: 0, top: 0, right: 500, bottom: 2 },
    surface: { left: -5_000_000_000 / 1024, top: 0, right: 0, bottom: 4 } });
  assert.equal(result.visible.xRaw, 5_000_000_000);
  assert.equal(viewportContains(result.rect, result.visible), true);
});

test("overscan/quantization yield to an exactly capped visible rectangle", () => {
  const options = { widthRaw: 100_000_000, heightRaw: 4096, zoom: 1,
    viewport: { left: 0.125, top: 0, right: 8192.125, bottom: 2 },
    surface: { left: -300, top: 0, right: 0, bottom: 4 } };
  const result = viewportCover(options);
  assert.equal(result.rect.widthRaw, VIEWPORT_POLICY.maxTilePixels * 1024);
  assert.equal(viewportContains(result.rect, result.visible), true);
  assert.throws(() => viewportCover({ ...options, viewport: { ...options.viewport, right: 8193.125 } }), /tile dimension/);
});

test("empty/outside sheets remain empty and zoom rejects CSS clamping", () => {
  const options = { widthRaw: 0, heightRaw: 1024, zoom: 1,
    viewport: { left: 0, top: 0, right: 100, bottom: 100 },
    surface: { left: 0, top: 0, right: 0, bottom: 1 } };
  assert.equal(viewportCover(options), null);
  assert.equal(viewportCover({ ...options, widthRaw: 1024, surface: { ...options.surface, left: -200 } }), null);
  assert.deepEqual(viewportSurfaceSize(16_000_000 * 1024, 1024, 0.25), { width: 4_000_000, height: 0.25 });
  assert.throws(() => viewportSurfaceSize(16_000_000 * 1024, 1024, 1), /surface limit/);
  assert.throws(() => viewportSurfaceSize(1024, 1024, NaN), /zoom/);
});

test("collapsed fractional DOM bounds cannot create a one-unit ready tile", () => {
  for (const viewport of [
    { left: 0.1, right: 0.1, top: 0.1, bottom: 2.1 },
    { left: 0.1, right: 2.1, top: 0.1, bottom: 0.1 },
  ]) {
    assert.equal(viewportCover({ widthRaw: 4096, heightRaw: 4096, zoom: 1,
      viewport, surface: { left: 0, top: 0, right: 4, bottom: 4 } }), null);
  }
});

test("only actual full-sheet admission limits enable the fallback", () => {
  const error = { code: "limit_exceeded", resource: "rows", limit: 4096, actual: 6000 };
  assert.equal(viewportFallbackError(error), true);
  for (const change of [{ code: "parse_failed" }, { resource: "maxRows" }, { resource: "fontBytes" },
    { resource: "text_bytes" }, { actual: 4096 }, { actual: "6000" }, { limit: null }]) {
    assert.equal(viewportFallbackError({ ...error, ...change }), false);
  }
});

test("cache charges UTF8 plus retained JS text and protects mounted text", () => {
  const cache = createViewportTileCache();
  const unicode = "한😀";
  const first = cache.insert("first", { svg: unicode, logicalRect: rect() });
  assert.equal(first.charge, 7 + 2 * 3);
  cache.attach("first");
  for (let n = 0; n < 20; n++) cache.insert(String(n), { svg: "<svg/>", logicalRect: rect() });
  assert.equal(cache.inspect().records, 8);
  assert.equal(cache.get("first"), first);
  assert.throws(() => cache.remove("first"), /attached/);
  assert.throws(() => cache.insert("first", { svg: "<svg/>" }), /Duplicate/);
  assert.throws(() => cache.insert("oversized", { svg: "a".repeat(2 * 1024 * 1024 + 1) }), /byte limit/);
  cache.clear();
  const svg = "a".repeat(2 * 1024 * 1024);
  cache.insert("mounted", { svg, logicalRect: rect() }); cache.attach("mounted");
  for (let n = 0; n < 10; n++) cache.insert(`large-${n}`, { svg, logicalRect: rect() });
  assert.equal(cache.inspect().records, 5);
  assert.equal(cache.inspect().bytes, 30 * 1024 * 1024);
  assert.equal(cache.inspect().peakBytes <= VIEWPORT_POLICY.maxCacheBytes, true);
  assert.equal(cache.get("mounted") !== null, true);
});

test("incoming text has a full slot/6MiB reservation before CPU dispatch", async () => {
  const cache = createViewportTileCache();
  const svg = "a".repeat(2 * 1024 * 1024);
  for (let n = 0; n < 5; n++) cache.insert(String(n), { svg, logicalRect: rect() });
  cache.attach("0"); cache.reserveIncoming();
  assert.equal(cache.inspect().records, 4);
  assert.equal(cache.inspect().incomingBytes, 6 * 1024 * 1024);
  assert.equal(cache.inspect().chargedBytes, 30 * 1024 * 1024);
  assert.equal(cache.get("0") !== null, true);
  cache.clear(); assert.equal(cache.inspect().incomingBytes, 6 * 1024 * 1024);
  cache.releaseIncoming(); assert.equal(cache.inspect().chargedBytes, 0);
  const f = fixture(); f.scheduler.reset({ documentId: "a" });
  f.scheduler.request(cover());
  assert.equal(f.scheduler.inspect().cache.incomingBytes, 6 * 1024 * 1024);
  await tick(); f.scheduler.reset({ documentId: "b" });
  assert.equal(f.scheduler.inspect().cache.incomingBytes, 6 * 1024 * 1024);
  f.scheduler.dispose(); assert.equal(f.scheduler.inspect().cache.incomingBytes, 6 * 1024 * 1024);
  f.succeed(0); await tick(); assert.equal(f.scheduler.inspect().cache.incomingBytes, 0);
});

test("transport context and adopted coverage are immutable snapshots", async () => {
  const f = fixture(); const context = { documentId: "original", revision: "1" };
  f.scheduler.reset(context); context.documentId = "changed"; context.revision = "2";
  f.scheduler.request(cover()); await tick();
  assert.equal(f.calls[0].input.context.documentId, "original");
  assert.equal(f.calls[0].input.context.revision, "1");
  const tile = { logicalRect: rect(), svg: "<svg/>" };
  f.calls[0].resolve(tile); await tick();
  tile.logicalRect.xRaw = 999999; tile.svg = "changed";
  assert.equal(f.scheduler.inspect().ready, true);
  assert.equal(f.adopted[0].tile.logicalRect.xRaw, 0);
  assert.equal(f.adopted[0].tile.svg, "<svg/>");
  assert.equal(Object.isFrozen(f.adopted[0].tile.logicalRect), true);
  assert.equal(Object.isFrozen(f.calls[0].input.context), true);
  f.scheduler.dispose();
});

test("scroll burst coalesces to one active and one latest with no stale mount", async () => {
  const f = fixture(); f.scheduler.reset({ documentId: "a" });
  f.scheduler.request(cover()); await tick();
  for (let n = 1; n <= 100; n++) f.scheduler.request(cover(n * 2048));
  assert.equal(f.calls.length, 1);
  assert.equal(f.scheduler.inspect().peakActive, 1);
  assert.equal(f.scheduler.inspect().peakDesired, 1);
  f.succeed(0); await tick();
  assert.equal(f.adopted.length, 0);
  assert.equal(f.calls.length, 2);
  assert.equal(f.calls[1].input.rect.xRaw, 100 * 2048);
  f.succeed(1); await tick();
  assert.equal(f.adopted.length, 1);
  assert.equal(f.scheduler.inspect().ready, true);
  f.scheduler.dispose();
});

test("same quantized rectangle follows latest visible bounds without another CPU call", async () => {
  const f = fixture(); f.scheduler.reset({ documentId: "a" });
  const request = { rect: rect(0, 4096), visible: rect(0, 512) };
  f.scheduler.request(request); await tick();
  request.rect.widthRaw = 1;
  f.scheduler.request({ rect: rect(0, 4096), visible: rect(2048, 512) });
  f.succeed(0); await tick();
  assert.equal(f.calls.length, 1);
  assert.equal(f.adopted.length, 1);
  assert.equal(f.adopted[0].tile.logicalRect.widthRaw, 4096);
  assert.equal(f.scheduler.inspect().ready, true);
  f.scheduler.dispose();
});

test("deadline reports failure but retains active CPU ownership until real settlement", async () => {
  const f = fixture(); f.scheduler.reset({ documentId: "a" });
  f.scheduler.request(cover()); await tick();
  [...f.timers.values()][0]();
  assert.equal(f.errors.length, 1);
  assert.equal(f.scheduler.inspect().active, 1);
  assert.equal(f.scheduler.inspect().ready, false);
  f.scheduler.request(cover()); await tick(); assert.equal(f.calls.length, 1);
  f.scheduler.request(cover(2048)); await tick(); assert.equal(f.calls.length, 1);
  f.succeed(0); await tick();
  assert.equal(f.adopted.length, 0);
  assert.equal(f.calls.length, 2);
  f.succeed(1); await tick(); assert.equal(f.scheduler.inspect().ready, true);
  f.scheduler.dispose();
});

test("reset preserves old active promise while new context waits; dispose ignores late paint", async () => {
  const f = fixture(); const a = { documentId: "a" }, b = { documentId: "b" };
  f.scheduler.reset(a); f.scheduler.request(cover()); await tick();
  f.scheduler.reset(b); f.scheduler.request(cover(2048)); await tick();
  assert.equal(f.calls.length, 1);
  f.succeed(0); await tick();
  assert.equal(f.adopted.length, 0);
  assert.notEqual(f.calls[1].input.context, b);
  assert.equal(f.calls[1].input.context.documentId, "b");
  assert.equal(f.calls[1].input.namespace, "2");
  f.scheduler.dispose(); f.succeed(1); await tick();
  assert.equal(f.adopted.length, 0);
  assert.equal(f.scheduler.inspect().active, 0);
  assert.equal(f.scheduler.inspect().cache.records, 0);
});

test("failed first paint stays unready and repeats only after viewport change", async () => {
  const f = fixture(); f.scheduler.reset({ documentId: "a" });
  f.scheduler.request(cover()); await tick(); f.calls[0].reject(new Error("tile failed")); await tick();
  assert.equal(f.scheduler.inspect().ready, false);
  assert.equal(f.adopted.length, 0);
  f.scheduler.request(cover()); await tick(); assert.equal(f.calls.length, 1);
  // A new visible position inside the same requested cover permits one retry.
  f.scheduler.request({ rect: rect(), visible: rect(128, 512) }); await tick();
  assert.equal(f.calls.length, 2);
  f.succeed(1); await tick(); assert.equal(f.scheduler.inspect().ready, true);
  f.scheduler.request(cover(4096)); await tick();
  f.calls[2].resolve({ logicalRect: rect(), svg: "<svg/>" }); await tick();
  assert.equal(f.scheduler.inspect().ready, false);
  assert.equal(f.errors.length, 2);
  assert.equal(f.adopted.length, 1);
  f.scheduler.dispose();
});

test("returning to cached cover reuses its SVG without another request", async () => {
  const f = fixture(); f.scheduler.reset({ documentId: "a" });
  f.scheduler.request(cover()); await tick(); f.succeed(0); await tick();
  f.scheduler.request(cover(4096)); await tick(); f.succeed(1); await tick();
  f.scheduler.request(cover()); await tick();
  assert.equal(f.calls.length, 2);
  assert.equal(f.adopted.length, 3);
  assert.equal(f.adopted[2].tile.svg, f.adopted[0].tile.svg);
  assert.equal(f.scheduler.inspect().ready, true);
  assert.equal(f.timers.size, 0);
  f.scheduler.dispose();
});
