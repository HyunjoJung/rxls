//! `.xlsb` (BIFF12 / Excel binary workbook) reading.
//!
//! A `.xlsb` is a ZIP of **binary** parts — `xl/workbook.bin`, `xl/sharedStrings.bin`,
//! `xl/styles.bin`, `xl/worksheets/sheetN.bin` — the same package shape as `.xlsx`
//! but with BIFF12 record streams instead of XML. A record is
//! `[recordType: var-uint][recordSize: var-uint][payload]`. This module decodes the
//! shared strings, number formats (for date detection), merged ranges, and the
//! common cell records into the shared [`Cell`] model, reusing the `.xlsx` ZIP
//! plumbing and the [`crate::format`] classifier. Panic-free / bounds-checked.
//!
//! Reference: [MS-XLSB]. Formula token decompilation is best-effort (cached values
//! are still read); cell `.bin` records beyond the common value kinds are skipped.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;

use crate::error::{Error, Result};
use crate::{
    format, rk_to_f64, Cell, CellEntry, Color, Comment, DataValidation, DvKind, DvOp, PageSetup,
    ProtectionOptions, Sheet, SheetType, Table, Workbook,
};

#[derive(Clone, Default)]
struct SharedString {
    text: String,
    runs: Vec<crate::TextRun>,
}

// BIFF12 record type ids ([MS-XLSB] 2.4).
const BRT_ROW_HDR: u32 = 0;
const BRT_CELL_RK: u32 = 2;
const BRT_CELL_ERROR: u32 = 3;
const BRT_CELL_BOOL: u32 = 4;
const BRT_CELL_REAL: u32 = 5;
const BRT_CELL_ST: u32 = 6;
const BRT_CELL_ISST: u32 = 7;
const BRT_FMLA_STRING: u32 = 8;
const BRT_FMLA_NUM: u32 = 9;
const BRT_FMLA_BOOL: u32 = 10;
const BRT_FMLA_ERROR: u32 = 11;
const BRT_SST_ITEM: u32 = 19;
const BRT_NAME: u32 = 39;
const BRT_SUP_BOOK_SRC: u32 = 355;
const BRT_SUP_SELF: u32 = 357;
const BRT_SUP_SAME: u32 = 358;
const BRT_BEGIN_SUP_BOOK: u32 = 360;
const BRT_SUP_NAME_START: u32 = 577;
const BRT_END_SUP_BOOK: u32 = 588;
const BRT_SUP_ADDIN: u32 = 667;
const BRT_ARR_FMLA: u32 = 0x01AA;
const BRT_SHR_FMLA: u32 = 0x01AB;
const BRT_EXTERN_SHEET: u32 = 0x016A;
const BRT_FMT: u32 = 44;
const BRT_XF: u32 = 47;
const BRT_COL_INFO: u32 = 60;
const BRT_DVAL: u32 = 64;
const BRT_BEGIN_WS_VIEW: u32 = 137;
const BRT_END_WS_VIEW: u32 = 138;
const BRT_WS_PROP: u32 = 147;
const BRT_PANE: u32 = 151;
const BRT_BEGIN_AFILTER: u32 = 161;
const BRT_BUNDLE_SH: u32 = 156;
const BRT_BOOK_VIEW: u32 = 158;
const BRT_WB_PROP: u32 = 153; // 0x99 — workbook properties, carries the 1904 flag
const BRT_MERGE_CELL: u32 = 176;
const BRT_BEGIN_LIST: u32 = 288;
const BRT_BEGIN_LIST_COL: u32 = 291;
const BRT_MARGINS: u32 = 475;
const BRT_PRINT_OPTIONS: u32 = 476;
const BRT_PAGE_SETUP: u32 = 477;
const BRT_BEGIN_HEADER_FOOTER: u32 = 478;
const BRT_HLINK: u32 = 0x01EE;
const BRT_BOOK_PROTECTION: u32 = 534;
const BRT_SHEET_PROTECTION: u32 = 535;
const BRT_LIST_PART: u32 = 550;
const BRT_COMMENT_AUTHOR: u32 = 632;
const BRT_BEGIN_COMMENT: u32 = 635;
const BRT_END_COMMENT: u32 = 636;
const BRT_COMMENT_TEXT: u32 = 637;
const BRT_TABLE_STYLE_CLIENT: u32 = 649;
const BRT_DVAL_LIST: u32 = 681;
const BRT_BEGIN_CELL_XFS: u32 = 0x0269;
const BRT_END_CELL_XFS: u32 = 0x026A;
const MAX_DVAL_RANGES: usize = 8192;
const MAX_TABLE_COLUMNS: usize = 16_384;
const MAX_XLSB_COL_INDEX: u32 = 16_383;
const MAX_XLSB_ROW_INDEX: u32 = 1_048_575;
const MAX_XLSB_SUPPORTING_LINKS: usize = 1 << 20;
const MAX_XLSB_EXTERNAL_NAMES: usize = 1 << 20;

/// A cursor over one BIFF12 record stream that yields `(record_type, payload)`,
/// bounded by the buffer — a hostile size never reads past the end.
struct RecReader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> RecReader<'a> {
    fn new(b: &'a [u8]) -> Self {
        RecReader { b, pos: 0 }
    }
    /// Read a variable-width unsigned int of up to `max_bytes` (7 bits each, high
    /// bit = continue).
    fn var(&mut self, max_bytes: usize) -> Option<u32> {
        let mut val: u32 = 0;
        for i in 0..max_bytes {
            let byte = *self.b.get(self.pos)?;
            self.pos += 1;
            val |= u32::from(byte & 0x7F) << (7 * i);
            if byte & 0x80 == 0 {
                break;
            }
        }
        Some(val)
    }
    /// Next record as `(type, payload)`, or `None` at end / on truncation.
    fn next(&mut self) -> Option<(u32, &'a [u8])> {
        if self.pos >= self.b.len() {
            return None;
        }
        let rt = self.var(2)?;
        let sz = self.var(4)? as usize;
        let start = self.pos;
        let end = start.checked_add(sz)?;
        if end > self.b.len() {
            return None;
        }
        self.pos = end;
        Some((rt, &self.b[start..end]))
    }
}

fn u16le(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn i32le(b: &[u8], o: usize) -> Option<i32> {
    b.get(o..o + 4)
        .map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn f64le(b: &[u8], o: usize) -> Option<f64> {
    b.get(o..o + 8)
        .and_then(|s| s.try_into().ok())
        .map(f64::from_le_bytes)
}

/// An `XLWideString`: `cch: u32` then `cch` UTF-16LE code units. Returns the
/// string and the byte length consumed.
fn wide_string(b: &[u8], o: usize) -> Option<(String, usize)> {
    let cch = u32le(b, o)? as usize;
    let bytes = cch.checked_mul(2)?;
    let chars = b.get(o + 4..o + 4 + bytes)?;
    let units: Vec<u16> = chars
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some((String::from_utf16_lossy(&units), 4 + bytes))
}

/// Detect `.xlsb` by the presence of `xl/workbook.bin` in the ZIP.
pub(crate) fn is_xlsb(bytes: &[u8]) -> bool {
    zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map(|mut z| part_index(&mut z, "xl/workbook.bin").is_some())
        .unwrap_or(false)
}

fn part(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, name: &str) -> Option<Vec<u8>> {
    const MAX_PART: u64 = 256 << 20;
    let idx = part_index(zip, name)?;
    let f = zip.by_index(idx).ok()?;
    let mut v = Vec::new();
    f.take(MAX_PART).read_to_end(&mut v).ok()?;
    Some(v)
}

fn part_index(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, name: &str) -> Option<usize> {
    if let Some(idx) = zip.index_for_name(name) {
        return Some(idx);
    }
    let wanted = canonical_part_name(name);
    for idx in 0..zip.len() {
        let Ok(file) = zip.by_index(idx) else {
            continue;
        };
        if canonical_part_name(file.name()).eq_ignore_ascii_case(&wanted) {
            return Some(idx);
        }
    }
    None
}

fn canonical_part_name(name: &str) -> String {
    name.replace('\\', "/").trim_start_matches('/').to_string()
}

pub(crate) fn open(bytes: &[u8]) -> Result<Workbook> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|_| Error::Zip("not a valid .xlsb ZIP container"))?;
    crate::ziputil::validate_compression(&mut zip)?;

    let shared = part(&mut zip, "xl/sharedStrings.bin")
        .map(|b| parse_shared_strings(&b))
        .unwrap_or_default();
    let styles = part(&mut zip, "xl/styles.bin")
        .map(|b| parse_styles(&b))
        .unwrap_or_default();
    let properties = crate::xlsx::parse_doc_properties(
        crate::xlsx::part_str(&mut zip, "docProps/core.xml").as_deref(),
        crate::xlsx::part_str(&mut zip, "docProps/app.xml").as_deref(),
    );
    let workbook_rels_xml =
        crate::xlsx::part_str(&mut zip, "xl/_rels/workbook.bin.rels").unwrap_or_default();
    let rels = crate::xlsx::parse_rels(&workbook_rels_xml);
    let rel_types = crate::xlsx::parse_rel_types(&workbook_rels_xml);
    let workbook_bin = part(&mut zip, "xl/workbook.bin").ok_or(Error::MissingWorkbook)?;
    let external_names = load_external_defined_names(&mut zip, &workbook_bin, &rels);
    let (
        names,
        date1904,
        active_sheet,
        defined_names,
        protect_structure,
        sheet_builtin_names,
        extern_sheets,
        formula_names,
        local_defined_names,
    ) = parse_workbook(&workbook_bin, &external_names);
    let formula_sheet_names: Vec<String> = names.iter().map(|(name, _, _)| name.clone()).collect();

    let mut budget = crate::MAX_TEXT_BYTES;
    let mut sheets = Vec::with_capacity(names.len().min(1 << 16));
    let mut selected_sheet_fallback = None;
    for (sheet_index, (name, rid, hs_state)) in names.into_iter().enumerate() {
        let sheet_type = xlsb_sheet_type(&rid, hs_state, rel_types.get(&rid).map(String::as_str));
        let is_worksheet = sheet_type == SheetType::WorkSheet;
        let target = rels.get(&rid).cloned().unwrap_or_default();
        let path = normalize_target(&target);
        let sheet_rels_xml = if is_worksheet {
            crate::xlsx::part_str(&mut zip, &sheet_rels_path(&path)).unwrap_or_default()
        } else {
            String::new()
        };
        let sheet_rels = if is_worksheet && !sheet_rels_xml.is_empty() {
            crate::xlsx::parse_rels(&sheet_rels_xml)
        } else {
            HashMap::new()
        };
        let comments = if is_worksheet {
            parse_sheet_comments(&mut zip, &path, &sheet_rels, &sheet_rels_xml)
        } else {
            Vec::new()
        };
        let (cells, merges, read_hyperlinks, metadata) = if is_worksheet {
            part(&mut zip, &path)
                .map(|b| {
                    parse_sheet(
                        &b,
                        &shared,
                        &styles,
                        date1904,
                        &sheet_rels,
                        &mut budget,
                        &formula_sheet_names,
                        &extern_sheets,
                        &external_names,
                        &formula_names,
                    )
                })
                .unwrap_or_default()
        } else {
            (
                Vec::new(),
                Vec::new(),
                Vec::new(),
                SheetReadMetadata::default(),
            )
        };
        if is_worksheet && metadata.selected && selected_sheet_fallback.is_none() {
            selected_sheet_fallback = Some(sheet_index);
        }
        let tables = if is_worksheet {
            parse_sheet_tables(
                &mut zip,
                &path,
                &sheet_rels,
                &sheet_rels_xml,
                &metadata.table_rel_ids,
            )
        } else {
            Vec::new()
        };
        sheets.push(Sheet {
            name,
            is_worksheet,
            sheet_type: Some(sheet_type),
            cells,
            read_merges: merges,
            read_hyperlinks,
            comments,
            tables,
            freeze: metadata.freeze,
            autofilter: metadata.autofilter,
            data_validations: metadata.data_validations,
            page_setup: metadata.page_setup,
            tab_color: metadata.tab_color,
            print_gridlines: metadata.print_gridlines,
            print_headings: metadata.print_headings,
            hide_gridlines: metadata.hide_gridlines,
            zoom: metadata.zoom,
            show_headers: metadata.show_headers,
            right_to_left: metadata.right_to_left,
            protect: metadata.protect,
            protect_options: metadata.protect_options,
            row_outline: metadata.row_outline,
            col_outline: metadata.col_outline,
            row_heights: metadata.row_heights,
            col_widths: metadata.col_widths,
            hidden_rows: metadata.hidden_rows,
            hidden_cols: metadata.hidden_cols,
            rich: metadata.rich,
            outline_summary_below: metadata.outline_summary_below.unwrap_or(true),
            outline_summary_right: metadata.outline_summary_right.unwrap_or(true),
            collapsed_rows: metadata.collapsed_rows,
            // hsState: 0 = visible, 1 = hidden, 2 = veryHidden ([MS-XLSB] 2.4.301).
            hidden: hs_state == 1,
            very_hidden: hs_state == 2,
            ..Default::default()
        });
    }
    apply_xlsb_sheet_builtin_names(&mut sheets, sheet_builtin_names);
    Ok(Workbook {
        sheets,
        properties,
        defined_names,
        local_defined_names,
        date1904,
        active_sheet: active_sheet.or(selected_sheet_fallback).unwrap_or_default(),
        text_truncated: budget == 0,
        protect_structure,
        ..Default::default()
    })
}

fn xlsb_sheet_type(rid: &str, hs_state: u32, rel_type: Option<&str>) -> SheetType {
    if rid.is_empty() && hs_state == 2 {
        return SheetType::Vba;
    }
    let kind = rel_type
        .and_then(|ty| ty.rsplit('/').next())
        .unwrap_or("worksheet");
    match kind.to_ascii_lowercase().as_str() {
        "chartsheet" => SheetType::ChartSheet,
        "dialogsheet" => SheetType::DialogSheet,
        "macrosheet" | "xlmacrosheet" | "xlintlmacrosheet" => SheetType::MacroSheet,
        _ => SheetType::WorkSheet,
    }
}

fn normalize_target(target: &str) -> String {
    let t = target.trim_start_matches('/');
    if t.starts_with("xl/") {
        t.to_string()
    } else {
        format!("xl/{t}")
    }
}

fn sheet_rels_path(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{path}.rels"),
    }
}

fn normalize_part_target(base: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut dir: Vec<&str> = match base.rfind('/') {
        Some(i) => base[..i].split('/').collect(),
        None => Vec::new(),
    };
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                dir.pop();
            }
            other => dir.push(other),
        }
    }
    dir.join("/")
}

/// Load one external-name table per workbook supporting-link record.
///
/// `BrtExternSheet.Xti.externalLink` indexes the complete supporting-link
/// sequence, not just external workbooks ([MS-XLSB] 2.5.173). Keeping empty
/// slots for self/same-sheet/add-in links is therefore required for PtgNameX
/// to select the `BrtSupBookSrc` relationship it actually references.
fn load_external_defined_names(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    workbook: &[u8],
    rels: &HashMap<String, String>,
) -> Vec<Vec<String>> {
    parse_supporting_link_rel_ids(workbook)
        .into_iter()
        .map(|rel_id| {
            let Some(target) = rel_id.as_ref().and_then(|id| rels.get(id)) else {
                return Vec::new();
            };
            let path = normalize_part_target("xl/workbook.bin", target);
            part(zip, &path)
                .map(|bytes| parse_external_defined_names(&bytes))
                .unwrap_or_default()
        })
        .collect()
}

/// Return the workbook relationship id for each supporting-link record, with
/// `None` retaining the index of non-external link kinds.
fn parse_supporting_link_rel_ids(b: &[u8]) -> Vec<Option<String>> {
    let mut links = Vec::new();
    let mut records = RecReader::new(b);
    while links.len() < MAX_XLSB_SUPPORTING_LINKS {
        let Some((rt, payload)) = records.next() else {
            break;
        };
        match rt {
            BRT_SUP_BOOK_SRC => {
                // BrtSupBookSrc.strRelID is a RelID/XLWideString.
                links.push(wide_string(payload, 0).map(|(id, _)| id));
            }
            BRT_SUP_SELF | BRT_SUP_SAME | BRT_SUP_ADDIN => links.push(None),
            _ => {}
        }
    }
    links
}

/// Parse the one-based BrtSupNameStart table from an External Link part.
///
/// BrtBeginSupBook starts with the two-byte external-reference type, while a
/// BrtSupNameStart payload is directly an XLNameWideString (the same binary
/// layout as XLWideString, limited to 255 characters by the format). Malformed
/// name records retain an empty slot so later one-based indexes cannot shift.
fn parse_external_defined_names(b: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_supbook = false;
    let mut records = RecReader::new(b);
    while names.len() < MAX_XLSB_EXTERNAL_NAMES {
        let Some((rt, payload)) = records.next() else {
            break;
        };
        match rt {
            BRT_BEGIN_SUP_BOOK => in_supbook = u16le(payload, 0).is_some(),
            BRT_SUP_NAME_START if in_supbook => names.push(
                wide_string(payload, 0)
                    .map(|(name, _)| name)
                    .unwrap_or_default(),
            ),
            BRT_END_SUP_BOOK => in_supbook = false,
            _ => {}
        }
    }
    names
}

/// Number formats from `styles.bin` (for date detection), shared with `.xlsx`.
#[derive(Default)]
struct Styles {
    xf_numfmt: Vec<u16>,
    custom: HashMap<u16, String>,
}

impl Styles {
    fn kind(&self, style_idx: usize) -> format::Kind {
        let id = self.xf_numfmt.get(style_idx).copied().unwrap_or(0);
        format::classify(id, self.custom.get(&id).map(String::as_str))
    }
}

fn parse_styles(b: &[u8]) -> Styles {
    let mut s = Styles::default();
    let mut r = RecReader::new(b);
    let mut in_cell_xfs = false;
    while let Some((rt, p)) = r.next() {
        match rt {
            BRT_FMT => {
                // ifmt:u16, stFmtCode: XLWideString.
                if let (Some(ifmt), Some((code, _))) = (u16le(p, 0), wide_string(p, 2)) {
                    s.custom.insert(ifmt, code);
                }
            }
            BRT_BEGIN_CELL_XFS => {
                in_cell_xfs = true;
            }
            BRT_END_CELL_XFS => {
                in_cell_xfs = false;
            }
            BRT_XF if in_cell_xfs => {
                // BrtXF: ixfeParent:u16, iFmt:u16, ...
                if let Some(ifmt) = u16le(p, 2) {
                    s.xf_numfmt.push(ifmt);
                }
            }
            _ => {}
        }
    }
    s
}

fn parse_shared_strings(b: &[u8]) -> Vec<SharedString> {
    let mut out = Vec::new();
    let mut r = RecReader::new(b);
    while let Some((rt, p)) = r.next() {
        if rt == BRT_SST_ITEM {
            out.push(parse_shared_string(p));
        }
    }
    out
}

fn parse_shared_string(p: &[u8]) -> SharedString {
    let flags = p.first().copied().unwrap_or_default();
    let Some((text, used)) = wide_string(p, 1) else {
        return SharedString::default();
    };
    if flags & 0x01 == 0 {
        return SharedString {
            text,
            runs: Vec::new(),
        };
    }

    let count_offset = 1 + used;
    let Some(count) = u32le(p, count_offset).map(|value| value as usize) else {
        return SharedString {
            text,
            runs: Vec::new(),
        };
    };
    let available = p.len().saturating_sub(count_offset + 4) / 6;
    let mut starts = Vec::with_capacity(count.min(available));
    for index in 0..count.min(available) {
        if let Some(start) = u32le(p, count_offset + 4 + index * 6) {
            starts.push(start as usize);
        }
    }
    starts.sort_unstable();
    starts.dedup();

    let text_units = text.encode_utf16().count();
    let mut runs = Vec::with_capacity(starts.len());
    for (index, start) in starts.iter().copied().enumerate() {
        if start >= text_units {
            continue;
        }
        let end = starts
            .get(index + 1)
            .copied()
            .unwrap_or(text_units)
            .min(text_units);
        if start < end {
            let mut unit = 0usize;
            let fragment = text
                .chars()
                .filter(|ch| {
                    let position = unit;
                    unit += ch.len_utf16();
                    position >= start && position < end
                })
                .collect::<String>();
            runs.push(crate::TextRun::new(fragment, crate::Font::default()));
        }
    }
    SharedString { text, runs }
}

/// Returns the `(sheet name, rel id, hsState)` triples, whether the workbook
/// uses the 1904 date system (`BrtWbProp` bit 0, matching calamine and
/// `[MS-XLSB]`), the active sheet index from `BrtBookView.itabCur`, workbook
/// defined names, workbook structure protection from `BrtBookProtection`, and
/// selected sheet-local built-in names used by sheet metadata facades.
/// `hsState` is the sheet visibility (0 = visible, 1 = hidden, 2 = veryHidden).
#[allow(clippy::type_complexity)]
fn parse_workbook(
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

fn parse_brt_extern_sheets(p: &[u8]) -> Vec<crate::ptg::ExternSheet> {
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

struct SheetBuiltinName {
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

fn apply_xlsb_sheet_builtin_names(sheets: &mut [Sheet], names: Vec<SheetBuiltinName>) {
    for name in names {
        let Some(sheet) = sheets.get_mut(name.sheet_index) else {
            continue;
        };
        match name.kind {
            SheetBuiltinKind::PrintArea => {
                if let Some(range) = name.ranges.into_iter().next() {
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
fn nullable_wide(b: &[u8], o: usize) -> Option<(String, usize)> {
    let cch = u32le(b, o)?;
    if cch == 0xFFFF_FFFF {
        return Some((String::new(), 4));
    }
    wide_string(b, o)
}

type WorkbookSheets = Vec<(String, String, u32)>;
type DefinedNames = Vec<(String, String)>;
type SheetRange = (u32, u16, u32, u16);
type SheetRanges = Vec<SheetRange>;
type Merges = SheetRanges;
type Hyperlinks = Vec<(u32, u16, String)>;
type AutoFilter = Option<SheetRange>;

#[derive(Default)]
struct SheetReadMetadata {
    freeze: Option<(u32, u16)>,
    autofilter: AutoFilter,
    data_validations: Vec<DataValidation>,
    page_setup: Option<PageSetup>,
    tab_color: Option<Color>,
    table_rel_ids: Vec<String>,
    print_gridlines: bool,
    print_headings: bool,
    hide_gridlines: bool,
    zoom: Option<u16>,
    show_headers: Option<bool>,
    right_to_left: bool,
    selected: bool,
    protect: bool,
    protect_options: Option<ProtectionOptions>,
    row_outline: BTreeMap<u32, u8>,
    col_outline: BTreeMap<u16, u8>,
    outline_summary_below: Option<bool>,
    outline_summary_right: Option<bool>,
    collapsed_rows: BTreeSet<u32>,
    row_heights: BTreeMap<u32, f32>,
    col_widths: BTreeMap<u16, f32>,
    hidden_rows: BTreeSet<u32>,
    hidden_cols: BTreeSet<u16>,
    rich: BTreeMap<(u32, u16), Vec<crate::TextRun>>,
}

fn parse_sheet_comments(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    sheet_path: &str,
    sheet_rels: &HashMap<String, String>,
    sheet_rels_xml: &str,
) -> Vec<Comment> {
    if sheet_rels_xml.is_empty() {
        return Vec::new();
    }
    let rel_types = crate::xlsx::parse_rel_types(sheet_rels_xml);
    let Some(target) = rel_types
        .iter()
        .find(|(_, ty)| ty.rsplit('/').next() == Some("comments"))
        .and_then(|(id, _)| sheet_rels.get(id))
    else {
        return Vec::new();
    };
    let path = normalize_part_target(sheet_path, target);
    part(zip, &path)
        .map(|b| parse_comments(&b))
        .unwrap_or_default()
}

fn parse_comments(b: &[u8]) -> Vec<Comment> {
    struct PendingComment {
        row: u32,
        col: u16,
        text: String,
        author: Option<String>,
    }

    let mut authors = Vec::new();
    let mut out = Vec::new();
    let mut pending: Option<PendingComment> = None;
    let mut r = RecReader::new(b);
    while let Some((rt, p)) = r.next() {
        match rt {
            BRT_COMMENT_AUTHOR => {
                if let Some((author, _)) = wide_string(p, 0) {
                    authors.push(author);
                }
            }
            BRT_BEGIN_COMMENT => {
                let (Some(author_id), Some(row), Some(col)) =
                    (u32le(p, 0), u32le(p, 4), u32le(p, 12))
                else {
                    pending = None;
                    continue;
                };
                let author = authors.get(author_id as usize).cloned();
                pending = Some(PendingComment {
                    row,
                    col: col.min(u32::from(u16::MAX)) as u16,
                    text: String::new(),
                    author,
                });
            }
            BRT_COMMENT_TEXT => {
                if let (Some(comment), Some(text)) = (pending.as_mut(), rich_string_text(p)) {
                    comment.text.push_str(&text);
                }
            }
            BRT_END_COMMENT => {
                if let Some(comment) = pending.take() {
                    out.push(Comment {
                        row: comment.row,
                        col: comment.col,
                        text: comment.text,
                        author: comment.author,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

fn rich_string_text(b: &[u8]) -> Option<String> {
    // RichStr starts with one flag byte followed by the plain XLWideString; the
    // trailing rich-format runs are not needed for the public comment text.
    wide_string(b, 1).map(|(s, _)| s)
}

#[derive(Clone, Debug)]
struct BrtFormulaDefinition {
    anchor: (u32, u16),
    range: (u32, u16, u32, u16),
    rgce: Vec<u8>,
    rgb_extra: Vec<u8>,
    is_array: bool,
}

type BrtFormulaDefinitions = HashMap<(u32, u16), BrtFormulaDefinition>;

#[allow(clippy::too_many_arguments)]
fn parse_sheet(
    b: &[u8],
    shared: &[SharedString],
    styles: &Styles,
    date1904: bool,
    sheet_rels: &HashMap<String, String>,
    budget: &mut usize,
    sheet_names: &[String],
    extern_sheets: &[crate::ptg::ExternSheet],
    external_names: &[Vec<String>],
    defined_names: &[String],
) -> (Vec<CellEntry>, Merges, Hyperlinks, SheetReadMetadata) {
    let mut cells = Vec::new();
    let mut merges = Vec::new();
    let mut hyperlinks = Vec::new();
    let mut metadata = SheetReadMetadata::default();
    let mut selected_view_rank = 0u8;
    let mut in_selected_view = false;
    let mut pending_dval_list: Option<String> = None;
    let mut formula_definitions = BrtFormulaDefinitions::new();
    let mut last_formula_cell: Option<(u32, u16)> = None;
    let mut row: u32 = 0;
    let mut r = RecReader::new(b);
    while let Some((rt, p)) = r.next() {
        match rt {
            BRT_DVAL_LIST => {
                pending_dval_list = wide_string(p, 0).map(|(formula, _)| formula);
            }
            BRT_DVAL => {
                metadata
                    .data_validations
                    .extend(parse_dval(p, pending_dval_list.take()));
            }
            BRT_SHEET_PROTECTION => {
                apply_sheet_protection(p, &mut metadata);
            }
            BRT_WS_PROP => {
                apply_ws_prop_metadata(p, &mut metadata);
            }
            BRT_MARGINS => {
                if let Some(margins) = parse_page_margins(p) {
                    page_setup_mut(&mut metadata).margins = Some(margins);
                }
            }
            BRT_PRINT_OPTIONS => {
                parse_print_options(p, &mut metadata);
            }
            BRT_PAGE_SETUP => {
                parse_page_setup(p, &mut metadata);
            }
            BRT_BEGIN_HEADER_FOOTER => {
                parse_header_footer(p, &mut metadata);
            }
            BRT_BEGIN_WS_VIEW => {
                let Some(rank) = parse_sheet_view(p, &mut metadata, selected_view_rank) else {
                    in_selected_view = false;
                    continue;
                };
                selected_view_rank = rank;
                in_selected_view = true;
            }
            BRT_END_WS_VIEW => {
                in_selected_view = false;
            }
            BRT_PANE if in_selected_view => {
                if let Some(freeze) = parse_pane_freeze(p) {
                    metadata.freeze = Some(freeze);
                }
            }
            BRT_BEGIN_AFILTER => {
                metadata.autofilter = parse_unchecked_rfx(p);
            }
            BRT_COL_INFO => {
                apply_col_outline(p, &mut metadata);
            }
            BRT_ROW_HDR => {
                if let Some(rr) = u32le(p, 0) {
                    row = rr;
                }
                apply_row_outline(p, &mut metadata);
            }
            BRT_ARR_FMLA | BRT_SHR_FMLA => {
                if let Some(definition) = parse_brt_formula_definition(rt, p, last_formula_cell) {
                    apply_brt_formula_definition(
                        &definition,
                        &mut cells,
                        sheet_names,
                        extern_sheets,
                        external_names,
                        defined_names,
                    );
                    formula_definitions.insert(definition.anchor, definition);
                }
            }
            BRT_MERGE_CELL => {
                // UncheckedRfX: rwFirst:u32, rwLast:u32, colFirst:u32, colLast:u32.
                if let (Some(rf), Some(rl), Some(cf), Some(cl)) =
                    (u32le(p, 0), u32le(p, 4), u32le(p, 8), u32le(p, 12))
                {
                    merges.push((
                        rf,
                        cf.min(u32::from(u16::MAX)) as u16,
                        rl,
                        cl.min(u32::from(u16::MAX)) as u16,
                    ));
                }
            }
            BRT_HLINK => {
                hyperlinks.extend(parse_hlink(p, sheet_rels));
            }
            BRT_LIST_PART => {
                if let Some(rel_id) = parse_list_part_rel_id(p) {
                    metadata.table_rel_ids.push(rel_id);
                }
            }
            BRT_CELL_RK | BRT_CELL_REAL | BRT_CELL_ISST | BRT_CELL_ST | BRT_CELL_BOOL
            | BRT_CELL_ERROR | BRT_FMLA_NUM | BRT_FMLA_STRING | BRT_FMLA_BOOL | BRT_FMLA_ERROR => {
                if *budget == 0 {
                    continue;
                }
                // Cell: col:u32 (0..4), iStyleRef:u24 + flags (4..8). Value at 8.
                let Some(col_u) = u32le(p, 0) else { continue };
                let col = col_u.min(u32::from(u16::MAX)) as u16;
                if matches!(
                    rt,
                    BRT_FMLA_NUM | BRT_FMLA_STRING | BRT_FMLA_BOOL | BRT_FMLA_ERROR
                ) {
                    last_formula_cell = Some((row, col));
                }
                let style_idx = (u32::from(*p.get(4).unwrap_or(&0))
                    | u32::from(*p.get(5).unwrap_or(&0)) << 8
                    | u32::from(*p.get(6).unwrap_or(&0)) << 16)
                    as usize;
                decode_cell(
                    rt,
                    p,
                    col,
                    style_idx,
                    row,
                    shared,
                    styles,
                    date1904,
                    &mut cells,
                    &mut metadata.rich,
                    budget,
                    sheet_names,
                    extern_sheets,
                    external_names,
                    defined_names,
                    &formula_definitions,
                );
            }
            _ => {}
        }
    }
    (cells, merges, hyperlinks, metadata)
}

fn parse_list_part_rel_id(p: &[u8]) -> Option<String> {
    let (rel_id, _) = wide_string(p, 0)?;
    (!rel_id.is_empty()).then_some(rel_id)
}

fn parse_ws_prop_tab_color(p: &[u8]) -> Option<Color> {
    // BrtWsProp starts with 2 bytes of worksheet property flags plus one byte of
    // filter/conditional-format flags, followed by the 8-byte BrtColor tab color.
    parse_brt_color(p.get(3..11)?)
}

fn apply_sheet_protection(p: &[u8], metadata: &mut SheetReadMetadata) {
    let Some(locked) = u32le(p, 2) else {
        return;
    };
    metadata.protect = locked != 0;
    if !metadata.protect {
        metadata.protect_options = None;
        return;
    }

    let options = ProtectionOptions {
        format_cells: u32le(p, 14).unwrap_or(0) != 0,
        format_columns: u32le(p, 18).unwrap_or(0) != 0,
        format_rows: u32le(p, 22).unwrap_or(0) != 0,
        insert_columns: u32le(p, 26).unwrap_or(0) != 0,
        insert_rows: u32le(p, 30).unwrap_or(0) != 0,
        insert_hyperlinks: u32le(p, 34).unwrap_or(0) != 0,
        delete_columns: u32le(p, 38).unwrap_or(0) != 0,
        delete_rows: u32le(p, 42).unwrap_or(0) != 0,
        sort: u32le(p, 50).unwrap_or(0) != 0,
        auto_filter: u32le(p, 54).unwrap_or(0) != 0,
        pivot_tables: u32le(p, 58).unwrap_or(0) != 0,
    };
    metadata.protect_options = (options != ProtectionOptions::default()).then_some(options);
}

fn apply_ws_prop_metadata(p: &[u8], metadata: &mut SheetReadMetadata) {
    metadata.tab_color = parse_ws_prop_tab_color(p);
    if let Some(flags) = u16le(p, 0) {
        metadata.outline_summary_below = Some(flags & 0x0040 != 0);
        metadata.outline_summary_right = Some(flags & 0x0080 != 0);
    }
}

fn apply_row_outline(p: &[u8], metadata: &mut SheetReadMetadata) {
    let (Some(row), Some(height_twips), Some(flags)) = (u32le(p, 0), u16le(p, 8), u16le(p, 10))
    else {
        return;
    };
    if height_twips > 0 {
        metadata
            .row_heights
            .insert(row, f32::from(height_twips) / 20.0);
    }
    if flags & (1 << 5) != 0 {
        metadata.hidden_rows.insert(row);
    }
    let level = ((flags >> 8) & 0x07) as u8;
    if level > 0 {
        metadata.row_outline.insert(row, level);
    }
    if flags & (1 << 11) != 0 {
        metadata.collapsed_rows.insert(row);
    }
}

fn apply_col_outline(p: &[u8], metadata: &mut SheetReadMetadata) {
    let (Some(first), Some(last), Some(width_256), Some(flags)) =
        (u32le(p, 0), u32le(p, 4), u32le(p, 8), u16le(p, 16))
    else {
        return;
    };
    let level = ((flags >> 8) & 0x07) as u8;
    if first > last || first > MAX_XLSB_COL_INDEX {
        return;
    }
    for col in first..=last.min(MAX_XLSB_COL_INDEX) {
        let col = col as u16;
        if width_256 > 0 {
            metadata.col_widths.insert(col, width_256 as f32 / 256.0);
        }
        if flags & 0x01 != 0 {
            metadata.hidden_cols.insert(col);
        }
        if level > 0 {
            metadata.col_outline.insert(col, level);
        }
    }
}

fn parse_brt_color(p: &[u8]) -> Option<Color> {
    let flags = *p.first()?;
    let valid_rgb = flags & 0x01 != 0;
    let color_type = flags >> 1;
    if !valid_rgb || color_type != 0x02 {
        return None;
    }
    Some(Color::rgb(*p.get(4)?, *p.get(5)?, *p.get(6)?))
}

fn parse_sheet_tables(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    sheet_path: &str,
    sheet_rels: &HashMap<String, String>,
    sheet_rels_xml: &str,
    table_rel_ids: &[String],
) -> Vec<Table> {
    if sheet_rels_xml.is_empty() {
        return Vec::new();
    }

    let rel_types = crate::xlsx::parse_rel_types(sheet_rels_xml);
    let mut rel_ids: Vec<String> = table_rel_ids
        .iter()
        .filter(|id| {
            rel_types
                .get(*id)
                .is_some_and(|ty| ty.rsplit('/').next() == Some("table"))
        })
        .cloned()
        .collect();
    if rel_ids.is_empty() {
        rel_ids.extend(
            rel_types
                .iter()
                .filter(|(_, ty)| ty.rsplit('/').next() == Some("table"))
                .map(|(id, _)| id.clone()),
        );
    }

    let mut seen = HashSet::new();
    rel_ids
        .into_iter()
        .filter(|id| seen.insert(id.clone()))
        .filter_map(|id| sheet_rels.get(&id).cloned())
        .map(|target| normalize_part_target(sheet_path, &target))
        .filter_map(|path| part(zip, &path))
        .filter_map(|bytes| parse_table(&bytes))
        .collect()
}

fn parse_table(b: &[u8]) -> Option<Table> {
    let mut range: Option<SheetRange> = None;
    let mut name: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut columns = Vec::new();
    let mut style: Option<String> = None;

    let mut r = RecReader::new(b);
    while let Some((rt, p)) = r.next() {
        match rt {
            BRT_BEGIN_LIST => {
                if let Some((parsed_range, parsed_name, parsed_display_name)) =
                    parse_table_begin_list(p)
                {
                    range = Some(parsed_range);
                    name = parsed_name;
                    display_name = parsed_display_name;
                }
            }
            BRT_BEGIN_LIST_COL if columns.len() < MAX_TABLE_COLUMNS => {
                if let Some(column) = parse_table_column(p) {
                    columns.push(column);
                }
            }
            BRT_TABLE_STYLE_CLIENT => {
                style = parse_table_style_client(p);
            }
            _ => {}
        }
    }

    Some(Table {
        range: range?,
        name: display_name.or(name).unwrap_or_default(),
        columns,
        style,
    })
}

type XlsbTableHeader = (SheetRange, Option<String>, Option<String>);

fn parse_table_begin_list(p: &[u8]) -> Option<XlsbTableHeader> {
    let range = parse_unchecked_rfx(p.get(0..16)?)?;
    let mut offset = 64usize;
    let (name, used) = nullable_wide_opt(p, offset)?;
    offset = offset.checked_add(used)?;
    let (display_name, _) = nullable_wide_opt(p, offset)?;
    Some((
        range,
        name.filter(|s| !s.is_empty()),
        display_name.filter(|s| !s.is_empty()),
    ))
}

fn parse_table_column(p: &[u8]) -> Option<String> {
    let mut offset = 24usize;
    let (name, used) = nullable_wide_opt(p, offset)?;
    offset = offset.checked_add(used)?;
    let (caption, _) = nullable_wide_opt(p, offset)?;
    caption.or(name).and_then(|s| (!s.is_empty()).then_some(s))
}

fn parse_table_style_client(p: &[u8]) -> Option<String> {
    let (style, _) = wide_string(p, 2)?;
    (!style.is_empty()).then_some(style)
}

fn page_setup_mut(metadata: &mut SheetReadMetadata) -> &mut PageSetup {
    metadata.page_setup.get_or_insert_with(PageSetup::default)
}

fn parse_page_margins(p: &[u8]) -> Option<(f64, f64, f64, f64, f64, f64)> {
    Some((
        xlsb_margin_at(p, 0)?,
        xlsb_margin_at(p, 8)?,
        xlsb_margin_at(p, 16)?,
        xlsb_margin_at(p, 24)?,
        xlsb_margin_at(p, 32)?,
        xlsb_margin_at(p, 40)?,
    ))
}

fn xlsb_margin_at(p: &[u8], offset: usize) -> Option<f64> {
    f64le(p, offset).filter(|value| value.is_finite() && *value >= 0.0 && *value < 49.0)
}

fn parse_print_options(p: &[u8], metadata: &mut SheetReadMetadata) {
    let Some(flags) = u16le(p, 0) else { return };
    if flags & 0x0001 != 0 {
        page_setup_mut(metadata).center_horizontally = true;
    }
    if flags & 0x0002 != 0 {
        page_setup_mut(metadata).center_vertically = true;
    }
    metadata.print_headings = flags & 0x0004 != 0;
    metadata.print_gridlines = flags & 0x0008 != 0;
}

fn parse_page_setup(p: &[u8], metadata: &mut SheetReadMetadata) {
    let (Some(page_start), Some(flags)) = (i32le(p, 20), u16le(p, 32)) else {
        return;
    };

    let ps = page_setup_mut(metadata);
    ps.paper_size = nonzero_u32_as_u16(p, 0);
    ps.scale = nonzero_u32_as_u16(p, 4);
    ps.fit_to_width = nonzero_u32_as_u16(p, 24);
    ps.fit_to_height = nonzero_u32_as_u16(p, 28);
    if flags & 0x0040 == 0 {
        ps.landscape = flags & 0x0002 != 0;
    }
    if flags & 0x0080 != 0 && page_start > 0 {
        ps.first_page_number = u16::try_from(page_start).ok();
    }
}

fn nonzero_u32_as_u16(p: &[u8], offset: usize) -> Option<u16> {
    let value = u32le(p, offset)?;
    if value == 0 {
        return None;
    }
    u16::try_from(value).ok()
}

fn parse_header_footer(p: &[u8], metadata: &mut SheetReadMetadata) {
    let Some((header, footer)) = parse_header_footer_pair(p) else {
        return;
    };
    if !header.is_empty() {
        page_setup_mut(metadata).header = Some(header);
    }
    if !footer.is_empty() {
        page_setup_mut(metadata).footer = Some(footer);
    }
}

fn parse_header_footer_pair(p: &[u8]) -> Option<(String, String)> {
    let mut offset = 2usize;
    let (header, used) = nullable_wide(p, offset)?;
    offset = offset.checked_add(used)?;
    let (footer, _) = nullable_wide(p, offset)?;
    Some((header, footer))
}

fn parse_dval(p: &[u8], list_formula: Option<String>) -> Vec<DataValidation> {
    let Some(flags) = u32le(p, 0) else {
        return Vec::new();
    };
    let Some(kind) = parse_dval_kind(flags & 0x0F) else {
        return Vec::new();
    };
    let operator = parse_dval_op((flags >> 20) & 0x0F).unwrap_or(DvOp::Between);
    let allow_blank = flags & (1 << 8) != 0;
    let show_input_message = flags & (1 << 18) != 0;
    let show_error_message = flags & (1 << 19) != 0;

    let Some((ranges, ranges_len)) = parse_unchecked_sq_rfx(p, 4) else {
        return Vec::new();
    };
    let strings_offset = 4 + ranges_len;
    let Some((error, prompt, strings_len)) = parse_dval_strings(p, strings_offset) else {
        return Vec::new();
    };
    let formula1_offset = strings_offset + strings_len;
    let Some((parsed_formula1, formula1_len)) = parse_dv_formula(p, formula1_offset) else {
        return Vec::new();
    };
    let formula2_offset = formula1_offset + formula1_len;
    let Some((parsed_formula2, _formula2_len)) = parse_dv_formula(p, formula2_offset) else {
        return Vec::new();
    };

    let formula1 = list_formula.unwrap_or(parsed_formula1);
    if formula1.is_empty() {
        return Vec::new();
    }
    let formula2 = (!parsed_formula2.is_empty()).then_some(parsed_formula2);
    let Some((&sqref, rest)) = ranges.split_first() else {
        return Vec::new();
    };

    let base = DataValidation {
        sqref,
        kind,
        operator,
        formula1,
        formula2,
        allow_blank,
        show_input_message,
        show_error_message,
        prompt,
        error,
    };
    let mut out = Vec::with_capacity(ranges.len().min(MAX_DVAL_RANGES));
    out.push(base.clone());
    for sqref in rest.iter().take(MAX_DVAL_RANGES - 1) {
        let mut clone = base.clone();
        clone.sqref = *sqref;
        out.push(clone);
    }
    out
}

fn parse_dval_kind(value: u32) -> Option<DvKind> {
    match value {
        1 => Some(DvKind::Whole),
        2 => Some(DvKind::Decimal),
        3 => Some(DvKind::List),
        4 => Some(DvKind::Date),
        5 => Some(DvKind::Time),
        6 => Some(DvKind::TextLength),
        7 => Some(DvKind::Custom),
        _ => None,
    }
}

fn parse_dval_op(value: u32) -> Option<DvOp> {
    match value {
        0 => Some(DvOp::Between),
        1 => Some(DvOp::NotBetween),
        2 => Some(DvOp::Equal),
        3 => Some(DvOp::NotEqual),
        4 => Some(DvOp::GreaterThan),
        5 => Some(DvOp::LessThan),
        6 => Some(DvOp::GreaterThanOrEqual),
        7 => Some(DvOp::LessThanOrEqual),
        _ => None,
    }
}

fn parse_unchecked_sq_rfx(p: &[u8], offset: usize) -> Option<(SheetRanges, usize)> {
    let crfx = i32::from_le_bytes(p.get(offset..offset + 4)?.try_into().ok()?);
    if crfx <= 0 {
        return None;
    }
    let count = usize::try_from(crfx).ok()?;
    let start = offset.checked_add(4)?;
    let ranges_len = count.checked_mul(16)?;
    let end = start.checked_add(ranges_len)?;
    p.get(start..end)?;

    let retained_count = count.min(MAX_DVAL_RANGES);
    let mut ranges = Vec::with_capacity(retained_count);
    for i in 0..retained_count {
        let pos = start + i * 16;
        let range = parse_unchecked_rfx(p.get(pos..pos + 16)?)?;
        ranges.push(range);
    }
    Some((ranges, end - offset))
}

type DvalStrings = (Option<(String, String)>, Option<(String, String)>, usize);

fn parse_dval_strings(p: &[u8], offset: usize) -> Option<DvalStrings> {
    let (error_title, used1) = nullable_wide_opt(p, offset)?;
    let (error_message, used2) = nullable_wide_opt(p, offset + used1)?;
    let (prompt_title, used3) = nullable_wide_opt(p, offset + used1 + used2)?;
    let (prompt_message, used4) = nullable_wide_opt(p, offset + used1 + used2 + used3)?;
    let error = match (error_title, error_message) {
        (None, None) => None,
        (title, message) => Some((title.unwrap_or_default(), message.unwrap_or_default())),
    };
    let prompt = match (prompt_title, prompt_message) {
        (None, None) => None,
        (title, message) => Some((title.unwrap_or_default(), message.unwrap_or_default())),
    };
    Some((error, prompt, used1 + used2 + used3 + used4))
}

fn nullable_wide_opt(b: &[u8], o: usize) -> Option<(Option<String>, usize)> {
    let cch = u32le(b, o)?;
    if cch == 0xFFFF_FFFF {
        return Some((None, 4));
    }
    wide_string(b, o).map(|(s, used)| (Some(s), used))
}

fn parse_dv_formula(p: &[u8], offset: usize) -> Option<(String, usize)> {
    let cce = u32le(p, offset)? as usize;
    let rgce_start = offset + 4;
    let rgce_end = rgce_start.checked_add(cce)?;
    let rgce = p.get(rgce_start..rgce_end)?;
    let cb = u32le(p, rgce_end)? as usize;
    let end = rgce_end.checked_add(4)?.checked_add(cb)?;
    p.get(rgce_end + 4..end)?;
    Some((crate::ptg::decompile(rgce, true), end - offset))
}

fn parse_sheet_view(p: &[u8], metadata: &mut SheetReadMetadata, current_rank: u8) -> Option<u8> {
    let flags = u16le(p, 0)?;
    let i_wbk_view = u32le(p, 26).unwrap_or(0);
    let rank = if i_wbk_view == 0 { 2 } else { 1 };
    if rank <= current_rank {
        return None;
    }

    metadata.freeze = None;
    metadata.hide_gridlines = flags & (1 << 2) == 0;
    metadata.show_headers = Some(flags & (1 << 3) != 0);
    metadata.right_to_left = flags & (1 << 5) != 0;
    metadata.selected = flags & (1 << 6) != 0;
    metadata.zoom = u16le(p, 18).filter(|&zoom| zoom != 0);
    Some(rank)
}

fn parse_pane_freeze(p: &[u8]) -> Option<(u32, u16)> {
    let flags = *p.get(28)?;
    if flags & 0x03 == 0 {
        return None;
    }
    let rows = f64le(p, 0)?.max(0.0).floor() as u32;
    let cols_u = f64le(p, 8)?.max(0.0).floor() as u32;
    let cols = cols_u.min(u32::from(u16::MAX)) as u16;
    if rows == 0 && cols == 0 {
        None
    } else {
        Some((rows, cols))
    }
}

fn parse_unchecked_rfx(p: &[u8]) -> AutoFilter {
    let (Some(rf), Some(rl), Some(cf), Some(cl)) =
        (u32le(p, 0), u32le(p, 4), u32le(p, 8), u32le(p, 12))
    else {
        return None;
    };
    if rf > rl || cf > cl {
        return None;
    }
    Some((
        rf,
        cf.min(u32::from(u16::MAX)) as u16,
        rl,
        cl.min(u32::from(u16::MAX)) as u16,
    ))
}

fn parse_hlink(p: &[u8], sheet_rels: &HashMap<String, String>) -> Hyperlinks {
    const MAX_HYPERLINK_CELLS: usize = 1 << 16;
    let (Some(rf), Some(rl), Some(cf), Some(cl)) =
        (u32le(p, 0), u32le(p, 4), u32le(p, 8), u32le(p, 12))
    else {
        return Vec::new();
    };
    let Some((rel_id, rel_len)) = nullable_wide(p, 16) else {
        return Vec::new();
    };
    let location_offset = 16 + rel_len;
    let Some((location, _location_len)) = wide_string(p, location_offset) else {
        return Vec::new();
    };

    let target = match (sheet_rels.get(&rel_id), location.is_empty()) {
        (Some(url), true) => url.clone(),
        (Some(url), false) => format!("{url}#{location}"),
        (None, false) if rel_id.is_empty() => format!("#{location}"),
        _ => return Vec::new(),
    };

    let c0 = cf.min(u32::from(u16::MAX)) as u16;
    let c1 = cl.min(u32::from(u16::MAX)) as u16;
    if rf > rl || c0 > c1 {
        return Vec::new();
    }

    let mut hyperlinks = Vec::new();
    'links: for row in rf..=rl {
        for col in c0..=c1 {
            if hyperlinks.len() >= MAX_HYPERLINK_CELLS {
                break 'links;
            }
            hyperlinks.push((row, col, target.clone()));
        }
    }
    hyperlinks
}

fn parse_brt_parsed_formula(p: &[u8], offset: usize) -> Option<(&[u8], &[u8])> {
    let cce = usize::try_from(u32le(p, offset)?).ok()?;
    let rgce_start = offset.checked_add(4)?;
    let rgce_end = rgce_start.checked_add(cce)?;
    let rgce = p.get(rgce_start..rgce_end)?;
    let cb = usize::try_from(u32le(p, rgce_end)?).ok()?;
    let extra_start = rgce_end.checked_add(4)?;
    let extra_end = extra_start.checked_add(cb)?;
    Some((rgce, p.get(extra_start..extra_end)?))
}

fn parse_brt_formula_definition(
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

fn apply_brt_formula_definition(
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
fn decode_cell(
    rt: u32,
    p: &[u8],
    col: u16,
    style_idx: usize,
    row: u32,
    shared: &[SharedString],
    styles: &Styles,
    date1904: bool,
    cells: &mut Vec<CellEntry>,
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
    let push = |cells: &mut Vec<CellEntry>, budget: &mut usize, value: Cell, text: String| {
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
            style: None,
            hyperlink: None,
        });
    };
    let number = |f: f64, cells: &mut Vec<CellEntry>, budget: &mut usize| {
        let kind = styles.kind(style_idx);
        let display = format::render_value(f, kind, date1904);
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
                let display = format::render_value(f, kind, date1904);
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
                    push(cells, budget, Cell::Text(s.text.clone()), s.text.clone());
                    if !s.runs.is_empty() {
                        rich.insert((row, col), s.runs.clone());
                    }
                }
            }
        }
        BRT_CELL_ST => {
            if let Some((s, _)) = wide_string(p, 8) {
                push(cells, budget, Cell::Text(s.clone()), s);
            }
        }
        BRT_FMLA_STRING => {
            if let Some((s, used)) = wide_string(p, 8) {
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
                    s,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Color, SheetMetadata, SheetType, SheetVisible};
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// `XLWideString`: `cch:u32` + UTF-16LE chars.
    fn wstr(s: &str) -> Vec<u8> {
        let units: Vec<u16> = s.encode_utf16().collect();
        let mut v = (units.len() as u32).to_le_bytes().to_vec();
        for u in units {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    fn null_wstr() -> Vec<u8> {
        0xFFFF_FFFFu32.to_le_bytes().to_vec()
    }

    /// Frame a BIFF12 record: var-uint type, var-uint size, payload.
    fn rec(rt: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        if rt < 0x80 {
            v.push(rt as u8);
        } else {
            v.push((rt & 0x7F) as u8 | 0x80);
            v.push((rt >> 7) as u8 & 0x7F);
        }
        let mut sz = payload.len();
        loop {
            let mut b = (sz & 0x7F) as u8;
            sz >>= 7;
            if sz > 0 {
                b |= 0x80;
            }
            v.push(b);
            if sz == 0 {
                break;
            }
        }
        v.extend_from_slice(payload);
        v
    }

    fn xf(numfmt: u16) -> Vec<u8> {
        let mut v = vec![0u8; 16];
        v[2..4].copy_from_slice(&numfmt.to_le_bytes());
        v
    }

    #[test]
    fn record_framing_var_uints() {
        // A 2-byte record type (156 = BrtBundleSh) and small size round-trip.
        let r = rec(BRT_BUNDLE_SH, &[1, 2, 3]);
        let mut rr = RecReader::new(&r);
        let (rt, p) = rr.next().unwrap();
        assert_eq!(rt, BRT_BUNDLE_SH);
        assert_eq!(p, &[1, 2, 3]);
        assert!(rr.next().is_none());
    }

    #[test]
    fn xlsb_supporting_link_relationships_preserve_non_external_slots() {
        let mut workbook = rec(BRT_SUP_SELF, &[]);
        workbook.extend_from_slice(&rec(BRT_SUP_BOOK_SRC, &wstr("rIdExternal")));
        workbook.extend_from_slice(&rec(BRT_SUP_SAME, &[]));
        workbook.extend_from_slice(&rec(BRT_SUP_ADDIN, &[]));

        assert_eq!(
            parse_supporting_link_rel_ids(&workbook),
            vec![None, Some("rIdExternal".to_string()), None, None]
        );
    }

    #[test]
    fn xlsb_external_link_names_follow_sup_name_start_order() {
        let mut external_link = rec(BRT_SUP_NAME_START, &wstr("OutsideBook"));
        let mut begin = 0u16.to_le_bytes().to_vec(); // sbt: external workbook
        begin.extend_from_slice(&wstr("rIdPath"));
        begin.extend_from_slice(&null_wstr());
        external_link.extend_from_slice(&rec(BRT_BEGIN_SUP_BOOK, &begin));
        external_link.extend_from_slice(&rec(BRT_SUP_NAME_START, &wstr("External.Rate_β")));
        external_link.extend_from_slice(&rec(BRT_SUP_NAME_START, &[1]));
        external_link.extend_from_slice(&rec(BRT_SUP_NAME_START, &wstr("Second_Name")));
        external_link.extend_from_slice(&rec(BRT_END_SUP_BOOK, &[]));
        external_link.extend_from_slice(&rec(BRT_SUP_NAME_START, &wstr("AfterBook")));

        assert_eq!(
            parse_external_defined_names(&external_link),
            vec![
                "External.Rate_β".to_string(),
                String::new(),
                "Second_Name".to_string()
            ]
        );
    }

    #[test]
    fn xlsb_namex_resolves_names_from_external_link_parts() {
        let external_name = "External.Rate_β";

        // The XTI points at supporting-link slot 1. Slot 0 is deliberately a
        // self-link to prove the loader retains non-external index positions.
        let mut workbook = rec(BRT_SUP_SELF, &[]);
        workbook.extend_from_slice(&rec(BRT_SUP_BOOK_SRC, &wstr("rIdExternal")));
        let mut extern_sheet = 1u32.to_le_bytes().to_vec();
        extern_sheet.extend_from_slice(&1u32.to_le_bytes());
        extern_sheet.extend_from_slice(&(-2i32).to_le_bytes());
        extern_sheet.extend_from_slice(&(-2i32).to_le_bytes());
        workbook.extend_from_slice(&rec(BRT_EXTERN_SHEET, &extern_sheet));

        let namex = [0x39, 0, 0, 1, 0, 0, 0];
        let mut defined_name = 0u32.to_le_bytes().to_vec();
        defined_name.push(0); // chKey
        defined_name.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // workbook scope
        defined_name.extend_from_slice(&wstr("Imported"));
        defined_name.extend_from_slice(&(namex.len() as u32).to_le_bytes());
        defined_name.extend_from_slice(&namex);
        defined_name.extend_from_slice(&0u32.to_le_bytes()); // formula cb
        defined_name.extend_from_slice(&null_wstr()); // comment
        workbook.extend_from_slice(&rec(BRT_NAME, &defined_name));

        let mut bundle = vec![0u8; 8];
        bundle.extend_from_slice(&wstr("rIdSheet"));
        bundle.extend_from_slice(&wstr("Data"));
        workbook.extend_from_slice(&rec(BRT_BUNDLE_SH, &bundle));

        let mut external_begin = 0u16.to_le_bytes().to_vec();
        external_begin.extend_from_slice(&wstr("rIdPath"));
        external_begin.extend_from_slice(&null_wstr());
        let mut external_link = rec(BRT_BEGIN_SUP_BOOK, &external_begin);
        external_link.extend_from_slice(&rec(BRT_SUP_NAME_START, &wstr(external_name)));
        external_link.extend_from_slice(&rec(587, &[])); // BrtSupNameEnd
        external_link.extend_from_slice(&rec(BRT_END_SUP_BOOK, &[]));

        let mut sheet = rec(BRT_ROW_HDR, &0u32.to_le_bytes());
        sheet.extend_from_slice(&rec(
            BRT_FMLA_NUM,
            &brt_numeric_formula(0, 12.5, &namex, &[]),
        ));
        let imported = [0x23, 1, 0, 0, 0]; // PtgName: workbook BrtName 1
        sheet.extend_from_slice(&rec(
            BRT_FMLA_NUM,
            &brt_numeric_formula(1, 12.5, &imported, &[]),
        ));

        let workbook_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdSheet" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/><Relationship Id="rIdExternal" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLink" Target="externalLinks/externalLink1.bin"/></Relationships>"#;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", workbook.as_slice()),
            ("xl/_rels/workbook.bin.rels", workbook_rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", sheet.as_slice()),
            (
                "xl/externalLinks/externalLink1.bin",
                external_link.as_slice(),
            ),
        ] {
            writer.start_file(path, options).unwrap();
            writer.write_all(body).unwrap();
        }

        let workbook = Workbook::open(&writer.finish().unwrap().into_inner()).unwrap();
        let external_formula = format!("[ixti:0]!{external_name}");
        assert_eq!(
            workbook.defined_names(),
            &[("Imported".to_string(), external_formula.clone())]
        );
        assert_eq!(
            workbook.sheets[0].cell(0, 0),
            Some(&Cell::Formula {
                formula: external_formula,
                cached: Box::new(Cell::Number(12.5)),
            })
        );
        assert_eq!(
            workbook.evaluate_cell("Data", 0, 0),
            crate::FormulaEvaluation::Fallback {
                cached: Cell::Number(12.5),
                reason: crate::FormulaUnsupportedReason::ExternalRef,
            }
        );
        assert_eq!(
            workbook.evaluate_cell("Data", 0, 1),
            crate::FormulaEvaluation::Fallback {
                cached: Cell::Number(12.5),
                reason: crate::FormulaUnsupportedReason::ExternalRef,
            }
        );
    }

    #[test]
    fn reads_a_synthetic_xlsb() {
        // workbook.bin: one BrtBundleSh(rId1, "시트1").
        let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
        wb_bin.extend_from_slice(&wstr("rId1")); // strRelID (non-null)
        wb_bin.extend_from_slice(&wstr("시트1")); // strName
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        // sharedStrings.bin: BrtSSTItem("품목").
        let mut item = vec![1u8]; // flags: rich string
        item.extend_from_slice(&wstr("품목"));
        item.extend_from_slice(&2u32.to_le_bytes()); // two StrRun boundaries
        item.extend_from_slice(&0u32.to_le_bytes());
        item.extend_from_slice(&1u16.to_le_bytes()); // font index (not exposed safely)
        item.extend_from_slice(&1u32.to_le_bytes());
        item.extend_from_slice(&2u16.to_le_bytes());
        let sst = rec(BRT_SST_ITEM, &item);

        // sheet1.bin: RowHdr(0), CellIsst(0,0 → isst 0), CellReal(0,1 → 42.0).
        let mut sh = rec(BRT_ROW_HDR, &[0, 0, 0, 0]);
        let mut isst = vec![0u8; 8]; // col=0, styleRef/flags
        isst.extend_from_slice(&0u32.to_le_bytes()); // isst = 0
        sh.extend_from_slice(&rec(BRT_CELL_ISST, &isst));
        let mut real = vec![1, 0, 0, 0, 0, 0, 0, 0]; // col=1, styleRef
        real.extend_from_slice(&42.0f64.to_le_bytes());
        sh.extend_from_slice(&rec(BRT_CELL_REAL, &real));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (name, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/sharedStrings.bin", sst.as_slice()),
            ("xl/worksheets/sheet1.bin", sh.as_slice()),
        ] {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        assert_eq!(wb.sheets[0].name, "시트1");
        assert_eq!(
            wb.sheets[0].cell(0, 0),
            Some(&Cell::Text("품목".to_string()))
        );
        assert_eq!(wb.sheets[0].cell(0, 1), Some(&Cell::Number(42.0)));
        assert_eq!(
            wb.sheets[0]
                .rich_text_runs(0, 0)
                .expect("rich boundaries")
                .iter()
                .map(|run| run.text.as_str())
                .collect::<Vec<_>>(),
            ["품", "목"]
        );
    }

    #[test]
    fn xlsb_book_view_active_tab_surfaces_workbook_metadata() {
        let mut wb_bin = Vec::new();
        for (rid, name) in [("rId1", "Data"), ("rId2", "Summary")] {
            let mut bundle = vec![0u8; 8]; // hsState + iTabID
            bundle.extend_from_slice(&wstr(rid));
            bundle.extend_from_slice(&wstr(name));
            wb_bin.extend_from_slice(&rec(BRT_BUNDLE_SH, &bundle));
        }
        let mut book_view = Vec::new();
        book_view.extend_from_slice(&0i32.to_le_bytes()); // xWn
        book_view.extend_from_slice(&0i32.to_le_bytes()); // yWn
        book_view.extend_from_slice(&0u32.to_le_bytes()); // dxWn
        book_view.extend_from_slice(&0u32.to_le_bytes()); // dyWn
        book_view.extend_from_slice(&600u32.to_le_bytes()); // iTabRatio
        book_view.extend_from_slice(&0u32.to_le_bytes()); // itabFirst
        book_view.extend_from_slice(&1u32.to_le_bytes()); // itabCur
        wb_bin.extend_from_slice(&rec(158, &book_view)); // BrtBookView

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/><Relationship Id="rId2" Target="worksheets/sheet2.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", [].as_slice()),
            ("xl/worksheets/sheet2.bin", [].as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
        let metadata = wb.metadata();

        assert_eq!(wb.active_sheet_index(), Some(1));
        assert_eq!(wb.active_sheet_name(), Some("Summary"));
        assert_eq!(metadata.active_sheet, Some(1));
        assert_eq!(metadata.active_sheet_name, Some("Summary"));
        assert_eq!(
            <Workbook as crate::Reader>::metadata(&wb).active_sheet_name,
            Some("Summary")
        );
    }

    #[test]
    fn xlsb_selected_sheet_view_falls_back_to_active_sheet_metadata() {
        const BRT_BEGIN_WS_VIEWS: u32 = 133;
        const BRT_END_WS_VIEWS: u32 = 134;

        let mut wb_bin = Vec::new();
        for (rid, name) in [("rId1", "Data"), ("rId2", "Summary")] {
            let mut bundle = vec![0u8; 8]; // hsState + iTabID
            bundle.extend_from_slice(&wstr(rid));
            bundle.extend_from_slice(&wstr(name));
            wb_bin.extend_from_slice(&rec(BRT_BUNDLE_SH, &bundle));
        }

        let mut ws_view = Vec::new();
        ws_view.extend_from_slice(&(1u16 << 6).to_le_bytes()); // fSelected
        ws_view.extend_from_slice(&0u32.to_le_bytes()); // xlView normal
        ws_view.extend_from_slice(&0u32.to_le_bytes()); // rwTop
        ws_view.extend_from_slice(&0u32.to_le_bytes()); // colLeft
        ws_view.push(0x40); // icvHdr
        ws_view.push(0); // reserved2
        ws_view.extend_from_slice(&0u16.to_le_bytes()); // reserved3
        ws_view.extend_from_slice(&100u16.to_le_bytes()); // wScale
        ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScaleNormal
        ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScaleSLV
        ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScalePLV
        ws_view.extend_from_slice(&0u32.to_le_bytes()); // iWbkView

        let mut selected_sheet = rec(BRT_BEGIN_WS_VIEWS, &[]);
        selected_sheet.extend_from_slice(&rec(BRT_BEGIN_WS_VIEW, &ws_view));
        selected_sheet.extend_from_slice(&rec(BRT_END_WS_VIEW, &[]));
        selected_sheet.extend_from_slice(&rec(BRT_END_WS_VIEWS, &[]));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/><Relationship Id="rId2" Target="worksheets/sheet2.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", [].as_slice()),
            ("xl/worksheets/sheet2.bin", selected_sheet.as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
        let metadata = wb.metadata();

        assert_eq!(wb.active_sheet_index(), Some(1));
        assert_eq!(wb.active_sheet_name(), Some("Summary"));
        assert_eq!(metadata.active_sheet, Some(1));
        assert_eq!(metadata.active_sheet_name, Some("Summary"));
        assert_eq!(
            <Workbook as crate::Reader>::metadata(&wb).active_sheet_name,
            Some("Summary")
        );
    }

    #[test]
    fn xlsb_shared_strings_part_lookup_is_case_insensitive() {
        // calamine/tests/issue_419.xlsb stores the shared-string part as
        // `xl/SharedStrings.bin`; the cell stream still references it through
        // BrtCellIsst.
        let mut wb_bin = vec![0u8; 8];
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("Sheet1"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let mut item = vec![0u8];
        item.extend_from_slice(&wstr("Hello"));
        let sst = rec(BRT_SST_ITEM, &item);

        let mut sh = rec(BRT_ROW_HDR, &[0, 0, 0, 0]);
        let mut isst = vec![0u8; 8];
        isst.extend_from_slice(&0u32.to_le_bytes());
        sh.extend_from_slice(&rec(BRT_CELL_ISST, &isst));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (name, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/SharedStrings.bin", sst.as_slice()),
            ("xl/worksheets/sheet1.bin", sh.as_slice()),
        ] {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(
            wb.sheets[0].cell(0, 0),
            Some(&Cell::Text("Hello".to_string()))
        );
    }

    #[test]
    fn xlsb_styles_use_only_cell_xfs_for_cell_style_indexes() {
        // Real styles.bin parts can contain a non-cell XF group before
        // BrtBeginCellXFs. Cell records index only the XF records inside
        // BrtBeginCellXFs; collecting earlier BrtXF records shifts style
        // indexes and can turn plain numeric cells into dates.
        let mut wb_bin = vec![0u8; 8];
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("Sheet1"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let mut fmt = 164u16.to_le_bytes().to_vec();
        fmt.extend_from_slice(&wstr("yyyy-mm-dd"));
        let mut styles = rec(BRT_FMT, &fmt);
        styles.extend_from_slice(&rec(0x0272, &1u32.to_le_bytes()));
        styles.extend_from_slice(&rec(BRT_XF, &xf(0)));
        styles.extend_from_slice(&rec(0x0273, &[]));
        styles.extend_from_slice(&rec(BRT_BEGIN_CELL_XFS, &2u32.to_le_bytes()));
        styles.extend_from_slice(&rec(BRT_XF, &xf(164)));
        styles.extend_from_slice(&rec(BRT_XF, &xf(0)));
        styles.extend_from_slice(&rec(BRT_END_CELL_XFS, &[]));

        let mut sh = rec(BRT_ROW_HDR, &[0, 0, 0, 0]);
        let mut date = vec![0u8; 8];
        date.extend_from_slice(&44_197.0f64.to_le_bytes());
        sh.extend_from_slice(&rec(BRT_CELL_REAL, &date));
        let mut number = vec![1, 0, 0, 0, 1, 0, 0, 0];
        number.extend_from_slice(&15.0f64.to_le_bytes());
        sh.extend_from_slice(&rec(BRT_CELL_REAL, &number));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (name, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/styles.bin", styles.as_slice()),
            ("xl/worksheets/sheet1.bin", sh.as_slice()),
        ] {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets[0].cell(0, 0), Some(&Cell::Date(44_197.0)));
        assert_eq!(wb.sheets[0].cell(0, 1), Some(&Cell::Number(15.0)));
    }

    #[test]
    fn xlsb_hyperlinks_surface_public_metadata() {
        let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("Links"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let url = "https://example.com/xlsb";
        let mut hlink = Vec::new();
        for value in [1u32, 2, 1, 1] {
            hlink.extend_from_slice(&value.to_le_bytes());
        }
        hlink.extend_from_slice(&wstr("rId2"));
        hlink.extend_from_slice(&wstr(""));
        hlink.extend_from_slice(&wstr("Open bid"));
        hlink.extend_from_slice(&wstr(""));
        let sheet = rec(0x01EE, &hlink);

        let wb_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/></Relationships>"#;
        let sheet_rels = format!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="{url}" TargetMode="External"/></Relationships>"#
        );

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", wb_rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", sheet.as_slice()),
            ("xl/worksheets/_rels/sheet1.bin.rels", sheet_rels.as_bytes()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();

        assert_eq!(
            wb.sheets[0].hyperlinks(),
            &[(1, 1, url.to_string()), (2, 1, url.to_string())]
        );
    }

    #[test]
    fn xlsb_comments_surface_public_metadata() {
        const BRT_BEGIN_COMMENTS: u32 = 628;
        const BRT_END_COMMENTS: u32 = 629;
        const BRT_BEGIN_COMMENT_AUTHORS: u32 = 630;
        const BRT_END_COMMENT_AUTHORS: u32 = 631;
        const BRT_COMMENT_AUTHOR: u32 = 632;
        const BRT_BEGIN_COMMENT_LIST: u32 = 633;
        const BRT_END_COMMENT_LIST: u32 = 634;
        const BRT_BEGIN_COMMENT: u32 = 635;
        const BRT_END_COMMENT: u32 = 636;
        const BRT_COMMENT_TEXT: u32 = 637;

        let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("Notes"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let mut comment = Vec::new();
        comment.extend_from_slice(&0u32.to_le_bytes()); // iauthor
        for value in [2u32, 2, 3, 3] {
            comment.extend_from_slice(&value.to_le_bytes()); // UncheckedRfX: D3
        }
        comment.extend_from_slice(&[0u8; 16]); // guid

        let mut rich_text = vec![0x01]; // RichStr flags: fRichStr=1, fExtStr=0
        rich_text.extend_from_slice(&wstr("검토 필요"));
        rich_text.extend_from_slice(&0u32.to_le_bytes()); // zero StrRun entries

        let mut comments = rec(BRT_BEGIN_COMMENTS, &[]);
        comments.extend_from_slice(&rec(BRT_BEGIN_COMMENT_AUTHORS, &[]));
        comments.extend_from_slice(&rec(BRT_COMMENT_AUTHOR, &wstr("auditor")));
        comments.extend_from_slice(&rec(BRT_END_COMMENT_AUTHORS, &[]));
        comments.extend_from_slice(&rec(BRT_BEGIN_COMMENT_LIST, &[]));
        comments.extend_from_slice(&rec(BRT_BEGIN_COMMENT, &comment));
        comments.extend_from_slice(&rec(BRT_COMMENT_TEXT, &rich_text));
        comments.extend_from_slice(&rec(BRT_END_COMMENT, &[]));
        comments.extend_from_slice(&rec(BRT_END_COMMENT_LIST, &[]));
        comments.extend_from_slice(&rec(BRT_END_COMMENTS, &[]));

        let wb_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/></Relationships>"#;
        let sheet_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="../comments1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", wb_rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", [].as_slice()),
            ("xl/worksheets/_rels/sheet1.bin.rels", sheet_rels.as_bytes()),
            ("xl/comments1.bin", comments.as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
        let comments = wb.sheets[0].comments();

        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].row, 2);
        assert_eq!(comments[0].col, 3);
        assert_eq!(comments[0].text, "검토 필요");
        assert_eq!(comments[0].author.as_deref(), Some("auditor"));
    }

    #[test]
    fn xlsb_tables_surface_public_metadata() {
        const BRT_BEGIN_LIST: u32 = 288;
        const BRT_BEGIN_LIST_COL: u32 = 291;
        const BRT_BEGIN_LIST_COLS: u32 = 293;
        const BRT_LIST_PART: u32 = 550;
        const BRT_TABLE_STYLE_CLIENT: u32 = 649;

        let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("Tables"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let sheet = rec(BRT_LIST_PART, &wstr("rId2"));

        let mut begin_list = Vec::new();
        for value in [0u32, 2, 0, 2] {
            begin_list.extend_from_slice(&value.to_le_bytes()); // A1:C3
        }
        begin_list.extend_from_slice(&0u32.to_le_bytes()); // lt = LTRANGE
        begin_list.extend_from_slice(&1u32.to_le_bytes()); // idList
        begin_list.extend_from_slice(&1u32.to_le_bytes()); // crwHeader
        begin_list.extend_from_slice(&0u32.to_le_bytes()); // crwTotals
        begin_list.extend_from_slice(&0u32.to_le_bytes()); // table flags
        for _ in 0..6 {
            begin_list.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // DXF ids
        }
        begin_list.extend_from_slice(&0u32.to_le_bytes()); // dwConnID
        begin_list.extend_from_slice(&null_wstr()); // stName
        begin_list.extend_from_slice(&wstr("SalesTable")); // stDisplayName
        begin_list.extend_from_slice(&wstr("")); // stComment
        begin_list.extend_from_slice(&null_wstr()); // stStyleHeader
        begin_list.extend_from_slice(&null_wstr()); // stStyleData
        begin_list.extend_from_slice(&null_wstr()); // stStyleAgg

        let mut table = rec(BRT_BEGIN_LIST, &begin_list);
        table.extend_from_slice(&rec(BRT_BEGIN_LIST_COLS, &3u32.to_le_bytes()));
        for (idx, caption) in ["Item", "Qty", "Total"].iter().enumerate() {
            let mut column = Vec::new();
            column.extend_from_slice(&((idx + 1) as u32).to_le_bytes()); // idField
            column.extend_from_slice(&0u32.to_le_bytes()); // ilta
            column.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // nDxfHdr
            column.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // nDxfInsertRow
            column.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // nDxfAgg
            column.extend_from_slice(&0u32.to_le_bytes()); // idqsif
            column.extend_from_slice(&null_wstr()); // stName
            column.extend_from_slice(&wstr(caption)); // stCaption
            column.extend_from_slice(&null_wstr()); // stTotal
            column.extend_from_slice(&null_wstr()); // stStyleHeader
            column.extend_from_slice(&null_wstr()); // stStyleInsertRow
            column.extend_from_slice(&null_wstr()); // stStyleAgg
            table.extend_from_slice(&rec(BRT_BEGIN_LIST_COL, &column));
        }
        let mut style = 0b100u16.to_le_bytes().to_vec(); // fRowStripes
        style.extend_from_slice(&wstr("TableStyleMedium9"));
        table.extend_from_slice(&rec(BRT_TABLE_STYLE_CLIENT, &style));

        let wb_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/></Relationships>"#;
        let sheet_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", wb_rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", sheet.as_slice()),
            ("xl/worksheets/_rels/sheet1.bin.rels", sheet_rels.as_bytes()),
            ("xl/tables/table1.bin", table.as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
        let tables = wb.sheets[0].tables();

        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "SalesTable");
        assert_eq!(tables[0].range, (0, 0, 2, 2));
        assert_eq!(tables[0].columns, vec!["Item", "Qty", "Total"]);
        assert_eq!(tables[0].style.as_deref(), Some("TableStyleMedium9"));
    }

    #[test]
    fn xlsb_sheet_view_and_autofilter_surface_public_metadata() {
        const BRT_BEGIN_WS_VIEWS: u32 = 133;
        const BRT_END_WS_VIEWS: u32 = 134;
        const BRT_BEGIN_WS_VIEW: u32 = 137;
        const BRT_END_WS_VIEW: u32 = 138;
        const BRT_BEGIN_AFILTER: u32 = 161;
        const BRT_END_AFILTER: u32 = 162;
        const BRT_PANE: u32 = 151;

        let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("View"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let mut ws_view = Vec::new();
        let flags = (1u16 << 4) // fDspZeros
            | (1u16 << 5) // fRightToLeft
            | (1u16 << 6) // fSelected
            | (1u16 << 7) // fDspRuler
            | (1u16 << 8) // fDspGuts
            | (1u16 << 9); // fDefaultHdr
        ws_view.extend_from_slice(&flags.to_le_bytes());
        ws_view.extend_from_slice(&0u32.to_le_bytes()); // xlView normal
        ws_view.extend_from_slice(&0u32.to_le_bytes()); // rwTop
        ws_view.extend_from_slice(&0u32.to_le_bytes()); // colLeft
        ws_view.push(0x40); // icvHdr
        ws_view.push(0); // reserved2
        ws_view.extend_from_slice(&0u16.to_le_bytes()); // reserved3
        ws_view.extend_from_slice(&125u16.to_le_bytes()); // wScale
        ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScaleNormal
        ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScaleSLV
        ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScalePLV
        ws_view.extend_from_slice(&0u32.to_le_bytes()); // iWbkView

        let mut pane = Vec::new();
        pane.extend_from_slice(&1.0f64.to_le_bytes()); // frozen rows
        pane.extend_from_slice(&2.0f64.to_le_bytes()); // frozen columns
        pane.extend_from_slice(&1u32.to_le_bytes()); // rwTop
        pane.extend_from_slice(&2u32.to_le_bytes()); // colLeft
        pane.extend_from_slice(&0u32.to_le_bytes()); // pnnAct
        pane.push(0x01); // fFrozen

        let mut autofilter = Vec::new();
        for value in [0u32, 9, 0, 3] {
            autofilter.extend_from_slice(&value.to_le_bytes());
        }

        let mut sheet = rec(BRT_BEGIN_WS_VIEWS, &[]);
        sheet.extend_from_slice(&rec(BRT_BEGIN_WS_VIEW, &ws_view));
        sheet.extend_from_slice(&rec(BRT_PANE, &pane));
        sheet.extend_from_slice(&rec(BRT_END_WS_VIEW, &[]));
        sheet.extend_from_slice(&rec(BRT_END_WS_VIEWS, &[]));
        sheet.extend_from_slice(&rec(BRT_BEGIN_AFILTER, &autofilter));
        sheet.extend_from_slice(&rec(BRT_END_AFILTER, &[]));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", sheet.as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
        let sheet = &wb.sheets[0];

        assert_eq!(
            sheet.sheet_view(),
            crate::SheetView {
                freeze: Some((1, 2)),
                hide_gridlines: true,
                zoom: Some(125),
                show_headers: Some(false),
                right_to_left: true,
            }
        );
        assert_eq!(sheet.autofilter_range(), Some((0, 0, 9, 3)));
    }

    #[test]
    fn xlsb_sheet_view_explicit_visible_headers_are_preserved() {
        const BRT_BEGIN_WS_VIEWS: u32 = 133;
        const BRT_END_WS_VIEWS: u32 = 134;
        const BRT_BEGIN_WS_VIEW: u32 = 137;
        const BRT_END_WS_VIEW: u32 = 138;

        let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("View"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let mut ws_view = Vec::new();
        let flags = (1u16 << 2) // display gridlines
            | (1u16 << 3); // display row/column headings
        ws_view.extend_from_slice(&flags.to_le_bytes());
        ws_view.extend_from_slice(&0u32.to_le_bytes()); // xlView normal
        ws_view.extend_from_slice(&0u32.to_le_bytes()); // rwTop
        ws_view.extend_from_slice(&0u32.to_le_bytes()); // colLeft
        ws_view.push(0x40); // icvHdr
        ws_view.push(0); // reserved2
        ws_view.extend_from_slice(&0u16.to_le_bytes()); // reserved3
        ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScale
        ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScaleNormal
        ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScaleSLV
        ws_view.extend_from_slice(&0u16.to_le_bytes()); // wScalePLV
        ws_view.extend_from_slice(&0u32.to_le_bytes()); // iWbkView

        let mut sheet = rec(BRT_BEGIN_WS_VIEWS, &[]);
        sheet.extend_from_slice(&rec(BRT_BEGIN_WS_VIEW, &ws_view));
        sheet.extend_from_slice(&rec(BRT_END_WS_VIEW, &[]));
        sheet.extend_from_slice(&rec(BRT_END_WS_VIEWS, &[]));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", sheet.as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();

        assert_eq!(wb.sheets[0].sheet_view().show_headers, Some(true));
    }

    #[test]
    fn xlsb_ws_prop_tab_color_surfaces_public_metadata() {
        const BRT_WS_PROP: u32 = 147;

        let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("Color"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let mut color = Vec::new();
        color.push(1 | (2 << 1)); // fValidRGB + xColorType=ARGB
        color.push(0); // index, ignored for ARGB
        color.extend_from_slice(&0i16.to_le_bytes()); // nTintAndShade
        color.extend_from_slice(&[0x12, 0x34, 0x56, 0xFF]); // RGB + alpha

        let mut ws_prop = Vec::new();
        ws_prop.extend_from_slice(&0u16.to_le_bytes()); // worksheet property flags
        ws_prop.push(0); // filter/conditional-format flags
        ws_prop.extend_from_slice(&color);
        ws_prop.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // rwSync ignored
        ws_prop.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // colSync ignored
        ws_prop.extend_from_slice(&wstr("")); // code name

        let sheet = rec(BRT_WS_PROP, &ws_prop);

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", sheet.as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();

        assert_eq!(wb.sheets[0].tab_color(), Some(Color::rgb(0x12, 0x34, 0x56)));
    }

    #[test]
    fn xlsb_sheet_protection_surfaces_public_metadata() {
        fn sheet_protection() -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&0u16.to_le_bytes()); // protpwd
            for allowed in [
                1u32, // fLocked
                0,    // fObjects (not modeled)
                0,    // fScenarios (not modeled)
                1,    // fFormatCells
                0,    // fFormatColumns
                1,    // fFormatRows
                0,    // fInsertColumns
                1,    // fInsertRows
                1,    // fInsertHyperlinks
                0,    // fDeleteColumns
                1,    // fDeleteRows
                1,    // fSelLockedCells (not modeled)
                1,    // fSort
                1,    // fAutoFilter
                0,    // fPivotTables
                1,    // fSelUnlockedCells (not modeled)
            ] {
                out.extend_from_slice(&allowed.to_le_bytes());
            }
            out
        }

        let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("Protected"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let sheet = rec(BRT_SHEET_PROTECTION, &sheet_protection());

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", sheet.as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
        let sheet = &wb.sheets[0];

        assert!(sheet.is_protected());
        assert_eq!(
            sheet.protection_options(),
            Some(crate::ProtectionOptions {
                sort: true,
                auto_filter: true,
                format_cells: true,
                format_rows: true,
                insert_rows: true,
                insert_hyperlinks: true,
                delete_rows: true,
                ..Default::default()
            })
        );

        let metadata = sheet.metadata();
        assert!(metadata.protected);
        assert_eq!(metadata.protection_options, sheet.protection_options());
    }

    #[test]
    fn xlsb_book_protection_surfaces_workbook_metadata() {
        const BRT_BOOK_PROTECTION: u32 = 534;

        fn book_protection(flags: u16) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&0u16.to_le_bytes()); // protpwdBook
            out.extend_from_slice(&0u16.to_le_bytes()); // protpwdRev
            out.extend_from_slice(&flags.to_le_bytes()); // wFlags
            out
        }

        let mut wb_bin = rec(BRT_BOOK_PROTECTION, &book_protection(0x0001));
        let mut sheet_ref = vec![0u8; 8]; // hsState + iTabID
        sheet_ref.extend_from_slice(&wstr("rId1"));
        sheet_ref.extend_from_slice(&wstr("LockedBook"));
        wb_bin.extend_from_slice(&rec(BRT_BUNDLE_SH, &sheet_ref));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", [].as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();

        assert!(wb.is_structure_protected());
        assert!(wb.metadata().structure_protected);
    }

    #[test]
    fn xlsb_outline_records_surface_public_metadata() {
        fn row_hdr(row: u32, level: u8, collapsed: bool) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&row.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // ixfe
            out.extend_from_slice(&400u16.to_le_bytes()); // miyRw: 20 pt
            let mut flags = u16::from(level) << 8;
            if collapsed {
                flags |= 1 << 11;
                flags |= 1 << 5; // hidden
            }
            out.extend_from_slice(&flags.to_le_bytes());
            out
        }

        fn col_info(first: u32, last: u32, level: u8) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&first.to_le_bytes());
            out.extend_from_slice(&last.to_le_bytes());
            out.extend_from_slice(&0x08FFu32.to_le_bytes()); // default width
            out.extend_from_slice(&0u32.to_le_bytes()); // ixfe
            out.extend_from_slice(&((u16::from(level) << 8) | 1).to_le_bytes());
            out
        }

        let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("Outline"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let mut ws_prop = Vec::new();
        ws_prop.extend_from_slice(&0u16.to_le_bytes()); // summaries above/left
        ws_prop.push(0); // filter/conditional-format flags
        ws_prop.extend_from_slice(&[0u8; 8]); // no tab color
        ws_prop.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // rwSync ignored
        ws_prop.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // colSync ignored
        ws_prop.extend_from_slice(&wstr("")); // code name

        let mut sheet = rec(BRT_WS_PROP, &ws_prop);
        sheet.extend_from_slice(&rec(BRT_COL_INFO, &col_info(1, 3, 3)));
        sheet.extend_from_slice(&rec(BRT_ROW_HDR, &row_hdr(2, 2, true)));
        sheet.extend_from_slice(&rec(BRT_ROW_HDR, &row_hdr(3, 2, false)));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", sheet.as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
        let sheet = &wb.sheets[0];

        assert_eq!(sheet.row_outline_levels().get(&2), Some(&2));
        assert_eq!(sheet.row_outline_levels().get(&3), Some(&2));
        assert!(sheet.collapsed_rows().contains(&2));
        assert_eq!(sheet.col_outline_levels().get(&1), Some(&3));
        assert_eq!(sheet.col_outline_levels().get(&3), Some(&3));
        assert_eq!(sheet.row_heights().get(&2), Some(&20.0));
        assert!(sheet.hidden_rows().contains(&2));
        assert_eq!(
            sheet.column_widths().get(&1),
            Some(&(0x08FF as f32 / 256.0))
        );
        assert!(sheet.hidden_columns().contains(&1));
        assert!(!sheet.outline_summary_below());
        assert!(!sheet.outline_summary_right());

        let metadata = sheet.metadata();
        assert_eq!(metadata.row_outline_levels.get(&2), Some(&2));
        assert_eq!(metadata.col_outline_levels.get(&1), Some(&3));
        assert!(metadata.collapsed_rows.contains(&2));
        assert!(!metadata.outline_summary_below);
        assert!(!metadata.outline_summary_right);
    }

    #[test]
    fn xlsb_page_setup_records_surface_public_metadata() {
        const BRT_MARGINS: u32 = 475;
        const BRT_PRINT_OPTIONS: u32 = 476;
        const BRT_PAGE_SETUP: u32 = 477;
        const BRT_BEGIN_HEADER_FOOTER: u32 = 478;
        const BRT_END_HEADER_FOOTER: u32 = 479;

        let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("Print"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let mut margins = Vec::new();
        for margin in [0.7, 0.8, 0.9, 1.0, 0.3, 0.4] {
            margins.extend_from_slice(&f64::to_le_bytes(margin));
        }

        let mut page_setup = Vec::new();
        page_setup.extend_from_slice(&9u32.to_le_bytes()); // iPaperSize: A4
        page_setup.extend_from_slice(&80u32.to_le_bytes()); // iScale
        page_setup.extend_from_slice(&600u32.to_le_bytes()); // iRes
        page_setup.extend_from_slice(&600u32.to_le_bytes()); // iVRes
        page_setup.extend_from_slice(&1u32.to_le_bytes()); // iCopies
        page_setup.extend_from_slice(&3i32.to_le_bytes()); // iPageStart
        page_setup.extend_from_slice(&2u32.to_le_bytes()); // iFitWidth
        page_setup.extend_from_slice(&1u32.to_le_bytes()); // iFitHeight
        page_setup.extend_from_slice(&((1u16 << 1) | (1u16 << 7)).to_le_bytes());
        page_setup.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // szRelID null

        let mut header_footer = Vec::new();
        header_footer.extend_from_slice(&0u16.to_le_bytes());
        header_footer.extend_from_slice(&wstr("&CQuarterly"));
        header_footer.extend_from_slice(&wstr("&RPage &P"));
        for _ in 0..4 {
            header_footer.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }

        let mut sheet = rec(BRT_MARGINS, &margins);
        sheet.extend_from_slice(&rec(BRT_PRINT_OPTIONS, &0b1111u16.to_le_bytes()));
        sheet.extend_from_slice(&rec(BRT_PAGE_SETUP, &page_setup));
        sheet.extend_from_slice(&rec(BRT_BEGIN_HEADER_FOOTER, &header_footer));
        sheet.extend_from_slice(&rec(BRT_END_HEADER_FOOTER, &[]));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", sheet.as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
        let ps = wb.sheets[0].page_setup().expect("page setup");

        assert!(ps.landscape);
        assert_eq!(ps.paper_size, Some(9));
        assert_eq!(ps.scale, Some(80));
        assert_eq!(ps.fit_to_width, Some(2));
        assert_eq!(ps.fit_to_height, Some(1));
        assert_eq!(ps.first_page_number, Some(3));
        assert_eq!(ps.margins, Some((0.7, 0.8, 0.9, 1.0, 0.3, 0.4)));
        assert!(ps.center_horizontally);
        assert!(ps.center_vertically);
        assert!(wb.sheets[0].print_headings());
        assert!(wb.sheets[0].print_gridlines());
        assert_eq!(ps.header.as_deref(), Some("&CQuarterly"));
        assert_eq!(ps.footer.as_deref(), Some("&RPage &P"));
    }

    #[test]
    fn xlsb_data_validations_surface_public_metadata() {
        const BRT_DVAL: u32 = 64;
        const BRT_BEGIN_DVALS: u32 = 573;
        const BRT_END_DVALS: u32 = 574;
        const BRT_DVAL_LIST: u32 = 681;

        let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("Validation"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let mut dvals = Vec::new();
        dvals.extend_from_slice(&0u16.to_le_bytes()); // DVals flags
        dvals.extend_from_slice(&0u32.to_le_bytes()); // xLeft
        dvals.extend_from_slice(&0u32.to_le_bytes()); // yTop
        dvals.extend_from_slice(&0u32.to_le_bytes()); // unused
        dvals.extend_from_slice(&1u32.to_le_bytes()); // idvMac

        let mut dval = Vec::new();
        let flags = 3u32 // valType=list
            | (1u32 << 8) // fAllowBlank
            | (1u32 << 18); // fShowInputMsg
        dval.extend_from_slice(&flags.to_le_bytes());
        dval.extend_from_slice(&2i32.to_le_bytes()); // two UncheckedRfX ranges
        for value in [0u32, 0, 0, 0, 2, 3, 0, 0] {
            dval.extend_from_slice(&value.to_le_bytes()); // A1, A3:A4
        }
        dval.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // strErrorTitle null
        dval.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // strError null
        dval.extend_from_slice(&wstr("Pick")); // strPromptTitle
        dval.extend_from_slice(&wstr("Choose one")); // strPrompt
        dval.extend_from_slice(&0u32.to_le_bytes()); // formula1.cce
        dval.extend_from_slice(&0u32.to_le_bytes()); // formula1.cb
        dval.extend_from_slice(&0u32.to_le_bytes()); // formula2.cce
        dval.extend_from_slice(&0u32.to_le_bytes()); // formula2.cb

        let mut sheet = rec(BRT_BEGIN_DVALS, &dvals);
        sheet.extend_from_slice(&rec(BRT_DVAL_LIST, &wstr("\"Yes,No\"")));
        sheet.extend_from_slice(&rec(BRT_DVAL, &dval));
        sheet.extend_from_slice(&rec(BRT_END_DVALS, &[]));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", sheet.as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
        let validations = wb.sheets[0].data_validations();

        assert_eq!(validations.len(), 2);
        assert_eq!(validations[0].sqref, (0, 0, 0, 0));
        assert_eq!(validations[1].sqref, (2, 0, 3, 0));
        assert_eq!(validations[0].kind, crate::DvKind::List);
        assert_eq!(validations[0].operator, crate::DvOp::Between);
        assert_eq!(validations[0].formula1, "\"Yes,No\"");
        assert!(validations[0].allow_blank);
        assert!(validations[0].show_input_message);
        assert!(!validations[0].show_error_message);
        assert_eq!(
            validations[0].prompt.as_ref(),
            Some(&("Pick".to_string(), "Choose one".to_string()))
        );
    }

    #[test]
    fn xlsb_data_validations_consume_ranges_beyond_retained_cap() {
        let mut dval = Vec::new();
        dval.extend_from_slice(&3u32.to_le_bytes()); // valType=list
        dval.extend_from_slice(&((MAX_DVAL_RANGES + 1) as i32).to_le_bytes());
        for _ in 0..=MAX_DVAL_RANGES {
            for value in [0u32, 0, 0, 0] {
                dval.extend_from_slice(&value.to_le_bytes());
            }
        }
        for _ in 0..4 {
            dval.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        dval.extend_from_slice(&0u32.to_le_bytes()); // formula1.cce
        dval.extend_from_slice(&0u32.to_le_bytes()); // formula1.cb
        dval.extend_from_slice(&0u32.to_le_bytes()); // formula2.cce
        dval.extend_from_slice(&0u32.to_le_bytes()); // formula2.cb

        let validations = parse_dval(&dval, Some("\"A\"".to_string()));

        assert_eq!(validations.len(), MAX_DVAL_RANGES);
        assert_eq!(validations[0].kind, crate::DvKind::List);
        assert_eq!(validations[0].formula1, "\"A\"");
    }

    #[test]
    fn xlsb_defined_name_is_read_from_brt_name_record() {
        let mut name = Vec::new();
        name.extend_from_slice(&0u32.to_le_bytes()); // flags: visible, non-built-in
        name.push(0); // chKey
        name.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // workbook scope
        name.extend_from_slice(&wstr("Answer"));
        name.extend_from_slice(&3u32.to_le_bytes()); // cce
        name.extend_from_slice(&[0x1E, 42, 0]); // PtgInt(42)
        name.extend_from_slice(&0u32.to_le_bytes()); // formula cb
        name.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // null comment

        let mut wb_bin = rec(39, &name);
        let mut local_name = Vec::new();
        local_name.extend_from_slice(&0u32.to_le_bytes());
        local_name.push(0);
        local_name.extend_from_slice(&0u32.to_le_bytes()); // zero-based sheet scope
        local_name.extend_from_slice(&wstr("Rate"));
        local_name.extend_from_slice(&3u32.to_le_bytes());
        local_name.extend_from_slice(&[0x1E, 7, 0]);
        local_name.extend_from_slice(&0u32.to_le_bytes());
        local_name.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        wb_bin.extend_from_slice(&rec(39, &local_name));
        let mut bundle = vec![0u8; 8]; // hsState + iTabID
        bundle.extend_from_slice(&wstr("rId1"));
        bundle.extend_from_slice(&wstr("S1"));
        wb_bin.extend_from_slice(&rec(BRT_BUNDLE_SH, &bundle));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;
        let mut formula = vec![0u8; 8];
        formula.extend_from_slice(&42.0f64.to_le_bytes());
        formula.extend_from_slice(&[0, 0]);
        let rgce = [0x23, 1, 0, 0, 0]; // PtgName, one-based BrtName index 1
        formula.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
        formula.extend_from_slice(&rgce);
        formula.extend_from_slice(&0u32.to_le_bytes());
        let sheet = rec(BRT_FMLA_NUM, &formula);
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", sheet.as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
        assert_eq!(
            wb.defined_names(),
            &[("Answer".to_string(), "42".to_string())]
        );
        assert_eq!(
            wb.local_defined_names(),
            &[crate::LocalDefinedName {
                sheet: "S1".into(),
                name: "Rate".into(),
                refers_to: "7".into(),
            }]
        );
        assert_eq!(
            wb.sheets[0].cell(0, 0),
            Some(&Cell::Formula {
                formula: "Answer".to_string(),
                cached: Box::new(Cell::Number(42.0))
            })
        );
    }

    #[test]
    fn xlsb_sheet_local_builtin_names_surface_page_setup() {
        fn name_builtin(name_text: &str, sheet_index: u32, rgce: &[u8]) -> Vec<u8> {
            let mut name = Vec::new();
            name.extend_from_slice(&0x20u32.to_le_bytes()); // flags: built-in
            name.push(0); // chKey: no macro shortcut
            name.extend_from_slice(&sheet_index.to_le_bytes());
            name.extend_from_slice(&wstr(name_text));
            name.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
            name.extend_from_slice(rgce);
            name.extend_from_slice(&0u32.to_le_bytes()); // formula cb
            name.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // null comment
            name
        }

        fn ptg_area(r0: u32, c0: u16, r1: u32, c1: u16) -> Vec<u8> {
            let mut rgce = vec![0x25]; // PtgArea with BIFF12 row widths
            rgce.extend_from_slice(&r0.to_le_bytes());
            rgce.extend_from_slice(&r1.to_le_bytes());
            rgce.extend_from_slice(&c0.to_le_bytes());
            rgce.extend_from_slice(&c1.to_le_bytes());
            rgce
        }

        let print_area = ptg_area(1, 1, 5, 3);
        let mut print_titles = ptg_area(0, 0, 1, MAX_XLSB_COL_INDEX as u16);
        print_titles.extend_from_slice(&ptg_area(0, 0, 1_048_575, 2));
        print_titles.push(0x10); // PtgUnion

        let mut wb_bin = rec(BRT_NAME, &name_builtin("_xlnm.Print_Area", 0, &print_area));
        wb_bin.extend_from_slice(&rec(
            BRT_NAME,
            &name_builtin("_xlnm.Print_Titles", 0, &print_titles),
        ));
        let mut bundle = vec![0u8; 8]; // hsState + iTabID
        bundle.extend_from_slice(&wstr("rId1"));
        bundle.extend_from_slice(&wstr("S1"));
        wb_bin.extend_from_slice(&rec(BRT_BUNDLE_SH, &bundle));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", [].as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();

        assert!(wb.defined_names().is_empty());
        let ps = wb.sheets[0].page_setup().expect("page setup");
        assert_eq!(ps.print_area, Some((1, 1, 5, 3)));
        assert_eq!(ps.repeat_rows, Some((0, 1)));
        assert_eq!(ps.repeat_cols, Some((0, 2)));
    }

    #[test]
    fn xlsb_sheet_local_filter_database_name_surfaces_autofilter() {
        fn name_builtin(name_text: &str, sheet_index: u32, rgce: &[u8]) -> Vec<u8> {
            let mut name = Vec::new();
            name.extend_from_slice(&0x20u32.to_le_bytes()); // flags: built-in
            name.push(0); // chKey: no macro shortcut
            name.extend_from_slice(&sheet_index.to_le_bytes());
            name.extend_from_slice(&wstr(name_text));
            name.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
            name.extend_from_slice(rgce);
            name.extend_from_slice(&0u32.to_le_bytes()); // formula cb
            name.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // null comment
            name
        }

        let mut filter_area = vec![0x25]; // PtgArea with BIFF12 row widths
        filter_area.extend_from_slice(&2u32.to_le_bytes());
        filter_area.extend_from_slice(&9u32.to_le_bytes());
        filter_area.extend_from_slice(&1u16.to_le_bytes());
        filter_area.extend_from_slice(&4u16.to_le_bytes());

        let mut wb_bin = rec(
            BRT_NAME,
            &name_builtin("_xlnm._FilterDatabase", 0, &filter_area),
        );
        let mut bundle = vec![0u8; 8]; // hsState + iTabID
        bundle.extend_from_slice(&wstr("rId1"));
        bundle.extend_from_slice(&wstr("S1"));
        wb_bin.extend_from_slice(&rec(BRT_BUNDLE_SH, &bundle));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", [].as_slice()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();

        assert!(wb.defined_names().is_empty());
        assert_eq!(wb.sheets[0].autofilter_range(), Some((2, 1, 9, 4)));
        assert_eq!(wb.sheets[0].page_setup(), None);
    }

    #[test]
    fn xlsb_doc_properties_surface_through_workbook_metadata() {
        let mut wb_bin = vec![0u8; 8]; // hsState + iTabID
        wb_bin.extend_from_slice(&wstr("rId1"));
        wb_bin.extend_from_slice(&wstr("S1"));
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;
        let core = r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/"><dc:title>Binary Report</dc:title><dc:subject>Procurement</dc:subject><dc:creator>rxls xlsb</dc:creator><cp:keywords>bid,binary</cp:keywords><dc:description>XLSB public metadata</dc:description><cp:lastModifiedBy>reviewer</cp:lastModifiedBy><dcterms:created>2026-06-24T01:02:03Z</dcterms:created></cp:coreProperties>"#;
        let app = r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>Excel</Application><Company>ACME XLSB</Company></Properties>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (path, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", [].as_slice()),
            ("docProps/core.xml", core.as_bytes()),
            ("docProps/app.xml", app.as_bytes()),
        ] {
            zw.start_file(path, opt).unwrap();
            zw.write_all(body).unwrap();
        }

        let wb = Workbook::open(&zw.finish().unwrap().into_inner()).unwrap();
        let metadata = wb.metadata();
        assert_eq!(metadata.properties.title.as_deref(), Some("Binary Report"));
        assert_eq!(metadata.properties.subject.as_deref(), Some("Procurement"));
        assert_eq!(metadata.properties.creator.as_deref(), Some("rxls xlsb"));
        assert_eq!(metadata.properties.keywords.as_deref(), Some("bid,binary"));
        assert_eq!(
            metadata.properties.description.as_deref(),
            Some("XLSB public metadata")
        );
        assert_eq!(
            metadata.properties.last_modified_by.as_deref(),
            Some("reviewer")
        );
        assert_eq!(
            metadata.properties.created.as_deref(),
            Some("2026-06-24T01:02:03Z")
        );
        assert_eq!(metadata.properties.company.as_deref(), Some("ACME XLSB"));
    }

    #[test]
    fn xlsb_formula_is_decoded() {
        // BrtFmlaNum: cell(8) + xnum 30.0 + grbitFlags(2) + cce(4) + rgce. The rgce
        // is BIFF12 SUM(A1:A2): PtgArea (u32 rows) + PtgFuncVar(SUM, 1 arg).
        let mut p = vec![0u8; 8]; // col + style
        p.extend_from_slice(&30.0f64.to_le_bytes()); // xnum
        p.extend_from_slice(&[0, 0]); // grbitFlags
        let rgce: Vec<u8> = vec![
            0x25, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, // PtgArea A1:A2 (r0=0,r1=1,c0=0,c1=0)
            0x22, 0x01, 0x04, 0x00, // PtgFuncVar SUM(1 arg)
        ];
        p.extend_from_slice(&(rgce.len() as u32).to_le_bytes()); // cce
        p.extend_from_slice(&rgce);
        p.extend_from_slice(&0u32.to_le_bytes()); // cb
        let mut cells = Vec::new();
        let mut rich = BTreeMap::new();
        let mut budget = crate::MAX_TEXT_BYTES;
        decode_cell(
            BRT_FMLA_NUM,
            &p,
            0,
            0,
            0,
            &[],
            &Styles::default(),
            false,
            &mut cells,
            &mut rich,
            &mut budget,
            &[],
            &[],
            &[],
            &[],
            &BrtFormulaDefinitions::new(),
        );
        assert_eq!(cells.len(), 1);
        match &cells[0].value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "SUM($A$1:$A$2)");
                assert_eq!(**cached, Cell::Number(30.0));
            }
            o => panic!("expected a formula cell, got {o:?}"),
        }
    }

    fn brt_numeric_formula(col: u32, cached: f64, rgce: &[u8], rgb_extra: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&col.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&cached.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
        body.extend_from_slice(rgce);
        body.extend_from_slice(&(rgb_extra.len() as u32).to_le_bytes());
        body.extend_from_slice(rgb_extra);
        body
    }

    fn brt_formula_definition(
        range: (u32, u32, u32, u32),
        is_array: bool,
        rgce: &[u8],
        rgb_extra: &[u8],
    ) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&range.0.to_le_bytes());
        body.extend_from_slice(&range.1.to_le_bytes());
        body.extend_from_slice(&range.2.to_le_bytes());
        body.extend_from_slice(&range.3.to_le_bytes());
        if is_array {
            body.push(0);
        }
        body.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
        body.extend_from_slice(rgce);
        body.extend_from_slice(&(rgb_extra.len() as u32).to_le_bytes());
        body.extend_from_slice(rgb_extra);
        body
    }

    #[test]
    fn xlsb_shared_formula_is_reconstructed_for_each_cell() {
        let exp = [0x01, 0, 0, 0, 0]; // PtgExp row 0
        let exp_col = 1u32.to_le_bytes(); // PtgExtraCol B
        let shared_rgce = [0x2C, 0, 0, 0, 0, 0xFF, 0xFF]; // one column left
        let mut sheet = rec(BRT_ROW_HDR, &0u32.to_le_bytes());
        sheet.extend_from_slice(&rec(
            BRT_FMLA_NUM,
            &brt_numeric_formula(1, 10.0, &exp, &exp_col),
        ));
        sheet.extend_from_slice(&rec(
            BRT_SHR_FMLA,
            &brt_formula_definition((0, 1, 1, 1), false, &shared_rgce, &[]),
        ));
        sheet.extend_from_slice(&rec(BRT_ROW_HDR, &1u32.to_le_bytes()));
        sheet.extend_from_slice(&rec(
            BRT_FMLA_NUM,
            &brt_numeric_formula(1, 20.0, &exp, &exp_col),
        ));

        let mut budget = crate::MAX_TEXT_BYTES;
        let (cells, _, _, _) = parse_sheet(
            &sheet,
            &[],
            &Styles::default(),
            false,
            &HashMap::new(),
            &mut budget,
            &[],
            &[],
            &[],
            &[],
        );
        for (row, expected) in [(0, "A1"), (1, "A2")] {
            let cell = cells
                .iter()
                .find(|cell| (cell.row, cell.col) == (row, 1))
                .unwrap();
            match &cell.value {
                Cell::Formula { formula, .. } => assert_eq!(formula, expected),
                other => panic!("expected shared formula at row {row}, got {other:?}"),
            }
        }
    }

    #[test]
    fn xlsb_array_formula_and_array_constant_are_reconstructed() {
        let exp = [0x01, 0, 0, 0, 0];
        let exp_col = 0u32.to_le_bytes();
        let array_rgce = [0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut array_extra = 1u32.to_le_bytes().to_vec();
        array_extra.extend_from_slice(&2u32.to_le_bytes());
        array_extra.push(0);
        array_extra.extend_from_slice(&1.0f64.to_le_bytes());
        array_extra.push(0);
        array_extra.extend_from_slice(&2.0f64.to_le_bytes());

        let mut sheet = rec(BRT_ROW_HDR, &0u32.to_le_bytes());
        sheet.extend_from_slice(&rec(
            BRT_FMLA_NUM,
            &brt_numeric_formula(0, 1.0, &exp, &exp_col),
        ));
        sheet.extend_from_slice(&rec(
            BRT_ARR_FMLA,
            &brt_formula_definition((0, 0, 0, 1), true, &array_rgce, &array_extra),
        ));
        sheet.extend_from_slice(&rec(
            BRT_FMLA_NUM,
            &brt_numeric_formula(1, 2.0, &exp, &exp_col),
        ));

        let mut budget = crate::MAX_TEXT_BYTES;
        let (cells, _, _, _) = parse_sheet(
            &sheet,
            &[],
            &Styles::default(),
            false,
            &HashMap::new(),
            &mut budget,
            &[],
            &[],
            &[],
            &[],
        );
        for col in 0..=1 {
            let cell = cells
                .iter()
                .find(|cell| (cell.row, cell.col) == (0, col))
                .unwrap();
            match &cell.value {
                Cell::Formula { formula, .. } => assert_eq!(formula, "{1,2}"),
                other => panic!("expected array formula at col {col}, got {other:?}"),
            }
        }
    }

    #[test]
    fn xlsb_formula_string_is_decoded() {
        // BrtFmlaString stores its cached string inside the formula cell record
        // (`XLWideString`), unlike BIFF8 `.xls` which uses a following STRING
        // record for string-result formulas.
        let mut p = vec![0u8; 8]; // col + style
        p.extend_from_slice(&wstr("cached"));
        p.extend_from_slice(&[0, 0]); // grbitFlags
        let rgce = vec![0x17, 1, 0, b'x']; // PtgStr("x")
        p.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
        p.extend_from_slice(&rgce);
        p.extend_from_slice(&0u32.to_le_bytes());

        let mut cells = Vec::new();
        let mut rich = BTreeMap::new();
        let mut budget = crate::MAX_TEXT_BYTES;
        decode_cell(
            BRT_FMLA_STRING,
            &p,
            0,
            0,
            0,
            &[],
            &Styles::default(),
            false,
            &mut cells,
            &mut rich,
            &mut budget,
            &[],
            &[],
            &[],
            &[],
            &BrtFormulaDefinitions::new(),
        );

        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].text, "cached");
        match &cells[0].value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "\"x\"");
                assert_eq!(**cached, Cell::Text("cached".to_string()));
            }
            other => panic!("expected a string-result formula cell, got {other:?}"),
        }
    }

    #[test]
    fn xlsb_formula_resolves_3d_sheet_range() {
        let mut p = vec![0u8; 8];
        p.extend_from_slice(&9.0f64.to_le_bytes());
        p.extend_from_slice(&[0, 0]);
        let mut rgce = vec![0x3A, 0, 0]; // PtgRef3d, ixti 0
        rgce.extend_from_slice(&4u32.to_le_bytes());
        rgce.extend_from_slice(&2u16.to_le_bytes());
        p.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
        p.extend_from_slice(&rgce);
        p.extend_from_slice(&0u32.to_le_bytes());

        let sheet_names = vec!["Start".to_string(), "End Sheet".to_string()];
        let extern_sheets = vec![crate::ptg::ExternSheet {
            supbook_index: 0,
            first_sheet: 0,
            last_sheet: 1,
        }];
        let mut cells = Vec::new();
        let mut rich = BTreeMap::new();
        let mut budget = crate::MAX_TEXT_BYTES;
        decode_cell(
            BRT_FMLA_NUM,
            &p,
            0,
            0,
            0,
            &[],
            &Styles::default(),
            false,
            &mut cells,
            &mut rich,
            &mut budget,
            &sheet_names,
            &extern_sheets,
            &[],
            &[],
            &BrtFormulaDefinitions::new(),
        );

        match &cells[0].value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "Start:'End Sheet'!$C$5");
                assert_eq!(cached.as_ref(), &Cell::Number(9.0));
            }
            other => panic!("expected 3D formula, got {other:?}"),
        }
    }

    #[test]
    fn xlsb_extern_sheet_record_preserves_first_and_last_sheet() {
        let mut p = 1u32.to_le_bytes().to_vec();
        p.extend_from_slice(&0u32.to_le_bytes()); // supporting link index
        p.extend_from_slice(&2i32.to_le_bytes());
        p.extend_from_slice(&4i32.to_le_bytes());
        assert_eq!(
            parse_brt_extern_sheets(&p),
            vec![crate::ptg::ExternSheet {
                supbook_index: 0,
                first_sheet: 2,
                last_sheet: 4
            }]
        );
    }

    #[test]
    fn xlsb_formula_string_budget_exhaustion_sets_partial_signal() {
        // BrtFmlaString: cell(8) + XLWideString cached value + grbitFlags(2) +
        // CellParsedFormula. If the cached display text cannot fit in the shared
        // workbook text budget, parsing must leave a partial-extraction signal.
        let mut p = vec![0u8; 8]; // col + style
        p.extend_from_slice(&wstr("toolong"));
        p.extend_from_slice(&[0, 0]); // grbitFlags
        let rgce = vec![0x17, 1, 0, b'x']; // PtgStr("x")
        p.extend_from_slice(&(rgce.len() as u32).to_le_bytes());
        p.extend_from_slice(&rgce);
        p.extend_from_slice(&0u32.to_le_bytes());

        let sh = rec(BRT_FMLA_STRING, &p);
        let mut budget = "toolong".len() - 1;
        let (cells, _merges, _hyperlinks, _metadata) = parse_sheet(
            &sh,
            &[],
            &Styles::default(),
            false,
            &HashMap::new(),
            &mut budget,
            &[],
            &[],
            &[],
            &[],
        );

        assert!(cells.is_empty());
        assert_eq!(budget, 0);
    }

    #[test]
    fn bundle_sh_hsstate_visibility() {
        // BrtBundleSh: hsState:u32 (0 visible / 1 hidden / 2 veryHidden), iTabID:u32,
        // strRelID, strName. Build one with hsState=1 and assert it parses as hidden.
        let bundle = |hs_state: u32| {
            let mut p = hs_state.to_le_bytes().to_vec(); // hsState
            p.extend_from_slice(&0u32.to_le_bytes()); // iTabID
            p.extend_from_slice(&wstr("rId1")); // strRelID
            p.extend_from_slice(&wstr("S1")); // strName
            let (names, _, _, _, _, _, _, _, _) = parse_workbook(&rec(BRT_BUNDLE_SH, &p), &[]);
            names
        };
        assert_eq!(bundle(0), vec![("S1".to_string(), "rId1".to_string(), 0)]);
        assert_eq!(bundle(1), vec![("S1".to_string(), "rId1".to_string(), 1)]);
        assert_eq!(bundle(2), vec![("S1".to_string(), "rId1".to_string(), 2)]);
    }

    #[test]
    fn xlsb_hidden_sheet_end_to_end() {
        // workbook.bin: one BrtBundleSh with hsState=1 (hidden) for "Secret".
        let mut wb_bin = 1u32.to_le_bytes().to_vec(); // hsState = 1 (hidden)
        wb_bin.extend_from_slice(&0u32.to_le_bytes()); // iTabID
        wb_bin.extend_from_slice(&wstr("rId1")); // strRelID
        wb_bin.extend_from_slice(&wstr("Secret")); // strName
        let wb_bin = rec(BRT_BUNDLE_SH, &wb_bin);

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.bin"/></Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (name, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", [].as_slice()),
        ] {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        assert_eq!(wb.sheets[0].name, "Secret");
        assert!(wb.sheets[0].is_hidden(), "hsState=1 => hidden");
        assert!(!wb.sheets[0].is_very_hidden());
    }

    #[test]
    fn xlsb_bundle_sheet_preserves_sheet_types_end_to_end() {
        let bundle = |name: &str, rid: Option<&str>, hs_state: u32, tab_id: u32| {
            let mut p = hs_state.to_le_bytes().to_vec();
            p.extend_from_slice(&tab_id.to_le_bytes());
            if let Some(rid) = rid {
                p.extend_from_slice(&wstr(rid));
            } else {
                p.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            }
            p.extend_from_slice(&wstr(name));
            rec(BRT_BUNDLE_SH, &p)
        };

        let mut wb_bin = bundle("Data", Some("rId1"), 0, 1);
        wb_bin.extend_from_slice(&bundle("Chart", Some("rId2"), 0, 2));
        wb_bin.extend_from_slice(&bundle("Macro", Some("rId3"), 1, 3));
        wb_bin.extend_from_slice(&bundle("Dialog", Some("rId4"), 2, 4));
        wb_bin.extend_from_slice(&bundle("Module", None, 2, 5));

        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/>
            <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet1.bin"/>
            <Relationship Id="rId3" Type="http://schemas.microsoft.com/office/2006/relationships/xlMacrosheet" Target="macrosheets/sheet1.bin"/>
            <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/dialogsheet" Target="dialogsheets/sheet1.bin"/>
        </Relationships>"#;

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        for (name, body) in [
            ("xl/workbook.bin", wb_bin.as_slice()),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", [].as_slice()),
        ] {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(
            wb.sheets_metadata(),
            vec![
                SheetMetadata {
                    name: "Data".to_string(),
                    typ: SheetType::WorkSheet,
                    visible: SheetVisible::Visible,
                },
                SheetMetadata {
                    name: "Chart".to_string(),
                    typ: SheetType::ChartSheet,
                    visible: SheetVisible::Visible,
                },
                SheetMetadata {
                    name: "Macro".to_string(),
                    typ: SheetType::MacroSheet,
                    visible: SheetVisible::Hidden,
                },
                SheetMetadata {
                    name: "Dialog".to_string(),
                    typ: SheetType::DialogSheet,
                    visible: SheetVisible::VeryHidden,
                },
                SheetMetadata {
                    name: "Module".to_string(),
                    typ: SheetType::Vba,
                    visible: SheetVisible::VeryHidden,
                },
            ]
        );
        assert_eq!(
            wb.worksheets()
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            vec!["Data".to_string()]
        );
    }

    #[test]
    fn parses_date1904_flag() {
        // BrtWbProp with bit 0 set => 1904 date system (matches calamine/MS-XLSB).
        let on = rec(BRT_WB_PROP, &[0x01, 0, 0, 0]);
        assert!(parse_workbook(&on, &[]).1, "bit 0 set => 1904");
        let off = rec(BRT_WB_PROP, &[0x00, 0, 0, 0]);
        assert!(!parse_workbook(&off, &[]).1, "bit 0 clear => 1900");
    }
}
