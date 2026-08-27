use super::{i16le, resolve_encoding, u16le, u32le, u64le, Ctx};
use crate::model::{DocProperties, PageSetup, Sheet, SheetType};
use encoding_rs::{Encoding, WINDOWS_1252};

pub(super) fn parse_window1_active_sheet(data: &[u8]) -> Option<usize> {
    u16le(data, 10).map(usize::from)
}

pub(super) fn parse_ole_doc_properties(bytes: &[u8]) -> DocProperties {
    let mut properties = DocProperties::default();
    if let Some(summary) = crate::ole::read_optional_stream(
        bytes,
        &["/\u{0005}SummaryInformation", "\u{0005}SummaryInformation"],
    ) {
        for (id, value) in property_strings(&summary) {
            match id {
                2 => properties.title = Some(value),
                3 => properties.subject = Some(value),
                4 => properties.creator = Some(value),
                5 => properties.keywords = Some(value),
                6 => properties.description = Some(value),
                8 => properties.last_modified_by = Some(value),
                12 => properties.created = Some(value),
                13 if properties.created.is_none() => properties.created = Some(value),
                _ => {}
            }
        }
    }
    if let Some(doc_summary) = crate::ole::read_optional_stream(
        bytes,
        &[
            "/\u{0005}DocumentSummaryInformation",
            "\u{0005}DocumentSummaryInformation",
        ],
    ) {
        for (id, value) in property_strings(&doc_summary) {
            if id == 15 {
                properties.company = Some(value);
            }
        }
    }
    properties
}

fn property_strings(data: &[u8]) -> Vec<(u32, String)> {
    if u16le(data, 0) != Some(0xFFFE) {
        return Vec::new();
    }
    let set_count = (u32le(data, 24).unwrap_or(0) as usize).min(data.len().saturating_sub(28) / 20);
    let mut strings = Vec::new();
    for set_idx in 0..set_count {
        let Some(entry) = 28usize.checked_add(set_idx.saturating_mul(20)) else {
            continue;
        };
        let Some(offset_field) = entry.checked_add(16) else {
            continue;
        };
        let Some(section_offset) = u32le(data, offset_field).map(|offset| offset as usize) else {
            continue;
        };
        collect_property_section_strings(data, section_offset, &mut strings);
    }
    strings
}

fn collect_property_section_strings(
    data: &[u8],
    section_offset: usize,
    out: &mut Vec<(u32, String)>,
) {
    let Some(section_size) = u32le(data, section_offset).map(|size| size as usize) else {
        return;
    };
    let Some(section_end) = section_offset.checked_add(section_size) else {
        return;
    };
    if section_end > data.len() {
        return;
    }
    let max_entries = section_size.saturating_sub(8) / 8;
    let count = (u32le(data, section_offset + 4).unwrap_or(0) as usize).min(max_entries);
    let mut entries = Vec::new();
    for idx in 0..count {
        let Some(entry) = section_offset
            .checked_add(8)
            .and_then(|offset| offset.checked_add(idx.saturating_mul(8)))
        else {
            continue;
        };
        let Some(id) = u32le(data, entry) else {
            continue;
        };
        let Some(value_offset) = u32le(data, entry + 4).map(|offset| offset as usize) else {
            continue;
        };
        let Some(value_start) = section_offset.checked_add(value_offset) else {
            continue;
        };
        if value_start < section_end {
            entries.push((id, value_start));
        }
    }

    let mut encoding = WINDOWS_1252;
    for &(id, value_start) in &entries {
        if id == 1 && (u32le(data, value_start).unwrap_or(0) & 0xFFFF) == 0x0002 {
            if let Some(codepage) = u16le(data, value_start + 4) {
                if codepage != 1200 {
                    encoding = resolve_encoding(codepage);
                }
            }
        }
    }

    for (id, value_start) in entries {
        let value_type = u32le(data, value_start).unwrap_or(0) & 0xFFFF;
        let value = match value_type {
            0x001E => read_property_lpstr(data, value_start + 4, encoding),
            0x001F => read_property_lpwstr(data, value_start + 4),
            0x0040 => read_property_filetime(data, value_start + 4),
            _ => None,
        };
        if let Some(value) = value {
            out.push((id, value));
        }
    }
}

fn read_property_lpstr(
    data: &[u8],
    value_offset: usize,
    encoding: &'static Encoding,
) -> Option<String> {
    let len = u32le(data, value_offset)? as usize;
    let start = value_offset.checked_add(4)?;
    let end = start.checked_add(len)?;
    let bytes = data.get(start..end)?;
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    let (decoded, _, _) = encoding.decode(bytes);
    Some(decoded.into_owned())
}

fn read_property_lpwstr(data: &[u8], value_offset: usize) -> Option<String> {
    let chars = u32le(data, value_offset)? as usize;
    let start = value_offset.checked_add(4)?;
    let byte_len = chars.checked_mul(2)?;
    let end = start.checked_add(byte_len)?;
    let words = data
        .get(start..end)?
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .take_while(|word| *word != 0)
        .collect::<Vec<_>>();
    String::from_utf16(&words).ok()
}

fn read_property_filetime(data: &[u8], value_offset: usize) -> Option<String> {
    const FILETIME_TICKS_PER_SECOND: i128 = 10_000_000;
    const SECONDS_FROM_FILETIME_TO_UNIX_EPOCH: i128 = 11_644_473_600;

    let ticks = u64le(data, value_offset)? as i128;
    let unix_seconds = ticks / FILETIME_TICKS_PER_SECOND - SECONDS_FROM_FILETIME_TO_UNIX_EPOCH;
    let days = i64::try_from(unix_seconds.div_euclid(86_400)).ok()?;
    let seconds_of_day = unix_seconds.rem_euclid(86_400);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_from_unix_days(days);
    if !(1..=9999).contains(&year) {
        return None;
    }
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

fn civil_from_unix_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { y + 1 } else { y }, month, day)
}

pub(super) enum ParsedLbl {
    GlobalUser(RawDefinedName),
    LocalUser {
        sheet_index: usize,
        name: RawDefinedName,
    },
    SheetBuiltin(SheetBuiltinName),
}

pub(super) struct RawDefinedName {
    pub(super) name: String,
    pub(super) rgce: Vec<u8>,
    pub(super) rgb_extra: Vec<u8>,
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
    ranges: Vec<(u32, u16, u32, u16)>,
}

pub(super) fn parse_extern_sheets(data: &[u8]) -> Vec<crate::ptg::ExternSheet> {
    let count = usize::from(u16le(data, 0).unwrap_or(0));
    data.get(2..)
        .unwrap_or_default()
        .chunks_exact(6)
        .take(count)
        .filter_map(|xti| {
            Some(crate::ptg::ExternSheet {
                supbook_index: usize::from(u16le(xti, 0)?),
                first_sheet: i32::from(i16le(xti, 2)?),
                last_sheet: i32::from(i16le(xti, 4)?),
            })
        })
        .collect()
}

/// Parse the name payload of a BIFF5/8 `EXTERNNAME` record. The six-byte
/// prefix contains option/automatic-link metadata; the trailing value is the
/// same short string dialect used by `BOUNDSHEET` (a compressed-or-wide
/// `ShortXLUnicodeString` in BIFF8, codepage bytes in BIFF5/7).
pub(super) fn parse_extern_name(data: &[u8], ctx: Ctx) -> Option<String> {
    let name = read_short_string(data, 6, ctx)?;
    (!name.is_empty()).then_some(name)
}

pub(super) fn parse_lbl_formula_name(data: &[u8], ctx: Ctx) -> Option<String> {
    let flags = u16le(data, 0)?;
    let cch = usize::from(*data.get(3)?);
    if flags & 0x0020 != 0 {
        if cch != 1 {
            return None;
        }
        let id = *data.get(14)?;
        return Some(
            match id {
                0x00 => "Consolidate_Area",
                0x01 => "Auto_Open",
                0x02 => "Auto_Close",
                0x03 => "Extract",
                0x04 => "Database",
                0x05 => "Criteria",
                0x06 => "Print_Area",
                0x07 => "Print_Titles",
                0x08 => "Recorder",
                0x09 => "Data_Form",
                0x0A => "Auto_Activate",
                0x0B => "Auto_Deactivate",
                0x0C => "Sheet_Title",
                0x0D => "_FilterDatabase",
                _ => return Some(format!("BuiltinName{id:02X}")),
            }
            .to_string(),
        );
    }
    read_name_no_cch(data, 14, cch, ctx).map(|(name, _)| name)
}

/// Parse a workbook-global `Lbl` record. Workbook-global user names are surfaced
/// through `Workbook::defined_names`; selected sheet-local built-ins become
/// existing sheet metadata facades.
pub(super) fn parse_lbl(data: &[u8], ctx: Ctx) -> Option<ParsedLbl> {
    let flags = u16le(data, 0)?;
    let builtin = flags & 0x0020 != 0;
    let cch = *data.get(3)? as usize;
    let cce = u16le(data, 4)? as usize;
    let itab = u16le(data, 8)?;
    let (name, used) = if builtin {
        if cch != 1 {
            return None;
        }
        (builtin_name(*data.get(14)?)?, 1)
    } else {
        let (name, used) = read_name_no_cch(data, 14, cch, ctx)?;
        if name.is_empty() {
            return None;
        }
        (NameKind::User(name), used)
    };
    let rgce_start = 14usize.checked_add(used)?;
    let rgce = data.get(rgce_start..rgce_start.checked_add(cce)?)?;
    match name {
        NameKind::User(name) => {
            let rgce_end = rgce_start.checked_add(cce)?;
            let raw = RawDefinedName {
                name,
                rgce: rgce.to_vec(),
                rgb_extra: data.get(rgce_end..).unwrap_or_default().to_vec(),
            };
            if itab == 0 {
                Some(ParsedLbl::GlobalUser(raw))
            } else {
                Some(ParsedLbl::LocalUser {
                    sheet_index: usize::from(itab - 1),
                    name: raw,
                })
            }
        }
        NameKind::Builtin(kind) => {
            let sheet_index = usize::from(itab.checked_sub(1)?);
            let ranges = parse_lbl_ranges(rgce)?;
            Some(ParsedLbl::SheetBuiltin(SheetBuiltinName {
                sheet_index,
                kind,
                ranges,
            }))
        }
    }
}

enum NameKind {
    User(String),
    Builtin(SheetBuiltinKind),
}

fn builtin_name(id: u8) -> Option<NameKind> {
    match id {
        0x06 => Some(NameKind::Builtin(SheetBuiltinKind::PrintArea)),
        0x07 => Some(NameKind::Builtin(SheetBuiltinKind::PrintTitles)),
        0x0D => Some(NameKind::Builtin(SheetBuiltinKind::FilterDatabase)),
        _ => None,
    }
}

pub(super) fn apply_sheet_builtin_names(sheets: &mut [Sheet], names: Vec<SheetBuiltinName>) {
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
                    apply_print_title_range(setup, range);
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

fn apply_print_title_range(setup: &mut PageSetup, range: (u32, u16, u32, u16)) {
    let (r0, c0, r1, c1) = range;
    if c0 == 0 && c1 >= 255 {
        setup.repeat_rows = Some((r0, r1));
    }
    if r0 == 0 && r1 >= u32::from(u16::MAX) {
        setup.repeat_cols = Some((c0, c1));
    }
}

fn parse_lbl_ranges(rgce: &[u8]) -> Option<Vec<(u32, u16, u32, u16)>> {
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    while offset < rgce.len() {
        let token = rgce[offset];
        match token {
            0x24 | 0x44 | 0x64 => {
                let (row, col) = parse_lbl_ref(rgce, offset + 1)?;
                ranges.push((row, col, row, col));
                offset += 5;
            }
            0x1A | 0x3A | 0x5A | 0x7A => {
                let (row, col) = parse_lbl_ref(rgce, offset + 3)?;
                ranges.push((row, col, row, col));
                offset += 7;
            }
            0x25 | 0x45 | 0x65 => {
                ranges.push(parse_lbl_area(rgce, offset + 1)?);
                offset += 9;
            }
            0x1B | 0x3B | 0x5B | 0x7B => {
                ranges.push(parse_lbl_area(rgce, offset + 3)?);
                offset += 11;
            }
            0x10 => offset += 1, // PtgUnion
            _ => return None,
        }
    }
    (!ranges.is_empty()).then_some(ranges)
}

fn parse_lbl_ref(rgce: &[u8], offset: usize) -> Option<(u32, u16)> {
    let row = u32::from(u16le(rgce, offset)?);
    let col = u16le(rgce, offset + 2)? & 0x3FFF;
    Some((row, col))
}

fn parse_lbl_area(rgce: &[u8], offset: usize) -> Option<(u32, u16, u32, u16)> {
    let r0 = u32::from(u16le(rgce, offset)?);
    let r1 = u32::from(u16le(rgce, offset + 2)?);
    let c0 = u16le(rgce, offset + 4)? & 0x3FFF;
    let c1 = u16le(rgce, offset + 6)? & 0x3FFF;
    Some((r0.min(r1), c0.min(c1), r0.max(r1), c0.max(c1)))
}

/// Parse a `BOUNDSHEET` record into `(sheet name, sheet_type, hidden,
/// very_hidden)`.
pub(super) fn parse_boundsheet(data: &[u8], ctx: Ctx) -> (String, SheetType, bool, bool) {
    // lbPlyPos(4), hsState(1), dt(1), then sheet name string.
    // hsState low 2 bits: 0 = visible, 1 = hidden, 2 = veryHidden ([MS-XLS] 2.4.28).
    let hs_state = data.get(4).copied().unwrap_or(0) & 0x03;
    let sheet_type = match data.get(5).copied().unwrap_or(0) {
        0x00 => SheetType::WorkSheet,
        0x01 => SheetType::MacroSheet,
        0x02 => SheetType::ChartSheet,
        0x06 => SheetType::Vba,
        _ => SheetType::ChartSheet,
    };
    let name = read_short_string(data, 6, ctx).unwrap_or_default();
    (name, sheet_type, hs_state == 1, hs_state == 2)
}

/// Sheet-name / short string: cch:u8, then (BIFF8) grbit + char data, or
/// (BIFF5/7) raw codepage bytes.
pub(super) fn read_short_string(data: &[u8], off: usize, ctx: Ctx) -> Option<String> {
    let cch = *data.get(off)? as usize;
    if ctx.biff8 {
        let grbit = *data.get(off + 1)?;
        decode_chars(data, off + 2, cch, grbit)
    } else {
        let bytes = data.get(off + 1..off + 1 + cch)?;
        Some(ctx.enc.decode(bytes).0.into_owned())
    }
}

/// Cell string: cch:u16, then (BIFF8) grbit + char data, or (BIFF5/7) raw
/// codepage bytes.
pub(super) fn read_xl_string(data: &[u8], off: usize, ctx: Ctx) -> Option<String> {
    let cch = u16le(data, off)? as usize;
    if ctx.biff8 {
        let grbit = *data.get(off + 2)?;
        decode_chars(data, off + 3, cch, grbit)
    } else {
        let bytes = data.get(off + 2..off + 2 + cch)?;
        Some(ctx.enc.decode(bytes).0.into_owned())
    }
}

/// Name string without the leading `cch`: BIFF8 stores a grbit byte before the
/// characters; BIFF5/7 stores raw codepage bytes.
fn read_name_no_cch(data: &[u8], off: usize, cch: usize, ctx: Ctx) -> Option<(String, usize)> {
    if ctx.biff8 {
        let grbit = *data.get(off)?;
        let char_bytes = if grbit & 0x01 != 0 {
            cch.checked_mul(2)?
        } else {
            cch
        };
        let s = decode_chars(data, off + 1, cch, grbit)?;
        Some((s, 1 + char_bytes))
    } else {
        let bytes = data.get(off..off + cch)?;
        Some((ctx.enc.decode(bytes).0.into_owned(), cch))
    }
}

/// Decode `cch` BIFF8 characters at `off`, compressed (Latin-1) or UTF-16LE
/// per the grbit `fHighByte` bit.
pub(super) fn decode_chars(data: &[u8], off: usize, cch: usize, grbit: u8) -> Option<String> {
    if grbit & 0x01 != 0 {
        let units: Vec<u16> = data
            .get(off..off + cch.checked_mul(2)?)?
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(String::from_utf16_lossy(&units))
    } else {
        let bytes = data.get(off..off + cch)?;
        Some(bytes.iter().map(|&b| b as char).collect())
    }
}
