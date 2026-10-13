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
  validateInspectedCell<T>(value: T, depth?: number): T;
}>;
