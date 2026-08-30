export declare const PROTOCOL: "rxls.render-worker.v1";
export declare const MAX_INPUT_BYTES: 33554432;
export declare const MAX_FONT_BYTES: 67108864;
export declare const MAX_FONT_MANIFEST_BYTES: 4194304;
export declare const MAX_FONT_FILES: 512;
export declare const MAX_FONT_FILE_BYTES: 33554432;
export declare const MAX_OPEN_DOCUMENTS: 4;
export declare const MAX_OPEN_RESOURCE_BYTES: 134217728;
export declare const MAX_OPTIONS_BYTES: 65536;
export declare const MAX_PENDING_REQUESTS: 32;
export declare const MAX_PENDING_RESOURCE_BYTES: 134217728;
export declare const MAX_OUTPUT_BYTES: 16777216;
export declare const MAX_PNG_BYTES: 16777216;
export declare const MAX_SHEETS: 255;
export declare const MAX_PAGES: 512;
export declare const MIN_DPI: 36;
export declare const MAX_DPI: 300;

export type RenderBinary = ArrayBuffer | ArrayBufferView;
export type RenderJsonPrimitive = boolean | number | string | null;
export type RenderJsonValue =
  | RenderJsonPrimitive
  | readonly RenderJsonValue[]
  | { readonly [key: string]: RenderJsonValue };

export interface RenderRange {
  readonly firstRow: number;
  readonly firstCol: number;
  readonly lastRow: number;
  readonly lastCol: number;
}

export interface RenderLimits {
  readonly maxRows?: number;
  readonly maxColumns?: number;
  readonly maxCells?: number;
  readonly maxConditionalRules?: number;
  readonly maxConditionalEvaluations?: number;
  readonly maxDrawingObjects?: number;
  readonly maxMediaBytes?: number;
  readonly maxImageDimension?: number;
  readonly maxImagePixels?: number;
  readonly maxDecodedMediaBytes?: number;
  readonly maxChartSeries?: number;
  readonly maxChartPoints?: number;
  readonly maxTextBytes?: number;
  readonly maxGlyphs?: number;
  readonly maxTextRuns?: number;
  readonly maxTextLines?: number;
  readonly maxPathCommands?: number;
  readonly maxSceneNodes?: number;
  readonly maxDimensionRaw?: number;
  readonly maxOutputBytes?: number;
  readonly maxLogicalPages?: number;
  readonly maxPages?: number;
  readonly maxTotalSceneNodes?: number;
  readonly maxBackendCommands?: number;
  readonly maxRasterDimension?: number;
  readonly maxRasterPixels?: number;
  readonly maxPngBytes?: number;
  readonly maxImageBytes?: number;
  readonly maxImages?: number;
  readonly maxFontBytes?: number;
}

export interface RenderOptions {
  readonly range?: RenderRange;
  readonly gridlines?: boolean;
  readonly includeHidden?: boolean;
  readonly omitSparsePages?: boolean;
  readonly singlePageSheets?: boolean;
  readonly limits?: RenderLimits;
}

export interface FontPackMember {
  readonly name: string;
  readonly bytes: RenderBinary;
}

export interface FontPack {
  readonly manifest: RenderBinary;
  readonly members: readonly FontPackMember[];
}

export interface ValidatedFontPackMember {
  readonly nameBytes: Uint8Array;
  readonly bytes: Uint8Array;
}

export interface ValidatedFontPack {
  readonly manifest: Uint8Array;
  readonly members: readonly ValidatedFontPackMember[];
}

export interface RenderProtocolErrorDetails {
  readonly resource?: string | null;
  readonly limit?: number | null;
  readonly actual?: number | null;
}

export interface RenderErrorPayload {
  readonly code: string;
  readonly message: string;
  readonly location: string;
  readonly resource: string | null;
  readonly limit: number | null;
  readonly actual: number | null;
}

export declare class RenderProtocolError extends Error {
  readonly name: "RenderProtocolError";
  readonly code: string;
  readonly location: string;
  readonly resource: string | null;
  readonly limit: number | null;
  readonly actual: number | null;
  constructor(
    code: string,
    message: string,
    location?: string,
    details?: RenderProtocolErrorDetails,
  );
}

export interface RenderCapabilityLimits {
  readonly maxInputBytes: number;
  readonly maxImageBytes: number;
  readonly maxImages: number;
  readonly maxFontBytes: number;
  readonly maxRows: number;
  readonly maxColumns: number;
  readonly maxCells: number;
  readonly maxConditionalRules: number;
  readonly maxConditionalEvaluations: number;
  readonly maxDrawingObjects: number;
  readonly maxMediaBytes: number;
  readonly maxImageDimension: number;
  readonly maxImagePixels: number;
  readonly maxDecodedMediaBytes: number;
  readonly maxChartSeries: number;
  readonly maxChartPoints: number;
  readonly maxTextBytes: number;
  readonly maxGlyphs: number;
  readonly maxTextRuns: number;
  readonly maxTextLines: number;
  readonly maxPathCommands: number;
  readonly maxSceneNodes: number;
  readonly maxDimensionRaw: number;
  readonly maxOutputBytes: number;
  readonly maxSheets: number;
  readonly maxLogicalPages: number;
  readonly maxPages: number;
  readonly maxTotalSceneNodes: number;
  readonly maxBackendCommands: number;
  readonly maxRasterDimension: number;
  readonly maxRasterPixels: number;
  readonly maxPngBytes: number;
  readonly minDpi: number;
  readonly maxDpi: number;
}

export interface RenderCapabilities {
  readonly schemaVersion: 1;
  readonly protocol: typeof PROTOCOL;
  readonly outputs: readonly ["sheet-svg", "tile-svg", "page-svg", "page-png"];
  readonly limits: RenderCapabilityLimits;
  readonly fontUploads: {
    readonly supported: true;
    readonly verified: true;
    readonly maxBytes: number;
    readonly bundleSchema: "rxls.font-bundle.v1";
  };
  readonly embeddedImages: {
    readonly bounded: true;
    readonly painted: true;
  };
}

export interface WorkbookSheetInspection {
  readonly index: number;
  readonly name: string;
  readonly embeddedImages: number;
}

export interface WorkbookInspection {
  readonly schemaVersion: 1;
  readonly sheetCount: number;
  readonly sheets: readonly WorkbookSheetInspection[];
  readonly embeddedImages: number;
  readonly embeddedImageBytes: number;
  readonly fontPackSha256: string | null;
  readonly fontFaces: number;
}

export interface RenderReportRange {
  readonly first_row: number;
  readonly first_col: number;
  readonly last_row: number;
  readonly last_col: number;
}

export interface RenderedFontFace {
  readonly source_pack_sha256: string;
  readonly face_sha256: string;
  readonly family: string;
  readonly weight: number;
  readonly italic: boolean;
  readonly substituted: boolean;
}

export interface RenderWarning {
  readonly code: string;
  readonly occurrences: number;
  readonly first_cell: { readonly row: number; readonly col: number } | null;
}

export interface RenderReport {
  readonly schema_version: number;
  readonly sheet_index: number;
  readonly sheet_name: string;
  readonly range: RenderReportRange;
  readonly rows_considered: number;
  readonly columns_considered: number;
  readonly cells_considered: number;
  readonly visible_rows: number;
  readonly visible_columns: number;
  readonly rendered_regions: number;
  readonly hidden_rows_skipped: number;
  readonly hidden_columns_skipped: number;
  readonly merged_regions: number;
  readonly text_bytes: number;
  readonly glyphs: number;
  readonly scene_nodes: number;
  readonly svg_bytes: number;
  readonly font_pack_sha256: string | null;
  readonly font_faces: readonly RenderedFontFace[];
  readonly warnings: readonly RenderWarning[];
}

export interface PrintPageMapEntry {
  readonly output_index: number;
  readonly displayed_page_number: number;
  readonly area_index: number;
  readonly horizontal_index: number;
  readonly vertical_index: number;
  readonly manual_col_break_before: boolean;
  readonly manual_row_break_before: boolean;
  readonly body_range: RenderReportRange;
  readonly repeat_rows: readonly [number, number] | null;
  readonly repeat_cols: readonly [number, number] | null;
  readonly scale_permille: number;
}

export interface PrintManifest {
  readonly schema_version: number;
  readonly sheet_index: number;
  readonly sheet_name: string;
  readonly source_report: RenderReport;
  readonly source_reports: readonly RenderReport[];
  readonly paper: {
    readonly code: number;
    readonly width_raw: number;
    readonly height_raw: number;
  };
  readonly content_rect: {
    readonly x_raw: number;
    readonly y_raw: number;
    readonly width_raw: number;
    readonly height_raw: number;
  };
  readonly layout_override?: "single_page_sheets";
  readonly page_order?: "down_then_over" | "over_then_down" | "unknown";
  readonly manual_row_breaks: readonly number[];
  readonly manual_col_breaks: readonly number[];
  readonly scale_permille: number;
  readonly logical_pages: number;
  readonly sparse_pages_omitted: number;
  readonly pages: readonly PrintPageMapEntry[];
  readonly warnings: readonly { readonly code: string; readonly occurrences: number }[];
}

export interface OpenDocumentResult {
  readonly documentId: string;
  readonly workbook: WorkbookInspection;
}

export interface CloseDocumentResult {
  readonly documentId: string;
  readonly closed: boolean;
}

export interface PreparePagesResult {
  readonly documentId: string;
  readonly sheetIndex: number;
  readonly manifest: PrintManifest;
}

export interface RenderSheetResult {
  readonly documentId: string;
  readonly sheetIndex: number;
  readonly mimeType: "image/svg+xml";
  readonly svg: string;
}

export interface RenderTileResult extends RenderSheetResult {
  readonly range: RenderRange;
}

export interface RenderPageResult extends RenderSheetResult {
  readonly pageIndex: number;
}

export interface RenderPagePngResult {
  readonly documentId: string;
  readonly sheetIndex: number;
  readonly pageIndex: number;
  readonly dpi: number;
  readonly mimeType: "image/png";
  readonly bytes: Uint8Array;
}

export type RenderOperation =
  | "capabilities"
  | "open"
  | "close"
  | "prepare-pages"
  | "render-sheet"
  | "render-tile"
  | "render-page"
  | "render-page-png";

export interface RenderOperationPayloads {
  readonly capabilities: Readonly<Record<string, never>>;
  readonly open: {
    readonly documentId: string;
    readonly bytes: RenderBinary;
    readonly fontPack?: FontPack;
  };
  readonly close: { readonly documentId: string };
  readonly "prepare-pages": {
    readonly documentId: string;
    readonly sheetIndex: number;
    readonly options?: RenderOptions;
  };
  readonly "render-sheet": {
    readonly documentId: string;
    readonly sheetIndex: number;
    readonly options?: RenderOptions;
  };
  readonly "render-tile": {
    readonly documentId: string;
    readonly sheetIndex: number;
    readonly range: RenderRange;
    readonly options?: RenderOptions;
  };
  readonly "render-page": {
    readonly documentId: string;
    readonly sheetIndex: number;
    readonly pageIndex: number;
    readonly options?: RenderOptions;
  };
  readonly "render-page-png": {
    readonly documentId: string;
    readonly sheetIndex: number;
    readonly pageIndex: number;
    readonly dpi?: number;
    readonly options?: RenderOptions;
  };
}

export interface RenderOperationResults {
  readonly capabilities: RenderCapabilities;
  readonly open: OpenDocumentResult;
  readonly close: CloseDocumentResult;
  readonly "prepare-pages": PreparePagesResult;
  readonly "render-sheet": RenderSheetResult;
  readonly "render-tile": RenderTileResult;
  readonly "render-page": RenderPageResult;
  readonly "render-page-png": RenderPagePngResult;
}

export type RenderWorkerRequestMessage<
  Operation extends RenderOperation = RenderOperation,
> = Operation extends RenderOperation
  ? {
      readonly protocol: typeof PROTOCOL;
      readonly type: "request";
      readonly requestId: string;
      readonly operation: Operation;
      readonly payload: RenderOperationPayloads[Operation];
    }
  : never;

export interface RenderWorkerCancelMessage {
  readonly protocol: typeof PROTOCOL;
  readonly type: "cancel";
  readonly requestId: string;
}

export interface ParsedRenderWorkerRequestMessage {
  readonly protocol: typeof PROTOCOL;
  readonly type: "request";
  readonly requestId: string;
  readonly operation: RenderOperation;
  readonly payload: Readonly<Record<string, unknown>>;
}

export interface RenderWorkerReadyMessage {
  readonly protocol: typeof PROTOCOL;
  readonly type: "ready";
  readonly capabilities: RenderCapabilities;
}

export interface RenderWorkerProgressMessage {
  readonly protocol: typeof PROTOCOL;
  readonly type: "progress";
  readonly requestId: string;
  readonly completed: number;
  readonly total: number;
  readonly stage: string;
}

export type RenderWorkerSuccessMessage<
  Operation extends RenderOperation = RenderOperation,
> = Operation extends RenderOperation
  ? {
      readonly protocol: typeof PROTOCOL;
      readonly type: "result";
      readonly requestId: string;
      readonly ok: true;
      readonly result: RenderOperationResults[Operation];
      readonly error: null;
    }
  : never;

export interface RenderWorkerFailureMessage {
  readonly protocol: typeof PROTOCOL;
  readonly type: "result";
  readonly requestId: string;
  readonly ok: false;
  readonly result: null;
  readonly error: RenderErrorPayload;
}

export type RenderWorkerResultMessage =
  | RenderWorkerSuccessMessage
  | RenderWorkerFailureMessage;
export type RenderWorkerOutgoingMessage =
  | RenderWorkerReadyMessage
  | RenderWorkerProgressMessage
  | RenderWorkerResultMessage;

export declare function asBytes(value: RenderBinary, location?: string): Uint8Array;
export declare function validateRequestId(value: unknown): string;
export declare function validateDocumentId(value: unknown): string;
export declare function parseWorkerMessage(
  message: unknown,
): ParsedRenderWorkerRequestMessage | RenderWorkerCancelMessage;
export declare function preflightRequest<Operation extends RenderOperation>(request: {
  readonly operation: Operation;
  readonly payload: RenderOperationPayloads[Operation];
}): number;
export declare function preflightRequest(request: ParsedRenderWorkerRequestMessage): number;
export declare function validateRange(value: unknown): RenderRange;
export declare function boundedIndex(
  value: unknown,
  location: string,
  countLimit: number,
  resource: string,
): number;
export declare function positiveInteger(value: unknown, location: string): number;
export declare function optionsJson(options?: RenderOptions | RenderJsonValue): string;
export declare function encodeFontBundle(fontPack?: FontPack | null): Uint8Array;
export declare function fontPackByteLength(fontPack?: FontPack | null): number;
export declare function validateFontPack(fontPack: unknown): ValidatedFontPack;
export declare function validateSvgOutput(svg: unknown, maxBytes?: number): number;
export declare function normalizeError(error: unknown): RenderErrorPayload;
export declare function limitError(
  resource: string,
  limit: number,
  actual: number,
  location?: string,
): RenderProtocolError;
