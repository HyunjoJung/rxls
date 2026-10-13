import type { EditState, RecalculationSummary } from "./protocol.mjs";

/** Internal contract configuration; never exported through package subpaths. */
export function createResponseValidators(options: {
  readonly invalid: (message: string) => Error;
  readonly maxSheets: number;
  readonly maxOutputBytes: number;
  readonly maxHistoryEntries: number;
  readonly maxHistoryBytes: number;
}): Readonly<{
  validateWorkbookInspection<T>(value: T): T;
  validateDocumentProperties<T>(value: T): T;
  validateEditState<T>(value: T): T;
  validateRecalculationMatch(editState: EditState, summary: RecalculationSummary): void;
  validateInspectedCell<T>(value: T, depth?: number): T;
}>;
