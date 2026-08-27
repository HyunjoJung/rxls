use std::collections::BTreeMap;

use crate::{format, rk_to_f64, Cell, CellEntry};

use super::style::Styles;
use super::{
    u32le, wide_string, BrtFormulaDefinition, BrtFormulaDefinitions, SharedString, BRT_ARR_FMLA,
    BRT_CELL_BOOL, BRT_CELL_ERROR, BRT_CELL_ISST, BRT_CELL_REAL, BRT_CELL_RK, BRT_CELL_ST,
    BRT_FMLA_BOOL, BRT_FMLA_ERROR, BRT_FMLA_NUM, BRT_FMLA_STRING, BRT_SHR_FMLA, MAX_XLSB_COL_INDEX,
    MAX_XLSB_ROW_INDEX,
};

pub(super) fn parse_brt_parsed_formula(p: &[u8], offset: usize) -> Option<(&[u8], &[u8])> {
    let cce = usize::try_from(u32le(p, offset)?).ok()?;
    let rgce_start = offset.checked_add(4)?;
    let rgce_end = rgce_start.checked_add(cce)?;
    let rgce = p.get(rgce_start..rgce_end)?;
    let cb = usize::try_from(u32le(p, rgce_end)?).ok()?;
    let extra_start = rgce_end.checked_add(4)?;
    let extra_end = extra_start.checked_add(cb)?;
    Some((rgce, p.get(extra_start..extra_end)?))
}

pub(super) fn parse_brt_formula_definition(
    rt: u32,
    p: &[u8],
    last_formula_cell: Option<(u32, u16)>,
) -> Option<BrtFormulaDefinition> {
    let row_first = u32le(p, 0)?;
    let row_last = u32le(p, 4)?;
    let col_first = u16::try_from(u32le(p, 8)?).ok()?;
    let col_last = u16::try_from(u32le(p, 12)?).ok()?;
    if row_first > row_last
        || col_first > col_last
        || row_last > MAX_XLSB_ROW_INDEX
        || u32::from(col_last) > MAX_XLSB_COL_INDEX
    {
        return None;
    }
    let (is_array, formula_offset) = match rt {
        BRT_ARR_FMLA => (true, 17),
        BRT_SHR_FMLA => (false, 16),
        _ => return None,
    };
    let (rgce, rgb_extra) = parse_brt_parsed_formula(p, formula_offset)?;
    let anchor = if is_array {
        (row_first, col_first)
    } else {
        last_formula_cell.unwrap_or((row_first, col_first))
    };
    Some(BrtFormulaDefinition {
        anchor,
        range: (row_first, col_first, row_last, col_last),
        rgce: rgce.to_vec(),
        rgb_extra: rgb_extra.to_vec(),
        is_array,
    })
}

fn decompile_brt_formula_source(
    rgce: &[u8],
    rgb_extra: &[u8],
    context: &crate::ptg::Context<'_>,
    definitions: &BrtFormulaDefinitions,
) -> Option<String> {
    let (tokens, extra, base_row, base_col) =
        if let Some(anchor) = crate::ptg::exp_anchor(rgce, rgb_extra, true) {
            let definition = definitions.get(&anchor)?;
            let (row_first, col_first, row_last, col_last) = definition.range;
            if context.base_row < row_first
                || context.base_row > row_last
                || context.base_col < col_first
                || context.base_col > col_last
            {
                return None;
            }
            let base = if definition.is_array {
                definition.anchor
            } else {
                (context.base_row, context.base_col)
            };
            (
                definition.rgce.as_slice(),
                definition.rgb_extra.as_slice(),
                base.0,
                base.1,
            )
        } else {
            (rgce, rgb_extra, context.base_row, context.base_col)
        };
    let resolved = crate::ptg::Context {
        base_row,
        base_col,
        ..*context
    };
    let formula = crate::ptg::decompile_parsed_with_context(tokens, extra, &resolved);
    (!formula.is_empty()).then_some(formula)
}

pub(super) fn apply_brt_formula_definition(
    definition: &BrtFormulaDefinition,
    cells: &mut [CellEntry],
    sheet_names: &[String],
    extern_sheets: &[crate::ptg::ExternSheet],
    external_names: &[Vec<String>],
    defined_names: &[String],
) {
    let context = crate::ptg::Context {
        biff12: true,
        biff5: false,
        name_formula: false,
        base_row: definition.anchor.0,
        base_col: definition.anchor.1,
        sheet_names,
        extern_sheets,
        external_names,
        defined_names,
    };
    let formula = crate::ptg::decompile_parsed_with_context(
        &definition.rgce,
        &definition.rgb_extra,
        &context,
    );
    if formula.is_empty() {
        return;
    }
    if let Some(cell) = cells
        .iter_mut()
        .rev()
        .find(|cell| (cell.row, cell.col) == definition.anchor)
    {
        match &mut cell.value {
            Cell::Formula {
                formula: source, ..
            } => *source = formula,
            cached => {
                cell.value = Cell::Formula {
                    formula,
                    cached: Box::new(cached.clone()),
                };
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_cell(
    rt: u32,
    p: &[u8],
    col: u16,
    style_idx: usize,
    row: u32,
    shared: &[SharedString],
    styles: &Styles,
    date1904: bool,
    cells: &mut Vec<CellEntry>,
    xlsb_cell_font_sizes_pt: &mut Vec<Option<u16>>,
    rich: &mut BTreeMap<(u32, u16), Vec<crate::TextRun>>,
    budget: &mut usize,
    sheet_names: &[String],
    extern_sheets: &[crate::ptg::ExternSheet],
    external_names: &[Vec<String>],
    defined_names: &[String],
    formula_definitions: &BrtFormulaDefinitions,
) {
    let formula_context = crate::ptg::Context {
        biff12: true,
        biff5: false,
        name_formula: false,
        base_row: row,
        base_col: col,
        sheet_names,
        extern_sheets,
        external_names,
        defined_names,
    };
    let mut push = |cells: &mut Vec<CellEntry>, budget: &mut usize, value: Cell, text: String| {
        if text.len() > *budget {
            *budget = 0;
            return;
        }
        *budget -= text.len();
        cells.push(CellEntry {
            row,
            col,
            value,
            text,
            style: (style_idx != 0)
                .then(|| styles.cell_style(style_idx).cloned())
                .flatten(),
            xlsx_font_size_pt: None,
            hyperlink: None,
        });
        xlsb_cell_font_sizes_pt.push(styles.xlsb_cell_font_size_pt(style_idx));
    };
    let mut number = |f: f64, cells: &mut Vec<CellEntry>, budget: &mut usize| {
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
        push(cells, budget, cell, display);
    };
    match rt {
        BRT_CELL_REAL => {
            if let Some(s) = p.get(8..16) {
                number(
                    f64::from_le_bytes(s.try_into().unwrap_or([0; 8])),
                    cells,
                    budget,
                );
            }
        }
        BRT_FMLA_NUM => {
            if let Some(s) = p.get(8..16) {
                let f = f64::from_le_bytes(s.try_into().unwrap_or([0; 8]));
                let kind = styles.kind(style_idx);
                let display = styles.custom_format(style_idx).map_or_else(
                    || format::render_indexed(f, styles.format_id(style_idx), date1904),
                    |code| format::render_format(f, code, date1904),
                );
                let cached = if kind.is_datetime() {
                    Cell::Date(f)
                } else {
                    Cell::Number(f)
                };
                push(
                    cells,
                    budget,
                    wrap_fmla(p, 16, cached, &formula_context, formula_definitions),
                    display,
                );
            }
        }
        BRT_CELL_RK => {
            if let Some(rk) = u32le(p, 8) {
                number(rk_to_f64(rk), cells, budget);
            }
        }
        BRT_CELL_ISST => {
            if let Some(isst) = u32le(p, 8) {
                if let Some(s) = shared.get(isst as usize) {
                    let display = styles.render_text(style_idx, &s.text);
                    push(cells, budget, Cell::Text(s.text.clone()), display);
                    if !s.runs.is_empty() {
                        rich.insert((row, col), s.runs.clone());
                    }
                }
            }
        }
        BRT_CELL_ST => {
            if let Some((s, _)) = wide_string(p, 8) {
                let display = styles.render_text(style_idx, &s);
                push(cells, budget, Cell::Text(s), display);
            }
        }
        BRT_FMLA_STRING => {
            if let Some((s, used)) = wide_string(p, 8) {
                let display = styles.render_text(style_idx, &s);
                push(
                    cells,
                    budget,
                    wrap_fmla(
                        p,
                        8 + used,
                        Cell::Text(s.clone()),
                        &formula_context,
                        formula_definitions,
                    ),
                    display,
                );
            }
        }
        BRT_CELL_BOOL => {
            let b = p.get(8).copied().unwrap_or(0) != 0;
            push(
                cells,
                budget,
                Cell::Bool(b),
                if b { "TRUE" } else { "FALSE" }.to_string(),
            );
        }
        BRT_FMLA_BOOL => {
            let b = p.get(8).copied().unwrap_or(0) != 0;
            let text = if b { "TRUE" } else { "FALSE" }.to_string();
            push(
                cells,
                budget,
                wrap_fmla(p, 9, Cell::Bool(b), &formula_context, formula_definitions),
                text,
            );
        }
        BRT_CELL_ERROR => {
            let code = crate::error_code(p.get(8).copied().unwrap_or(0)).to_string();
            push(cells, budget, Cell::Error(code.clone()), code);
        }
        BRT_FMLA_ERROR => {
            let code = crate::error_code(p.get(8).copied().unwrap_or(0)).to_string();
            push(
                cells,
                budget,
                wrap_fmla(
                    p,
                    9,
                    Cell::Error(code.clone()),
                    &formula_context,
                    formula_definitions,
                ),
                code,
            );
        }
        _ => {}
    }
}

/// Wrap a cached value as `Cell::Formula` by decoding the `BrtFmla*` formula that
/// follows it. Layout after the cached value: `grbitFlags:u16`, then a
/// `CellParsedFormula` (`cce:u32`, `rgce[cce]`, …). `value_end` is the byte offset
/// just past the cached value. Falls back to the bare cached value if the rgce is
/// absent or decompiles to nothing.
fn wrap_fmla(
    p: &[u8],
    value_end: usize,
    cached: Cell,
    context: &crate::ptg::Context<'_>,
    formula_definitions: &BrtFormulaDefinitions,
) -> Cell {
    let Some((rgce, rgb_extra)) = parse_brt_parsed_formula(p, value_end.saturating_add(2)) else {
        return cached;
    };
    let Some(f) = decompile_brt_formula_source(rgce, rgb_extra, context, formula_definitions)
    else {
        return cached;
    };
    if f.is_empty() {
        cached
    } else {
        Cell::Formula {
            formula: f,
            cached: Box::new(cached),
        }
    }
}
