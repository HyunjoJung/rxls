import {
  RenderWorkerClient,
  getRenderWorkerUrl
} from "../../js/client.mjs";
import {
  MAX_FONT_FILES,
  validateSvgOutput
} from "../../js/protocol.mjs";
import { runBrowserScenario } from "./scenario.mjs";

const result = document.querySelector("#result");

try {
  await runBrowserScenario({
    RenderWorkerClient,
    workerUrl: getRenderWorkerUrl(),
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
