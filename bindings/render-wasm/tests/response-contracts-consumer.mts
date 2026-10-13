import { createResponseValidators } from "../js/response-contracts.mjs";

const validators = createResponseValidators({
  invalid: (message) => new Error(message),
  maxSheets: 255,
  maxOutputBytes: 16 * 1024 * 1024,
  maxHistoryEntries: 20,
  maxHistoryBytes: 32 * 1024 * 1024
});
const input: unknown = { kind: "blank" };
const validated: unknown = validators.validateInspectedCell(input);
void validated;
const cell = { kind: "number" as const, value: 42 };
const same: typeof cell = validators.validateInspectedCell(cell);
void same;
// @ts-expect-error The internal validator set is read-only.
validators.validateEditState = (value: unknown) => value;
// @ts-expect-error Limits are numbers, not numeric strings.
createResponseValidators({ invalid: (message) => new Error(message), maxSheets: "255", maxOutputBytes: 10, maxHistoryEntries: 20, maxHistoryBytes: 32 });
