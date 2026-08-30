import type {
  CloseDocumentResult,
  FontPack,
  OpenDocumentResult,
  PreparePagesResult,
  RenderBinary,
  RenderCapabilities,
  RenderOperation,
  RenderOperationPayloads,
  RenderOperationResults,
  RenderOptions,
  RenderPagePngResult,
  RenderPageResult,
  RenderRange,
  RenderSheetResult,
  RenderTileResult,
} from "./protocol.mjs";

export type {
  CloseDocumentResult,
  FontPack,
  FontPackMember,
  OpenDocumentResult,
  PreparePagesResult,
  PrintManifest,
  PrintPageMapEntry,
  RenderBinary,
  RenderCapabilities,
  RenderErrorPayload,
  RenderLimits,
  RenderOperation,
  RenderOperationPayloads,
  RenderOperationResults,
  RenderOptions,
  RenderPagePngResult,
  RenderPageResult,
  RenderRange,
  RenderReport,
  RenderSheetResult,
  RenderTileResult,
  WorkbookInspection,
} from "./protocol.mjs";

export interface RenderAbortSignal {
  readonly aborted: boolean;
  addEventListener(
    type: "abort",
    listener: () => void,
    options?: { readonly once?: boolean },
  ): void;
  removeEventListener(type: "abort", listener: () => void): void;
}

export interface RenderProgress {
  readonly completed: number;
  readonly total: number;
  readonly stage: string;
}

export interface RenderRequestOptions {
  readonly signal?: RenderAbortSignal;
  readonly onProgress?: (progress: RenderProgress) => void;
}

export interface RenderOpenOptions extends RenderRequestOptions {
  readonly documentId?: string;
  readonly fontPack?: FontPack;
}

export interface RenderWorkerMessageEvent {
  readonly data: unknown;
}

export interface RenderWorkerErrorEvent {
  readonly message?: string;
}

export interface RenderWorkerLike {
  postMessage(message: unknown): void;
  postMessage(message: unknown, transfer: ArrayBuffer[]): void;
  addEventListener(
    type: "message",
    listener: (event: RenderWorkerMessageEvent) => void,
  ): void;
  addEventListener(
    type: "error",
    listener: (event: RenderWorkerErrorEvent) => void,
  ): void;
  addEventListener(type: "messageerror", listener: () => void): void;
  terminate?(): void;
}

export interface RenderWorkerConstructorOptions {
  readonly type: "module";
  readonly name: "rxls-render-worker";
}

export interface RenderWorkerConstructor {
  new (
    scriptURL: string | URL,
    options: RenderWorkerConstructorOptions,
  ): RenderWorkerLike;
}

export interface RenderWorkerClientOptions {
  readonly WorkerClass?: RenderWorkerConstructor;
}

export interface RenderRequest<Result> extends Promise<Result> {
  readonly requestId?: string;
}

export declare class RenderWorkerClient {
  constructor(
    workerOrUrl: RenderWorkerLike | string | URL,
    options?: RenderWorkerClientOptions,
  );
  capabilities(options?: RenderRequestOptions): RenderRequest<RenderCapabilities>;
  open(bytes: RenderBinary, options?: RenderOpenOptions): RenderRequest<OpenDocumentResult>;
  closeDocument(
    documentId: string,
    options?: RenderRequestOptions,
  ): RenderRequest<CloseDocumentResult>;
  preparePages(
    documentId: string,
    sheetIndex: number,
    renderOptions?: RenderOptions,
    requestOptions?: RenderRequestOptions,
  ): RenderRequest<PreparePagesResult>;
  renderSheet(
    documentId: string,
    sheetIndex: number,
    renderOptions?: RenderOptions,
    requestOptions?: RenderRequestOptions,
  ): RenderRequest<RenderSheetResult>;
  renderTile(
    documentId: string,
    sheetIndex: number,
    range: RenderRange,
    renderOptions?: RenderOptions,
    requestOptions?: RenderRequestOptions,
  ): RenderRequest<RenderTileResult>;
  renderPage(
    documentId: string,
    sheetIndex: number,
    pageIndex: number,
    renderOptions?: RenderOptions,
    requestOptions?: RenderRequestOptions,
  ): RenderRequest<RenderPageResult>;
  renderPagePng(
    documentId: string,
    sheetIndex: number,
    pageIndex: number,
    dpi?: number,
    renderOptions?: RenderOptions,
    requestOptions?: RenderRequestOptions,
  ): RenderRequest<RenderPagePngResult>;
  request<Operation extends RenderOperation>(
    operation: Operation,
    payload: RenderOperationPayloads[Operation],
    options?: RenderRequestOptions,
  ): RenderRequest<RenderOperationResults[Operation]>;
  cancel(requestId: string): boolean;
  terminate(): void;
}

export declare function getRenderWorkerUrl(): URL;
export declare function createRenderWorkerClient(
  workerUrl?: RenderWorkerLike | string | URL,
  options?: RenderWorkerClientOptions,
): RenderWorkerClient;
