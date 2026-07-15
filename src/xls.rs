//! The legacy `.xls` (OLE2 / BIFF) reader.
//!
//! `.xls` is an OLE2/CFB compound file whose `Workbook`/`Book` stream is a
//! sequence of BIFF records. The OLE2 container is decoded in [`crate::ole`];
//! this module walks the BIFF record stream on top of it — the SST, the cell
//! records (LABELSST/LABEL/RK/MULRK/NUMBER/BOOLERR/FORMULA/STRING), merges,
//! hyperlinks, comments, outline/protection records, data-validation/page setup
//! metadata, and the codepage/date/format globals — into the [`crate::Workbook`]
//! model.

use crate::format::Formats;
use crate::model::{
    Cell, CellEntry, Color, Comment, DataValidation, DocProperties, DvKind, DvOp, PageSetup, Sheet,
    SheetType,
};
use crate::{error_code, rk_to_f64, Error, Result, Workbook, MAX_TEXT_BYTES};

use encoding_rs::{
    Encoding, BIG5, EUC_KR, GBK, SHIFT_JIS, UTF_8, WINDOWS_1251, WINDOWS_1252, WINDOWS_1253,
    WINDOWS_1254, WINDOWS_1255, WINDOWS_1256, WINDOWS_1258, WINDOWS_874,
};
use std::collections::{BTreeMap, HashMap, VecDeque};

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
const XF: u16 = 0x00E0;
const FORMAT: u16 = 0x041E;
const PALETTE: u16 = 0x0092;
const HEADER: u16 = 0x0014;
const FOOTER: u16 = 0x0015;
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
const SHEETEXT: u16 = 0x0862;
const LABELSST: u16 = 0x00FD;
const LABEL: u16 = 0x0204;
const RSTRING: u16 = 0x00D6;
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
const BIFF_DEFAULT_PALETTE: [Color; 56] = [
    Color::rgb(0x00, 0x00, 0x00),
    Color::rgb(0xFF, 0xFF, 0xFF),
    Color::rgb(0xFF, 0x00, 0x00),
    Color::rgb(0x00, 0xFF, 0x00),
    Color::rgb(0x00, 0x00, 0xFF),
    Color::rgb(0xFF, 0xFF, 0x00),
    Color::rgb(0xFF, 0x00, 0xFF),
    Color::rgb(0x00, 0xFF, 0xFF),
    Color::rgb(0x80, 0x00, 0x00),
    Color::rgb(0x00, 0x80, 0x00),
    Color::rgb(0x00, 0x00, 0x80),
    Color::rgb(0x80, 0x80, 0x00),
    Color::rgb(0x80, 0x00, 0x80),
    Color::rgb(0x00, 0x80, 0x80),
    Color::rgb(0xC0, 0xC0, 0xC0),
    Color::rgb(0x80, 0x80, 0x80),
    Color::rgb(0x99, 0x99, 0xFF),
    Color::rgb(0x99, 0x33, 0x66),
    Color::rgb(0xFF, 0xFF, 0xCC),
    Color::rgb(0xCC, 0xFF, 0xFF),
    Color::rgb(0x66, 0x00, 0x66),
    Color::rgb(0xFF, 0x80, 0x80),
    Color::rgb(0x00, 0x66, 0xCC),
    Color::rgb(0xCC, 0xCC, 0xFF),
    Color::rgb(0x00, 0x00, 0x80),
    Color::rgb(0xFF, 0x00, 0xFF),
    Color::rgb(0xFF, 0xFF, 0x00),
    Color::rgb(0x00, 0xFF, 0xFF),
    Color::rgb(0x80, 0x00, 0x80),
    Color::rgb(0x80, 0x00, 0x00),
    Color::rgb(0x00, 0x80, 0x80),
    Color::rgb(0x00, 0x00, 0xFF),
    Color::rgb(0x00, 0xCC, 0xFF),
    Color::rgb(0xCC, 0xFF, 0xFF),
    Color::rgb(0xCC, 0xFF, 0xCC),
    Color::rgb(0xFF, 0xFF, 0x99),
    Color::rgb(0x99, 0xCC, 0xFF),
    Color::rgb(0xFF, 0x99, 0xCC),
    Color::rgb(0xCC, 0x99, 0xFF),
    Color::rgb(0xFF, 0xCC, 0x99),
    Color::rgb(0x33, 0x66, 0xFF),
    Color::rgb(0x33, 0xCC, 0xCC),
    Color::rgb(0x99, 0xCC, 0x00),
    Color::rgb(0xFF, 0xCC, 0x00),
    Color::rgb(0xFF, 0x99, 0x00),
    Color::rgb(0xFF, 0x66, 0x00),
    Color::rgb(0x66, 0x66, 0x99),
    Color::rgb(0x96, 0x96, 0x96),
    Color::rgb(0x00, 0x33, 0x66),
    Color::rgb(0x33, 0x99, 0x66),
    Color::rgb(0x00, 0x33, 0x00),
    Color::rgb(0x33, 0x33, 0x00),
    Color::rgb(0x99, 0x33, 0x00),
    Color::rgb(0x99, 0x33, 0x66),
    Color::rgb(0x33, 0x33, 0x99),
    Color::rgb(0x33, 0x33, 0x33),
];

type SheetRange = (u32, u16, u32, u16);
type SheetRanges = Vec<SheetRange>;

#[derive(Clone, Debug)]
struct FormulaDefinition {
    anchor: (u32, u16),
    range: SheetRange,
    rgce: Vec<u8>,
    rgb_extra: Vec<u8>,
    is_array: bool,
}

type FormulaDefinitions = HashMap<(usize, u32, u16), FormulaDefinition>;

/// Decode context: the BIFF generation and the codepage for 8-bit strings.
#[derive(Clone, Copy)]
struct Ctx {
    /// `true` for BIFF8 (UTF-16 strings with a grbit byte); `false` for
    /// BIFF5/7 (raw codepage bytes, no grbit, no SST).
    biff8: bool,
    /// Codec for BIFF5/7 8-bit strings (cp1252 default, cp949 for Korean, …).
    enc: &'static Encoding,
}

/// Resolve a BIFF `CODEPAGE` value to its `encoding_rs` codec.
///
/// Compatibility policy for BIFF5/7 is intentionally deterministic:
///
/// - missing and unknown declarations fall back to Windows-1252, Excel's
///   historical Western default;
/// - codepages 949 (Windows/UHC) and 51949 (EUC-KR) use `encoding_rs`'s
///   Windows-949-compatible `EUC_KR` decoder;
/// - malformed byte sequences are decoded lossily as U+FFFD by `encoding_rs`;
/// - callers with a missing or incorrect declaration can use
///   [`Workbook::open_with_codepage`] to force the intended codepage.
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
        let mut wb = crate::ole::read_workbook_stream(bytes)?;
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
        let mut last_formula: Option<(usize, u32, u16, Option<String>)> = None;
        let mut formula_definitions = FormulaDefinitions::new();
        let mut formats = Formats::default();
        let mut palette = BIFF_DEFAULT_PALETTE;
        // Per-workbook text budget (shared across sheets) — see MAX_TEXT_BYTES.
        let mut budget = MAX_TEXT_BYTES;

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
                XF => formats.push_xf(data),
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
                        cells: Vec::new(),
                        hidden,
                        very_hidden,
                        ..Default::default()
                    });
                    frozen_views.push(false);
                    sheet_page_setups.push(XlsPageSetup::default());
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
                                apply_row_outline(data, &mut sheets[si]);
                            }
                        }
                    }
                }
                COLINFO => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                apply_col_outline(data, &mut sheets[si]);
                            }
                        }
                    }
                }
                WSBOOL => {
                    if depth == 1 {
                        if let Some(si) = cur_sheet {
                            if si < sheets.len() {
                                apply_wsbool_outline(data, &mut sheets[si]);
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
                HEADER | FOOTER | LEFTMARGIN | RIGHTMARGIN | TOPMARGIN | BOTTOMMARGIN
                | PRINTHEADERS | PRINTGRIDLINES | HCENTER | VCENTER | SETUP => {
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
            ..Default::default()
        })
    }
}

fn parse_window1_active_sheet(data: &[u8]) -> Option<usize> {
    u16le(data, 10).map(usize::from)
}

fn parse_ole_doc_properties(bytes: &[u8]) -> DocProperties {
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

#[derive(Default)]
struct XlsPageSetup {
    setup: PageSetup,
    touched: bool,
    left_margin: Option<f64>,
    right_margin: Option<f64>,
    top_margin: Option<f64>,
    bottom_margin: Option<f64>,
    header_margin: Option<f64>,
    footer_margin: Option<f64>,
    print_headings: bool,
    print_gridlines: bool,
}

impl XlsPageSetup {
    fn apply_record(&mut self, typ: u16, data: &[u8], ctx: Ctx) {
        match typ {
            HEADER => self.set_header(data, ctx),
            FOOTER => self.set_footer(data, ctx),
            LEFTMARGIN => self.left_margin = read_margin(data),
            RIGHTMARGIN => self.right_margin = read_margin(data),
            TOPMARGIN => self.top_margin = read_margin(data),
            BOTTOMMARGIN => self.bottom_margin = read_margin(data),
            PRINTHEADERS => self.print_headings = u16le(data, 0).unwrap_or(0) != 0,
            PRINTGRIDLINES => self.print_gridlines = u16le(data, 0).unwrap_or(0) != 0,
            HCENTER => {
                self.setup.center_horizontally = u16le(data, 0).unwrap_or(0) != 0;
                self.touched = true;
            }
            VCENTER => {
                self.setup.center_vertically = u16le(data, 0).unwrap_or(0) != 0;
                self.touched = true;
            }
            SETUP => self.set_setup(data),
            _ => {}
        }
        if matches!(typ, LEFTMARGIN | RIGHTMARGIN | TOPMARGIN | BOTTOMMARGIN)
            && read_margin(data).is_some()
        {
            self.touched = true;
        }
    }

    fn set_header(&mut self, data: &[u8], ctx: Ctx) {
        if let Some(text) = read_xl_string(data, 0, ctx).filter(|text| !text.is_empty()) {
            self.setup.header = Some(text);
            self.touched = true;
        }
    }

    fn set_footer(&mut self, data: &[u8], ctx: Ctx) {
        if let Some(text) = read_xl_string(data, 0, ctx).filter(|text| !text.is_empty()) {
            self.setup.footer = Some(text);
            self.touched = true;
        }
    }

    fn set_setup(&mut self, data: &[u8]) {
        let Some(flags) = u16le(data, 10) else {
            return;
        };
        let no_printer_settings = flags & 0x0004 != 0;
        let no_orientation = flags & 0x0040 != 0;
        if !no_printer_settings {
            self.setup.paper_size = nonzero_u16le(data, 0);
            self.setup.scale = nonzero_u16le(data, 2);
            if !no_orientation {
                self.setup.landscape = flags & 0x0002 == 0;
            }
        }
        self.setup.fit_to_width = nonzero_u16le(data, 6);
        self.setup.fit_to_height = nonzero_u16le(data, 8);
        if flags & 0x0080 != 0 {
            if let Some(page_start) = i16le(data, 4).filter(|page| *page > 0) {
                self.setup.first_page_number = Some(page_start as u16);
            }
        }
        self.header_margin = read_margin_at(data, 16);
        self.footer_margin = read_margin_at(data, 24);
        self.touched = true;
    }

    fn into_page_setup(mut self) -> Option<PageSetup> {
        if let (Some(left), Some(right), Some(top), Some(bottom), Some(header), Some(footer)) = (
            self.left_margin,
            self.right_margin,
            self.top_margin,
            self.bottom_margin,
            self.header_margin,
            self.footer_margin,
        ) {
            self.setup.margins = Some((left, right, top, bottom, header, footer));
        }
        self.touched.then_some(self.setup)
    }
}

fn read_margin(data: &[u8]) -> Option<f64> {
    read_margin_at(data, 0)
}

fn nonzero_u16le(data: &[u8], offset: usize) -> Option<u16> {
    u16le(data, offset).filter(|value| *value != 0)
}

fn read_margin_at(data: &[u8], offset: usize) -> Option<f64> {
    f64le(data, offset).filter(|value| value.is_finite() && *value >= 0.0 && *value < 49.0)
}

fn apply_sheet_page_setups(sheets: &mut [Sheet], setups: Vec<XlsPageSetup>) {
    for (sheet, setup) in sheets.iter_mut().zip(setups) {
        sheet.print_headings = setup.print_headings;
        sheet.print_gridlines = setup.print_gridlines;
        if let Some(page_setup) = setup.into_page_setup() {
            sheet.page_setup = Some(page_setup);
        }
    }
}

fn parse_sheet_ext_tab_color(data: &[u8], palette: &[Color; 56]) -> Option<Color> {
    if u16le(data, 0)? != SHEETEXT {
        return None;
    }
    let icv_plain = (u32le(data, 16)? & 0x7F) as u8;
    if icv_plain == 0x7F {
        return None;
    }
    biff_palette_color(icv_plain, palette)
}

fn apply_palette_record(data: &[u8], palette: &mut [Color; 56]) {
    let Some(count) = u16le(data, 0).map(|count| count as usize) else {
        return;
    };
    for idx in 0..count.min(palette.len()) {
        let offset = 2 + idx * 4;
        let Some(rgb) = data.get(offset..offset + 4) else {
            return;
        };
        palette[idx] = Color::rgb(rgb[0], rgb[1], rgb[2]);
    }
}

fn biff_palette_color(icv: u8, palette: &[Color; 56]) -> Option<Color> {
    let idx = icv.checked_sub(0x08)? as usize;
    palette.get(idx).copied()
}

struct XlsNoteSh {
    row: u32,
    col: u16,
    id_obj: u16,
    author: Option<String>,
}

fn parse_note_obj_id(data: &[u8]) -> Option<u16> {
    if u16le(data, 0)? != 0x0015 || u16le(data, 2)? != 0x0012 {
        return None;
    }
    let object_type = u16le(data, 4)?;
    (object_type == 0x0019).then(|| u16le(data, 6)).flatten()
}

fn parse_txo_text(chunks: &[&[u8]], budget: &mut usize) -> Option<String> {
    if *budget == 0 {
        return None;
    }
    let first = *chunks.first()?;
    let cch = u16le(first, 10)? as usize;
    if cch == 0 {
        return None;
    }
    if cch > MAX_TEXT_BYTES {
        *budget = 0;
        return None;
    }

    let mut text = String::with_capacity(cch);
    let mut remaining = cch;
    for chunk in chunks.iter().skip(1) {
        if remaining == 0 {
            break;
        }
        let grbit = *chunk.first()?;
        if grbit & 0x01 != 0 {
            let available = chunk.len().saturating_sub(1) / 2;
            let take = remaining.min(available);
            let units = chunk[1..1 + take * 2]
                .chunks_exact(2)
                .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
                .collect::<Vec<_>>();
            text.push_str(&String::from_utf16_lossy(&units));
            remaining -= take;
        } else {
            let take = remaining.min(chunk.len().saturating_sub(1));
            for &byte in &chunk[1..1 + take] {
                text.push(byte as char);
            }
            remaining -= take;
        }
    }
    if remaining != 0 {
        return None;
    }
    if text.len() > *budget {
        *budget = 0;
        return None;
    }
    *budget -= text.len();
    Some(text)
}

fn parse_note_sh(data: &[u8], ctx: Ctx) -> Option<XlsNoteSh> {
    let row = u32::from(u16le(data, 0)?);
    let col = u16le(data, 2)?;
    let id_obj = u16le(data, 6)?;
    let (author, _used) = read_xl_unicode_string(data, 8, ctx)?;
    Some(XlsNoteSh {
        row,
        col,
        id_obj,
        author: (!author.is_empty()).then_some(author),
    })
}

fn parse_dv(data: &[u8], ctx: Ctx) -> Vec<DataValidation> {
    let Some(flags) = u32le(data, 0) else {
        return Vec::new();
    };
    let Some(kind) = parse_dv_kind(flags & 0x0F) else {
        return Vec::new();
    };
    let operator = parse_dv_op((flags >> 20) & 0x0F).unwrap_or(DvOp::Between);
    let mut offset = 4usize;
    let Some((prompt_title, used)) = read_xl_unicode_string(data, offset, ctx) else {
        return Vec::new();
    };
    offset += used;
    let Some((error_title, used)) = read_xl_unicode_string(data, offset, ctx) else {
        return Vec::new();
    };
    offset += used;
    let Some((prompt_message, used)) = read_xl_unicode_string(data, offset, ctx) else {
        return Vec::new();
    };
    offset += used;
    let Some((error_message, used)) = read_xl_unicode_string(data, offset, ctx) else {
        return Vec::new();
    };
    offset += used;

    let Some((formula1, used)) = parse_dv_formula(data, offset) else {
        return Vec::new();
    };
    offset += used;
    let Some((formula2, used)) = parse_dv_formula(data, offset) else {
        return Vec::new();
    };
    offset += used;
    let Some((ranges, _used)) = parse_dv_sqref(data, offset) else {
        return Vec::new();
    };

    let prompt = if prompt_title.is_empty() && prompt_message.is_empty() {
        None
    } else {
        Some((prompt_title, prompt_message))
    };
    let error = if error_title.is_empty() && error_message.is_empty() {
        None
    } else {
        Some((error_title, error_message))
    };
    let base = DataValidation {
        sqref: (0, 0, 0, 0),
        kind,
        operator,
        formula1,
        formula2: (!formula2.is_empty()).then_some(formula2),
        allow_blank: flags & (1 << 8) != 0,
        show_input_message: flags & (1 << 18) != 0,
        show_error_message: flags & (1 << 19) != 0,
        prompt,
        error,
    };

    ranges
        .into_iter()
        .map(|sqref| DataValidation {
            sqref,
            ..base.clone()
        })
        .collect()
}

fn parse_dv_kind(value: u32) -> Option<DvKind> {
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

fn parse_dv_op(value: u32) -> Option<DvOp> {
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

fn read_xl_unicode_string(data: &[u8], off: usize, ctx: Ctx) -> Option<(String, usize)> {
    let cch = u16le(data, off)? as usize;
    if ctx.biff8 {
        let grbit = *data.get(off + 2)?;
        let char_bytes = if grbit & 0x01 != 0 {
            cch.checked_mul(2)?
        } else {
            cch
        };
        let text = decode_chars(data, off + 3, cch, grbit)?;
        Some((text, 3 + char_bytes))
    } else {
        let text = read_xl_string(data, off, ctx)?;
        Some((text, 2 + cch))
    }
}

fn parse_dv_formula(data: &[u8], offset: usize) -> Option<(String, usize)> {
    let cce = u16le(data, offset)? as usize;
    let start = offset.checked_add(4)?;
    let end = start.checked_add(cce)?;
    let rgce = data.get(start..end)?;
    Some((crate::ptg::decompile(rgce, false), end - offset))
}

fn parse_dv_sqref(data: &[u8], offset: usize) -> Option<(SheetRanges, usize)> {
    let cref = u16le(data, offset)? as usize;
    if cref == 0 {
        return None;
    }
    let start = offset.checked_add(2)?;
    let bytes = cref.checked_mul(8)?;
    let end = start.checked_add(bytes)?;
    data.get(start..end)?;

    let retained = cref.min(MAX_DV_RANGES);
    let mut ranges = Vec::with_capacity(retained);
    for i in 0..retained {
        let pos = start + i * 8;
        let Some(range) = parse_ref8u(data.get(pos..pos + 8)?) else {
            continue;
        };
        ranges.push(range);
    }
    Some((ranges, end - offset))
}

fn parse_ref8u(data: &[u8]) -> Option<SheetRange> {
    let r0 = u32::from(u16le(data, 0)?);
    let r1 = u32::from(u16le(data, 2)?);
    let c0 = u16le(data, 4)?;
    let c1 = u16le(data, 6)?;
    Some((r0.min(r1), c0.min(c1), r0.max(r1), c0.max(c1)))
}

enum ParsedLbl {
    GlobalUser(RawDefinedName),
    LocalUser {
        sheet_index: usize,
        name: RawDefinedName,
    },
    SheetBuiltin(SheetBuiltinName),
}

struct RawDefinedName {
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
    ranges: Vec<(u32, u16, u32, u16)>,
}

fn parse_extern_sheets(data: &[u8]) -> Vec<crate::ptg::ExternSheet> {
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
fn parse_extern_name(data: &[u8], ctx: Ctx) -> Option<String> {
    let name = read_short_string(data, 6, ctx)?;
    (!name.is_empty()).then_some(name)
}

fn parse_lbl_formula_name(data: &[u8], ctx: Ctx) -> Option<String> {
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
fn parse_lbl(data: &[u8], ctx: Ctx) -> Option<ParsedLbl> {
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

fn apply_sheet_builtin_names(sheets: &mut [Sheet], names: Vec<SheetBuiltinName>) {
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
fn parse_boundsheet(data: &[u8], ctx: Ctx) -> (String, SheetType, bool, bool) {
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
fn read_short_string(data: &[u8], off: usize, ctx: Ctx) -> Option<String> {
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
fn read_xl_string(data: &[u8], off: usize, ctx: Ctx) -> Option<String> {
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
fn decode_chars(data: &[u8], off: usize, cch: usize, grbit: u8) -> Option<String> {
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

fn parse_formula_definition(typ: u16, data: &[u8]) -> Option<FormulaDefinition> {
    let row_first = u32::from(u16le(data, 0)?);
    let row_last = u32::from(u16le(data, 2)?);
    let col_first = u16::from(*data.get(4)?);
    let col_last = u16::from(*data.get(5)?);
    if row_first > row_last || col_first > col_last {
        return None;
    }
    let formula_start = match typ {
        SHRFMLA => 8,
        ARRAY => 12,
        _ => return None,
    };
    let cce = usize::from(u16le(data, formula_start)?);
    let rgce_start = formula_start.checked_add(2)?;
    let rgce_end = rgce_start.checked_add(cce)?;
    let rgce = data.get(rgce_start..rgce_end)?.to_vec();
    Some(FormulaDefinition {
        anchor: (row_first, col_first),
        range: (row_first, col_first, row_last, col_last),
        rgce,
        rgb_extra: data.get(rgce_end..).unwrap_or_default().to_vec(),
        is_array: typ == ARRAY,
    })
}

fn formula_context<'a>(
    ctx: Ctx,
    row: u32,
    col: u16,
    sheet_names: &'a [String],
    extern_sheets: &'a [crate::ptg::ExternSheet],
    external_names: &'a [Vec<String>],
    defined_names: &'a [String],
) -> crate::ptg::Context<'a> {
    crate::ptg::Context {
        biff12: false,
        biff5: !ctx.biff8,
        name_formula: false,
        base_row: row,
        base_col: col,
        sheet_names,
        extern_sheets,
        external_names,
        defined_names,
    }
}

#[allow(clippy::too_many_arguments)]
fn decompile_formula_source(
    rgce: &[u8],
    rgb_extra: &[u8],
    sheet_idx: usize,
    row: u32,
    col: u16,
    ctx: Ctx,
    definitions: &FormulaDefinitions,
    sheet_names: &[String],
    extern_sheets: &[crate::ptg::ExternSheet],
    external_names: &[Vec<String>],
    defined_names: &[String],
) -> Option<String> {
    let (tokens, extra, base_row, base_col) =
        if let Some((anchor_row, anchor_col)) = crate::ptg::exp_anchor(rgce, rgb_extra, false) {
            let definition = definitions.get(&(sheet_idx, anchor_row, anchor_col))?;
            let (row_first, col_first, row_last, col_last) = definition.range;
            if row < row_first || row > row_last || col < col_first || col > col_last {
                return None;
            }
            let (base_row, base_col) = if definition.is_array {
                definition.anchor
            } else {
                (row, col)
            };
            (
                definition.rgce.as_slice(),
                definition.rgb_extra.as_slice(),
                base_row,
                base_col,
            )
        } else {
            (rgce, rgb_extra, row, col)
        };
    let context = formula_context(
        ctx,
        base_row,
        base_col,
        sheet_names,
        extern_sheets,
        external_names,
        defined_names,
    );
    let formula = crate::ptg::decompile_parsed_with_context(tokens, extra, &context);
    (!formula.is_empty()).then_some(formula)
}

#[allow(clippy::too_many_arguments)]
fn apply_formula_definition(
    sheet_idx: usize,
    definition: &FormulaDefinition,
    cells: &mut [CellEntry],
    last_formula: &mut Option<(usize, u32, u16, Option<String>)>,
    ctx: Ctx,
    sheet_names: &[String],
    extern_sheets: &[crate::ptg::ExternSheet],
    external_names: &[Vec<String>],
    defined_names: &[String],
) {
    let context = formula_context(
        ctx,
        definition.anchor.0,
        definition.anchor.1,
        sheet_names,
        extern_sheets,
        external_names,
        defined_names,
    );
    let formula = crate::ptg::decompile_parsed_with_context(
        &definition.rgce,
        &definition.rgb_extra,
        &context,
    );
    if formula.is_empty() {
        return;
    }
    if let Some((si, row, col, source)) = last_formula.as_mut() {
        if (*si, *row, *col) == (sheet_idx, definition.anchor.0, definition.anchor.1) {
            *source = Some(formula.clone());
        }
    }
    if let Some(cell) = cells
        .iter_mut()
        .rev()
        .find(|cell| cell.row == definition.anchor.0 && cell.col == definition.anchor.1)
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
    typ: u16,
    data: &[u8],
    sst: &[String],
    sheet_idx: usize,
    cells: &mut Vec<CellEntry>,
    last_formula: &mut Option<(usize, u32, u16, Option<String>)>,
    formats: &Formats,
    budget: &mut usize,
    sheet_names: &[String],
    extern_sheets: &[crate::ptg::ExternSheet],
    external_names: &[Vec<String>],
    defined_names: &[String],
    ctx: Ctx,
    formula_definitions: &FormulaDefinitions,
) {
    if *budget == 0 {
        return; // text budget exhausted — stop accumulating
    }
    let (Some(row), Some(col)) = (u16le(data, 0), u16le(data, 2)) else {
        return;
    };
    let (row, col) = (u32::from(row), col);
    // Cell `ixfe` (the format index) sits right after row/col for value records.
    let ixfe = u16le(data, 4).unwrap_or(0);
    match typ {
        LABELSST => {
            if let Some(isst) = u32le(data, 6) {
                if let Some(s) = sst.get(isst as usize) {
                    push_text(cells, row, col, s.clone(), budget);
                }
            }
        }
        // LABEL / RSTRING / STRING text payloads may span CONTINUE records, so
        // they are gathered and decoded in the main record loop via
        // `decode_string_cell`, not here.
        NUMBER => {
            if let Some(b) = data.get(6..14) {
                let f = f64::from_le_bytes(b.try_into().unwrap_or([0; 8]));
                push_number(cells, row, col, f, ixfe, formats, budget);
            }
        }
        RK => {
            if let Some(rk) = u32le(data, 6) {
                push_number(cells, row, col, rk_to_f64(rk), ixfe, formats, budget);
            }
        }
        MULRK => {
            // row, colFirst, [ixfe(2)+rk(4)]*, colLast(2)
            let col_last = u16le(data, data.len().wrapping_sub(2)).unwrap_or(col);
            let count = (col_last as i32 - col as i32 + 1).max(0) as usize;
            // Clamp to what the record can actually hold (libxls-style guard).
            let count = count.min(data.len().saturating_sub(6) / 6);
            for k in 0..count {
                let base = 4 + k * 6;
                let cell_ixfe = u16le(data, base).unwrap_or(0);
                if let Some(rk) = u32le(data, base + 2) {
                    push_number(
                        cells,
                        row,
                        col + k as u16,
                        rk_to_f64(rk),
                        cell_ixfe,
                        formats,
                        budget,
                    );
                }
            }
        }
        BOOLERR => {
            // row, col, ixfe, bBoolErr(u8), fError(u8).
            if let (Some(&v), Some(&is_err)) = (data.get(6), data.get(7)) {
                if is_err == 0 {
                    let b = v != 0;
                    let text = if b { "TRUE" } else { "FALSE" }.to_string();
                    push_cell(cells, row, col, Cell::Bool(b), text, budget);
                } else {
                    let code = error_code(v).to_string();
                    push_cell(cells, row, col, Cell::Error(code.clone()), code, budget);
                }
            }
        }
        FORMULA | FORMULA_ALT => {
            // Cached result at [6..14]; string results signalled by 0xFFFF tail
            // with a leading 0x00, with the value in the following STRING record.
            // The `rgce` token blob (after result(8) + grbit(2) + chn(4) + cce(2))
            // is decompiled to the formula source; when recovered, the cell is a
            // `Cell::Formula { formula, cached }`, else just the cached value.
            let formula = u16le(data, 20).and_then(|cce| {
                let end = 22usize.saturating_add(cce as usize).min(data.len());
                decompile_formula_source(
                    data.get(22..end).unwrap_or_default(),
                    data.get(end..).unwrap_or_default(),
                    sheet_idx,
                    row,
                    col,
                    ctx,
                    formula_definitions,
                    sheet_names,
                    extern_sheets,
                    external_names,
                    defined_names,
                )
            });
            if let Some(res) = data.get(6..14) {
                if res[6] == 0xFF && res[7] == 0xFF {
                    match res[0] {
                        0x00 => *last_formula = Some((sheet_idx, row, col, formula)),
                        0x01 => {
                            let b = res[2] != 0;
                            let text = if b { "TRUE" } else { "FALSE" }.to_string();
                            push_cell(
                                cells,
                                row,
                                col,
                                wrap_formula(&formula, Cell::Bool(b)),
                                text,
                                budget,
                            );
                        }
                        0x02 => {
                            let code = error_code(res[2]).to_string();
                            let cell = wrap_formula(&formula, Cell::Error(code.clone()));
                            push_cell(cells, row, col, cell, code, budget);
                        }
                        _ => {
                            // 0x03 empty cached result. Surface formula identity
                            // when rgce decompiled. Pushed directly because
                            // `push_cell` skips empty-text cells; gated on the text
                            // budget so a flood of blank-result formulas can't grow
                            // the cell vector past the global allocation bound.
                            if let (Some(fs), true) = (formula, *budget > 0) {
                                let cost = fs.len().min(*budget);
                                *budget -= cost;
                                cells.push(CellEntry {
                                    row,
                                    col,
                                    value: Cell::Formula {
                                        formula: fs,
                                        cached: Box::new(Cell::Text(String::new())),
                                    },
                                    text: String::new(),
                                    style: None,
                                    hyperlink: None,
                                });
                            }
                        }
                    }
                } else {
                    let f = f64::from_le_bytes(res.try_into().unwrap_or([0; 8]));
                    match formula {
                        Some(fs) => {
                            let cached = if formats.is_datetime(ixfe) {
                                Cell::Date(f)
                            } else {
                                Cell::Number(f)
                            };
                            let text = formats.render(f, ixfe);
                            let cell = Cell::Formula {
                                formula: fs,
                                cached: Box::new(cached),
                            };
                            push_cell(cells, row, col, cell, text, budget);
                        }
                        None => push_number(cells, row, col, f, ixfe, formats, budget),
                    }
                }
            }
        }
        _ => {}
    }
}

/// Wrap a cached value as `Cell::Formula` when the formula source was recovered,
/// else return the cached value unchanged.
fn wrap_formula(formula: &Option<String>, cached: Cell) -> Cell {
    match formula {
        Some(f) => Cell::Formula {
            formula: f.clone(),
            cached: Box::new(cached),
        },
        None => cached,
    }
}

/// Decode a `LABEL` / `RSTRING` / `STRING` cell whose text may span CONTINUE
/// records. `chunks[0]` is the record body; `chunks[1..]` are the CONTINUE
/// bodies. This replaces the single-record arms once in `decode_cell`: the
/// payload is reassembled across the record boundary before decoding.
#[allow(clippy::too_many_arguments)]
fn decode_string_cell(
    typ: u16,
    chunks: &[&[u8]],
    sheet_idx: usize,
    cells: &mut Vec<CellEntry>,
    rich: &mut BTreeMap<(u32, u16), Vec<crate::TextRun>>,
    last_formula: &mut Option<(usize, u32, u16, Option<String>)>,
    ctx: Ctx,
    budget: &mut usize,
) {
    if *budget == 0 {
        return;
    }
    let Some(&first) = chunks.first() else {
        return;
    };
    match typ {
        // LABEL / RSTRING carry row, col, ixfe (6 bytes) then the string; the
        // rich-run table trailing an RSTRING is irrelevant to plain text.
        LABEL | RSTRING => {
            let (Some(row), Some(col)) = (u16le(first, 0), u16le(first, 2)) else {
                return;
            };
            let decoded = if typ == RSTRING && ctx.biff8 {
                crate::sst::read_continued_rich(chunks, 6)
            } else {
                read_continued_xl_string(chunks, 6, ctx)
            };
            if let Some(s) = decoded {
                if typ == RSTRING {
                    let runs = parse_rstring_runs(first, 6, ctx, &s);
                    if !runs.is_empty() {
                        rich.insert((u32::from(row), col), runs);
                    }
                }
                push_text(cells, u32::from(row), col, s, budget);
            }
        }
        // STRING is the cached string result of the preceding FORMULA.
        STRING => {
            if let Some((si, r, c, fs)) = last_formula.take() {
                if si == sheet_idx {
                    if let Some(s) = read_continued_xl_string(chunks, 0, ctx) {
                        match fs {
                            // Preserve formula identity: a string-result formula
                            // becomes `Cell::Formula { cached: Text }`, not bare text.
                            Some(fstr) => {
                                let cell = Cell::Formula {
                                    formula: fstr,
                                    cached: Box::new(Cell::Text(s.clone())),
                                };
                                push_cell(cells, r, c, cell, s, budget);
                            }
                            None => push_text(cells, r, c, s, budget),
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn parse_rstring_runs(data: &[u8], off: usize, ctx: Ctx, text: &str) -> Vec<crate::TextRun> {
    if !ctx.biff8 {
        return Vec::new();
    }
    let Some(cch) = u16le(data, off).map(usize::from) else {
        return Vec::new();
    };
    let Some(flags) = data.get(off + 2).copied() else {
        return Vec::new();
    };
    if flags & 0x08 == 0 {
        return Vec::new();
    }
    let mut pos = off + 3;
    let Some(run_count) = u16le(data, pos).map(usize::from) else {
        return Vec::new();
    };
    pos += 2;
    if flags & 0x04 != 0 {
        pos = pos.saturating_add(4);
    }
    pos = pos.saturating_add(cch.saturating_mul(if flags & 0x01 != 0 { 2 } else { 1 }));
    let available = data.len().saturating_sub(pos) / 4;
    let mut starts = Vec::with_capacity(run_count.min(available));
    for index in 0..run_count.min(available) {
        if let Some(start) = u16le(data, pos + index * 4) {
            starts.push(usize::from(start));
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
        let mut unit = 0usize;
        let fragment = text
            .chars()
            .filter(|ch| {
                let position = unit;
                unit += ch.len_utf16();
                position >= start && position < end
            })
            .collect::<String>();
        if !fragment.is_empty() {
            runs.push(crate::TextRun::new(fragment, crate::Font::default()));
        }
    }
    runs
}

/// Read an `XLUnicodeString` (BIFF8) or codepage byte string (BIFF5/7) that may
/// span CONTINUE records, starting `off` bytes into `chunks[0]` (to step over a
/// cell-record header). The single-chunk case reduces to [`read_xl_string`].
fn read_continued_xl_string(chunks: &[&[u8]], off: usize, ctx: Ctx) -> Option<String> {
    if ctx.biff8 {
        // cch(2) + grbit(1) + chars, with the compression flag re-read at each
        // CONTINUE boundary (the SST split rules).
        crate::sst::read_continued_plain(chunks, off)
    } else {
        // BIFF5/7: cch(2) then `cch` raw codepage bytes, the byte run continuing
        // across CONTINUE boundaries with no per-chunk flag.
        let first = *chunks.first()?;
        let cch = u16le(first, off)? as usize;
        let mut bytes: Vec<u8> = Vec::with_capacity(cch.min(1 << 20));
        let (mut ci, mut p) = (0usize, off + 2);
        while bytes.len() < cch {
            while ci < chunks.len() && p >= chunks[ci].len() {
                ci += 1;
                p = 0;
            }
            let Some(chunk) = chunks.get(ci) else { break };
            bytes.push(chunk[p]);
            p += 1;
        }
        Some(ctx.enc.decode(&bytes).0.into_owned())
    }
}

/// Parse a `MERGECELLS` record ([MS-XLS] 2.4.168): `cmcs:u16` then `cmcs` ×
/// `Ref8U { rwFirst, rwLast, colFirst, colLast }` (all `u16`). Returns ranges as
/// `(first_row, first_col, last_row, last_col)`. The declared count is clamped to
/// what the record body can hold (a hostile count must not over-read or alloc).
fn parse_mergecells(data: &[u8]) -> Vec<(u32, u16, u32, u16)> {
    let Some(count) = u16le(data, 0) else {
        return Vec::new();
    };
    let count = (count as usize).min(data.len().saturating_sub(2) / 8);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let b = 2 + i * 8;
        if let (Some(rf), Some(rl), Some(cf), Some(cl)) = (
            u16le(data, b),
            u16le(data, b + 2),
            u16le(data, b + 4),
            u16le(data, b + 6),
        ) {
            out.push((u32::from(rf), cf, u32::from(rl), cl));
        }
    }
    out
}

struct XlsSheetView {
    frozen: bool,
    hide_gridlines: bool,
    zoom: Option<u16>,
    show_headers: Option<bool>,
    right_to_left: bool,
    selected: bool,
}

fn parse_window2(data: &[u8]) -> Option<XlsSheetView> {
    let flags = u16le(data, 0)?;
    Some(XlsSheetView {
        frozen: flags & (1 << 3) != 0,
        hide_gridlines: flags & (1 << 1) == 0,
        zoom: u16le(data, 12).filter(|&zoom| zoom != 0),
        show_headers: Some(flags & (1 << 2) != 0),
        right_to_left: flags & (1 << 6) != 0,
        selected: flags & (1 << 9) != 0,
    })
}

fn parse_pane_freeze(data: &[u8]) -> Option<(u32, u16)> {
    let cols = u16le(data, 0)?;
    let rows = u16le(data, 2)?;
    if rows == 0 && cols == 0 {
        None
    } else {
        Some((u32::from(rows), cols))
    }
}

fn apply_row_outline(data: &[u8], sheet: &mut Sheet) {
    let (Some(row), Some(height_twips), Some(options)) =
        (u16le(data, 0), u16le(data, 6), u32le(data, 12))
    else {
        return;
    };
    let level = (options & 0x07) as u8;
    let row = u32::from(row);
    if height_twips > 0 {
        sheet
            .row_heights
            .insert(row, f32::from(height_twips) / 20.0);
    }
    if options & 0x20 != 0 {
        sheet.hidden_rows.insert(row);
    }
    if level > 0 {
        sheet.row_outline.insert(row, level);
    }
    if options & 0x10 != 0 {
        sheet.collapsed_rows.insert(row);
    }
}

fn apply_col_outline(data: &[u8], sheet: &mut Sheet) {
    let (Some(first), Some(last), Some(width_256), Some(options)) = (
        u16le(data, 0),
        u16le(data, 2),
        u16le(data, 4),
        u16le(data, 8),
    ) else {
        return;
    };
    if first > last {
        return;
    }
    let level = ((options >> 8) & 0x07) as u8;
    for col in first..=last {
        if width_256 > 0 {
            sheet.col_widths.insert(col, f32::from(width_256) / 256.0);
        }
        if options & 0x01 != 0 {
            sheet.hidden_cols.insert(col);
        }
        if level > 0 {
            sheet.col_outline.insert(col, level);
        }
    }
}

fn apply_wsbool_outline(data: &[u8], sheet: &mut Sheet) {
    let Some(flags) = u16le(data, 0) else {
        return;
    };
    sheet.outline_summary_below = flags & 0x0040 != 0;
    sheet.outline_summary_right = flags & 0x0080 != 0;
}

fn parse_hlink(data: &[u8]) -> Vec<(u32, u16, String)> {
    if data.len() < 8 {
        return Vec::new();
    }
    let (Some(rf), Some(rl), Some(cf), Some(cl)) = (
        u16le(data, 0),
        u16le(data, 2),
        u16le(data, 4),
        u16le(data, 6),
    ) else {
        return Vec::new();
    };
    let Some(url) = hlink_url(data) else {
        return Vec::new();
    };

    let first_row = u32::from(rf.min(rl));
    let last_row = u32::from(rf.max(rl));
    let first_col = cf.min(cl);
    let last_col = cf.max(cl);
    let mut out = Vec::new();
    'rows: for row in first_row..=last_row {
        for col in first_col..=last_col {
            if out.len() >= MAX_HLINK_ANCHORS {
                break 'rows;
            }
            out.push((row, col, url.clone()));
        }
    }
    out
}

fn hlink_url(data: &[u8]) -> Option<String> {
    for off in 8..data.len().saturating_sub(6) {
        let Some(cch) = u32le(data, off).map(|n| n as usize) else {
            continue;
        };
        if !(1..=2048).contains(&cch) {
            continue;
        }
        let start = off + 4;
        let end = start.checked_add(cch.checked_mul(2)?)?;
        if end > data.len() {
            continue;
        }
        if let Some(url) = decode_hlink_url_units(&data[start..end]) {
            return Some(url);
        }
    }
    for off in 8..data.len().saturating_sub(2) {
        if let Some(url) = decode_hlink_zero_terminated(&data[off..]) {
            return Some(url);
        }
    }
    None
}

fn decode_hlink_url_units(bytes: &[u8]) -> Option<String> {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    let url = String::from_utf16(&units).ok()?;
    is_external_hlink_url(&url).then_some(url)
}

fn decode_hlink_zero_terminated(bytes: &[u8]) -> Option<String> {
    let mut units = Vec::new();
    for chunk in bytes.chunks_exact(2).take(2048) {
        let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
        if unit == 0 {
            return decode_hlink_url_units(&units_to_bytes(&units));
        }
        units.push(unit);
    }
    None
}

fn units_to_bytes(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len() * 2);
    for unit in units {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

fn is_external_hlink_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("ftp://")
        || lower.starts_with("mailto:")
}

fn push_text(cells: &mut Vec<CellEntry>, row: u32, col: u16, s: String, budget: &mut usize) {
    push_cell(cells, row, col, Cell::Text(s.clone()), s, budget);
}

fn push_number(
    cells: &mut Vec<CellEntry>,
    row: u32,
    col: u16,
    value: f64,
    ixfe: u16,
    formats: &Formats,
    budget: &mut usize,
) {
    let text = formats.render(value, ixfe);
    let cell = if formats.is_datetime(ixfe) {
        Cell::Date(value)
    } else {
        Cell::Number(value)
    };
    push_cell(cells, row, col, cell, text, budget);
}

fn push_cell(
    cells: &mut Vec<CellEntry>,
    row: u32,
    col: u16,
    value: Cell,
    text: String,
    budget: &mut usize,
) {
    if !text.is_empty() {
        // Bound total accumulated text so shared-string reference amplification
        // (cloning one large pooled string into very many cells) cannot exhaust
        // memory; once the budget is spent, further cells are dropped.
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{extract_text, format_number, SheetMetadata, SheetType, SheetVisible};
    use std::io::{Cursor, Write};

    fn ctx8() -> Ctx {
        Ctx {
            biff8: true,
            enc: WINDOWS_1252,
        }
    }

    fn rec(typ: u16, body: &[u8]) -> Vec<u8> {
        let mut v = typ.to_le_bytes().to_vec();
        v.extend_from_slice(&(body.len() as u16).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    fn wrap_xls(stream: &[u8], name: &str) -> Vec<u8> {
        wrap_xls_with_extra_streams(stream, name, &[])
    }

    fn encode_legacy_text(encoding: &'static Encoding, value: &str) -> Vec<u8> {
        let (bytes, _, had_errors) = encoding.encode(value);
        assert!(
            !had_errors,
            "{value:?} is not representable in {}",
            encoding.name()
        );
        bytes.into_owned()
    }

    /// Build one complete BIFF5 `Book` stream so codepage tests exercise the
    /// global declaration, sheet-name path, and cell-string path together.
    fn biff5_single_label(
        declared_codepage: Option<u16>,
        encoding: &'static Encoding,
        sheet_name: &str,
        label: &str,
    ) -> Vec<u8> {
        let sheet_name = encode_legacy_text(encoding, sheet_name);
        let label = encode_legacy_text(encoding, label);

        let mut global_bof = vec![0x00, 0x05, 0x05, 0x00];
        global_bof.extend_from_slice(&[0u8; 4]);
        let mut stream = rec(BOF, &global_bof);
        if let Some(codepage) = declared_codepage {
            stream.extend_from_slice(&rec(CODEPAGE, &codepage.to_le_bytes()));
        }

        let mut boundsheet = vec![0, 0, 0, 0, 0, 0, sheet_name.len() as u8];
        boundsheet.extend_from_slice(&sheet_name);
        stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut sheet_bof = vec![0x00, 0x05, 0x10, 0x00];
        sheet_bof.extend_from_slice(&[0u8; 4]);
        stream.extend_from_slice(&rec(BOF, &sheet_bof));
        let mut label_record = vec![0, 0, 0, 0, 0, 0];
        label_record.extend_from_slice(&(label.len() as u16).to_le_bytes());
        label_record.extend_from_slice(&label);
        stream.extend_from_slice(&rec(LABEL, &label_record));
        stream.extend_from_slice(&rec(EOF, &[]));

        wrap_xls(&stream, "/Book")
    }

    fn wrap_xls_with_extra_streams(
        stream: &[u8],
        name: &str,
        extra: &[(&str, Vec<u8>)],
    ) -> Vec<u8> {
        let mut comp = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        comp.create_stream(name).unwrap().write_all(stream).unwrap();
        for (stream_name, body) in extra {
            comp.create_stream(stream_name)
                .unwrap()
                .write_all(body)
                .unwrap();
        }
        comp.flush().unwrap();
        comp.into_inner().into_inner()
    }

    #[derive(Clone, Copy)]
    enum TestPropertyValue<'a> {
        Lpstr(&'a str),
        Filetime(u64),
    }

    fn property_set_stream(fmtid: [u8; 16], properties: &[(u32, &str)]) -> Vec<u8> {
        let properties = properties
            .iter()
            .map(|(id, value)| (*id, TestPropertyValue::Lpstr(value)))
            .collect::<Vec<_>>();
        property_set_stream_values(fmtid, &properties)
    }

    fn property_set_stream_values(
        fmtid: [u8; 16],
        properties: &[(u32, TestPropertyValue<'_>)],
    ) -> Vec<u8> {
        let section_offset = 48u32;
        let mut section = Vec::new();
        section.extend_from_slice(&0u32.to_le_bytes()); // section size, patched below
        section.extend_from_slice(&(properties.len() as u32).to_le_bytes());

        let table_start = section.len();
        section.resize(section.len() + properties.len() * 8, 0);

        let mut value_offsets = Vec::new();
        for &(_id, value) in properties {
            value_offsets.push(section.len() as u32);
            match value {
                TestPropertyValue::Lpstr(value) => {
                    section.extend_from_slice(&0x1Eu32.to_le_bytes()); // VT_LPSTR
                    section.extend_from_slice(&((value.len() + 1) as u32).to_le_bytes());
                    section.extend_from_slice(value.as_bytes());
                    section.push(0);
                }
                TestPropertyValue::Filetime(value) => {
                    section.extend_from_slice(&0x40u32.to_le_bytes()); // VT_FILETIME
                    section.extend_from_slice(&value.to_le_bytes());
                }
            }
            while section.len() % 4 != 0 {
                section.push(0);
            }
        }

        let section_size = section.len() as u32;
        section[0..4].copy_from_slice(&section_size.to_le_bytes());
        for (idx, ((id, _value), value_offset)) in properties.iter().zip(value_offsets).enumerate()
        {
            let entry = table_start + idx * 8;
            section[entry..entry + 4].copy_from_slice(&id.to_le_bytes());
            section[entry + 4..entry + 8].copy_from_slice(&value_offset.to_le_bytes());
        }

        let mut stream = Vec::new();
        stream.extend_from_slice(&0xFFFEu16.to_le_bytes()); // little endian property set
        stream.extend_from_slice(&0u16.to_le_bytes()); // version
        stream.extend_from_slice(&0u32.to_le_bytes()); // system identifier
        stream.extend_from_slice(&[0u8; 16]); // CLSID
        stream.extend_from_slice(&1u32.to_le_bytes()); // one property set
        stream.extend_from_slice(&fmtid);
        stream.extend_from_slice(&section_offset.to_le_bytes());
        stream.extend_from_slice(&section);
        stream
    }

    #[test]
    fn rk_decoding() {
        // integer 12, not /100: rk = (12 << 2) | 0x02
        assert_eq!(rk_to_f64((12i32 << 2) as u32 | 0x02), 12.0);
        // integer 250 with /100 flag => 2.5
        assert_eq!(rk_to_f64((250i32 << 2) as u32 | 0x03), 2.5);
    }

    #[test]
    fn number_formatting() {
        assert_eq!(format_number(10.0), "10");
        assert_eq!(format_number(2.5), "2.5");
    }

    #[test]
    fn short_and_long_strings() {
        // ShortXLUnicodeString "Hi" compressed
        let mut d = vec![0u8; 6];
        d[5] = 0x00; // worksheet
        d.push(2); // cch
        d.push(0x00); // grbit compressed
        d.extend_from_slice(b"Hi");
        let (name, sheet_type, hidden, very_hidden) = parse_boundsheet(&d, ctx8());
        assert_eq!(name, "Hi");
        assert_eq!(sheet_type, SheetType::WorkSheet);
        assert!(!hidden);
        assert!(!very_hidden);
    }

    #[test]
    fn boundsheet_hsstate_visibility() {
        // hsState (byte at offset 4) low 2 bits: 0 visible, 1 hidden, 2 veryHidden.
        let boundsheet = |hs_state: u8| {
            let mut d = vec![0u8; 6];
            d[4] = hs_state;
            d[5] = 0x00; // dt = worksheet
            d.push(2); // cch
            d.push(0x00); // grbit compressed
            d.extend_from_slice(b"S1");
            parse_boundsheet(&d, ctx8())
        };
        let (_, _, hidden, very_hidden) = boundsheet(0);
        assert!(!hidden && !very_hidden, "0 => visible");
        let (_, _, hidden, very_hidden) = boundsheet(1);
        assert!(hidden && !very_hidden, "1 => hidden");
        let (_, _, hidden, very_hidden) = boundsheet(2);
        assert!(!hidden && very_hidden, "2 => veryHidden");
    }

    #[test]
    fn xls_hidden_sheet_end_to_end() {
        // A workbook with a visible "S1" and a hidden "S2" (BOUNDSHEET hsState=1)
        // must surface `is_hidden()` on the second sheet.
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        // BOUNDSHEET "S1" visible (hsState=0).
        let mut bs1 = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs1.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs1));
        // BOUNDSHEET "S2" hidden (hsState=1 at offset 4).
        let mut bs2 = vec![0, 0, 0, 0, 1, 0, 2, 0x00];
        bs2.extend_from_slice(b"S2");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs2));
        stream.extend_from_slice(&rec(EOF, &[]));
        // Two empty worksheet substreams (sheets map to top-level BOFs in order).
        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(EOF, &[]));
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        assert_eq!(wb.sheets.len(), 2);
        assert!(!wb.sheets[0].is_hidden(), "S1 visible");
        assert!(wb.sheets[1].is_hidden(), "S2 hidden");
        assert!(!wb.sheets[1].is_very_hidden());
    }

    #[test]
    fn xls_boundsheet_preserves_sheet_types_end_to_end() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);

        let boundsheet = |name: &str, dt: u8, hs_state: u8| {
            let mut bs = vec![0, 0, 0, 0, hs_state, dt, name.len() as u8, 0x00];
            bs.extend_from_slice(name.as_bytes());
            rec(BOUNDSHEET, &bs)
        };
        stream.extend_from_slice(&boundsheet("Data", 0x00, 0));
        stream.extend_from_slice(&boundsheet("Macro", 0x01, 1));
        stream.extend_from_slice(&boundsheet("Chart", 0x02, 0));
        stream.extend_from_slice(&boundsheet("Vba", 0x06, 2));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        for _ in 0..4 {
            stream.extend_from_slice(&rec(BOF, &s_bof));
            stream.extend_from_slice(&rec(EOF, &[]));
        }

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        assert_eq!(
            wb.sheets_metadata(),
            vec![
                SheetMetadata {
                    name: "Data".to_string(),
                    typ: SheetType::WorkSheet,
                    visible: SheetVisible::Visible,
                },
                SheetMetadata {
                    name: "Macro".to_string(),
                    typ: SheetType::MacroSheet,
                    visible: SheetVisible::Hidden,
                },
                SheetMetadata {
                    name: "Chart".to_string(),
                    typ: SheetType::ChartSheet,
                    visible: SheetVisible::Visible,
                },
                SheetMetadata {
                    name: "Vba".to_string(),
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
    fn xls_window1_active_tab_surfaces_workbook_metadata() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);

        for name in ["Data", "Summary"] {
            let mut bs = vec![0, 0, 0, 0, 0, 0, name.len() as u8, 0x00];
            bs.extend_from_slice(name.as_bytes());
            stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        }

        let mut window1 = Vec::new();
        window1.extend_from_slice(&0i16.to_le_bytes()); // xWn
        window1.extend_from_slice(&0i16.to_le_bytes()); // yWn
        window1.extend_from_slice(&1i16.to_le_bytes()); // dxWn
        window1.extend_from_slice(&1i16.to_le_bytes()); // dyWn
        window1.extend_from_slice(&0u16.to_le_bytes()); // flags
        window1.extend_from_slice(&1u16.to_le_bytes()); // itabCur
        window1.extend_from_slice(&0u16.to_le_bytes()); // itabFirst
        window1.extend_from_slice(&1u16.to_le_bytes()); // ctabSel
        window1.extend_from_slice(&600u16.to_le_bytes()); // wTabRatio
        stream.extend_from_slice(&rec(WINDOW1, &window1));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        for _ in 0..2 {
            stream.extend_from_slice(&rec(BOF, &s_bof));
            stream.extend_from_slice(&rec(EOF, &[]));
        }

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
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
    fn xls_global_protect_record_surfaces_workbook_metadata() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);

        let mut bs = vec![0, 0, 0, 0, 0, 0, 4, 0x00];
        bs.extend_from_slice(b"Data");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(PROTECT, &1u16.to_le_bytes()));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

        assert!(wb.is_structure_protected());
        assert!(wb.metadata().structure_protected);
        assert!(<Workbook as crate::Reader>::metadata(&wb).structure_protected);
        assert!(
            !wb.sheet_by_name("Data").unwrap().is_protected(),
            "global Protect must not be treated as worksheet protection"
        );
    }

    #[test]
    fn xls_selected_window2_falls_back_to_active_sheet_metadata() {
        const WINDOW2: u16 = 0x023E;

        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);

        for name in ["Data", "Summary"] {
            let mut bs = vec![0, 0, 0, 0, 0, 0, name.len() as u8, 0x00];
            bs.extend_from_slice(name.as_bytes());
            stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        }
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(EOF, &[]));

        stream.extend_from_slice(&rec(BOF, &s_bof));
        let mut window2 = (1u16 << 9).to_le_bytes().to_vec(); // fSelected
        window2.extend_from_slice(&0u16.to_le_bytes()); // top visible row
        window2.extend_from_slice(&0u16.to_le_bytes()); // left visible column
        window2.extend_from_slice(&0u32.to_le_bytes()); // header color index
        window2.extend_from_slice(&0u16.to_le_bytes()); // page-break preview zoom
        window2.extend_from_slice(&0u16.to_le_bytes()); // normal zoom
        stream.extend_from_slice(&rec(WINDOW2, &window2));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
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
    fn xls_defined_name_is_read_from_lbl_record() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);

        // Lbl: visible, non-built-in, workbook-global name "Answer" whose
        // NameParsedFormula is PtgInt(42).
        let mut lbl = Vec::new();
        lbl.extend_from_slice(&0u16.to_le_bytes()); // flags
        lbl.push(0); // chKey
        lbl.push(6); // cch
        lbl.extend_from_slice(&3u16.to_le_bytes()); // cce
        lbl.extend_from_slice(&0u16.to_le_bytes()); // reserved3
        lbl.extend_from_slice(&0u16.to_le_bytes()); // itab: workbook global
        lbl.extend_from_slice(&[0, 0, 0, 0]); // reserved4..7
        lbl.push(0x00); // Name grbit: compressed
        lbl.extend_from_slice(b"Answer");
        lbl.extend_from_slice(&[0x1E, 42, 0]); // PtgInt(42)
        stream.extend_from_slice(&rec(0x0018, &lbl));

        let mut local_lbl = Vec::new();
        local_lbl.extend_from_slice(&0u16.to_le_bytes());
        local_lbl.push(0);
        local_lbl.push(4);
        local_lbl.extend_from_slice(&3u16.to_le_bytes());
        local_lbl.extend_from_slice(&0u16.to_le_bytes());
        local_lbl.extend_from_slice(&1u16.to_le_bytes()); // one-based sheet scope
        local_lbl.extend_from_slice(&[0, 0, 0, 0]);
        local_lbl.push(0x00);
        local_lbl.extend_from_slice(b"Rate");
        local_lbl.extend_from_slice(&[0x1E, 7, 0]);
        stream.extend_from_slice(&rec(0x0018, &local_lbl));

        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        let mut formula = vec![0, 0, 0, 0, 0, 0];
        formula.extend_from_slice(&42.0f64.to_le_bytes());
        formula.extend_from_slice(&[0, 0]);
        formula.extend_from_slice(&[0, 0, 0, 0]);
        let rgce = [0x23, 1, 0, 0, 0]; // PtgName, one-based Lbl index 1
        formula.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
        formula.extend_from_slice(&rgce);
        stream.extend_from_slice(&rec(FORMULA, &formula));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
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
    fn xls_sheet_local_builtin_names_surface_filter_and_print_area() {
        fn builtin_name(id: u8, itab: u16, rgce: &[u8]) -> Vec<u8> {
            let mut lbl = Vec::new();
            lbl.extend_from_slice(&0x0020u16.to_le_bytes()); // fBuiltin
            lbl.push(0); // chKey
            lbl.push(1); // cch: one built-in id byte
            lbl.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
            lbl.extend_from_slice(&0u16.to_le_bytes()); // reserved3
            lbl.extend_from_slice(&itab.to_le_bytes()); // 1-based sheet scope
            lbl.extend_from_slice(&[0, 0, 0, 0]); // reserved4..7
            lbl.push(id);
            lbl.extend_from_slice(rgce);
            lbl
        }

        fn area3d(r0: u16, c0: u16, r1: u16, c1: u16) -> Vec<u8> {
            let mut rgce = vec![0x3B, 0, 0]; // PtgArea3d, ixti=0
            rgce.extend_from_slice(&r0.to_le_bytes());
            rgce.extend_from_slice(&r1.to_le_bytes());
            rgce.extend_from_slice(&c0.to_le_bytes());
            rgce.extend_from_slice(&c1.to_le_bytes());
            rgce
        }

        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(0x0018, &builtin_name(0x0D, 1, &area3d(0, 0, 4, 2))));
        stream.extend_from_slice(&rec(0x0018, &builtin_name(0x06, 1, &area3d(1, 1, 5, 3))));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

        assert_eq!(wb.sheets[0].autofilter_range(), Some((0, 0, 4, 2)));
        assert_eq!(
            wb.sheets[0].page_setup().and_then(|ps| ps.print_area),
            Some((1, 1, 5, 3))
        );
        assert!(wb.defined_names().is_empty());
    }

    #[test]
    fn xls_sheet_local_print_titles_surface_repeat_rows_and_cols() {
        fn builtin_name(id: u8, itab: u16, rgce: &[u8]) -> Vec<u8> {
            let mut lbl = Vec::new();
            lbl.extend_from_slice(&0x0020u16.to_le_bytes()); // fBuiltin
            lbl.push(0); // chKey
            lbl.push(1); // cch: one built-in id byte
            lbl.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
            lbl.extend_from_slice(&0u16.to_le_bytes()); // reserved3
            lbl.extend_from_slice(&itab.to_le_bytes()); // 1-based sheet scope
            lbl.extend_from_slice(&[0, 0, 0, 0]); // reserved4..7
            lbl.push(id);
            lbl.extend_from_slice(rgce);
            lbl
        }

        fn area3d(r0: u16, c0: u16, r1: u16, c1: u16) -> Vec<u8> {
            let mut rgce = vec![0x3B, 0, 0]; // PtgArea3d, ixti=0
            rgce.extend_from_slice(&r0.to_le_bytes());
            rgce.extend_from_slice(&r1.to_le_bytes());
            rgce.extend_from_slice(&c0.to_le_bytes());
            rgce.extend_from_slice(&c1.to_le_bytes());
            rgce
        }

        let mut print_titles = area3d(0, 0, 1, 255);
        print_titles.extend_from_slice(&area3d(0, 0, u16::MAX, 2));
        print_titles.push(0x10); // PtgUnion

        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(0x0018, &builtin_name(0x07, 1, &print_titles)));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        let page_setup = wb.sheets[0].page_setup().expect("page setup");

        assert_eq!(page_setup.repeat_rows, Some((0, 1)));
        assert_eq!(page_setup.repeat_cols, Some((0, 2)));
        assert!(wb.defined_names().is_empty());
    }

    #[test]
    fn xls_page_setup_records_surface_public_metadata() {
        fn xl_string(value: &str) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
            out.push(0x00); // compressed BIFF8 string
            out.extend_from_slice(value.as_bytes());
            out
        }

        fn margin(value: f64) -> Vec<u8> {
            value.to_le_bytes().to_vec()
        }

        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(0x0014, &xl_string("&CQuarterly report")));
        stream.extend_from_slice(&rec(0x0015, &xl_string("&CPage &P")));
        stream.extend_from_slice(&rec(0x0026, &margin(0.5)));
        stream.extend_from_slice(&rec(0x0027, &margin(0.6)));
        stream.extend_from_slice(&rec(0x0028, &margin(0.7)));
        stream.extend_from_slice(&rec(0x0029, &margin(0.8)));
        stream.extend_from_slice(&rec(0x002A, &1u16.to_le_bytes()));
        stream.extend_from_slice(&rec(0x002B, &1u16.to_le_bytes()));
        stream.extend_from_slice(&rec(0x0083, &1u16.to_le_bytes()));
        stream.extend_from_slice(&rec(0x0084, &1u16.to_le_bytes()));

        let mut setup = Vec::new();
        setup.extend_from_slice(&9u16.to_le_bytes()); // A4
        setup.extend_from_slice(&80u16.to_le_bytes()); // 80%
        setup.extend_from_slice(&3i16.to_le_bytes()); // first page number
        setup.extend_from_slice(&1u16.to_le_bytes()); // fit width
        setup.extend_from_slice(&2u16.to_le_bytes()); // fit height
        setup.extend_from_slice(&0x0080u16.to_le_bytes()); // fUsePage, landscape
        setup.extend_from_slice(&300u16.to_le_bytes()); // horizontal DPI
        setup.extend_from_slice(&300u16.to_le_bytes()); // vertical DPI
        setup.extend_from_slice(&0.2f64.to_le_bytes()); // header margin
        setup.extend_from_slice(&0.25f64.to_le_bytes()); // footer margin
        setup.extend_from_slice(&1u16.to_le_bytes()); // copies
        stream.extend_from_slice(&rec(0x00A1, &setup));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        let ps = wb.sheets[0].page_setup().expect("page setup");

        assert!(ps.landscape);
        assert_eq!(ps.paper_size, Some(9));
        assert_eq!(ps.scale, Some(80));
        assert_eq!(ps.first_page_number, Some(3));
        assert_eq!(ps.fit_to_width, Some(1));
        assert_eq!(ps.fit_to_height, Some(2));
        assert_eq!(ps.header.as_deref(), Some("&CQuarterly report"));
        assert_eq!(ps.footer.as_deref(), Some("&CPage &P"));
        assert!(ps.center_horizontally);
        assert!(ps.center_vertically);
        assert!(wb.sheets[0].print_headings());
        assert!(wb.sheets[0].print_gridlines());
        assert_eq!(ps.margins, Some((0.5, 0.6, 0.7, 0.8, 0.2, 0.25)));
    }

    #[test]
    fn xls_outline_records_surface_public_metadata() {
        fn row_record(row: u16, options: u32) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&row.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // first described column
            out.extend_from_slice(&1u16.to_le_bytes()); // last described column + 1
            out.extend_from_slice(&0x8000u16.to_le_bytes()); // default row height
            out.extend_from_slice(&0u16.to_le_bytes()); // unused
            out.extend_from_slice(&0u16.to_le_bytes()); // unused in BIFF5+
            out.extend_from_slice(&options.to_le_bytes());
            out
        }

        fn col_info(first: u16, last: u16, options: u16) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&first.to_le_bytes());
            out.extend_from_slice(&last.to_le_bytes());
            out.extend_from_slice(&0x08FFu16.to_le_bytes()); // default width
            out.extend_from_slice(&0u16.to_le_bytes()); // default XF
            out.extend_from_slice(&options.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // unused
            out
        }

        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(0x0081, &0u16.to_le_bytes())); // summaries above/left
        stream.extend_from_slice(&rec(0x0208, &row_record(2, 2 | (1 << 4) | (1 << 5))));
        stream.extend_from_slice(&rec(0x0208, &row_record(3, 2)));
        stream.extend_from_slice(&rec(0x007D, &col_info(1, 3, (3 << 8) | 1)));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        let sheet = &wb.sheets[0];

        assert_eq!(sheet.row_outline_levels().get(&2), Some(&2));
        assert_eq!(sheet.row_outline_levels().get(&3), Some(&2));
        assert!(sheet.collapsed_rows().contains(&2));
        assert_eq!(sheet.col_outline_levels().get(&1), Some(&3));
        assert_eq!(sheet.col_outline_levels().get(&3), Some(&3));
        assert_eq!(sheet.row_heights().get(&2), Some(&(0x8000 as f32 / 20.0)));
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
    fn xls_protect_record_surfaces_public_metadata() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);

        for name in ["Protected", "Plain"] {
            let mut bs = vec![0, 0, 0, 0, 0, 0, name.len() as u8, 0x00];
            bs.extend_from_slice(name.as_bytes());
            stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        }
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(PROTECT, &1u16.to_le_bytes()));
        stream.extend_from_slice(&rec(EOF, &[]));
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        let protected = wb.sheet_by_name("Protected").unwrap();
        let plain = wb.sheet_by_name("Plain").unwrap();

        assert!(protected.is_protected());
        assert_eq!(protected.protection_options(), None);
        assert!(!plain.is_protected());

        let metadata = wb.worksheet_metadata("Protected").unwrap();
        assert!(metadata.protected);
        assert_eq!(metadata.protection_options, None);

        let generic_metadata =
            <Workbook as crate::Reader>::worksheet_metadata(&wb, "Protected").unwrap();
        assert!(generic_metadata.protected);
        assert_eq!(generic_metadata.protection_options, None);
    }

    #[test]
    fn xls_sheet_ext_tab_color_surfaces_public_metadata() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));

        let mut sheet_ext = Vec::new();
        sheet_ext.extend_from_slice(&0x0862u16.to_le_bytes()); // FrtHeader.rt = SheetExt
        sheet_ext.extend_from_slice(&0u16.to_le_bytes()); // FrtHeader.grbitFrt
        sheet_ext.extend_from_slice(&[0u8; 8]); // FrtHeader reserved fields
        sheet_ext.extend_from_slice(&0x14u32.to_le_bytes()); // record size without optional tail
        sheet_ext.extend_from_slice(&0x0Au32.to_le_bytes()); // icvPlain: indexed red
        stream.extend_from_slice(&rec(0x0862, &sheet_ext));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

        assert_eq!(
            wb.sheets[0].tab_color(),
            Some(crate::Color::rgb(0xFF, 0, 0))
        );
    }

    #[test]
    fn xls_sheet_ext_tab_color_respects_custom_palette_record() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);

        let mut palette = 56u16.to_le_bytes().to_vec();
        for (idx, color) in BIFF_DEFAULT_PALETTE.iter().enumerate() {
            let rgb = if idx == 2 {
                [0x12, 0x34, 0x56]
            } else {
                color.as_rgb()
            };
            palette.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 0]);
        }
        stream.extend_from_slice(&rec(0x0092, &palette));

        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));

        let mut sheet_ext = Vec::new();
        sheet_ext.extend_from_slice(&0x0862u16.to_le_bytes());
        sheet_ext.extend_from_slice(&0u16.to_le_bytes());
        sheet_ext.extend_from_slice(&[0u8; 8]);
        sheet_ext.extend_from_slice(&0x14u32.to_le_bytes());
        sheet_ext.extend_from_slice(&0x0Au32.to_le_bytes());
        stream.extend_from_slice(&rec(0x0862, &sheet_ext));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

        assert_eq!(
            wb.sheets[0].tab_color(),
            Some(crate::Color::rgb(0x12, 0x34, 0x56))
        );
    }

    #[test]
    fn xls_data_validation_records_surface_public_metadata() {
        fn xl_unicode(value: &str) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
            out.push(0x00); // compressed BIFF8 string
            out.extend_from_slice(value.as_bytes());
            out
        }

        fn dv_formula_string(value: &str) -> Vec<u8> {
            let mut rgce = Vec::new();
            rgce.push(0x17); // PtgStr
            rgce.push(value.len() as u8);
            rgce.push(0x00); // compressed
            rgce.extend_from_slice(value.as_bytes());

            let mut out = Vec::new();
            out.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // unused
            out.extend_from_slice(&rgce);
            out
        }

        fn empty_dv_formula() -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out
        }

        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));

        let mut dval = Vec::new();
        dval.extend_from_slice(&0u16.to_le_bytes()); // DVal flags
        dval.extend_from_slice(&0u32.to_le_bytes()); // xLeft
        dval.extend_from_slice(&0u32.to_le_bytes()); // yTop
        dval.extend_from_slice(&(-1i32).to_le_bytes()); // idObj: none
        dval.extend_from_slice(&1u32.to_le_bytes()); // idvMac
        stream.extend_from_slice(&rec(0x01B2, &dval));

        let mut dv = Vec::new();
        let flags = 3u32 // valType=list
            | (1u32 << 7) // fStrLookup
            | (1u32 << 8) // fAllowBlank
            | (1u32 << 18) // fShowInputMsg
            | (1u32 << 19); // fShowErrorMsg
        dv.extend_from_slice(&flags.to_le_bytes());
        dv.extend_from_slice(&xl_unicode("Pick"));
        dv.extend_from_slice(&xl_unicode("Invalid"));
        dv.extend_from_slice(&xl_unicode("Choose one"));
        dv.extend_from_slice(&xl_unicode("Use the list"));
        dv.extend_from_slice(&dv_formula_string("Yes,No"));
        dv.extend_from_slice(&empty_dv_formula());
        dv.extend_from_slice(&2u16.to_le_bytes()); // SqRefU.cref
        for value in [1u16, 3, 0, 0, 5, 5, 2, 4] {
            dv.extend_from_slice(&value.to_le_bytes()); // A2:A4, C6:E6
        }
        stream.extend_from_slice(&rec(0x01BE, &dv));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        let validations = wb.sheets[0].data_validations();

        assert_eq!(validations.len(), 2);
        assert_eq!(validations[0].sqref, (1, 0, 3, 0));
        assert_eq!(validations[1].sqref, (5, 2, 5, 4));
        assert_eq!(validations[0].kind, crate::DvKind::List);
        assert_eq!(validations[0].operator, crate::DvOp::Between);
        assert_eq!(validations[0].formula1, "\"Yes,No\"");
        assert!(validations[0].allow_blank);
        assert!(validations[0].show_input_message);
        assert!(validations[0].show_error_message);
        assert_eq!(
            validations[0].prompt.as_ref(),
            Some(&("Pick".to_string(), "Choose one".to_string()))
        );
        assert_eq!(
            validations[0].error.as_ref(),
            Some(&("Invalid".to_string(), "Use the list".to_string()))
        );
    }

    #[test]
    fn xls_note_records_surface_public_comments() {
        fn xl_unicode(value: &str) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
            out.push(0x00); // compressed BIFF8 string
            out.extend_from_slice(value.as_bytes());
            out
        }

        fn txo(text: &str) -> Vec<Vec<u8>> {
            let mut record = Vec::new();
            record.extend_from_slice(&0u16.to_le_bytes()); // alignment/flags
            record.extend_from_slice(&0u16.to_le_bytes()); // rot
            record.extend_from_slice(&0u16.to_le_bytes()); // reserved4
            record.extend_from_slice(&0u32.to_le_bytes()); // reserved5
            record.extend_from_slice(&(text.len() as u16).to_le_bytes()); // cchText
            record.extend_from_slice(&16u16.to_le_bytes()); // cbRuns
            record.extend_from_slice(&0u16.to_le_bytes()); // ifntEmpty
            record.extend_from_slice(&0u16.to_le_bytes()); // empty ObjFmla.cce

            let mut text_continue = vec![0x00]; // compressed XLUnicodeStringNoCch
            text_continue.extend_from_slice(text.as_bytes());

            let mut run_continue = Vec::new();
            run_continue.extend_from_slice(&0u16.to_le_bytes()); // first run starts at 0
            run_continue.extend_from_slice(&0u16.to_le_bytes()); // ifnt
            run_continue.extend_from_slice(&0u32.to_le_bytes()); // reserved
            run_continue.extend_from_slice(&(text.len() as u16).to_le_bytes()); // last run
            run_continue.extend_from_slice(&0u16.to_le_bytes()); // ifnt
            run_continue.extend_from_slice(&0u32.to_le_bytes()); // reserved

            vec![
                rec(0x01B5, &record),
                rec(0x003C, &text_continue),
                rec(0x003C, &run_continue),
            ]
        }

        fn note_obj(id_obj: u16) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&0x0015u16.to_le_bytes()); // FtCmo.ft
            out.extend_from_slice(&0x0012u16.to_le_bytes()); // FtCmo.cb
            out.extend_from_slice(&0x0019u16.to_le_bytes()); // FtCmo.ot = Note
            out.extend_from_slice(&id_obj.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // common object flags
            out.extend_from_slice(&[0u8; 12]); // FtCmo unused fields
            out.extend_from_slice(&0x000Du16.to_le_bytes()); // FtNts.ft
            out.extend_from_slice(&0x0016u16.to_le_bytes()); // FtNts.cb
            out.extend_from_slice(&[0u8; 16]); // guid
            out.extend_from_slice(&0u16.to_le_bytes()); // fSharedNote
            out.extend_from_slice(&0u32.to_le_bytes()); // unused
            out.extend_from_slice(&0u16.to_le_bytes()); // FtEnd.ft
            out.extend_from_slice(&0u16.to_le_bytes()); // FtEnd.cb
            out
        }

        fn note(row: u16, col: u16, id_obj: u16, author: &str) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&row.to_le_bytes());
            out.extend_from_slice(&col.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // hidden unless hovered
            out.extend_from_slice(&id_obj.to_le_bytes());
            out.extend_from_slice(&xl_unicode(author));
            out.push(0); // unused2
            out
        }

        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(0x005D, &note_obj(1025)));
        for part in txo("Check source total") {
            stream.extend_from_slice(&part);
        }
        stream.extend_from_slice(&rec(0x001C, &note(2, 1, 1025, "Auditor")));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        let comments = wb.sheets[0].comments();

        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].row, 2);
        assert_eq!(comments[0].col, 1);
        assert_eq!(comments[0].text, "Check source total");
        assert_eq!(comments[0].author.as_deref(), Some("Auditor"));
    }

    #[test]
    fn xls_doc_properties_surface_through_workbook_metadata() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&rec(EOF, &[]));

        let summary = property_set_stream_values(
            [
                0xE0, 0x85, 0x9F, 0xF2, 0xF9, 0x4F, 0x68, 0x10, 0xAB, 0x91, 0x08, 0x00, 0x2B, 0x27,
                0xB3, 0xD9,
            ],
            &[
                (2, TestPropertyValue::Lpstr("Legacy Report")),
                (3, TestPropertyValue::Lpstr("Procurement")),
                (4, TestPropertyValue::Lpstr("rxls reader")),
                (5, TestPropertyValue::Lpstr("bid,legacy")),
                (6, TestPropertyValue::Lpstr("XLS public metadata")),
                (8, TestPropertyValue::Lpstr("reviewer")),
                (12, TestPropertyValue::Filetime(0x01DD_0375_1176_3780)),
            ],
        );
        let doc_summary = property_set_stream(
            [
                0x02, 0xD5, 0xCD, 0xD5, 0x9C, 0x2E, 0x1B, 0x10, 0x93, 0x97, 0x08, 0x00, 0x2B, 0x2C,
                0xF9, 0xAE,
            ],
            &[(15, "ACME")],
        );

        let wb = Workbook::open(&wrap_xls_with_extra_streams(
            &stream,
            "/Workbook",
            &[
                ("/\u{0005}SummaryInformation", summary),
                ("/\u{0005}DocumentSummaryInformation", doc_summary),
            ],
        ))
        .unwrap();
        let metadata = wb.metadata();

        assert_eq!(metadata.properties.title.as_deref(), Some("Legacy Report"));
        assert_eq!(metadata.properties.subject.as_deref(), Some("Procurement"));
        assert_eq!(metadata.properties.creator.as_deref(), Some("rxls reader"));
        assert_eq!(metadata.properties.keywords.as_deref(), Some("bid,legacy"));
        assert_eq!(
            metadata.properties.description.as_deref(),
            Some("XLS public metadata")
        );
        assert_eq!(
            metadata.properties.last_modified_by.as_deref(),
            Some("reviewer")
        );
        assert_eq!(
            metadata.properties.created.as_deref(),
            Some("2026-06-24T01:02:03Z")
        );
        assert_eq!(metadata.properties.company.as_deref(), Some("ACME"));
        assert_eq!(metadata.sheets[0].name, "S1");
    }

    #[test]
    fn biff5_label_decodes_cp949() {
        // BIFF5 string: u16 byte-length, then raw codepage bytes (no grbit).
        let (kr, _, _) = EUC_KR.encode("한글");
        let mut data = (kr.len() as u16).to_le_bytes().to_vec();
        data.extend_from_slice(&kr);
        let ctx5 = Ctx {
            biff8: false,
            enc: EUC_KR,
        };
        assert_eq!(read_xl_string(&data, 0, ctx5).as_deref(), Some("한글"));
        // The same bytes under cp1252 would be mojibake (not "한글").
        let ctx_western = Ctx {
            biff8: false,
            enc: WINDOWS_1252,
        };
        assert_ne!(
            read_xl_string(&data, 0, ctx_western).as_deref(),
            Some("한글")
        );
    }

    #[test]
    fn rstring_cell_is_decoded_like_label() {
        // RSTRING: row,col,ixfe, XLUnicodeString "Hi" (compressed), + run table.
        let mut data = vec![0u8; 6]; // row=0,col=0,ixfe=0
        data.extend_from_slice(&2u16.to_le_bytes()); // cch
        data.push(0x08); // grbit compressed + rich runs
        data.extend_from_slice(&2u16.to_le_bytes()); // cRun
        data.extend_from_slice(b"Hi");
        data.extend_from_slice(&0u16.to_le_bytes()); // ich
        data.extend_from_slice(&1u16.to_le_bytes()); // ifnt
        data.extend_from_slice(&1u16.to_le_bytes()); // ich
        data.extend_from_slice(&2u16.to_le_bytes()); // ifnt
        let mut cells = Vec::new();
        let mut rich = BTreeMap::new();
        let mut lf = None;
        let mut budget = MAX_TEXT_BYTES;
        decode_string_cell(
            RSTRING,
            &[&data],
            0,
            &mut cells,
            &mut rich,
            &mut lf,
            ctx8(),
            &mut budget,
        );
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].value, Cell::Text("Hi".to_string()));
        assert_eq!(cells[0].text, "Hi");
        assert_eq!(
            rich[&(0, 0)]
                .iter()
                .map(|run| run.text.as_str())
                .collect::<Vec<_>>(),
            ["H", "i"]
        );
    }

    #[test]
    fn label_spanning_continue_is_reassembled() {
        // A LABEL whose characters overflow the record cap continues into a
        // CONTINUE record; the bytes after the split must be reassembled, not
        // truncated. BIFF8 re-reads the compression flag at each chunk.
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));

        // Compressed LABEL at (0,0): cch=10 ("HELLOWORLD") split 5 / 5.
        let mut lbl = vec![0, 0, 0, 0, 0, 0];
        lbl.extend_from_slice(&10u16.to_le_bytes());
        lbl.push(0x00); // grbit compressed
        lbl.extend_from_slice(b"HELLO");
        stream.extend_from_slice(&rec(LABEL, &lbl));
        let mut cont = vec![0x00u8]; // continuation grbit, still compressed
        cont.extend_from_slice(b"WORLD");
        stream.extend_from_slice(&rec(CONTINUE, &cont));

        // Uncompressed LABEL at (1,0): cch=4 ("입찰공고") split 2 / 2 chars.
        let kr: Vec<u16> = "입찰공고".encode_utf16().collect();
        let mut lbl2 = vec![1, 0, 0, 0, 0, 0];
        lbl2.extend_from_slice(&(kr.len() as u16).to_le_bytes());
        lbl2.push(0x01); // grbit uncompressed (UTF-16LE)
        for u in &kr[..2] {
            lbl2.extend_from_slice(&u.to_le_bytes());
        }
        stream.extend_from_slice(&rec(LABEL, &lbl2));
        let mut cont2 = vec![0x01u8]; // continuation grbit, still uncompressed
        for u in &kr[2..] {
            cont2.extend_from_slice(&u.to_le_bytes());
        }
        stream.extend_from_slice(&rec(CONTINUE, &cont2));

        stream.extend_from_slice(&rec(EOF, &[]));

        let text = extract_text(&wrap_xls(&stream, "/Workbook")).unwrap();
        assert!(
            text.contains("HELLOWORLD"),
            "compressed split truncated: {text:?}"
        );
        assert!(
            text.contains("입찰공고"),
            "uncompressed split truncated: {text:?}"
        );
    }

    #[test]
    fn xls_merged_ranges_are_read() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));
        // sheet: BOF, MERGECELLS(1× Ref8U {rwFirst=0,rwLast=1,colFirst=0,colLast=2}), EOF
        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        let mut mc = 1u16.to_le_bytes().to_vec(); // count
        for v in [0u16, 1, 0, 2] {
            mc.extend_from_slice(&v.to_le_bytes());
        }
        stream.extend_from_slice(&rec(MERGECELLS, &mc));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        // (first_row, first_col, last_row, last_col) = A1:C2.
        assert_eq!(wb.sheets[0].merged_ranges(), &[(0, 0, 1, 2)]);
    }

    #[test]
    fn xls_hlink_record_surfaces_public_hyperlinks() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        let url = "https://example.com/bid";
        let mut hlink = Vec::new();
        for v in [0u16, 1, 1, 1] {
            hlink.extend_from_slice(&v.to_le_bytes());
        }
        hlink.extend_from_slice(&[0u8; 16]); // StdLink GUID placeholder.
        hlink.extend_from_slice(&1u32.to_le_bytes()); // link options placeholder.
        hlink.extend_from_slice(&[0u8; 16]); // URL moniker GUID placeholder.
        hlink.extend_from_slice(&((url.encode_utf16().count() + 1) as u32).to_le_bytes());
        for ch in url.encode_utf16() {
            hlink.extend_from_slice(&ch.to_le_bytes());
        }
        hlink.extend_from_slice(&0u16.to_le_bytes());
        stream.extend_from_slice(&rec(0x01B8, &hlink));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

        assert_eq!(
            wb.sheets[0].hyperlinks(),
            &[(0, 1, url.to_string()), (1, 1, url.to_string()),]
        );
    }

    #[test]
    fn xls_window2_and_pane_surface_sheet_view_metadata() {
        const PANE: u16 = 0x0041;
        const WINDOW2: u16 = 0x023E;

        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        let window2_flags = (1u16 << 3) // frozen panes
            | (1u16 << 6); // right-to-left view; gridlines/headers bits remain unset
        let mut window2 = window2_flags.to_le_bytes().to_vec();
        window2.extend_from_slice(&0u16.to_le_bytes()); // top visible row
        window2.extend_from_slice(&0u16.to_le_bytes()); // left visible column
        window2.extend_from_slice(&0u32.to_le_bytes()); // header color index
        window2.extend_from_slice(&0u16.to_le_bytes()); // page-break preview zoom
        window2.extend_from_slice(&0u16.to_le_bytes()); // normal zoom
        stream.extend_from_slice(&rec(WINDOW2, &window2));
        let mut pane = Vec::new();
        for value in [2u16, 1, 1, 2, 0] {
            pane.extend_from_slice(&value.to_le_bytes()); // freeze at C2
        }
        stream.extend_from_slice(&rec(PANE, &pane));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

        assert_eq!(
            wb.sheets[0].sheet_view(),
            crate::SheetView {
                freeze: Some((1, 2)),
                hide_gridlines: true,
                zoom: None,
                show_headers: Some(false),
                right_to_left: true,
            }
        );
    }

    #[test]
    fn xls_window2_explicit_visible_headers_are_preserved() {
        const WINDOW2: u16 = 0x023E;

        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        let window2_flags = (1u16 << 1) // display gridlines
            | (1u16 << 2); // display row/column headings
        let mut window2 = window2_flags.to_le_bytes().to_vec();
        window2.extend_from_slice(&0u16.to_le_bytes()); // top visible row
        window2.extend_from_slice(&0u16.to_le_bytes()); // left visible column
        window2.extend_from_slice(&0u32.to_le_bytes()); // header color index
        window2.extend_from_slice(&0u16.to_le_bytes()); // page-break preview zoom
        window2.extend_from_slice(&0u16.to_le_bytes()); // normal zoom
        stream.extend_from_slice(&rec(WINDOW2, &window2));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();

        assert_eq!(wb.sheets[0].sheet_view().show_headers, Some(true));
    }

    #[test]
    fn typed_cells_expose_value_kinds() {
        // globals: BOF, XF(ifmt=14 date), BOUNDSHEET "S1", EOF
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut xf = vec![0u8; 20];
        xf[2] = 14; // date format
        stream.extend_from_slice(&rec(XF, &xf));
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));
        // sheet: BOF, NUMBER(r0c0,ixfe0=date,45366), NUMBER(r0c1,ixfe?plain,12),
        //        BOOLERR(r1c0, TRUE), EOF
        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        let mut num = vec![0, 0, 0, 0, 0, 0];
        num.extend_from_slice(&45366.0f64.to_le_bytes());
        stream.extend_from_slice(&rec(NUMBER, &num));
        let mut num2 = vec![0, 0, 1, 0, 1, 0]; // r0c1, ixfe=1 (no XF[1] -> plain)
        num2.extend_from_slice(&12.0f64.to_le_bytes());
        stream.extend_from_slice(&rec(NUMBER, &num2));
        stream.extend_from_slice(&rec(BOOLERR, &[1, 0, 0, 0, 0, 0, 1, 0])); // r1c0 TRUE
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        let sheet = &wb.sheets[0];
        // Date keeps the raw serial; to_text renders the ISO string.
        assert_eq!(sheet.cell(0, 0), Some(&Cell::Date(45366.0)));
        assert!(sheet.to_text().contains("2024-03-15"));
        assert_eq!(sheet.cell(0, 1), Some(&Cell::Number(12.0)));
        assert_eq!(sheet.cell(1, 0), Some(&Cell::Bool(true)));
        assert_eq!(sheet.dimensions(), Some((0, 0, 1, 1)));
        assert_eq!(sheet.cells().count(), 3);
    }

    #[test]
    fn boolerr_and_formula_results() {
        let f = &Formats::default();
        let mut cells = Vec::new();
        let mut lf = None;
        let mut budget = MAX_TEXT_BYTES;
        // BOOLERR error cell: bBoolErr=0x07 (#DIV/0!), fError=1.
        decode_cell(
            BOOLERR,
            &[0, 0, 0, 0, 0, 0, 0x07, 1],
            &[],
            0,
            &mut cells,
            &mut lf,
            f,
            &mut budget,
            &[],
            &[],
            &[],
            &[],
            ctx8(),
            &FormulaDefinitions::new(),
        );
        // BOOLERR bool FALSE at row 1.
        decode_cell(
            BOOLERR,
            &[1, 0, 0, 0, 0, 0, 0x00, 0],
            &[],
            0,
            &mut cells,
            &mut lf,
            f,
            &mut budget,
            &[],
            &[],
            &[],
            &[],
            ctx8(),
            &FormulaDefinitions::new(),
        );
        // FORMULA cached error: res[0]=0x02, res[2]=0x2A (#N/A), tail 0xFFFF.
        let mut fmla = vec![2, 0, 0, 0, 0, 0]; // row 2, col 0, ixfe 0
        fmla.extend_from_slice(&[0x02, 0x00, 0x2A, 0x00, 0x00, 0x00, 0xFF, 0xFF]);
        decode_cell(
            FORMULA,
            &fmla,
            &[],
            0,
            &mut cells,
            &mut lf,
            f,
            &mut budget,
            &[],
            &[],
            &[],
            &[],
            ctx8(),
            &FormulaDefinitions::new(),
        );
        assert_eq!(cells[0].value, Cell::Error("#DIV/0!".to_string()));
        assert_eq!(cells[1].value, Cell::Bool(false));
        assert_eq!(cells[2].value, Cell::Error("#N/A".to_string()));
    }

    #[test]
    fn xls_formula_decompiled_to_source() {
        // A FORMULA record with a real rgce surfaces Cell::Formula { source, cached }.
        let f = &Formats::default();
        let mut cells = Vec::new();
        let mut lf = None;
        let mut budget = MAX_TEXT_BYTES;
        let mut p = vec![0, 0, 0, 0, 0, 0]; // row, col, ixfe
        p.extend_from_slice(&30.0f64.to_le_bytes()); // cached result (numeric)
        p.extend_from_slice(&[0, 0]); // grbit
        p.extend_from_slice(&[0, 0, 0, 0]); // chn
                                            // rgce = SUM(A1:A2): PtgArea(A1:A2), PtgFuncVar(1 arg, SUM=4).
        let rgce: Vec<u8> = vec![0x25, 0, 0, 1, 0, 0, 0, 0, 0, 0x22, 1, 4, 0];
        p.extend_from_slice(&(rgce.len() as u16).to_le_bytes()); // cce
        p.extend_from_slice(&rgce);
        decode_cell(
            FORMULA,
            &p,
            &[],
            0,
            &mut cells,
            &mut lf,
            f,
            &mut budget,
            &[],
            &[],
            &[],
            &[],
            ctx8(),
            &FormulaDefinitions::new(),
        );
        assert_eq!(cells.len(), 1);
        match &cells[0].value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "SUM($A$1:$A$2)");
                assert_eq!(**cached, Cell::Number(30.0));
            }
            other => panic!("expected a formula cell, got {other:?}"),
        }
    }

    #[test]
    fn xls_formula_resolves_namex_from_supbook_externname_table() {
        let mut globals_bof = vec![0x00, 0x06, 0x05, 0x00];
        globals_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &globals_bof);
        stream.extend_from_slice(&rec(SUPBOOK, &[1, 0, 1, 4]));

        let mut extern_name = vec![0, 0, 0, 0, 0, 0, 12, 0];
        extern_name.extend_from_slice(b"ExternalRate");
        stream.extend_from_slice(&rec(EXTERNNAME, &extern_name));
        stream.extend_from_slice(&rec(EXTERNSHEET, &[1, 0, 0, 0, 0, 0, 0, 0]));

        let mut bound = vec![0, 0, 0, 0, 0, 0, 4, 0];
        bound.extend_from_slice(b"Data");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bound));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut sheet_bof = vec![0x00, 0x06, 0x10, 0x00];
        sheet_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &sheet_bof));
        let mut formula = vec![0, 0, 0, 0, 0, 0];
        formula.extend_from_slice(&1.0f64.to_le_bytes());
        formula.extend_from_slice(&[0, 0]);
        formula.extend_from_slice(&[0, 0, 0, 0]);
        let namex = [0x39, 0, 0, 1, 0, 0, 0];
        formula.extend_from_slice(&(namex.len() as u16).to_le_bytes());
        formula.extend_from_slice(&namex);
        stream.extend_from_slice(&rec(FORMULA, &formula));
        stream.extend_from_slice(&rec(EOF, &[]));

        let workbook = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        match workbook.sheets[0].cell(0, 0).unwrap() {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "[ixti:0]!ExternalRate");
                assert_eq!(cached.as_ref(), &Cell::Number(1.0));
            }
            other => panic!("expected resolved NameX formula, got {other:?}"),
        }
        assert_eq!(
            workbook.evaluate_cell("Data", 0, 0),
            crate::FormulaEvaluation::Fallback {
                cached: Cell::Number(1.0),
                reason: crate::FormulaUnsupportedReason::ExternalRef,
            }
        );
    }

    #[test]
    fn xls_formula_record_type_0406_uses_formula_decoder() {
        // Apache POI's WrongFormulaRecordType.xls carries formula records with
        // sid 0x0406. The payload is the standard BIFF8 FORMULA layout.
        let f = &Formats::default();
        let mut cells = Vec::new();
        let mut lf = None;
        let mut budget = MAX_TEXT_BYTES;
        let mut p = vec![3, 0, 0, 0, 0, 0]; // row 3, col 0, ixfe 0
        p.extend_from_slice(&3.0f64.to_le_bytes()); // cached result (numeric)
        p.extend_from_slice(&[0, 0]); // grbit
        p.extend_from_slice(&[0, 0, 0, 0]); // chn
        let rgce: Vec<u8> = vec![0x25, 0, 0, 1, 0, 0, 0, 0, 0, 0x22, 1, 4, 0];
        p.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
        p.extend_from_slice(&rgce);

        decode_cell(
            0x0406,
            &p,
            &[],
            0,
            &mut cells,
            &mut lf,
            f,
            &mut budget,
            &[],
            &[],
            &[],
            &[],
            ctx8(),
            &FormulaDefinitions::new(),
        );

        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].text, "3");
        match &cells[0].value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "SUM($A$1:$A$2)");
                assert_eq!(**cached, Cell::Number(3.0));
            }
            other => panic!("expected a formula cell, got {other:?}"),
        }
    }

    fn numeric_formula_body(row: u16, col: u16, cached: f64, rgce: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&row.to_le_bytes());
        body.extend_from_slice(&col.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&cached.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
        body.extend_from_slice(rgce);
        body
    }

    #[test]
    fn xls_shared_formula_is_reconstructed_for_each_cell() {
        let mut global_bof = vec![0x00, 0x06, 0x05, 0x00];
        global_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &global_bof);
        let mut boundsheet = vec![0, 0, 0, 0, 0, 0, 4, 0];
        boundsheet.extend_from_slice(b"Data");
        stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut sheet_bof = vec![0x00, 0x06, 0x10, 0x00];
        sheet_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &sheet_bof));
        let exp = [0x01, 0, 0, 1, 0]; // shared anchor B1
        stream.extend_from_slice(&rec(FORMULA, &numeric_formula_body(0, 1, 10.0, &exp)));

        let shared_rgce = [0x2C, 0, 0, 0xFF, 0xFF]; // PtgRefN: one column left
        let mut shared = Vec::new();
        shared.extend_from_slice(&0u16.to_le_bytes());
        shared.extend_from_slice(&1u16.to_le_bytes());
        shared.extend_from_slice(&[1, 1, 0, 2]);
        shared.extend_from_slice(&(shared_rgce.len() as u16).to_le_bytes());
        shared.extend_from_slice(&shared_rgce);
        stream.extend_from_slice(&rec(SHRFMLA, &shared));
        stream.extend_from_slice(&rec(FORMULA, &numeric_formula_body(1, 1, 20.0, &exp)));
        stream.extend_from_slice(&rec(EOF, &[]));

        let workbook = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        for (row, expected) in [(0, "A1"), (1, "A2")] {
            match workbook.sheets[0].cell(row, 1).unwrap() {
                Cell::Formula { formula, .. } => assert_eq!(formula, expected),
                other => panic!("expected shared formula at row {row}, got {other:?}"),
            }
        }
    }

    #[test]
    fn xls_array_formula_and_array_constant_are_reconstructed() {
        let mut global_bof = vec![0x00, 0x06, 0x05, 0x00];
        global_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &global_bof);
        let mut boundsheet = vec![0, 0, 0, 0, 0, 0, 4, 0];
        boundsheet.extend_from_slice(b"Data");
        stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut sheet_bof = vec![0x00, 0x06, 0x10, 0x00];
        sheet_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &sheet_bof));
        let exp = [0x01, 0, 0, 0, 0];
        stream.extend_from_slice(&rec(FORMULA, &numeric_formula_body(0, 0, 1.0, &exp)));

        let array_rgce = [0x20, 0, 0, 0, 0, 0, 0, 0];
        let mut array = Vec::new();
        array.extend_from_slice(&0u16.to_le_bytes());
        array.extend_from_slice(&0u16.to_le_bytes());
        array.extend_from_slice(&[0, 1]); // A1:B1
        array.extend_from_slice(&0u16.to_le_bytes());
        array.extend_from_slice(&0u32.to_le_bytes());
        array.extend_from_slice(&(array_rgce.len() as u16).to_le_bytes());
        array.extend_from_slice(&array_rgce);
        array.extend_from_slice(&[1, 0, 0]); // two columns, one row
        array.push(0x01);
        array.extend_from_slice(&1.0f64.to_le_bytes());
        array.push(0x01);
        array.extend_from_slice(&2.0f64.to_le_bytes());
        stream.extend_from_slice(&rec(ARRAY, &array));
        stream.extend_from_slice(&rec(FORMULA, &numeric_formula_body(0, 1, 2.0, &exp)));
        stream.extend_from_slice(&rec(EOF, &[]));

        let workbook = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        for col in 0..=1 {
            match workbook.sheets[0].cell(0, col).unwrap() {
                Cell::Formula { formula, .. } => assert_eq!(formula, "{1,2}"),
                other => panic!("expected array formula at col {col}, got {other:?}"),
            }
        }
    }

    #[test]
    fn xls_formula_resolves_3d_sheet_names_and_absolute_markers() {
        let mut global_bof = vec![0x00, 0x06, 0x05, 0x00];
        global_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &global_bof);
        for name in ["Calc", "Input Data"] {
            let mut boundsheet = vec![0, 0, 0, 0, 0, 0, name.len() as u8, 0];
            boundsheet.extend_from_slice(name.as_bytes());
            stream.extend_from_slice(&rec(BOUNDSHEET, &boundsheet));
        }
        let extern_sheet = [
            1, 0, // cXTI
            0, 0, // iSupBook
            1, 0, // itabFirst
            1, 0, // itabLast
        ];
        stream.extend_from_slice(&rec(EXTERNSHEET, &extern_sheet));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut sheet_bof = vec![0x00, 0x06, 0x10, 0x00];
        sheet_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &sheet_bof));
        let mut formula = vec![0, 0, 0, 0, 0, 0];
        formula.extend_from_slice(&7.0f64.to_le_bytes());
        formula.extend_from_slice(&[0, 0]);
        formula.extend_from_slice(&[0, 0, 0, 0]);
        let rgce = [
            0x3A, 0, 0, // PtgRef3d, ixti 0
            2, 0, // absolute row 2
            1, 0, // absolute column 1
        ];
        formula.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
        formula.extend_from_slice(&rgce);
        stream.extend_from_slice(&rec(FORMULA, &formula));
        stream.extend_from_slice(&rec(EOF, &[]));
        stream.extend_from_slice(&rec(BOF, &sheet_bof));
        stream.extend_from_slice(&rec(EOF, &[]));

        let workbook = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        match workbook.sheets[0].cell(0, 0).unwrap() {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "'Input Data'!$B$3");
                assert_eq!(cached.as_ref(), &Cell::Number(7.0));
            }
            other => panic!("expected 3D formula, got {other:?}"),
        }
    }

    #[test]
    fn xls_blank_result_formula_keeps_identity() {
        // A FORMULA whose cached result is blank (res[0]=0x03) must still surface
        // as Cell::Formula when the rgce decompiles instead of being dropped.
        let f = &Formats::default();
        let mut cells = Vec::new();
        let mut lf = None;
        let mut budget = MAX_TEXT_BYTES;
        let mut p = vec![0, 0, 0, 0, 0, 0]; // row, col, ixfe
        p.extend_from_slice(&[0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF]); // blank cached result
        p.extend_from_slice(&[0, 0]); // grbit
        p.extend_from_slice(&[0, 0, 0, 0]); // chn
        let rgce: Vec<u8> = vec![0x25, 0, 0, 1, 0, 0, 0, 0, 0, 0x22, 1, 4, 0]; // SUM(A1:A2)
        p.extend_from_slice(&(rgce.len() as u16).to_le_bytes());
        p.extend_from_slice(&rgce);
        decode_cell(
            FORMULA,
            &p,
            &[],
            0,
            &mut cells,
            &mut lf,
            f,
            &mut budget,
            &[],
            &[],
            &[],
            &[],
            ctx8(),
            &FormulaDefinitions::new(),
        );
        assert_eq!(cells.len(), 1, "blank-result formula must still surface");
        match &cells[0].value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "SUM($A$1:$A$2)");
                assert_eq!(**cached, Cell::Text(String::new()));
            }
            other => panic!("expected a formula cell, got {other:?}"),
        }
    }

    #[test]
    fn filepass_workbook_is_refused() {
        let mut bof = vec![0x00, 0x06, 0x05, 0x00]; // vers BIFF8, dt globals
        bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &bof);
        stream.extend_from_slice(&rec(FILEPASS, &[0x01, 0x00])); // RC4
        stream.extend_from_slice(&rec(EOF, &[]));
        let bytes = wrap_xls(&stream, "/Workbook");
        assert!(matches!(Workbook::open(&bytes), Err(Error::Encrypted)));
    }

    fn encrypt_default_xor_payload(data: &mut [u8], initial_index: usize) {
        // MS-OFFCRYPTO XOR array for "VelvetSweatshop" (key 0xB359, verifier
        // 0x9A0A), precomputed from Method 1 so this test fixture does not call
        // the production key-derivation path.
        const DEFAULT_XOR_ARRAY: [u8; 16] = [
            0x87, 0x6B, 0x9A, 0xE2, 0x1E, 0xE3, 0x05, 0x62, 0x1E, 0x69, 0x96, 0x60, 0x98, 0x6E,
            0x94, 0x04,
        ];
        let mut index = initial_index % DEFAULT_XOR_ARRAY.len();
        for byte in data {
            *byte = byte.rotate_left(5) ^ DEFAULT_XOR_ARRAY[index];
            index = (index + 1) % DEFAULT_XOR_ARRAY.len();
        }
    }

    fn encrypt_default_xor_workbook_stream(stream: &mut [u8]) {
        let mut pos = 0usize;
        while pos + 4 <= stream.len() {
            let typ = u16le(stream, pos).unwrap_or(0);
            let len = u16le(stream, pos + 2).unwrap_or(0) as usize;
            let start = pos + 4;
            let end = start.saturating_add(len);
            if end > stream.len() {
                break;
            }
            match typ {
                BOF | FILEPASS => {}
                BOUNDSHEET => {
                    if start + 4 < end {
                        encrypt_default_xor_payload(&mut stream[start + 4..end], end + 4);
                    }
                }
                _ => encrypt_default_xor_payload(&mut stream[start..end], end),
            }
            pos = end;
        }
    }

    #[test]
    fn xor_default_password_workbook_is_deobfuscated() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00]; // vers BIFF8, dt globals
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        // FilePass: wEncryptionType=0 (XOR), key=0xB359, verifier=0x9A0A for
        // the default Excel password "VelvetSweatshop".
        stream.extend_from_slice(&rec(FILEPASS, &[0x00, 0x00, 0x59, 0xB3, 0x0A, 0x9A]));
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00]; // dt worksheet
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        let mut num = vec![0, 0, 0, 0, 0, 0]; // row 0, col 0, ixfe 0
        num.extend_from_slice(&42.0f64.to_le_bytes());
        stream.extend_from_slice(&rec(NUMBER, &num));
        stream.extend_from_slice(&rec(EOF, &[]));

        encrypt_default_xor_workbook_stream(&mut stream);
        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        assert_eq!(wb.sheets[0].name, "S1");
        assert_eq!(wb.sheets[0].cell(0, 0), Some(&Cell::Number(42.0)));
    }

    #[test]
    fn biff8_end_to_end_with_sst_and_codepage() {
        // globals: BOF, CODEPAGE(949), BOUNDSHEET "S1", SST["셀A"], EOF
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        stream.extend_from_slice(&rec(CODEPAGE, &949u16.to_le_bytes()));
        // BOUNDSHEET: lbPlyPos(4)=0, hsState(1)=0, dt(1)=0, name "S1" (cch=2,grbit=0)
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        // SST: cstTotal=1, cstUnique=1, "셀A" uncompressed (cch=2, grbit=1)
        let mut sst = 1u32.to_le_bytes().to_vec();
        sst.extend_from_slice(&1u32.to_le_bytes());
        sst.extend_from_slice(&2u16.to_le_bytes());
        sst.push(0x01);
        for u in "셀A".encode_utf16() {
            sst.extend_from_slice(&u.to_le_bytes());
        }
        stream.extend_from_slice(&rec(SST, &sst));
        stream.extend_from_slice(&rec(EOF, &[]));
        // sheet substream: BOF, LABELSST(row0,col0,ixfe0,isst0), EOF
        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        let labelsst = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]; // row,col,ixfe,isst=0
        stream.extend_from_slice(&rec(LABELSST, &labelsst));
        stream.extend_from_slice(&rec(EOF, &[]));

        let bytes = wrap_xls(&stream, "/Workbook");
        let text = extract_text(&bytes).unwrap();
        assert!(text.contains("# S1"), "{text:?}");
        assert!(text.contains("셀A"), "{text:?}");
    }

    #[test]
    fn biff5_supported_codepages_have_golden_text_end_to_end() {
        let cases = [
            // 949 is Windows Korean/UHC (a superset of KS X 1001).
            (949, EUC_KR, "내역서", "뷁 테스트"),
            // 51949 is the BIFF declaration used for EUC-KR.
            (51949, EUC_KR, "한국어", "조달청 입찰공고"),
            (932, SHIFT_JIS, "集計", "日本語テスト"),
            (1252, WINDOWS_1252, "Résumé", "Café € – naïve"),
        ];

        for (codepage, encoding, sheet_name, expected) in cases {
            let bytes = biff5_single_label(Some(codepage), encoding, sheet_name, expected);
            let workbook = Workbook::open(&bytes).expect("open BIFF5 codepage fixture");
            assert_eq!(workbook.sheets[0].name, sheet_name, "codepage {codepage}");
            assert_eq!(
                workbook.sheets[0].cell(0, 0),
                Some(&Cell::Text(expected.to_string())),
                "codepage {codepage}"
            );
        }
    }

    #[test]
    fn biff5_codepage_fallback_and_override_policy_is_stable() {
        // Missing and unknown declarations use the documented cp1252 fallback.
        for declared in [None, Some(65_000)] {
            let bytes = biff5_single_label(declared, WINDOWS_1252, "Western", "Café €");
            let workbook = Workbook::open(&bytes).expect("open fallback fixture");
            assert_eq!(
                workbook.sheets[0].cell(0, 0),
                Some(&Cell::Text("Café €".to_string()))
            );
        }

        // A caller can correct a wrong declaration without changing the file.
        let wrongly_declared = biff5_single_label(Some(1252), EUC_KR, "Sheet", "한글");
        assert_ne!(
            Workbook::open(&wrongly_declared).unwrap().sheets[0].cell(0, 0),
            Some(&Cell::Text("한글".to_string()))
        );
        let forced = Workbook::open_with_codepage(&wrongly_declared, Some(949)).unwrap();
        assert_eq!(
            forced.sheets[0].cell(0, 0),
            Some(&Cell::Text("한글".to_string()))
        );

        // Malformed byte sequences are replaced with U+FFFD, never panicked or
        // silently dropped. 0x81 is an incomplete Shift-JIS lead byte.
        let malformed = [1, 0, 0x81];
        let context = Ctx {
            biff8: false,
            enc: SHIFT_JIS,
        };
        assert_eq!(read_xl_string(&malformed, 0, context).as_deref(), Some("�"));
    }

    #[test]
    fn biff5_custom_format_record_uses_short_string_length() {
        // BIFF5 FORMAT stores ifmt:u16, cch:u8, then raw codepage bytes. The
        // percent format must feed the same XF rendering path used by BIFF8.
        let mut g_bof = vec![0x00, 0x05, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 4]);
        let mut stream = rec(BOF, &g_bof);

        let mut fmt = 165u16.to_le_bytes().to_vec();
        fmt.push(5);
        fmt.extend_from_slice(b"0.00%");
        stream.extend_from_slice(&rec(FORMAT, &fmt));

        let mut xf = vec![0u8; 16];
        xf[2..4].copy_from_slice(&165u16.to_le_bytes());
        stream.extend_from_slice(&rec(XF, &xf));

        let mut bs = vec![0, 0, 0, 0, 0, 0, 2];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));

        let mut s_bof = vec![0x00, 0x05, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 4]);
        stream.extend_from_slice(&rec(BOF, &s_bof));

        let mut num = vec![0, 0, 0, 0, 0, 0];
        num.extend_from_slice(&1.0f64.to_le_bytes());
        stream.extend_from_slice(&rec(NUMBER, &num));
        stream.extend_from_slice(&rec(EOF, &[]));

        let bytes = wrap_xls(&stream, "/Book");
        let text = extract_text(&bytes).unwrap();
        assert!(text.contains("100%"), "{text:?}");
    }

    #[test]
    fn date_cell_renders_iso_end_to_end() {
        // globals: BOF, XF(ifmt=14 date), BOUNDSHEET "S1", EOF
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        let mut xf = vec![0u8; 20]; // ifnt(2), ifmt(2)=14, ...
        xf[2] = 14;
        stream.extend_from_slice(&rec(XF, &xf));
        let mut bs = vec![0, 0, 0, 0, 0, 0, 2, 0x00];
        bs.extend_from_slice(b"S1");
        stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        stream.extend_from_slice(&rec(EOF, &[]));
        // sheet: BOF, NUMBER(row0,col0,ixfe0, serial 45366.0), EOF
        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        let mut num = vec![0, 0, 0, 0, 0, 0]; // row,col,ixfe=0
        num.extend_from_slice(&45366.0f64.to_le_bytes());
        stream.extend_from_slice(&rec(NUMBER, &num));
        stream.extend_from_slice(&rec(EOF, &[]));

        let bytes = wrap_xls(&stream, "/Workbook");
        let text = extract_text(&bytes).unwrap();
        assert!(text.contains("2024-03-15"), "{text:?}");
    }

    #[test]
    fn nested_substream_does_not_desync_sheets() {
        // A worksheet substream may embed nested substreams (charts, pivot
        // tables) as `BOF … EOF`. Those nested BOFs must not advance the sheet
        // index; otherwise every sheet after the first embedded object is
        // silently dropped. This reproduces that real-world bug.
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        for nm in ["S1", "S2"] {
            let mut bs = vec![0, 0, 0, 0, 0, 0, nm.len() as u8, 0x00];
            bs.extend_from_slice(nm.as_bytes());
            stream.extend_from_slice(&rec(BOUNDSHEET, &bs));
        }
        stream.extend_from_slice(&rec(EOF, &[]));

        // BIFF8 LABEL cell (compressed text) at (row, col).
        let label = |row: u16, col: u16, s: &str| {
            let mut d = vec![
                row as u8,
                (row >> 8) as u8,
                col as u8,
                (col >> 8) as u8,
                0,
                0,
            ];
            d.extend_from_slice(&(s.len() as u16).to_le_bytes());
            d.push(0x00); // grbit: compressed
            d.extend_from_slice(s.as_bytes());
            rec(LABEL, &d)
        };

        // Sheet 1: BOF, LABEL "AAA", embedded chart substream (BOF dt=0x20, EOF),
        // then the sheet's own EOF.
        let mut s_bof = vec![0x00, 0x06, 0x10, 0x00];
        s_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&label(0, 0, "AAA"));
        let mut chart_bof = vec![0x00, 0x06, 0x20, 0x00]; // dt = 0x0020 (chart)
        chart_bof.extend_from_slice(&[0u8; 12]);
        stream.extend_from_slice(&rec(BOF, &chart_bof));
        stream.extend_from_slice(&rec(EOF, &[])); // end chart
        stream.extend_from_slice(&rec(EOF, &[])); // end sheet 1

        // Sheet 2: BOF, LABEL "BBB", EOF.
        stream.extend_from_slice(&rec(BOF, &s_bof));
        stream.extend_from_slice(&label(0, 0, "BBB"));
        stream.extend_from_slice(&rec(EOF, &[]));

        let wb = Workbook::open(&wrap_xls(&stream, "/Workbook")).unwrap();
        assert_eq!(wb.sheets.len(), 2);
        assert_eq!(wb.sheets[0].cell(0, 0), Some(&Cell::Text("AAA".into())));
        // Without the depth fix, the embedded chart BOF shifts S2 out of range
        // and "BBB" is lost.
        assert_eq!(wb.sheets[1].cell(0, 0), Some(&Cell::Text("BBB".into())));
    }

    #[test]
    fn rejects_non_ole2() {
        assert!(matches!(extract_text(b"not an xls"), Err(Error::NotOle2)));
    }

    #[test]
    fn rejects_empty_workbook_stream() {
        let bytes = wrap_xls(&[], "/Workbook");
        assert!(matches!(Workbook::open(&bytes), Err(Error::Biff(_))));
        assert!(matches!(extract_text(&bytes), Err(Error::Biff(_))));
    }

    #[test]
    fn rejects_random_workbook_stream() {
        let bytes = wrap_xls(b"random stream payload", "/Workbook");
        assert!(matches!(Workbook::open(&bytes), Err(Error::Biff(_))));
        assert!(matches!(extract_text(&bytes), Err(Error::Biff(_))));
    }

    #[test]
    fn rejects_unsupported_biff_version() {
        let mut g_bof = vec![0x34, 0x12, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        stream.extend_from_slice(&rec(EOF, &[]));

        let bytes = wrap_xls(&stream, "/Workbook");
        assert!(matches!(Workbook::open(&bytes), Err(Error::Biff(_))));
    }

    #[test]
    fn rejects_non_workbook_global_bof() {
        let mut sheet_bof = vec![0x00, 0x06, 0x10, 0x00];
        sheet_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &sheet_bof);
        stream.extend_from_slice(&rec(EOF, &[]));

        let bytes = wrap_xls(&stream, "/Workbook");
        assert!(matches!(Workbook::open(&bytes), Err(Error::Biff(_))));
    }

    #[test]
    fn rejects_truncated_biff_header_and_body() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);

        let mut truncated_header = rec(BOF, &g_bof);
        truncated_header.extend_from_slice(&rec(EOF, &[]));
        truncated_header.extend_from_slice(&[0x09, 0x08, 0x01]);
        assert!(matches!(
            Workbook::open(&wrap_xls(&truncated_header, "/Workbook")),
            Err(Error::Biff(_))
        ));

        let mut truncated_body = rec(BOF, &g_bof);
        truncated_body.extend_from_slice(&LABEL.to_le_bytes());
        truncated_body.extend_from_slice(&8u16.to_le_bytes());
        truncated_body.extend_from_slice(&[0x00, 0x00]);
        assert!(matches!(
            Workbook::open(&wrap_xls(&truncated_body, "/Workbook")),
            Err(Error::Biff(_))
        ));
    }

    #[test]
    fn rejects_unbalanced_biff_substreams() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);

        let unterminated = rec(BOF, &g_bof);
        assert!(matches!(
            Workbook::open(&wrap_xls(&unterminated, "/Workbook")),
            Err(Error::Biff(_))
        ));

        let mut extra_eof = rec(BOF, &g_bof);
        extra_eof.extend_from_slice(&rec(EOF, &[]));
        extra_eof.extend_from_slice(&rec(EOF, &[]));
        assert!(matches!(
            Workbook::open(&wrap_xls(&extra_eof, "/Workbook")),
            Err(Error::Biff(_))
        ));
    }

    #[test]
    fn preserves_empty_biff_semantics_with_valid_headers() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        stream.extend_from_slice(&rec(EOF, &[]));

        let bytes = wrap_xls(&stream, "/Workbook");
        let wb = Workbook::open(&bytes).unwrap();
        assert!(wb.sheets.is_empty());
        assert!(matches!(extract_text(&bytes), Err(Error::NoText)));
    }

    #[test]
    fn accepts_cfb_allocation_padding_after_balanced_stream() {
        let mut g_bof = vec![0x00, 0x06, 0x05, 0x00];
        g_bof.extend_from_slice(&[0u8; 12]);
        let mut stream = rec(BOF, &g_bof);
        stream.extend_from_slice(&rec(EOF, &[]));
        stream.extend_from_slice(&[0u8; 17]);

        let bytes = wrap_xls(&stream, "/Workbook");
        let wb = Workbook::open(&bytes).unwrap();
        assert!(wb.sheets.is_empty());
    }
}
