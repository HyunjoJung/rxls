use super::super::style::Styles;
use super::super::SharedString;
use crate::{format, Cell, CellEntry, CellStyle};

#[allow(clippy::too_many_arguments)]
pub(super) fn build_cell(
    row: u32,
    col: u16,
    ctype: &str,
    style_idx: usize,
    value: &str,
    formula: &str,
    shared: &[SharedString],
    styles: &Styles,
    date1904: bool,
) -> Option<CellEntry> {
    // The cached value (the displayed result), if one is present and parseable.
    let cached: Option<(Cell, String)> = match ctype {
        "s" => value
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|idx| shared.get(idx))
            .map(|shared| {
                (
                    Cell::Text(shared.text.clone()),
                    styles.render_text(style_idx, &shared.text),
                )
            }),
        "str" | "inlineStr" if !value.is_empty() => Some((
            Cell::Text(value.to_string()),
            styles.render_text(style_idx, value),
        )),
        "b" if !value.trim().is_empty() => {
            let b = value.trim() == "1";
            Some((Cell::Bool(b), if b { "TRUE" } else { "FALSE" }.to_string()))
        }
        "e" if !value.is_empty() => Some((Cell::Error(value.to_string()), value.to_string())),
        // ISO-8601 date/time cell (`t="d"`, emitted by some non-Excel writers).
        "d" if !value.is_empty() => format::iso_date_to_serial(value).map(|serial| {
            let kind = styles.kind(style_idx);
            let display = if let Some(code) = styles.custom_format(style_idx) {
                format::render_format(serial, code, false)
            } else if kind.is_datetime() {
                format::render_value(serial, kind, false)
            } else {
                value.to_string()
            };
            (Cell::Date(serial), display)
        }),
        "str" | "inlineStr" | "b" | "e" | "d" => None,
        // "" or "n" → number.
        _ => value.trim().parse::<f64>().ok().map(|f| {
            let kind = styles.kind(style_idx);
            let display = styles.custom_format(style_idx).map_or_else(
                || format::render_indexed(f, styles.format_id(style_idx), date1904),
                |code| format::render_format(f, code, date1904),
            );
            let cell = if kind.is_datetime() {
                Cell::Date(f)
            } else {
                Cell::Number(f)
            };
            (cell, display)
        }),
    };

    // A `<f>` makes this a formula cell: surface the formula source via
    // `Cell::Formula` even when no cached value is present (an uncalculated
    // formula), keeping the cached value as the display text when there is one.
    let (value, text) = match (cached, formula.is_empty()) {
        (Some((cell, text)), true) => (cell, text),
        (Some((cell, text)), false) => (
            Cell::Formula {
                formula: formula.to_string(),
                cached: Box::new(cell),
            },
            text,
        ),
        (None, false) => (
            Cell::Formula {
                formula: formula.to_string(),
                cached: Box::new(Cell::Text(String::new())),
            },
            String::new(),
        ),
        (None, true) => return None,
    };
    Some(CellEntry {
        row,
        col,
        value,
        text,
        style: styles.cell_style(style_idx).cloned(),
        xlsx_font_size_pt: styles.xlsx_cell_font_size_pt(style_idx),
        hyperlink: None,
    })
}

// Architecture-neutral retained-allocation charges. The record allowance is
// deliberately 256 bytes: it exceeds the current 64-bit `CellEntry` layout and
// still permits one million short numeric cells within the 256 MiB workbook
// budget. The boxed-cell allowance likewise exceeds the current 64-bit `Cell`
// layout plus allocator bookkeeping. Compile-time assertions keep future model
// growth from silently invalidating those conservative bounds on any target.
pub(in crate::xlsx) const RETAINED_CELL_RECORD_BYTES: usize = 256;
const RETAINED_BOXED_CELL_BYTES: usize = 64;
const _: () = assert!(std::mem::size_of::<CellEntry>() <= RETAINED_CELL_RECORD_BYTES);
const _: () = assert!(std::mem::size_of::<Cell>() <= RETAINED_BOXED_CELL_BYTES);

pub(in crate::xlsx) fn retained_cell_cost(entry: &CellEntry) -> usize {
    RETAINED_CELL_RECORD_BYTES
        .saturating_add(entry.text.len())
        .saturating_add(retained_cell_value_heap_bytes(&entry.value))
        // Direct OOXML formats are retained both on the cell and in the
        // cell-XF overlay map. Charge both variable string copies; the fixed
        // record allowance covers their non-allocating structures.
        .saturating_add(retained_cell_style_heap_bytes(entry.style.as_ref()).saturating_mul(2))
}

fn retained_cell_value_heap_bytes(mut value: &Cell) -> usize {
    let mut bytes = 0usize;
    loop {
        match value {
            Cell::Text(source) | Cell::Error(source) => {
                return bytes.saturating_add(source.len());
            }
            Cell::Formula { formula, cached } => {
                bytes = bytes
                    .saturating_add(formula.len())
                    .saturating_add(RETAINED_BOXED_CELL_BYTES);
                value = cached;
            }
            Cell::Number(_) | Cell::Date(_) | Cell::Bool(_) => return bytes,
        }
    }
}

fn retained_cell_style_heap_bytes(style: Option<&CellStyle>) -> usize {
    style.map_or(0, |style| {
        style
            .font
            .as_ref()
            .and_then(|font| font.name.as_deref())
            .map(str::len)
            .unwrap_or(0)
            .saturating_add(style.num_fmt.as_deref().map(str::len).unwrap_or(0))
    })
}
