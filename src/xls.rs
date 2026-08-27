//! The legacy `.xls` (OLE2 / BIFF) reader.
//!
//! `.xls` is an OLE2/CFB compound file whose `Workbook`/`Book` stream is a
//! sequence of BIFF records. The OLE2 container is decoded in [`crate::ole`];
//! this module walks the BIFF record stream on top of it — the SST, the cell
//! records (LABELSST/LABEL/RK/MULRK/NUMBER/BOOLERR/FORMULA/STRING), merges,
//! hyperlinks, comments, outline/protection records, data-validation/page setup
//! metadata, and the codepage/date/format globals — into the [`crate::Workbook`]
//! model.

mod style;
mod workbook;
mod worksheet;

#[cfg(test)]
use style::parse_biff_xf;
use style::{apply_palette_record, XlsStyles, BIFF_DEFAULT_PALETTE};
use workbook::{
    apply_sheet_builtin_names, parse_boundsheet, parse_extern_name, parse_extern_sheets, parse_lbl,
    parse_lbl_formula_name, parse_ole_doc_properties, parse_window1_active_sheet,
    read_short_string, read_xl_string, ParsedLbl, SheetBuiltinName,
};
use worksheet::{
    apply_col_outline, apply_formula_definition, apply_row_outline, apply_sheet_page_setups,
    apply_wsbool_outline, decode_cell, decode_string_cell, formula_context, parse_dv,
    parse_formula_definition, parse_hlink, parse_mergecells, parse_note_obj_id, parse_note_sh,
    parse_pane_freeze, parse_sheet_ext_tab_color, parse_txo_text, parse_window2,
    retain_blank_cell_styles, FormulaDefinitions, PendingFormula, XlsPageSetup, XlsSheetDefaults,
};
#[cfg(test)]
use worksheet::{push_cell, retained_cell_cost, FormulaDefinition};

use crate::format::Formats;
#[cfg(test)]
use crate::model::{
    Alignment, Border, BorderStyle, Cell, CellEntry, CellProtection, Color, Fill, FormatPattern,
    FormatScript, HAlign, ImportedAxisMeasure, PrintLossKind, PrintPageOrder, VAlign,
};
use crate::model::{Comment, Sheet, SheetType, StyleFidelity};
#[cfg(test)]
use crate::rk_to_f64;
use crate::{Error, Result, Workbook, MAX_TEXT_BYTES};

use encoding_rs::{
    Encoding, BIG5, EUC_KR, GBK, SHIFT_JIS, UTF_8, WINDOWS_1251, WINDOWS_1252, WINDOWS_1253,
    WINDOWS_1254, WINDOWS_1255, WINDOWS_1256, WINDOWS_1258, WINDOWS_874,
};
#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::{BTreeSet, HashMap, VecDeque};

// --- BIFF record type ids ([MS-XLS] 2.3) ---
const BOF: u16 = 0x0809;
const EOF: u16 = 0x000A;
const SST: u16 = 0x00FC;
const CONTINUE: u16 = 0x003C;
const LBL: u16 = 0x0018;
const EXTERNSHEET: u16 = 0x0017;
const EXTERNNAME: u16 = 0x0023;
const SUPBOOK: u16 = 0x01AE;
const BOUNDSHEET: u16 = 0x0085;
const CODEPAGE: u16 = 0x0042;
const FILEPASS: u16 = 0x002F;
const PROTECT: u16 = 0x0012;
const DATEMODE: u16 = 0x0022;
const FONT: u16 = 0x0031;
const XF: u16 = 0x00E0;
const FORMAT: u16 = 0x041E;
const STYLE: u16 = 0x0293;
const PALETTE: u16 = 0x0092;
const HEADER: u16 = 0x0014;
const FOOTER: u16 = 0x0015;
const VERTICALPAGEBREAKS: u16 = 0x001A;
const HORIZONTALPAGEBREAKS: u16 = 0x001B;
const NOTE: u16 = 0x001C;
const LEFTMARGIN: u16 = 0x0026;
const RIGHTMARGIN: u16 = 0x0027;
const TOPMARGIN: u16 = 0x0028;
const BOTTOMMARGIN: u16 = 0x0029;
const PRINTHEADERS: u16 = 0x002A;
const PRINTGRIDLINES: u16 = 0x002B;
const HCENTER: u16 = 0x0083;
const VCENTER: u16 = 0x0084;
const SETUP: u16 = 0x00A1;
const HEADERFOOTER: u16 = 0x089C;
const SHEETEXT: u16 = 0x0862;
const LABELSST: u16 = 0x00FD;
const LABEL: u16 = 0x0204;
const RSTRING: u16 = 0x00D6;
const BLANK: u16 = 0x0201;
const MULBLANK: u16 = 0x00BE;
const RK: u16 = 0x027E;
const MULRK: u16 = 0x00BD;
const NUMBER: u16 = 0x0203;
const BOOLERR: u16 = 0x0205;
const FORMULA: u16 = 0x0006;
const FORMULA_ALT: u16 = 0x0406;
const ARRAY: u16 = 0x0221;
const SHRFMLA: u16 = 0x04BC;
const STRING: u16 = 0x0207;
const ROW: u16 = 0x0208;
const COLINFO: u16 = 0x007D;
const DEFAULTCOLWIDTH: u16 = 0x0055;
const STANDARDWIDTH: u16 = 0x0099;
const DEFAULTROWHEIGHT: u16 = 0x0225;
const PANE: u16 = 0x0041;
const OBJ: u16 = 0x005D;
const WINDOW1: u16 = 0x003D;
const WINDOW2: u16 = 0x023E;
const WSBOOL: u16 = 0x0081;
const MERGECELLS: u16 = 0x00E5;
const TXO: u16 = 0x01B5;
const HLINK: u16 = 0x01B8;
const DV: u16 = 0x01BE;
const USR_EXCL: u16 = 0x0194;
const FILE_LOCK: u16 = 0x0195;
const INTERFACE_HDR: u16 = 0x00E1;
const RRD_INFO: u16 = 0x0196;
const RRD_HEAD: u16 = 0x0138;

const DEFAULT_XOR_PASSWORD: &[u8] = b"VelvetSweatshop";
const MAX_HLINK_ANCHORS: usize = 4096;
const MAX_DV_RANGES: usize = 8192;
const MAX_XLS_STYLE_RECORDS: usize = 4096;
const MAX_XLS_RETAINED_STYLE_BYTES: usize = 64 << 20;
// Valid BIFF8 FONT records are at most 78 bytes (31 UTF-16 code units); XF is
// fixed at 20 bytes. Retain no hostile tail that the style decoder cannot use.
const MAX_BIFF_FONT_RECORD_BYTES: usize = 78;
const MAX_BIFF_XF_RECORD_BYTES: usize = 20;
const MAX_BIFF_DEFAULT_COL_WIDTH_CHARS: u16 = 255;
const MAX_BIFF_DEFAULT_ROW_HEIGHT_TWIPS: i16 = 8179;
const BIFF_APPLICATION_DEFAULT_COLUMN_WIDTH_TWIPS: u32 = 1_280;
const MIN_BIFF_ROW_HEIGHT_TWIPS: u16 = 2;
const MAX_BIFF_ROW_HEIGHT_TWIPS: u16 = 8192;
const BIFF_ROW_FLAG_UNSYNCED: u32 = 1 << 6;

/// Decode context: the BIFF generation and the codepage for 8-bit strings.
#[derive(Clone, Copy)]
struct Ctx {
    /// `true` for BIFF8 (UTF-16 strings with a grbit byte); `false` for
    /// BIFF5/7 (raw codepage bytes, no grbit, no SST).
    biff8: bool,
    /// Codec for BIFF5/7 8-bit strings (cp1252 default, cp949 for Korean, …).
    enc: &'static Encoding,
}

fn resolve_encoding(cp: u16) -> &'static Encoding {
    match cp {
        932 => SHIFT_JIS,
        936 => GBK,
        949 | 51949 | 1361 => EUC_KR, // 1361 (Johab) unsupported → UHC best-effort
        950 => BIG5,
        1251 => WINDOWS_1251,
        1253 => WINDOWS_1253,
        1254 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        874 => WINDOWS_874,
        1258 => WINDOWS_1258,
        65001 => UTF_8,
        _ => WINDOWS_1252,
    }
}

#[inline]
fn u16le(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
#[inline]
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
#[inline]
fn u64le(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8)
        .map(|s| u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}
#[inline]
fn i16le(b: &[u8], o: usize) -> Option<i16> {
    b.get(o..o + 2).map(|s| i16::from_le_bytes([s[0], s[1]]))
}
#[inline]
fn f64le(b: &[u8], o: usize) -> Option<f64> {
    let bytes: [u8; 8] = b.get(o..o + 8)?.try_into().ok()?;
    Some(f64::from_le_bytes(bytes))
}

fn maybe_decrypt_default_xor(wb: &mut [u8]) -> Result<bool> {
    let mut pos = 0usize;
    let mut decrypting = false;
    while pos + 4 <= wb.len() {
        let typ = u16le(wb, pos).unwrap_or(0);
        let len = u16le(wb, pos + 2).unwrap_or(0) as usize;
        let start = pos + 4;
        let end = start.saturating_add(len);
        if end > wb.len() {
            break;
        }

        if typ == FILEPASS {
            let enc_type = u16le(wb, start).unwrap_or(1);
            if enc_type != 0 {
                return Err(Error::Encrypted);
            }
            let (Some(key), Some(verifier)) = (u16le(wb, start + 2), u16le(wb, start + 4)) else {
                return Err(Error::Encrypted);
            };
            if xor_key_method1(DEFAULT_XOR_PASSWORD) == Some(key)
                && xor_password_verifier_method1(DEFAULT_XOR_PASSWORD) == verifier
            {
                decrypting = true;
            } else {
                return Err(Error::Encrypted);
            }
        } else if decrypting && !xor_unencrypted_record(typ) {
            if typ == BOUNDSHEET {
                // `lbPlyPos` (first 4 bytes of BoundSheet8) is explicitly left
                // unobfuscated; the key stream still advances as if those bytes
                // had been transformed.
                if start + 4 < end {
                    xor_decrypt_method1(&mut wb[start + 4..end], end + 4);
                }
            } else {
                xor_decrypt_method1(&mut wb[start..end], end);
            }
        }

        pos = end;
    }
    Ok(decrypting)
}

fn xor_unencrypted_record(typ: u16) -> bool {
    matches!(
        typ,
        BOF | FILEPASS | USR_EXCL | FILE_LOCK | INTERFACE_HDR | RRD_INFO | RRD_HEAD
    )
}

fn xor_password_verifier_method1(password: &[u8]) -> u16 {
    let mut verifier = 0u16;
    for byte in password
        .iter()
        .rev()
        .copied()
        .chain(std::iter::once(password.len() as u8))
    {
        let carry = u16::from((verifier & 0x4000) != 0);
        let shifted = (verifier << 1) & 0x7FFF;
        verifier = (carry | shifted) ^ u16::from(byte);
    }
    verifier ^ 0xCE4B
}

fn xor_key_method1(password: &[u8]) -> Option<u16> {
    const INITIAL_CODE: [u16; 15] = [
        0xE1F0, 0x1D0F, 0xCC9C, 0x84C0, 0x110C, 0x0E10, 0xF1CE, 0x313E, 0x1872, 0xE139, 0xD40F,
        0x84F9, 0x280C, 0xA96A, 0x4EC3,
    ];
    const XOR_MATRIX: [u16; 105] = [
        0xAEFC, 0x4DD9, 0x9BB2, 0x2745, 0x4E8A, 0x9D14, 0x2A09, 0x7B61, 0xF6C2, 0xFDA5, 0xEB6B,
        0xC6F7, 0x9DCF, 0x2BBF, 0x4563, 0x8AC6, 0x05AD, 0x0B5A, 0x16B4, 0x2D68, 0x5AD0, 0x0375,
        0x06EA, 0x0DD4, 0x1BA8, 0x3750, 0x6EA0, 0xDD40, 0xD849, 0xA0B3, 0x5147, 0xA28E, 0x553D,
        0xAA7A, 0x44D5, 0x6F45, 0xDE8A, 0xAD35, 0x4A4B, 0x9496, 0x390D, 0x721A, 0xEB23, 0xC667,
        0x9CEF, 0x29FF, 0x53FE, 0xA7FC, 0x5FD9, 0x47D3, 0x8FA6, 0x0F6D, 0x1EDA, 0x3DB4, 0x7B68,
        0xF6D0, 0xB861, 0x60E3, 0xC1C6, 0x93AD, 0x377B, 0x6EF6, 0xDDEC, 0x45A0, 0x8B40, 0x06A1,
        0x0D42, 0x1A84, 0x3508, 0x6A10, 0xAA51, 0x4483, 0x8906, 0x022D, 0x045A, 0x08B4, 0x1168,
        0x76B4, 0xED68, 0xCAF1, 0x85C3, 0x1BA7, 0x374E, 0x6E9C, 0x3730, 0x6E60, 0xDCC0, 0xA9A1,
        0x4363, 0x86C6, 0x1DAD, 0x3331, 0x6662, 0xCCC4, 0x89A9, 0x0373, 0x06E6, 0x0DCC, 0x1021,
        0x2042, 0x4084, 0x8108, 0x1231, 0x2462, 0x48C4,
    ];
    if !(1..=15).contains(&password.len()) {
        return None;
    }
    let mut key = INITIAL_CODE[password.len() - 1];
    let mut current = 0x68usize;
    for &byte in password.iter().rev() {
        let mut ch = byte;
        for _ in 0..7 {
            if ch & 0x40 != 0 {
                key ^= XOR_MATRIX[current];
            }
            ch = ch.wrapping_mul(2);
            current = current.saturating_sub(1);
        }
    }
    Some(key)
}

fn xor_array_method1(password: &[u8]) -> Option<[u8; 16]> {
    const PAD_ARRAY: [u8; 15] = [
        0xBB, 0xFF, 0xFF, 0xBA, 0xFF, 0xFF, 0xB9, 0x80, 0x00, 0xBE, 0x0F, 0x00, 0xBF, 0x0F, 0x00,
    ];
    let key = xor_key_method1(password)?;
    let high = (key >> 8) as u8;
    let low = (key & 0x00FF) as u8;
    let mut index = password.len();
    let mut obfuscation = [0u8; 16];
    if index % 2 == 1 {
        obfuscation[index] = xor_ror(PAD_ARRAY[0], high);
        index -= 1;
        obfuscation[index] = xor_ror(*password.last()?, low);
    }
    while index > 0 {
        index -= 1;
        obfuscation[index] = xor_ror(password[index], high);
        index -= 1;
        obfuscation[index] = xor_ror(password[index], low);
    }
    let mut index = 15usize;
    let mut pad_index = 15usize.saturating_sub(password.len());
    while pad_index > 0 {
        obfuscation[index] = xor_ror(PAD_ARRAY[pad_index], high);
        index = index.saturating_sub(1);
        pad_index -= 1;
        obfuscation[index] = xor_ror(PAD_ARRAY[pad_index], low);
        index = index.saturating_sub(1);
        pad_index = pad_index.saturating_sub(1);
    }
    Some(obfuscation)
}

fn xor_ror(byte1: u8, byte2: u8) -> u8 {
    (byte1 ^ byte2).rotate_right(1)
}

fn xor_decrypt_method1(data: &mut [u8], initial_index: usize) {
    let Some(array) = xor_array_method1(DEFAULT_XOR_PASSWORD) else {
        return;
    };
    let mut index = initial_index % array.len();
    for byte in data {
        *byte = (*byte ^ array[index]).rotate_right(5);
        index = (index + 1) % array.len();
    }
}

impl Workbook {
    /// Like [`open`](Self::open) but forces the codepage for BIFF5/7 8-bit
    /// strings, overriding the workbook's `CODEPAGE` record. Useful when a
    /// legacy file has a missing or wrong codepage (e.g. force `949` for a
    /// Korean workbook). Ignored for BIFF8 (which uses UTF-16).
    pub fn open_with_codepage(bytes: &[u8], force_codepage: Option<u16>) -> Result<Self> {
        let stream = crate::ole::read_workbook_stream(bytes)?;
        let mut wb = stream.bytes;
        let container_parse_mode = stream.container_mode;
        let default_xor_decrypted = maybe_decrypt_default_xor(&mut wb)?;
        if wb.is_empty() {
            return Err(Error::Biff("empty BIFF stream"));
        }
        let mut sst_strings: Vec<String> = Vec::new();
        let mut sheets: Vec<Sheet> = Vec::new();
        let mut frozen_views: Vec<bool> = Vec::new();
        let mut defined_names: Vec<(String, String)> = Vec::new();
        let mut raw_defined_names = Vec::new();
        let mut raw_local_defined_names = Vec::new();
        let mut formula_names: Vec<String> = Vec::new();
        let mut formula_sheet_names: Vec<String> = Vec::new();
        let mut extern_sheets: Vec<crate::ptg::ExternSheet> = Vec::new();
        let mut external_names: Vec<Vec<String>> = Vec::new();
        let mut current_supbook = None;
        let mut sheet_builtin_names: Vec<SheetBuiltinName> = Vec::new();
        let mut sheet_page_setups: Vec<XlsPageSetup> = Vec::new();
        let mut sheet_defaults: Vec<XlsSheetDefaults> = Vec::new();
        let mut sheet_explicit_hidden_cols: Vec<BTreeSet<u16>> = Vec::new();
        let mut sheet_note_texts: Vec<HashMap<u16, String>> = Vec::new();
        let mut sheet_unkeyed_note_texts: Vec<VecDeque<String>> = Vec::new();
        let mut pending_note_obj: Option<(usize, u16)> = None;
        let mut pending_sst: Option<Vec<&[u8]>> = None;
        let mut active_sheet = None;
        let mut selected_sheet_fallback = None;
        let mut protect_structure = false;
        // BOF/EOF nesting depth, and the count of top-level (depth-0) substreams.
        let mut depth = 0usize;
        let mut top_count = 0usize;
        let mut cur_sheet: Option<usize> = None;
        let mut last_formula: Option<PendingFormula> = None;
        let mut formula_definitions = FormulaDefinitions::new();
        let mut formats = Formats::default();
        let mut xls_styles = XlsStyles::default();
        let mut palette = BIFF_DEFAULT_PALETTE;
        // Per-workbook retained-cell budget (shared across sheets). The
        // MAX_TEXT_BYTES ceiling also accounts for entry/Box storage so empty
        // display formats cannot bypass the shared-string amplification bound.
        let mut budget = MAX_TEXT_BYTES;
        // Style cloning is independently bounded because repeated XF references
        // can amplify font/format strings across many materialized cells.
        let mut style_budget = MAX_XLS_RETAINED_STYLE_BYTES;

        // Decode context: assume BIFF8 until the first global BOF says otherwise;
        // codepage defaults to cp1252 and is refined by the CODEPAGE record (or
        // the forced override).
        let mut ctx = Ctx {
            biff8: true,
            enc: force_codepage.map(resolve_encoding).unwrap_or(WINDOWS_1252),
        };

        let mut pos = 0usize;
        let mut saw_global_header = false;
        while pos < wb.len() {
            // Some producers declare the Workbook stream at the containing CFB
            // allocation size and leave an all-zero tail after the final EOF.
            // A zero-length BIFF record is valid, so parsing that tail as records
            // eventually leaves one to three bytes and incorrectly reports a
            // truncated header.  Only accept padding after a balanced top-level
            // substream; non-zero trailing bytes remain a hard error below.
            if saw_global_header
                && depth == 0
                && wb
                    .get(pos..)
                    .is_some_and(|tail| tail.iter().all(|byte| *byte == 0))
            {
                pos = wb.len();
                break;
            }
            let header_end = pos
                .checked_add(4)
                .filter(|end| *end <= wb.len())
                .ok_or(Error::Biff("truncated BIFF record header"))?;
            let typ = u16le(&wb, pos).ok_or(Error::Biff("truncated BIFF record header"))?;
            let len =
                u16le(&wb, pos + 2).ok_or(Error::Biff("truncated BIFF record header"))? as usize;
            let end = header_end
                .checked_add(len)
                .filter(|end| *end <= wb.len())
                .ok_or(Error::Biff("truncated BIFF record"))?;
            let data = &wb[header_end..end];
            pos = end;

            if !saw_global_header && typ != BOF {
                return Err(Error::Biff(
                    "malformed BIFF stream: missing leading BOF record",
                ));
            }
            if !saw_global_header {
                saw_global_header = true;
            }

            // Any non-CONTINUE record terminates an in-progress SST.
            if typ != CONTINUE {
                if let Some(chunks) = pending_sst.take() {
                    sst_strings = crate::sst::parse(&chunks);
                }
            }

            match typ {
                BOF => {
                    let version = u16le(data, 0).ok_or(Error::Biff("malformed BIFF BOF record"))?;
                    if !matches!(version, 0x0500 | 0x0600) {
                        return Err(Error::Biff("unsupported BIFF version"));
                    }
                    // Only a *top-level* (depth-0) BOF starts a new substream:
                    // the workbook globals, then one per sheet in BOUNDSHEET
                    // order. BOFs nested inside a worksheet (embedded charts,
                    // pivot tables, …) must NOT advance the sheet index — that
                    // sequential desync silently dropped every sheet after the
                    // first embedded substream. This mirrors how xlrd/POI map
                    // substreams to sheets.
                    if depth == 0 {
                        top_count += 1;
                        cur_sheet = if top_count == 1 {
                            // First top-level substream = workbook globals; pin
                            // the BIFF generation. BOF.vers: 0x0600 = BIFF8.
                            if u16le(data, 2) != Some(0x0005) {
                                return Err(Error::Biff(
                                    "malformed BIFF stream: first BOF is not workbook globals",
                                ));
                            }
                            ctx.biff8 = version == 0x0600;
                            None
                        } else {
                            Some(top_count - 2)
                        };
                    }
                    depth += 1;
                }
                CODEPAGE => {
                    if force_codepage.is_none() {
                        if let Some(cp) = u16le(data, 0) {
                            // 1200 = UTF-16LE: leave default; the grbit path handles UTF-16.
                            if cp != 1200 {
                                ctx.enc = resolve_encoding(cp);
                            }
                        }
                    }
                }
                FILEPASS => {
                    if !default_xor_decrypted {
                        return Err(Error::Encrypted);
                    }
                }
                DATEMODE => formats.set_datemode(data),
                FONT => {
                    if cur_sheet.is_none() {
                        xls_styles.push_font(data);
                    }
                }
                XF => {
                    formats.push_xf(data);
                    if cur_sheet.is_none() {
                        xls_styles.push_xf(data);
                    }
                }
                FORMAT => formats.push_format(data, || {
                    if ctx.biff8 {
                        read_xl_string(data, 2, ctx)
                    } else {
                        read_short_string(data, 2, ctx)
                    }
                }),
                PALETTE => {
                    if cur_sheet.is_none() {
                        apply_palette_record(data, &mut palette);
                    }
                }
                STYLE => {
                    if cur_sheet.is_none() {
                        xls_styles.push_style(data);
                    }
                }
                LBL => {
                    if cur_sheet.is_none() {
                        if let Some(name) = parse_lbl_formula_name(data, ctx) {
                            formula_names.push(name);
                        }
                        match parse_lbl(data, ctx) {
                            Some(ParsedLbl::GlobalUser(name)) => raw_defined_names.push(name),
                            Some(ParsedLbl::LocalUser { sheet_index, name }) => {
                                raw_local_defined_names.push((sheet_index, name));
                            }
                            Some(ParsedLbl::SheetBuiltin(name)) => sheet_builtin_names.push(name),
                            None => {}
                        }
                    }
                }
                EXTERNSHEET => {
                    if cur_sheet.is_none() {
                        extern_sheets.extend(parse_extern_sheets(data));
                    }
                }
                SUPBOOK => {
                    if cur_sheet.is_none() {
                        external_names.push(Vec::new());
                        current_supbook = Some(external_names.len() - 1);
                    }
                }
                EXTERNNAME => {
                    if cur_sheet.is_none() {
                        if let (Some(supbook), Some(name)) =
                            (current_supbook, parse_extern_name(data, ctx))
                        {
                            external_names[supbook].push(name);
                        }
                    }
                }
                BOUNDSHEET => {
                    let (name, sheet_type, hidden, very_hidden) = parse_boundsheet(data, ctx);
                    formula_sheet_names.push(name.clone());
                    sheets.push(Sheet {
                        name,
                        is_worksheet: sheet_type == SheetType::WorkSheet,
                        sheet_type: Some(sheet_type),
                        style_fidelity: StyleFidelity::Partial,
                        cells: Vec::new(),
                        hidden,
                        very_hidden,
                        ..Default::default()
                    });
                    frozen_views.push(false);
                    sheet_page_setups.push(XlsPageSetup::default());
                    sheet_defaults.push(XlsSheetDefaults::default());
                    sheet_explicit_hidden_cols.push(BTreeSet::new());
                    sheet_note_texts.push(HashMap::new());
                    sheet_unkeyed_note_texts.push(VecDeque::new());
                }
                SST => pending_sst = Some(vec![data]),
                CONTINUE => {
                    if let Some(chunks) = pending_sst.as_mut() {
                        chunks.push(data);
                    }
                }
                EOF => {
                    if depth == 0 {
                        return Err(Error::Biff("unexpected BIFF EOF record"));
                    }
                    depth -= 1;
                    if depth == 0 && cur_sheet.is_none() {
                        xls_styles.compile(ctx, &formats, &palette);
                        for sheet in &mut sheets {
                            sheet.default_format = xls_styles.default_style(&mut style_budget);
                        }
                    }
                }
                WINDOW1 if cur_sheet.is_none() && active_sheet.is_none() => {
                    active_sheet = parse_window1_active_sheet(data);
                }
                MERGECELLS => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                sheets[si].read_merges.extend(parse_mergecells(data));
                            }
                        }
                    }
                }
                HLINK => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                sheets[si].read_hyperlinks.extend(parse_hlink(data));
                            }
                        }
                    }
                }
                DV => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                sheets[si].data_validations.extend(parse_dv(data, ctx));
                            }
                        }
                    }
                }
                OBJ => {
                    if depth == 1 {
                        pending_note_obj = cur_sheet
                            .filter(|si| *si < sheets.len())
                            .zip(parse_note_obj_id(data));
                    }
                }
                TXO => {
                    let mut chunks: Vec<&[u8]> = vec![data];
                    while pos + 4 <= wb.len() {
                        if u16le(&wb, pos) != Some(CONTINUE) {
                            break;
                        }
                        let clen = u16le(&wb, pos + 2).unwrap_or(0) as usize;
                        let cstart = pos + 4;
                        let cend = cstart.saturating_add(clen);
                        if cend > wb.len() {
                            break;
                        }
                        chunks.push(&wb[cstart..cend]);
                        pos = cend;
                    }
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                let keyed_note_id = match pending_note_obj.take() {
                                    Some((obj_si, id)) if obj_si == si => Some(id),
                                    stale => {
                                        pending_note_obj = stale;
                                        None
                                    }
                                };
                                if let Some(text) = parse_txo_text(&chunks, &mut budget)
                                    .filter(|text| !text.is_empty())
                                {
                                    match keyed_note_id {
                                        Some(id) => {
                                            sheet_note_texts[si].insert(id, text);
                                        }
                                        None => {
                                            sheet_unkeyed_note_texts[si].push_back(text);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                NOTE => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                if let Some(note) = parse_note_sh(data, ctx) {
                                    let text = sheet_note_texts[si]
                                        .remove(&note.id_obj)
                                        .or_else(|| sheet_unkeyed_note_texts[si].pop_front());
                                    if let Some(text) = text.filter(|text| !text.is_empty()) {
                                        sheets[si].comments.push(Comment {
                                            row: note.row,
                                            col: note.col,
                                            text,
                                            author: note.author,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
                WINDOW2 => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                if let Some(view) = parse_window2(data) {
                                    frozen_views[si] = view.frozen;
                                    sheets[si].freeze = None;
                                    sheets[si].hide_gridlines = view.hide_gridlines;
                                    sheets[si].show_headers = view.show_headers;
                                    sheets[si].right_to_left = view.right_to_left;
                                    sheets[si].zoom = view.zoom;
                                    if view.selected && selected_sheet_fallback.is_none() {
                                        selected_sheet_fallback = Some(si);
                                    }
                                }
                            }
                        }
                    }
                }
                SHEETEXT => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                sheets[si].tab_color = parse_sheet_ext_tab_color(data, &palette);
                            }
                        }
                    }
                }
                ROW => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                apply_row_outline(
                                    data,
                                    &mut sheets[si],
                                    &mut sheet_defaults[si].explicit_visible_rows,
                                    &xls_styles,
                                    &mut style_budget,
                                    ctx.biff8,
                                );
                            }
                        }
                    }
                }
                COLINFO => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                apply_col_outline(
                                    data,
                                    &mut sheets[si],
                                    &mut sheet_explicit_hidden_cols[si],
                                    &xls_styles,
                                    &mut style_budget,
                                );
                            }
                        }
                    }
                }
                DEFAULTCOLWIDTH | STANDARDWIDTH | DEFAULTROWHEIGHT => {
                    if depth == 1 {
                        if let Some(defaults) = cur_sheet.and_then(|si| sheet_defaults.get_mut(si))
                        {
                            defaults.apply_record(typ, data);
                        }
                    }
                }
                WSBOOL => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                apply_wsbool_outline(data, &mut sheets[si]);
                            }
                            if si < sheet_page_setups.len() {
                                sheet_page_setups[si].set_wsbool(data);
                            }
                        }
                    }
                }
                PROTECT => {
                    if depth == 1 && cur_sheet.is_none() {
                        protect_structure = u16le(data, 0).unwrap_or(0) != 0;
                    } else if depth == 1 {
                        if let Some(si) = cur_sheet.filter(|si| *si < sheets.len()) {
                            sheets[si].protect = u16le(data, 0).unwrap_or(0) != 0;
                            sheets[si].protect_options = None;
                        }
                    }
                }
                PANE => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() && frozen_views.get(si).copied().unwrap_or(false) {
                                sheets[si].freeze = parse_pane_freeze(data);
                            }
                        }
                    }
                }
                HEADER | FOOTER | VERTICALPAGEBREAKS | HORIZONTALPAGEBREAKS | LEFTMARGIN
                | RIGHTMARGIN | TOPMARGIN | BOTTOMMARGIN | PRINTHEADERS | PRINTGRIDLINES
                | HCENTER | VCENTER | SETUP | HEADERFOOTER => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheet_page_setups.len() {
                                sheet_page_setups[si].apply_record(typ, data, ctx);
                            }
                        }
                    }
                }
                ARRAY | SHRFMLA => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet.filter(|si| *si < sheets.len()) {
                            if let Some(definition) = parse_formula_definition(typ, data) {
                                let key = (si, definition.anchor.0, definition.anchor.1);
                                formula_definitions.insert(key, definition.clone());
                                apply_formula_definition(
                                    si,
                                    &definition,
                                    &mut sheets[si].cells,
                                    &mut last_formula,
                                    &mut budget,
                                    ctx,
                                    &formula_sheet_names,
                                    &extern_sheets,
                                    &external_names,
                                    &formula_names,
                                );
                            }
                        }
                    }
                }
                BLANK | MULBLANK => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet.filter(|si| *si < sheets.len()) {
                            retain_blank_cell_styles(
                                typ,
                                data,
                                &mut sheets[si].blank_styles,
                                &xls_styles,
                                &mut style_budget,
                            );
                        }
                    }
                }
                LABEL | RSTRING | STRING => {
                    // The text payload of these cell records can overflow into
                    // CONTINUE records (exactly like the SST). Gather the record
                    // body plus any following CONTINUE bodies into one logical
                    // byte stream before decoding — otherwise a long label or
                    // formula-string is silently truncated at the record cap.
                    let mut chunks: Vec<&[u8]> = vec![data];
                    while pos + 4 <= wb.len() {
                        if u16le(&wb, pos) != Some(CONTINUE) {
                            break;
                        }
                        let clen = u16le(&wb, pos + 2).unwrap_or(0) as usize;
                        let cstart = pos + 4;
                        let cend = cstart.saturating_add(clen);
                        if cend > wb.len() {
                            break;
                        }
                        chunks.push(&wb[cstart..cend]);
                        pos = cend;
                    }
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                let sheet = &mut sheets[si];
                                decode_string_cell(
                                    typ,
                                    &chunks,
                                    si,
                                    &mut sheet.cells,
                                    &mut sheet.rich,
                                    &mut last_formula,
                                    ctx,
                                    &mut budget,
                                    &formats,
                                    &xls_styles,
                                    &mut style_budget,
                                );
                            }
                        }
                    }
                }
                _ => {
                    // Cell records live at the top level of a worksheet
                    // substream (depth 1). Records nested inside an embedded
                    // chart / pivot substream (depth > 1) are skipped so their
                    // payload is never misread as cells (which would inflate the
                    // containing sheet with chart junk).
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                decode_cell(
                                    typ,
                                    data,
                                    &sst_strings,
                                    si,
                                    &mut sheets[si].cells,
                                    &mut last_formula,
                                    &formats,
                                    &mut budget,
                                    &xls_styles,
                                    &mut style_budget,
                                    &formula_sheet_names,
                                    &extern_sheets,
                                    &external_names,
                                    &formula_names,
                                    ctx,
                                    &formula_definitions,
                                );
                            }
                        }
                    }
                }
            }
        }
        if !saw_global_header {
            return Err(Error::Biff("missing BIFF stream header"));
        }
        if depth != 0 {
            return Err(Error::Biff("unterminated BIFF stream"));
        }
        if pos != wb.len() {
            return Err(Error::Biff("truncated BIFF record header"));
        }
        for (sheet, defaults) in sheets.iter_mut().zip(sheet_defaults) {
            defaults.apply_to(sheet);
        }
        apply_sheet_page_setups(&mut sheets, sheet_page_setups);
        apply_sheet_builtin_names(&mut sheets, sheet_builtin_names);
        defined_names.extend(raw_defined_names.into_iter().map(|name| {
            let context = formula_context(
                ctx,
                0,
                0,
                &formula_sheet_names,
                &extern_sheets,
                &external_names,
                &formula_names,
            );
            let context = crate::ptg::Context {
                name_formula: true,
                ..context
            };
            let refers_to =
                crate::ptg::decompile_parsed_with_context(&name.rgce, &name.rgb_extra, &context);
            (name.name, refers_to)
        }));
        let local_defined_names = raw_local_defined_names
            .into_iter()
            .filter_map(|(sheet_index, name)| {
                let sheet = formula_sheet_names.get(sheet_index)?.clone();
                let context = formula_context(
                    ctx,
                    0,
                    0,
                    &formula_sheet_names,
                    &extern_sheets,
                    &external_names,
                    &formula_names,
                );
                let context = crate::ptg::Context {
                    name_formula: true,
                    ..context
                };
                let refers_to = crate::ptg::decompile_parsed_with_context(
                    &name.rgce,
                    &name.rgb_extra,
                    &context,
                );
                Some(crate::LocalDefinedName {
                    sheet,
                    name: name.name,
                    refers_to,
                })
            })
            .collect();
        Ok(Workbook {
            sheets,
            properties: parse_ole_doc_properties(bytes),
            defined_names,
            local_defined_names,
            date1904: formats.date1904(),
            active_sheet: active_sheet.or(selected_sheet_fallback).unwrap_or_default(),
            protect_structure,
            text_truncated: budget == 0,
            container_parse_mode,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests;
