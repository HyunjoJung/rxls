/** Bounded geometry and request scheduling for a prepared read-only sheet. */
export const VIEWPORT_POLICY = Object.freeze({
  unitsPerPixel: 1024,
  quantumPixels: 256,
  overscanPixels: 256,
  maxTilePixels: 8192,
  maxSurfacePixels: 8_000_000,
  maxSvgBytes: 2 * 1024 * 1024,
  maxCacheRecords: 8,
  maxCacheBytes: 32 * 1024 * 1024,
  deadlineMs: 30_000,
});

const U64_MAX = 18_446_744_073_709_551_615n;
const encoder = new TextEncoder();

function boundedInteger(value, name, positive = false) {
  if (!Number.isSafeInteger(value) || value < (positive ? 1 : 0)) {
    throw new RangeError(`Invalid viewport ${name}.`);
  }
  return value;
}

function rectangle(value) {
  const result = {
    xRaw: boundedInteger(value?.xRaw, "x"),
    yRaw: boundedInteger(value?.yRaw, "y"),
    widthRaw: boundedInteger(value?.widthRaw, "width", true),
    heightRaw: boundedInteger(value?.heightRaw, "height", true),
  };
  boundedInteger(result.xRaw + result.widthRaw, "right");
  boundedInteger(result.yRaw + result.heightRaw, "bottom");
  return Object.freeze(result);
}

export function viewportContains(outer, inner) {
  return Boolean(outer && inner && outer.xRaw <= inner.xRaw &&
    outer.yRaw <= inner.yRaw &&
    outer.xRaw + outer.widthRaw >= inner.xRaw + inner.widthRaw &&
    outer.yRaw + outer.heightRaw >= inner.yRaw + inner.heightRaw);
}

/** Fall back only for an actual full-sheet layout/output admission failure. */
export function viewportFallbackError(error) {
  return error?.code === "limit_exceeded" &&
    ["rows", "columns", "cells", "dimension", "output_bytes", "outputBytes"].includes(error.resource) &&
    Number.isSafeInteger(error.limit) && error.limit >= 0 &&
    Number.isSafeInteger(error.actual) && error.actual > error.limit;
}

/** Reject zoom before publishing CSS dimensions that the browser may clamp. */
export function viewportSurfaceSize(widthRaw, heightRaw, zoom) {
  boundedInteger(widthRaw, "logical width");
  boundedInteger(heightRaw, "logical height");
  if (!Number.isFinite(zoom) || zoom < 0.25 || zoom > 3) {
    throw new RangeError("Viewport zoom must be between 25% and 300%.");
  }
  const width = widthRaw / VIEWPORT_POLICY.unitsPerPixel * zoom;
  const height = heightRaw / VIEWPORT_POLICY.unitsPerPixel * zoom;
  if (width > VIEWPORT_POLICY.maxSurfacePixels || height > VIEWPORT_POLICY.maxSurfacePixels) {
    throw new RangeError("The sheet exceeds the viewport surface limit at this zoom.");
  }
  return Object.freeze({ width, height });
}

/** DOM bounds account for actual padding/borders; raw math never uses bitwise casts. */
export function viewportCover({ widthRaw, heightRaw, zoom, viewport, surface }) {
  viewportSurfaceSize(widthRaw, heightRaw, zoom);
  for (const box of [viewport, surface]) {
    for (const key of ["left", "top", "right", "bottom"]) {
      if (!Number.isFinite(box?.[key])) throw new RangeError("Invalid viewport DOM bounds.");
    }
    if (box.right < box.left || box.bottom < box.top) {
      throw new RangeError("Invalid viewport DOM extent.");
    }
  }
  if (widthRaw === 0 || heightRaw === 0 || viewport.right === viewport.left ||
      viewport.bottom === viewport.top) return null;
  const scale = VIEWPORT_POLICY.unitsPerPixel / zoom;
  const left = Math.max(0, Math.floor((viewport.left - surface.left) * scale));
  const top = Math.max(0, Math.floor((viewport.top - surface.top) * scale));
  const right = Math.min(widthRaw, Math.ceil((viewport.right - surface.left) * scale));
  const bottom = Math.min(heightRaw, Math.ceil((viewport.bottom - surface.top) * scale));
  if (right <= left || bottom <= top) return null;
  const visible = rectangle({ xRaw: left, yRaw: top, widthRaw: right - left, heightRaw: bottom - top });
  const maximum = VIEWPORT_POLICY.maxTilePixels * VIEWPORT_POLICY.unitsPerPixel;
  if (visible.widthRaw > maximum || visible.heightRaw > maximum) {
    throw new RangeError("The visible viewport exceeds the tile dimension limit.");
  }
  const quantum = VIEWPORT_POLICY.quantumPixels * VIEWPORT_POLICY.unitsPerPixel;
  const overscan = VIEWPORT_POLICY.overscanPixels * VIEWPORT_POLICY.unitsPerPixel;
  function axis(first, end, extent) {
    let start = Math.max(0, Math.floor(first / quantum) * quantum - overscan);
    let stop = Math.min(extent, Math.ceil(end / quantum) * quantum + overscan);
    if (stop - start > maximum) {
      start = Math.max(0, Math.floor(first / quantum) * quantum);
      stop = Math.min(extent, Math.ceil(end / quantum) * quantum);
    }
    // Quantization must never exclude a visible edge or silently exceed the cap.
    if (stop - start > maximum) [start, stop] = [first, end];
    return [start, stop];
  }
  const [x, xEnd] = axis(left, right, widthRaw);
  const [y, yEnd] = axis(top, bottom, heightRaw);
  return Object.freeze({
    visible,
    rect: rectangle({ xRaw: x, yRaw: y, widthRaw: xEnd - x, heightRaw: yEnd - y }),
  });
}

function rectKey(rect) {
  return `${rect.xRaw}/${rect.yRaw}/${rect.widthRaw}/${rect.heightRaw}`;
}

/** Cache contains SVG text only; the host owns at most one mounted DOM tile. */
export function createViewportTileCache() {
  const records = new Map();
  let bytes = 0;
  let attachedKey = null;
  let peakBytes = 0;
  let peakRecords = 0;
  let incomingBytes = 0;
  const incomingCharge = VIEWPORT_POLICY.maxSvgBytes * 3;
  function room(requiredBytes, requiredRecords) {
    while (records.size + requiredRecords > VIEWPORT_POLICY.maxCacheRecords ||
        bytes + requiredBytes > VIEWPORT_POLICY.maxCacheBytes) {
      const evict = [...records.keys()].find((candidate) => candidate !== attachedKey);
      if (evict === undefined) throw new RangeError("Viewport cache budget cannot admit the tile.");
      bytes -= records.get(evict).charge;
      records.delete(evict);
    }
  }
  function highWater() {
    peakBytes = Math.max(peakBytes, bytes + incomingBytes);
    peakRecords = Math.max(peakRecords, records.size + Number(incomingBytes > 0));
  }
  return {
    reserveIncoming() {
      if (incomingBytes) throw new Error("An incoming viewport tile is already reserved.");
      room(incomingCharge, 1);
      incomingBytes = incomingCharge;
      highWater();
    },
    releaseIncoming() { incomingBytes = 0; },
    get(key) {
      const record = records.get(key);
      if (record) {
        records.delete(key);
        records.set(key, record);
      }
      return record ?? null;
    },
    insert(key, tile) {
      if (typeof tile?.svg !== "string") throw new TypeError("Missing viewport SVG.");
      if (tile.svg.length > VIEWPORT_POLICY.maxSvgBytes) throw new RangeError("Viewport SVG exceeds its byte limit.");
      const utf8Bytes = encoder.encode(tile.svg).byteLength;
      if (utf8Bytes > VIEWPORT_POLICY.maxSvgBytes) throw new RangeError("Viewport SVG exceeds its byte limit.");
      const charge = utf8Bytes + 2 * tile.svg.length;
      // Replacing a cached identity would hide the charge of its attached text.
      if (records.has(key)) throw new Error("Duplicate viewport cache identity.");
      const snapshot = Object.freeze({ ...tile, logicalRect: rectangle(tile.logicalRect) });
      room(charge, 1);
      // Transfer the incoming reservation into this exact text record atomically.
      incomingBytes = 0;
      const record = Object.freeze({ key, tile: snapshot, charge });
      records.set(key, record);
      bytes += charge;
      highWater();
      return record;
    },
    attach(key) {
      if (!records.has(key)) throw new Error("Unknown viewport cache tile.");
      attachedKey = key;
    },
    remove(key) {
      const record = records.get(key);
      if (!record) return;
      if (key === attachedKey) throw new Error("Cannot evict the attached viewport tile.");
      bytes -= record.charge;
      records.delete(key);
    },
    clear() {
      attachedKey = null;
      records.clear();
      bytes = 0;
      // Reset/dispose cannot release an outstanding CPU result's reservation.
    },
    inspect() {
      return Object.freeze({ bytes, records: records.size, incomingBytes,
        chargedBytes: bytes + incomingBytes, chargedRecords: records.size + Number(incomingBytes > 0),
        attachedKey, peakBytes, peakRecords });
    },
  };
}

/** Keep CPU ownership until the original promise settles, including after timeout/reset. */
export function createViewportScheduler({
  render, adopt, onState = () => {}, onError = () => {},
  setTimer = setTimeout, clearTimer = clearTimeout,
}) {
  const cache = createViewportTileCache();
  let context = null;
  let generation = 0;
  let sequence = 0;
  let namespace = 0n;
  let desired = null;
  let active = null;
  let attached = null;
  let failedKey = null;
  let disposed = false;
  let peakActive = 0;
  let peakDesired = 0;

  const isCurrent = (work) => !disposed && work.generation === generation && work.context === context;
  const state = () => ({
    ready: Boolean(attached && desired && viewportContains(attached.tile.logicalRect, desired.visible)),
    loading: Boolean(desired && !failedKey && !(attached && viewportContains(attached.tile.logicalRect, desired.visible))),
    active: Number(Boolean(active)), desired: Number(Boolean(desired)),
  });
  function publish() { onState(Object.freeze(state())); }
  function fail(work, error) {
    if (!isCurrent(work) || work.sequence !== desired?.sequence) return;
    failedKey = work.key;
    onError(error);
    publish();
  }
  function mount(record) {
    if (attached !== record) {
      adopt(record.tile, context);
      cache.attach(record.key);
      attached = record;
    }
    failedKey = null;
    publish();
  }
  function pump() {
    if (disposed || active || !desired || !context || failedKey === desired.key) return;
    if (attached && viewportContains(attached.tile.logicalRect, desired.visible)) {
      publish();
      return;
    }
    const cached = cache.get(desired.key);
    if (cached) {
      try { mount(cached); } catch (error) { fail(desired, error); }
      return;
    }
    if (namespace === U64_MAX) {
      fail(desired, new RangeError("Viewport SVG namespace exhausted."));
      return;
    }
    try { cache.reserveIncoming(); } catch (error) { fail(desired, error); return; }
    const work = { ...desired, namespace: String(++namespace), expired: false, timer: null };
    active = work;
    peakActive = Math.max(peakActive, 1);
    work.timer = setTimer(() => {
      work.expired = true;
      // Current scrolling may have superseded the active rectangle while waiting
      // for that same CPU work. Report the wait without freeing its ownership.
      if (isCurrent(work) && desired) {
        fail(desired, new Error("Viewport rendering exceeded its 30-second deadline."));
      }
    }, VIEWPORT_POLICY.deadlineMs);
    publish();
    // A synchronous throw is a settlement too; no AbortSignal disguises CPU work.
    Promise.resolve().then(() => render({ context: work.context, rect: work.rect, namespace: work.namespace }))
      .then((tile) => {
        if (work.expired || !isCurrent(work) || work.sequence !== desired?.sequence) return;
        if (!tile || !viewportContains(tile.logicalRect, desired.visible)) {
          throw new Error("Viewport output does not cover the visible sheet.");
        }
        const record = cache.insert(work.key, tile);
        try { mount(record); } catch (error) {
          cache.remove(work.key);
          throw error;
        }
      }).catch((error) => { if (!work.expired) fail(work, error); })
      .finally(() => {
        clearTimer(work.timer);
        cache.releaseIncoming();
        if (active === work) active = null;
        if (!disposed) {
          publish();
          pump();
        }
      });
  }
  return {
    reset(nextContext = null) {
      if (disposed) throw new Error("Viewport scheduler is disposed.");
      if (!Number.isSafeInteger(generation + 1)) throw new RangeError("Viewport generation exhausted.");
      generation += 1;
      context = nextContext === null ? null : Object.freeze({ ...nextContext });
      desired = attached = failedKey = null;
      cache.clear();
      publish();
      // An old active request remains owned until authoritative settlement.
    },
    request(cover) {
      if (disposed || !context) throw new Error("No active prepared viewport.");
      const rect = rectangle(cover?.rect);
      const visible = rectangle(cover?.visible);
      const max = VIEWPORT_POLICY.maxTilePixels * VIEWPORT_POLICY.unitsPerPixel;
      if (rect.widthRaw > max || rect.heightRaw > max || !viewportContains(rect, visible)) {
        throw new RangeError("Viewport request cannot cover the visible sheet within tile limits.");
      }
      const key = rectKey(rect);
      if (desired?.key === key && rectKey(desired.visible) === rectKey(visible)) {
        publish();
        pump();
        return;
      }
      if (desired?.key === key && !failedKey) {
        // A quantized request already covers this new visible rectangle. Keep
        // its sequence so a real result can cover the latest viewport directly.
        desired = { ...desired, visible };
        publish();
        pump();
        return;
      }
      if (!Number.isSafeInteger(sequence + 1)) throw new RangeError("Viewport sequence exhausted.");
      desired = { context, generation, sequence: ++sequence, key, rect, visible };
      peakDesired = Math.max(peakDesired, 1);
      failedKey = null;
      publish();
      pump();
    },
    dispose() {
      disposed = true;
      context = desired = attached = failedKey = null;
      cache.clear();
      if (active) clearTimer(active.timer);
      // Disposal discards output; it cannot settle synchronous WASM CPU work.
    },
    inspect() {
      return Object.freeze({ ...state(), generation, sequence, namespace: String(namespace),
        disposed, peakActive, peakDesired, cache: cache.inspect() });
    },
  };
}
