import init, * as wasm from "../node_modules/@rxls/render-worker/pkg/rxls_render_wasm.js";
import { installRenderWorker } from "@rxls/render-worker/worker-runtime";

const BOOTSTRAP_PROTOCOL = "rxls.vscode.worker.bootstrap.v1";
const CRASH_TEST_PROTOCOL = "rxls.vscode.worker.crash-test.v1";
const MAX_WASM_BYTES = 8 * 1024 * 1024;

globalThis.addEventListener("message", (event) => {
  if (event.data?.protocol === CRASH_TEST_PROTOCOL) {
    event.stopImmediatePropagation();
    crashBootstrap("intentional extension-host crash test");
  }
});

globalThis.addEventListener(
  "message",
  (event) => {
    const message = event.data;
    if (
      message === null ||
      typeof message !== "object" ||
      message.protocol !== BOOTSTRAP_PROTOCOL ||
      !(message.wasm instanceof ArrayBuffer) ||
      message.wasm.byteLength === 0 ||
      message.wasm.byteLength > MAX_WASM_BYTES
    ) {
      crashBootstrap("invalid bootstrap payload");
      return;
    }
    void initialize(message.wasm);
  },
  { once: true }
);

async function initialize(wasmBytes) {
  try {
    await init({ module_or_path: wasmBytes });
    installRenderWorker({ wasm });
  } catch (error) {
    crashBootstrap(error instanceof Error ? error.message : "WASM initialization failed");
  }
}

function crashBootstrap(message) {
  setTimeout(() => {
    throw new Error(`rxls worker bootstrap failed: ${message}`);
  }, 0);
}
