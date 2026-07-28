import {
  RenderWorkerClient,
  getRenderWorkerUrl
} from "@rxls/render-worker";
import {
  MAX_FONT_FILES,
  validateSvgOutput
} from "@rxls/render-worker/protocol";
import { runBrowserScenario } from "./scenario.mjs";

const result = document.querySelector("#result");

try {
  const workerUrl = getRenderWorkerUrl();
  const expectedWorkerUrl = new URL(
    "/installed-package/js/worker.mjs",
    location.href
  );
  if (!(workerUrl instanceof URL) || workerUrl.href !== expectedWorkerUrl.href) {
    throw new Error(`installed worker URL mismatch: ${workerUrl}`);
  }
  await runBrowserScenario({
    RenderWorkerClient,
    workerUrl,
    validateSvgOutput,
    maxFontFiles: MAX_FONT_FILES,
    result,
    viewer: document.querySelector("#viewer")
  });
} catch (error) {
  result.textContent = `FAIL ${error?.code ?? error?.name ?? "error"}: ${
    error?.message ?? error
  }`;
  result.id = "fail";
  document.title = "FAIL";
}
