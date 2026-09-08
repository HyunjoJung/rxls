import { gridCellReference, inlineCellValue } from "./grid-editor.js";

export const MAX_PASTE_BYTES = 1024 * 1024;
export const MAX_PASTE_CELLS = 10_000;
const encoder = new TextEncoder();

function checkSize(text) {
  if (
    text.length > MAX_PASTE_BYTES ||
    encoder.encode(text).length > MAX_PASTE_BYTES
  )
    throw new RangeError("Clipboard size exceeds the 1 MiB paste limit.");
}

/** Decode spreadsheet TSV without interpreting HTML or requesting clipboard access. */
export function parseClipboardRange(text) {
  if (typeof text !== "string" || !text.length)
    throw new Error("The clipboard is empty.");
  checkSize(text);
  const rows = [];
  let row = [];
  let value = "";
  let quoted = false;
  let closed = false;
  let fieldStart = true;
  let cellCount = 0;
  const field = () => {
    if (++cellCount > MAX_PASTE_CELLS)
      throw new RangeError("Paste is limited to 10,000 cells.");
    row.push(value);
    value = "";
    closed = false;
    fieldStart = true;
  };
  const record = () => {
    field();
    if (rows.length && row.length !== rows[0].length)
      throw new Error("The clipboard must contain a rectangular range.");
    rows.push(row);
    row = [];
  };
  let endedRecord = false;
  for (let i = 0; i < text.length; i++) {
    const char = text[i];
    endedRecord = false;
    if (quoted) {
      if (char === '"') {
        if (text[i + 1] === '"') {
          value += '"';
          i++;
        } else {
          quoted = false;
          closed = true;
        }
      } else if (char === "\r") {
        if (text[i + 1] === "\n") i++;
        value += "\n";
      } else value += char;
    } else if (char === "\t") field();
    else if (char === "\n" || char === "\r") {
      if (char === "\r" && text[i + 1] === "\n") i++;
      record();
      endedRecord = true;
    } else if (closed)
      throw new Error("Unexpected text after a clipboard quote.");
    else if (fieldStart && char === '"') {
      quoted = true;
      fieldStart = false;
    } else {
      value += char;
      fieldStart = false;
    }
  }
  if (quoted) throw new Error("The clipboard contains an unclosed quote.");
  if (!endedRecord) record();
  return rows;
}

/** Build a bounded typed transaction; a single-text fallback is always explicit. */
export function rangePasteRequest(
  text,
  row,
  col,
  { asText = false, sheetIndex = 0 } = {},
) {
  if (typeof text !== "string" || !text.length)
    throw new Error("The clipboard is empty.");
  checkSize(text);
  const rows = asText
    ? [[text.replace(/\r\n?/g, "\n")]]
    : parseClipboardRange(text);
  if (
    !Number.isInteger(row) ||
    !Number.isInteger(col) ||
    row < 0 ||
    col < 0 ||
    row + rows.length > 1_048_576 ||
    col + rows[0].length > 16_384
  )
    throw new RangeError("The pasted range exceeds the worksheet boundaries.");
  const values = rows.map((items) =>
    items.map((value) =>
      asText ? { kind: "text", value } : inlineCellValue(value),
    ),
  );
  checkSize(
    JSON.stringify({ sheetIndex, startRow: row, startCol: col, values }),
  );
  const start = gridCellReference(row, col);
  const end = gridCellReference(
    row + rows.length - 1,
    col + rows[0].length - 1,
  );
  return {
    rows,
    values,
    cellCount: rows.length * rows[0].length,
    reference: start === end ? start : `${start}:${end}`,
  };
}
