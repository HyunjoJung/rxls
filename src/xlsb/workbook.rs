use crate::{PageSetup, Sheet};

use super::cell::parse_brt_parsed_formula;
use super::{
    i32le, u16le, u32le, wide_string, DefinedNames, RecReader, SheetRange, WorkbookSheets,
    BRT_BOOK_PROTECTION, BRT_BOOK_VIEW, BRT_BUNDLE_SH, BRT_EXTERN_SHEET, BRT_NAME, BRT_WB_PROP,
    MAX_XLSB_COL_INDEX, MAX_XLSB_ROW_INDEX,
};

/// Returns the `(sheet name, rel id, hsState)` triples, whether the workbook
/// uses the 1904 date system (`BrtWbProp` bit 0, matching calamine and
/// `[MS-XLSB]`), the active sheet index from `BrtBookView.itabCur`, workbook
/// defined names, workbook structure protection from `BrtBookProtection`, and
/// selected sheet-local built-in names used by sheet metadata facades.
/// `hsState` is the sheet visibility (0 = visible, 1 = hidden, 2 = veryHidden).
#[allow(clippy::type_complexity)]
pub(super) fn parse_workbook(
    b: &[u8],
    external_names: &[Vec<String>],
) -> (
    WorkbookSheets,
    bool,
    Option<usize>,
    DefinedNames,
    bool,
    Vec<SheetBuiltinName>,
    Vec<crate::ptg::ExternSheet>,
    Vec<String>,
    Vec<crate::LocalDefinedName>,
) {
    let mut out = Vec::new();
    let mut defined_names = Vec::new();
    let mut raw_defined_names = Vec::new();
    let mut raw_local_defined_names = Vec::new();
    let mut sheet_builtin_names = Vec::new();
    let mut date1904 = false;
    let mut active_sheet = None;
    let mut protect_structure = false;
    let mut extern_sheets = Vec::new();
    let mut formula_names = Vec::new();
    let mut r = RecReader::new(b);
    while let Some((rt, p)) = r.next() {
        if rt == BRT_BUNDLE_SH {
            // hsState:u32, iTabID:u32, strRelID: XLNullableWideString, strName: XLWideString.
            let hs_state = u32le(p, 0).unwrap_or(0);
            let Some((rid, used)) = nullable_wide(p, 8) else {
                continue;
            };
            if let Some((name, _)) = wide_string(p, 8 + used) {
                out.push((name, rid, hs_state));
            }
        } else if rt == BRT_WB_PROP {
            date1904 = p.first().is_some_and(|byte| byte & 0x1 != 0);
        } else if rt == BRT_BOOK_VIEW && active_sheet.is_none() {
            active_sheet = u32le(p, 24).and_then(|index| usize::try_from(index).ok());
        } else if rt == BRT_NAME {
            if let Some((name, _)) = wide_string(p, 9) {
                formula_names.push(name);
            }
            match parse_brt_name(p) {
                Some(ParsedBrtName::GlobalUser(name)) => raw_defined_names.push(name),
                Some(ParsedBrtName::LocalUser { sheet_index, name }) => {
                    raw_local_defined_names.push((sheet_index, name));
                }
                Some(ParsedBrtName::SheetBuiltin(name)) => sheet_builtin_names.push(name),
                None => {}
            }
        } else if rt == BRT_EXTERN_SHEET {
            extern_sheets = parse_brt_extern_sheets(p);
        } else if rt == BRT_BOOK_PROTECTION {
            protect_structure |= u16le(p, 4).is_some_and(|flags| flags & 0x0001 != 0);
        }
    }
    let sheet_names: Vec<String> = out.iter().map(|(name, _, _)| name.clone()).collect();
    defined_names.extend(raw_defined_names.into_iter().map(|name| {
        let context = crate::ptg::Context {
            biff12: true,
            biff5: false,
            name_formula: true,
            base_row: 0,
            base_col: 0,
            sheet_names: &sheet_names,
            extern_sheets: &extern_sheets,
            external_names,
            defined_names: &formula_names,
        };
        let refers_to =
            crate::ptg::decompile_parsed_with_context(&name.rgce, &name.rgb_extra, &context);
        (name.name, refers_to)
    }));
    let local_defined_names = raw_local_defined_names
        .into_iter()
        .filter_map(|(sheet_index, name)| {
            let sheet = sheet_names.get(sheet_index)?.clone();
            let context = crate::ptg::Context {
                biff12: true,
                biff5: false,
                name_formula: true,
                base_row: 0,
                base_col: 0,
                sheet_names: &sheet_names,
                extern_sheets: &extern_sheets,
                external_names,
                defined_names: &formula_names,
            };
            let refers_to =
                crate::ptg::decompile_parsed_with_context(&name.rgce, &name.rgb_extra, &context);
            Some(crate::LocalDefinedName {
                sheet,
                name: name.name,
                refers_to,
            })
        })
        .collect();
    (
        out,
        date1904,
        active_sheet,
        defined_names,
        protect_structure,
        sheet_builtin_names,
        extern_sheets,
        formula_names,
        local_defined_names,
    )
}

pub(super) fn parse_brt_extern_sheets(p: &[u8]) -> Vec<crate::ptg::ExternSheet> {
    let count = usize::try_from(u32le(p, 0).unwrap_or(0)).unwrap_or(0);
    p.get(4..)
        .unwrap_or_default()
        .chunks_exact(12)
        .take(count)
        .filter_map(|xti| {
            Some(crate::ptg::ExternSheet {
                supbook_index: usize::try_from(u32le(xti, 0)?).ok()?,
                first_sheet: i32le(xti, 4)?,
                last_sheet: i32le(xti, 8)?,
            })
        })
        .collect()
}

/// Parse a `BrtName` record. Workbook-global user names are surfaced through
/// `Workbook::defined_names`; selected sheet-local built-ins become existing
/// sheet metadata facades.
fn parse_brt_name(p: &[u8]) -> Option<ParsedBrtName> {
    let flags = u32le(p, 0)?;
    let built_in = flags & 0x20 != 0;
    let itab = u32le(p, 5)?;
    let (name, used) = wide_string(p, 9)?;
    let formula_start = 9usize.checked_add(used)?;
    let (rgce, rgb_extra) = parse_brt_parsed_formula(p, formula_start)?;
    if built_in {
        if itab == 0xFFFF_FFFF {
            return None;
        }
        let kind = xlsb_builtin_name(&name)?;
        let sheet_index = usize::try_from(itab).ok()?;
        let ranges = parse_brt_name_ranges(rgce)?;
        Some(ParsedBrtName::SheetBuiltin(SheetBuiltinName {
            sheet_index,
            kind,
            ranges,
        }))
    } else if !name.is_empty() {
        let raw = RawBrtDefinedName {
            name,
            rgce: rgce.to_vec(),
            rgb_extra: rgb_extra.to_vec(),
        };
        if itab == 0xFFFF_FFFF {
            Some(ParsedBrtName::GlobalUser(raw))
        } else {
            Some(ParsedBrtName::LocalUser {
                sheet_index: usize::try_from(itab).ok()?,
                name: raw,
            })
        }
    } else {
        None
    }
}

enum ParsedBrtName {
    GlobalUser(RawBrtDefinedName),
    LocalUser {
        sheet_index: usize,
        name: RawBrtDefinedName,
    },
    SheetBuiltin(SheetBuiltinName),
}

struct RawBrtDefinedName {
    name: String,
    rgce: Vec<u8>,
    rgb_extra: Vec<u8>,
}

#[derive(Clone, Copy)]
enum SheetBuiltinKind {
    PrintArea,
    PrintTitles,
    FilterDatabase,
}

pub(super) struct SheetBuiltinName {
    sheet_index: usize,
    kind: SheetBuiltinKind,
    ranges: Vec<SheetRange>,
}

fn xlsb_builtin_name(name: &str) -> Option<SheetBuiltinKind> {
    let lower = name.to_ascii_lowercase();
    let name = lower.strip_prefix("_xlnm.").unwrap_or(&lower);
    match name {
        "_filterdatabase" => Some(SheetBuiltinKind::FilterDatabase),
        "print_area" => Some(SheetBuiltinKind::PrintArea),
        "print_titles" => Some(SheetBuiltinKind::PrintTitles),
        _ => None,
    }
}

pub(super) fn apply_xlsb_sheet_builtin_names(sheets: &mut [Sheet], names: Vec<SheetBuiltinName>) {
    for name in names {
        let Some(sheet) = sheets.get_mut(name.sheet_index) else {
            continue;
        };
        match name.kind {
            SheetBuiltinKind::PrintArea => {
                let mut first = None;
                for range in name.ranges {
                    first.get_or_insert(range);
                    sheet.print_metadata.push_print_area(range);
                }
                if let Some(range) = first {
                    sheet
                        .page_setup
                        .get_or_insert_with(PageSetup::default)
                        .print_area = Some(range);
                }
            }
            SheetBuiltinKind::PrintTitles => {
                let setup = sheet.page_setup.get_or_insert_with(PageSetup::default);
                for range in name.ranges {
                    apply_xlsb_print_title_range(setup, range);
                }
            }
            SheetBuiltinKind::FilterDatabase => {
                if let Some(range) = name.ranges.into_iter().next() {
                    sheet.autofilter = Some(range);
                }
            }
        }
    }
}

fn apply_xlsb_print_title_range(setup: &mut PageSetup, range: SheetRange) {
    let (r0, c0, r1, c1) = range;
    if c0 == 0 && u32::from(c1) >= MAX_XLSB_COL_INDEX {
        setup.repeat_rows = Some((r0, r1));
    }
    if r0 == 0 && r1 >= MAX_XLSB_ROW_INDEX {
        setup.repeat_cols = Some((c0, c1));
    }
}

fn parse_brt_name_ranges(rgce: &[u8]) -> Option<Vec<SheetRange>> {
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    while offset < rgce.len() {
        let token = rgce[offset];
        match token {
            0x24 | 0x44 | 0x64 => {
                let (row, col) = parse_brt_name_ref(rgce, offset + 1)?;
                ranges.push((row, col, row, col));
                offset += 7;
            }
            0x1A | 0x3A | 0x5A | 0x7A => {
                let (row, col) = parse_brt_name_ref(rgce, offset + 3)?;
                ranges.push((row, col, row, col));
                offset += 9;
            }
            0x25 | 0x45 | 0x65 => {
                ranges.push(parse_brt_name_area(rgce, offset + 1)?);
                offset += 13;
            }
            0x1B | 0x3B | 0x5B | 0x7B => {
                ranges.push(parse_brt_name_area(rgce, offset + 3)?);
                offset += 15;
            }
            0x10 => offset += 1, // PtgUnion
            _ => return None,
        }
    }
    (!ranges.is_empty()).then_some(ranges)
}

fn parse_brt_name_ref(rgce: &[u8], offset: usize) -> Option<(u32, u16)> {
    let row = u32le(rgce, offset)?;
    let col = u16le(rgce, offset + 4)? & 0x3FFF;
    Some((row, col))
}

fn parse_brt_name_area(rgce: &[u8], offset: usize) -> Option<SheetRange> {
    let r0 = u32le(rgce, offset)?;
    let r1 = u32le(rgce, offset + 4)?;
    let c0 = u16le(rgce, offset + 8)? & 0x3FFF;
    let c1 = u16le(rgce, offset + 10)? & 0x3FFF;
    Some((r0.min(r1), c0.min(c1), r0.max(r1), c0.max(c1)))
}

/// An `XLNullableWideString`: `cch == 0xFFFFFFFF` means null (empty), else a
/// normal `XLWideString`. Returns `(string, bytes_consumed)`.
pub(super) fn nullable_wide(b: &[u8], o: usize) -> Option<(String, usize)> {
    let cch = u32le(b, o)?;
    if cch == 0xFFFF_FFFF {
        return Some((String::new(), 4));
    }
    wide_string(b, o)
}
