// Optional owned-worker measurement. No loader fallback or new WASM instance.
export async function cachedMemoryProbeBody({ glueURL }) {
  const counters = { fetch: 0, instantiate: 0, instantiateStreaming: 0 };
  const restored = {};
  const errors = [];
  const installed = [];
  let value = null;
  const targets = [
    [globalThis, "fetch"],
    [WebAssembly, "instantiate"],
    [WebAssembly, "instantiateStreaming"],
  ];
  try {
    for (const [target, name] of targets) {
      const descriptor = Object.getOwnPropertyDescriptor(target, name);
      Object.defineProperty(target, name, {
        configurable: true, enumerable: descriptor?.enumerable ?? false, writable: true,
        value: () => { counters[name] += 1; throw new Error(`memory probe forbids ${name}`); },
      });
      installed.push({ target, name, descriptor });
    }
    const module = await import(glueURL);
    if (typeof module.default !== "function") throw new Error("cached generated initializer absent");
    const exports = await module.default();
    const again = await module.default();
    if (!(exports?.memory instanceof WebAssembly.Memory) || exports.memory !== again?.memory) {
      throw new Error("existing cached Memory identity unavailable");
    }
    const bytes = exports.memory.buffer.byteLength;
    if (!Number.isSafeInteger(bytes) || bytes <= 0 || bytes % 65536 !== 0) {
      throw new Error("existing linear-memory byte count invalid");
    }
    if (Object.values(counters).some((count) => count !== 0)) throw new Error("loader path was attempted");
    value = { bytes, sampledAtWorkerMs: performance.now(), cachedMemoryIdentity: true };
  } catch (error) {
    errors.push({ type: error?.name ?? "Error", message: String(error?.message ?? error).slice(0, 1024) });
  } finally {
    for (const { target, name, descriptor } of installed.reverse()) {
      try {
        if (descriptor) Object.defineProperty(target, name, descriptor);
        else if (!Reflect.deleteProperty(target, name)) throw new Error(`restore delete failed: ${name}`);
        const actual = Object.getOwnPropertyDescriptor(target, name);
        restored[name] = descriptor
          ? actual?.value === descriptor.value && actual?.get === descriptor.get && actual?.set === descriptor.set
            && actual?.configurable === descriptor.configurable && actual?.enumerable === descriptor.enumerable
            && actual?.writable === descriptor.writable
          : actual === undefined;
        if (!restored[name]) throw new Error(`descriptor differs after restore: ${name}`);
      } catch (error) {
        restored[name] = false;
        errors.push({ type: error?.name ?? "Error", message: String(error?.message ?? error).slice(0, 1024) });
      }
    }
  }
  return { available: value !== null && errors.length === 0, value: errors.length ? null : value,
    guardCounters: counters, restored, errors };
}

export async function sampleExistingWorker(page, phase, proof) {
  const result = { schemaVersion: 1, phase, available: false, value: null,
    scope: "existing owned dedicated-worker linear memory; separate from JS heap/process working sets",
    proof: null, errors: [] };
  try {
    if (typeof phase !== "string" || phase.length > 64 || !proof || proof.initialized !== true
      || proof.idle !== true || proof.currentCoverage !== true
      || typeof proof.namespace !== "string" || proof.namespace.length === 0 || proof.namespace.length > 1024) {
      throw new Error("authoritative initialized/current/idle worker proof absent");
    }
    const keys = ["initialized", "idle", "currentCoverage", "documentId", "sheetIndex", "geometryId",
      "revision", "namespace", "workerId"];
    result.proof = {};
    for (const key of keys) {
      if (proof[key] === undefined) continue;
      const value = proof[key];
      if (!(typeof value === "string" && value.length <= 1024)
        && !(typeof value === "number" && Number.isSafeInteger(value) && value >= 0)
        && !(typeof value === "boolean")) throw new Error(`invalid scalar current proof: ${key}`);
      result.proof[key] = value;
    }
    const pageURL = new URL(page.url());
    if (pageURL.protocol !== "http:" || pageURL.hostname !== "127.0.0.1" || pageURL.username || pageURL.password
      || !pageURL.pathname.startsWith("/rxls/") || Number(pageURL.port) < 1024 || Number(pageURL.port) > 65535) {
      throw new Error("owned standalone page origin/base differs");
    }
    const origin = pageURL.origin;
    const workerURL = new URL("/rxls/runtime/js/worker.mjs", origin).href;
    const workers = page.workers().filter((worker) => worker.url() === workerURL);
    if (workers.length !== 1) throw new Error(`exact current dedicated-worker selection ambiguous/unavailable: ${workers.length}`);
    result.workerURL = workers[0].url();
    result.glueURL = new URL("../pkg/rxls_render_wasm.js", result.workerURL).href;
    const observed = await workers[0].evaluate(cachedMemoryProbeBody, { glueURL: result.glueURL });
    Object.assign(result, observed);
  } catch (error) {
    result.errors.push({ type: error?.name ?? "Error", message: String(error?.message ?? error).slice(0, 1024) });
  }
  return result;
}
