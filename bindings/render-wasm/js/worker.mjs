import init, * as wasm from "../pkg/rxls_render_wasm.js";
import { installRenderWorker } from "./worker-runtime.mjs";

await init();
installRenderWorker({ wasm });
