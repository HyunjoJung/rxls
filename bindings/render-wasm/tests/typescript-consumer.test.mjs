import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const TYPESCRIPT_VERSION = "5.9.3";
const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function run(command, args, options = {}) {
  try {
    return execFileSync(command, args, {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      ...options,
    });
  } catch (error) {
    const stdout = typeof error?.stdout === "string" ? error.stdout : "";
    const stderr = typeof error?.stderr === "string" ? error.stderr : "";
    throw new Error(
      `${command} ${args.join(" ")} failed\n${stdout}${stderr}`.trimEnd(),
      { cause: error },
    );
  }
}

function runNpm(args, options = {}) {
  const configuredCli = process.env.npm_execpath;
  if (configuredCli && fs.existsSync(configuredCli)) {
    return run(process.execPath, [configuredCli, ...args], options);
  }
  if (process.platform === "win32") {
    const bundledCli = path.join(
      path.dirname(process.execPath),
      "node_modules",
      "npm",
      "bin",
      "npm-cli.js",
    );
    assert.ok(fs.existsSync(bundledCli), `npm CLI not found at ${bundledCli}`);
    return run(process.execPath, [bundledCli, ...args], options);
  }
  return run("npm", args, options);
}

const nodeNextConsumer = String.raw`
import {
  RenderWorkerClient,
  createRenderWorkerClient,
  getRenderWorkerUrl,
  type CloseDocumentResult,
  type FontPack,
  type OpenDocumentResult,
  type PreparePagesResult,
  type RenderCapabilities,
  type RenderOpenOptions,
  type RenderPagePngResult,
  type RenderPageResult,
  type RenderProgress,
  type RenderRequest,
  type RenderRequestOptions,
  type RenderSheetResult,
  type RenderTileResult,
  type RenderWorkerConstructor,
  type RenderWorkerLike,
} from "@rxls/render-worker";
import {
  MAX_DPI,
  MAX_FONT_BYTES,
  MAX_FONT_FILES,
  MAX_FONT_FILE_BYTES,
  MAX_FONT_MANIFEST_BYTES,
  MAX_INPUT_BYTES,
  MAX_OPEN_DOCUMENTS,
  MAX_OPEN_RESOURCE_BYTES,
  MAX_OPTIONS_BYTES,
  MAX_OUTPUT_BYTES,
  MAX_PAGES,
  MAX_PENDING_REQUESTS,
  MAX_PENDING_RESOURCE_BYTES,
  MAX_PNG_BYTES,
  MAX_SHEETS,
  MIN_DPI,
  PROTOCOL,
  RenderProtocolError,
  asBytes,
  boundedIndex,
  encodeFontBundle,
  fontPackByteLength,
  limitError,
  normalizeError,
  optionsJson,
  parseWorkerMessage,
  positiveInteger,
  preflightRequest,
  validateDocumentId,
  validateFontPack,
  validateRange,
  validateRequestId,
  validateSvgOutput,
  type RenderErrorPayload,
  type RenderOperationPayloads,
  type RenderOperationResults,
  type RenderWorkerOutgoingMessage,
  type RenderWorkerRequestMessage,
} from "@rxls/render-worker/protocol";
import {
  RenderWorkerRuntime,
  installRenderWorker,
  type RenderWasmModule,
  type RenderWasmSession,
  type RenderWorkerScope,
} from "@rxls/render-worker/worker-runtime";
import "@rxls/render-worker/worker";

declare const worker: RenderWorkerLike;
declare const WorkerClass: RenderWorkerConstructor;
const requestOptions: RenderRequestOptions = {
  onProgress(progress: RenderProgress): void {
    const completed: number = progress.completed;
    const total: number = progress.total;
    const stage: string = progress.stage;
    void [completed, total, stage];
  },
};
const fontPack: FontPack = {
  manifest: new Uint8Array(),
  members: [{ name: "font.ttf", bytes: new DataView(new ArrayBuffer(8)) }],
};
const openOptions: RenderOpenOptions = {
  documentId: "document-1",
  fontPack,
  ...requestOptions,
};
const client = new RenderWorkerClient(worker);
const clientFromUrl = new RenderWorkerClient(getRenderWorkerUrl(), { WorkerClass });
const factoryClient = createRenderWorkerClient(undefined, { WorkerClass });
const capabilities: RenderRequest<RenderCapabilities> = client.capabilities(requestOptions);
const opened: RenderRequest<OpenDocumentResult> = client.open(new Uint8Array(), openOptions);
const closed: RenderRequest<CloseDocumentResult> = client.closeDocument("document-1");
const pages: RenderRequest<PreparePagesResult> = client.preparePages("document-1", 0, {
  gridlines: false,
  limits: { maxPages: 4 },
});
const sheet: RenderRequest<RenderSheetResult> = client.renderSheet("document-1", 0);
const tile: RenderRequest<RenderTileResult> = client.renderTile(
  "document-1",
  0,
  { firstRow: 0, firstCol: 0, lastRow: 9, lastCol: 4 },
);
const page: RenderRequest<RenderPageResult> = client.renderPage("document-1", 0, 0);
const png: RenderRequest<RenderPagePngResult> = client.renderPagePng(
  "document-1",
  0,
  0,
  144,
);
const generic: RenderRequest<RenderOperationResults["render-page"]> = client.request(
  "render-page",
  { documentId: "document-1", sheetIndex: 0, pageIndex: 0 },
);
const requestId: string | undefined = generic.requestId;
const cancelled: boolean = client.cancel(requestId ?? "missing");
client.terminate();
void [
  clientFromUrl,
  factoryClient,
  capabilities,
  opened,
  closed,
  pages,
  sheet,
  tile,
  page,
  png,
  cancelled,
];

const range = validateRange({ firstRow: 0, firstCol: 0, lastRow: 1, lastCol: 1 });
const payload: RenderOperationPayloads["render-tile"] = {
  documentId: "document-1",
  sheetIndex: 0,
  range,
};
const message: RenderWorkerRequestMessage<"render-tile"> = {
  protocol: PROTOCOL,
  type: "request",
  requestId: "request-1",
  operation: "render-tile",
  payload,
};
const parsed = parseWorkerMessage(message);
if (parsed.type === "request") {
  const unvalidatedPayloadValue: unknown = parsed.payload["documentId"];
  void unvalidatedPayloadValue;
}
const resourceBytes: number = preflightRequest(message);
const encoded: Uint8Array = encodeFontBundle(fontPack);
const validated = validateFontPack(fontPack);
const normalized: RenderErrorPayload = normalizeError(new Error("failed"));
const protocolError: RenderProtocolError = limitError("pages", 1, 2);
const directError = new RenderProtocolError("invalid", "invalid request", "request");
const byteLength: number = validateSvgOutput("<svg></svg>");
const json: string = optionsJson({ gridlines: false });
const bytes: Uint8Array = asBytes(new Uint16Array(2));
const validatedRequestId: string = validateRequestId("request-1");
const validatedDocumentId: string = validateDocumentId("document-1");
const index: number = boundedIndex(0, "sheetIndex", MAX_SHEETS, "sheets");
const positive: number = positiveInteger(1, "dpi");
const fontBytes: number = fontPackByteLength(fontPack);
void [
  parsed,
  resourceBytes,
  encoded,
  validated,
  normalized,
  protocolError,
  directError,
  byteLength,
  json,
  bytes,
  validatedRequestId,
  validatedDocumentId,
  index,
  positive,
  fontBytes,
  MAX_DPI,
  MAX_FONT_BYTES,
  MAX_FONT_FILES,
  MAX_FONT_FILE_BYTES,
  MAX_FONT_MANIFEST_BYTES,
  MAX_INPUT_BYTES,
  MAX_OPEN_DOCUMENTS,
  MAX_OPEN_RESOURCE_BYTES,
  MAX_OPTIONS_BYTES,
  MAX_OUTPUT_BYTES,
  MAX_PAGES,
  MAX_PENDING_REQUESTS,
  MAX_PENDING_RESOURCE_BYTES,
  MAX_PNG_BYTES,
  MIN_DPI,
];

class Session implements RenderWasmSession {
  constructor(_bytes: Uint8Array, _fontBundle: Uint8Array) {}
  inspectionJson(): string { return "{}"; }
  printManifestJson(_sheetIndex: number, _optionsJson: string): string { return "{}"; }
  renderSheetSvg(_sheetIndex: number, _optionsJson: string): string { return "<svg></svg>"; }
  renderTileSvg(
    _sheetIndex: number,
    _firstRow: number,
    _firstCol: number,
    _lastRow: number,
    _lastCol: number,
    _optionsJson: string,
  ): string { return "<svg></svg>"; }
  renderPrintPageSvg(
    _sheetIndex: number,
    _pageIndex: number,
    _optionsJson: string,
  ): string { return "<svg></svg>"; }
  renderPrintPagePng(
    _sheetIndex: number,
    _pageIndex: number,
    _dpi: number,
    _optionsJson: string,
  ): Uint8Array { return new Uint8Array(); }
  free(): void {}
}
const wasm: RenderWasmModule = {
  RenderSession: Session,
  capabilitiesJson: () => "{}",
};
const outgoing: RenderWorkerOutgoingMessage[] = [];
const runtime = new RenderWorkerRuntime({
  wasm,
  send(workerMessage): void { outgoing.push(workerMessage); },
});
runtime.receive(message);
const runtimeCapabilities: RenderCapabilities = runtime.capabilities();
runtime.closeAll();
declare const scope: RenderWorkerScope;
const installed: RenderWorkerRuntime = installRenderWorker({ wasm, scope });
void [runtimeCapabilities, installed, outgoing];
`;

const bundlerConsumer = String.raw`
import {
  RenderWorkerClient,
  createRenderWorkerClient,
  getRenderWorkerUrl,
  type RenderCapabilities,
  type RenderPageResult,
} from "@rxls/render-worker";
import {
  PROTOCOL,
  validateRange,
  type RenderWorkerReadyMessage,
} from "@rxls/render-worker/protocol";
import {
  installRenderWorker,
  type RenderWasmModule,
  type RenderWorkerScope,
} from "@rxls/render-worker/worker-runtime";
import "@rxls/render-worker/worker";

const worker = new Worker(getRenderWorkerUrl(), { type: "module" });
const client = new RenderWorkerClient(worker);
const controller = new AbortController();
const page: Promise<RenderPageResult> = client.renderPage(
  "document-1",
  0,
  0,
  {},
  {
    signal: controller.signal,
    onProgress: ({ completed, total, stage }): void => {
      const row: readonly [number, number, string] = [completed, total, stage];
      void row;
    },
  },
);
const factory = createRenderWorkerClient();
const range = validateRange({ firstRow: 0, firstCol: 0, lastRow: 2, lastCol: 2 });
declare const capabilities: RenderCapabilities;
const ready: RenderWorkerReadyMessage = {
  protocol: PROTOCOL,
  type: "ready",
  capabilities,
};
declare const wasm: RenderWasmModule;
declare const scope: RenderWorkerScope;
const runtime = installRenderWorker({ wasm, scope });
void [page, factory, range, ready, runtime];
`;

function writeConsumer(consumerRoot) {
  fs.writeFileSync(
    path.join(consumerRoot, "package.json"),
    `${JSON.stringify({ name: "render-worker-type-consumer", private: true, type: "module" }, null, 2)}\n`,
  );
  fs.writeFileSync(path.join(consumerRoot, "node-next-consumer.mts"), nodeNextConsumer);
  fs.writeFileSync(path.join(consumerRoot, "bundler-consumer.ts"), bundlerConsumer);
  const shared = {
    allowUnreachableCode: false,
    exactOptionalPropertyTypes: true,
    lib: ["ES2022", "DOM"],
    noEmit: true,
    noFallthroughCasesInSwitch: true,
    noImplicitOverride: true,
    noUncheckedIndexedAccess: true,
    strict: true,
    target: "ES2022",
    useUnknownInCatchVariables: true,
    verbatimModuleSyntax: true,
  };
  fs.writeFileSync(
    path.join(consumerRoot, "tsconfig.node-next.json"),
    `${JSON.stringify({
      compilerOptions: {
        ...shared,
        module: "NodeNext",
        moduleResolution: "NodeNext",
      },
      files: ["node-next-consumer.mts"],
    }, null, 2)}\n`,
  );
  fs.writeFileSync(
    path.join(consumerRoot, "tsconfig.bundler.json"),
    `${JSON.stringify({
      compilerOptions: {
        ...shared,
        module: "ESNext",
        moduleResolution: "Bundler",
      },
      files: ["bundler-consumer.ts"],
    }, null, 2)}\n`,
  );
}

test("packed package type-checks in clean strict NodeNext and Bundler consumers", () => {
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "rxls-render-types-"));
  try {
    const archiveRoot = path.join(temporary, "archive");
    const consumerRoot = path.join(temporary, "consumer");
    fs.mkdirSync(archiveRoot);
    fs.mkdirSync(consumerRoot);
    const archiveName = runNpm([
      "pack",
      packageRoot,
      "--pack-destination",
      archiveRoot,
      "--silent",
    ]).trim();
    assert.match(archiveName, /^rxls-render-worker-\d+\.\d+\.\d+\.tgz$/);
    const archive = path.join(archiveRoot, archiveName);

    writeConsumer(consumerRoot);
    runNpm(
      [
        "install",
        "--ignore-scripts",
        "--no-audit",
        "--no-fund",
        "--package-lock=false",
        archive,
        `typescript@${TYPESCRIPT_VERSION}`,
      ],
      { cwd: consumerRoot },
    );
    const installedRoot = path.join(
      consumerRoot,
      "node_modules",
      "@rxls",
      "render-worker",
    );
    for (const declaration of [
      "js/client.d.mts",
      "js/protocol.d.mts",
      "js/worker-runtime.d.mts",
      "js/worker.d.mts",
    ]) {
      assert.ok(
        fs.statSync(path.join(installedRoot, declaration)).isFile(),
        `${declaration} was not packed`,
      );
    }
    const tsc = path.join(consumerRoot, "node_modules", "typescript", "bin", "tsc");
    run(process.execPath, [tsc, "--project", "tsconfig.node-next.json"], {
      cwd: consumerRoot,
    });
    run(process.execPath, [tsc, "--project", "tsconfig.bundler.json"], {
      cwd: consumerRoot,
    });
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true });
  }
});
