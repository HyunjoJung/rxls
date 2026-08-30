import type {
  RenderCapabilities,
  RenderWorkerOutgoingMessage,
} from "./protocol.mjs";

export type RenderMaybePromise<Value> = Value | PromiseLike<Value>;

export interface RenderWasmSession {
  inspectionJson(): RenderMaybePromise<string>;
  printManifestJson(sheetIndex: number, optionsJson: string): RenderMaybePromise<string>;
  renderSheetSvg(sheetIndex: number, optionsJson: string): RenderMaybePromise<string>;
  renderTileSvg(
    sheetIndex: number,
    firstRow: number,
    firstCol: number,
    lastRow: number,
    lastCol: number,
    optionsJson: string,
  ): RenderMaybePromise<string>;
  renderPrintPageSvg(
    sheetIndex: number,
    pageIndex: number,
    optionsJson: string,
  ): RenderMaybePromise<string>;
  renderPrintPagePng(
    sheetIndex: number,
    pageIndex: number,
    dpi: number,
    optionsJson: string,
  ): RenderMaybePromise<Uint8Array>;
  free?(): void;
}

export interface RenderWasmSessionConstructor {
  new (bytes: Uint8Array, fontBundle: Uint8Array): RenderWasmSession;
}

export interface RenderWasmModule {
  readonly RenderSession: RenderWasmSessionConstructor;
  capabilitiesJson(): string;
}

export type RenderWorkerSend = (
  message: RenderWorkerOutgoingMessage,
  transfer?: ArrayBuffer[],
) => void;

export interface RenderWorkerRuntimeOptions {
  readonly wasm: RenderWasmModule;
  readonly send: RenderWorkerSend;
}

export declare class RenderWorkerRuntime {
  constructor(options: RenderWorkerRuntimeOptions);
  receive(rawMessage: unknown): void;
  closeAll(): void;
  capabilities(): RenderCapabilities;
}

export interface RenderWorkerScope {
  postMessage(message: RenderWorkerOutgoingMessage): void;
  postMessage(message: RenderWorkerOutgoingMessage, transfer: ArrayBuffer[]): void;
  addEventListener(
    type: "message",
    listener: (event: { readonly data: unknown }) => void,
  ): void;
}

export interface InstallRenderWorkerOptions {
  readonly wasm: RenderWasmModule;
  readonly scope?: RenderWorkerScope;
}

export declare function installRenderWorker(
  options: InstallRenderWorkerOptions,
): RenderWorkerRuntime;
