use super::style::{biff_palette_color, XlsStyles};
use super::workbook::{decode_chars, read_xl_string};
use super::{
    f64le, i16le, u16le, u32le, Ctx, ARRAY, BIFF_APPLICATION_DEFAULT_COLUMN_WIDTH_TWIPS,
    BIFF_ROW_FLAG_UNSYNCED, BLANK, BOOLERR, BOTTOMMARGIN, DEFAULTCOLWIDTH, DEFAULTROWHEIGHT,
    FOOTER, FORMULA, FORMULA_ALT, HCENTER, HEADER, HEADERFOOTER, HORIZONTALPAGEBREAKS, LABEL,
    LABELSST, LEFTMARGIN, MAX_BIFF_DEFAULT_COL_WIDTH_CHARS, MAX_BIFF_DEFAULT_ROW_HEIGHT_TWIPS,
    MAX_BIFF_ROW_HEIGHT_TWIPS, MAX_DV_RANGES, MAX_HLINK_ANCHORS, MIN_BIFF_ROW_HEIGHT_TWIPS,
    MULBLANK, MULRK, NUMBER, PRINTGRIDLINES, PRINTHEADERS, RIGHTMARGIN, RK, RSTRING, SETUP,
    SHEETEXT, SHRFMLA, STANDARDWIDTH, STRING, TOPMARGIN, VCENTER, VERTICALPAGEBREAKS,
};
use crate::format::Formats;
use crate::model::{
    Cell, CellEntry, CellStyle, Color, DataValidation, DvKind, DvOp, HeaderFooterKind,
    ImportedAxisMeasure, PageSetup, PrintLossKind, PrintMetadata, PrintPageOrder, Sheet,
};
use crate::{error_code, rk_to_f64, MAX_TEXT_BYTES};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub(super) type SheetRange = (u32, u16, u32, u16);
type SheetRanges = Vec<SheetRange>;

#[derive(Clone, Debug)]
pub(super) struct FormulaDefinition {
    pub(super) anchor: (u32, u16),
    pub(super) range: SheetRange,
    pub(super) rgce: Vec<u8>,
    pub(super) rgb_extra: Vec<u8>,
    pub(super) is_array: bool,
}

pub(super) type FormulaDefinitions = HashMap<(usize, u32, u16), FormulaDefinition>;
pub(super) type PendingFormula = (usize, u32, u16, u16, Option<String>);

/// Sheet-wide BIFF geometry records are retained separately until the workbook
/// generation is known and record-order-independent precedence can be applied.
#[derive(Debug, Default)]
pub(super) struct XlsSheetDefaults {
    def_col_width_256: Option<u32>,
    standard_col_width_256: Option<u32>,
    row_height: Option<BiffDefaultRowHeight>,
    pub(super) explicit_visible_rows: BTreeSet<u32>,
}

#[derive(Debug, Clone, Copy)]
struct BiffDefaultRowHeight {
    twips: u32,
    hidden: bool,
    manual: bool,
}

impl XlsSheetDefaults {
    pub(super) fn apply_record(&mut self, typ: u16, data: &[u8]) {
        match typ {
            DEFAULTCOLWIDTH => {
                if let Some(width) = parse_biff_default_col_width_256(data) {
                    self.def_col_width_256 = Some(width);
                }
            }
            STANDARDWIDTH => {
                if let Some(width) = parse_biff_standard_width_256(data) {
                    self.standard_col_width_256 = Some(width);
                }
            }
            DEFAULTROWHEIGHT => {
                if let Some(height) = parse_biff_default_row_height(data) {
                    self.row_height = Some(height);
                }
            }
            _ => {}
        }
    }

    pub(super) fn apply_to(self, sheet: &mut Sheet) {
        // Calc treats STANDARDWIDTH as the sheet-wide authority for every BIFF
        // generation supported here, regardless of record order. DEFCOLWIDTH
        // is its fallback; when both are absent Calc starts from its fixed
        // 64-point application default. Explicit COLINFO widths still win per
        // column during rendering.
        let explicit_width_256 = self.standard_col_width_256.or(self.def_col_width_256);
        sheet.default_col_width = explicit_width_256.map(|width| width as f32 / 256.0);
        sheet.imported_default_column_axis_measure = explicit_width_256
            .map(ImportedAxisMeasure::CharacterWidth256)
            .or_else(|| {
                sheet.is_worksheet.then_some(ImportedAxisMeasure::Twips(
                    BIFF_APPLICATION_DEFAULT_COLUMN_WIDTH_TWIPS,
                ))
            });
        sheet.biff_application_default_col_width =
            sheet.is_worksheet && explicit_width_256.is_none();
        sheet.default_row_height = self.row_height.map(|height| height.twips as f32 / 20.0);
        sheet.automatic_default_row_height_candidate =
            self.row_height.is_some_and(|height| !height.manual);
        sheet.imported_default_row_axis_measure = self
            .row_height
            .map(|height| ImportedAxisMeasure::Twips(height.twips));
        sheet.biff_application_default_row_height = sheet.is_worksheet && self.row_height.is_none();
        if sheet.is_worksheet && self.row_height.is_some_and(|height| height.hidden) {
            sheet.default_hidden_row_exceptions = Some(self.explicit_visible_rows);
        }
    }
}

fn parse_biff_default_col_width_256(data: &[u8]) -> Option<u32> {
    if data.len() != 2 {
        return None;
    }
    u16le(data, 0)
        .filter(|width| (1..=MAX_BIFF_DEFAULT_COL_WIDTH_CHARS).contains(width))
        .map(|width| u32::from(width) * 256)
}

fn parse_biff_standard_width_256(data: &[u8]) -> Option<u32> {
    if data.len() != 2 {
        return None;
    }
    u16le(data, 0).filter(|width| *width > 0).map(u32::from)
}

fn parse_biff_default_row_height(data: &[u8]) -> Option<BiffDefaultRowHeight> {
    // rxls accepts BIFF5/8 workbooks. Both use the BIFF3+ four-byte layout:
    // option flags first, then a signed twip height. A two-byte body is the
    // incompatible BIFF2 layout and must not be reinterpreted here.
    if data.len() != 4 {
        return None;
    }
    let flags = u16le(data, 0)?;
    let twips = i16le(data, 2)?;
    let hidden = flags & 0x0002 != 0;
    let manual = flags & 0x0001 != 0;
    let minimum = if hidden { 0 } else { 1 };
    (minimum..=MAX_BIFF_DEFAULT_ROW_HEIGHT_TWIPS)
        .contains(&twips)
        .then(|| {
            Some(BiffDefaultRowHeight {
                twips: u32::try_from(twips).ok()?,
                hidden,
                manual,
            })
        })
        .flatten()
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
///   [`crate::Workbook::open_with_codepage`] to force the intended codepage.

#[derive(Default)]
pub(super) struct XlsPageSetup {
    pub(super) setup: PageSetup,
    pub(super) print_metadata: PrintMetadata,
    pub(super) raw_fit_to_width: Option<u16>,
    pub(super) raw_fit_to_height: Option<u16>,
    pub(super) touched: bool,
    pub(super) left_margin: Option<f64>,
    pub(super) right_margin: Option<f64>,
    pub(super) top_margin: Option<f64>,
    pub(super) bottom_margin: Option<f64>,
    pub(super) header_margin: Option<f64>,
    pub(super) footer_margin: Option<f64>,
    pub(super) print_headings: bool,
    pub(super) print_gridlines: bool,
}

impl XlsPageSetup {
    pub(super) fn apply_record(&mut self, typ: u16, data: &[u8], ctx: Ctx) {
        self.print_metadata.mark_source();
        match typ {
            HEADER => self.set_header(data, ctx),
            FOOTER => self.set_footer(data, ctx),
            HORIZONTALPAGEBREAKS => self.set_page_breaks(data, true, ctx),
            VERTICALPAGEBREAKS => self.set_page_breaks(data, false, ctx),
            LEFTMARGIN => self.left_margin = read_margin(data),
            RIGHTMARGIN => self.right_margin = read_margin(data),
            TOPMARGIN => self.top_margin = read_margin(data),
            BOTTOMMARGIN => self.bottom_margin = read_margin(data),
            PRINTHEADERS => {
                self.print_headings = u16le(data, 0).unwrap_or(0) != 0;
                self.print_metadata.set_print_headings(self.print_headings);
            }
            PRINTGRIDLINES => {
                self.print_gridlines = u16le(data, 0).unwrap_or(0) != 0;
                self.print_metadata
                    .set_print_gridlines(self.print_gridlines);
            }
            HCENTER => {
                self.setup.center_horizontally = u16le(data, 0).unwrap_or(0) != 0;
                self.print_metadata
                    .set_center_horizontally(self.setup.center_horizontally);
                self.touched = true;
            }
            VCENTER => {
                self.setup.center_vertically = u16le(data, 0).unwrap_or(0) != 0;
                self.print_metadata
                    .set_center_vertically(self.setup.center_vertically);
                self.touched = true;
            }
            SETUP => self.set_setup(data),
            HEADERFOOTER => self.set_extended_header_footer(data, ctx),
            _ => {}
        }
        if matches!(typ, LEFTMARGIN | RIGHTMARGIN | TOPMARGIN | BOTTOMMARGIN)
            && read_margin(data).is_some()
        {
            self.touched = true;
        }
    }

    fn set_header(&mut self, data: &[u8], ctx: Ctx) {
        match read_xl_string(data, 0, ctx) {
            Some(text) => {
                self.print_metadata
                    .set_header_footer(HeaderFooterKind::OddHeader, text.clone());
                if !text.is_empty() {
                    self.setup.header = Some(text);
                    self.touched = true;
                }
            }
            None if !data.is_empty() => self
                .print_metadata
                .add_loss(PrintLossKind::MalformedHeaderFooter),
            None => self
                .print_metadata
                .set_header_footer(HeaderFooterKind::OddHeader, String::new()),
        }
    }

    fn set_footer(&mut self, data: &[u8], ctx: Ctx) {
        match read_xl_string(data, 0, ctx) {
            Some(text) => {
                self.print_metadata
                    .set_header_footer(HeaderFooterKind::OddFooter, text.clone());
                if !text.is_empty() {
                    self.setup.footer = Some(text);
                    self.touched = true;
                }
            }
            None if !data.is_empty() => self
                .print_metadata
                .add_loss(PrintLossKind::MalformedHeaderFooter),
            None => self
                .print_metadata
                .set_header_footer(HeaderFooterKind::OddFooter, String::new()),
        }
    }

    fn set_setup(&mut self, data: &[u8]) {
        let Some(flags) = u16le(data, 10) else {
            self.print_metadata
                .add_loss(PrintLossKind::UnsupportedProperty);
            return;
        };
        self.print_metadata.set_page_order(if flags & 0x0001 != 0 {
            PrintPageOrder::OverThenDown
        } else {
            PrintPageOrder::DownThenOver
        });
        let no_printer_settings = flags & 0x0004 != 0;
        let no_orientation = flags & 0x0040 != 0;
        if !no_printer_settings {
            self.setup.paper_size = nonzero_u16le(data, 0);
            self.setup.scale = nonzero_u16le(data, 2);
            if !no_orientation {
                self.setup.landscape = flags & 0x0002 == 0;
            }
        }
        self.raw_fit_to_width = u16le(data, 6);
        self.raw_fit_to_height = u16le(data, 8);
        self.resolve_fit_dimensions();
        if flags & 0x0080 != 0 {
            if let Some(page_start) = i16le(data, 4).filter(|page| *page > 0) {
                self.setup.first_page_number = Some(page_start as u16);
            }
        }
        self.header_margin = read_margin_at(data, 16);
        self.footer_margin = read_margin_at(data, 24);
        self.touched = true;
    }

    pub(super) fn set_wsbool(&mut self, data: &[u8]) {
        let Some(flags) = u16le(data, 0) else {
            return;
        };
        // [MS-XLS] WsBool.fFitToPage is the authoritative mode bit. Retain it
        // separately from SETUP's possibly stale scale and fit values.
        self.print_metadata.set_fit_to_page(flags & 0x0100 != 0);
        self.resolve_fit_dimensions();
    }

    fn resolve_fit_dimensions(&mut self) {
        let retain_zero = self.print_metadata.fit_to_page() == Some(true);
        self.setup.fit_to_width = self
            .raw_fit_to_width
            .filter(|value| retain_zero || *value != 0);
        self.setup.fit_to_height = self
            .raw_fit_to_height
            .filter(|value| retain_zero || *value != 0);
    }

    fn set_page_breaks(&mut self, data: &[u8], rows: bool, ctx: Ctx) {
        let Some(count) = u16le(data, 0).map(usize::from) else {
            self.print_metadata
                .add_loss(PrintLossKind::InvalidPageBreak);
            return;
        };
        let stride = if ctx.biff8 { 6usize } else { 2usize };
        let format_limit = if rows { 1_026usize } else { 255usize };
        if count > format_limit {
            self.print_metadata.add_loss(PrintLossKind::LimitExceeded);
        }
        for index in 0..count.min(format_limit) {
            let Some(offset) = 2usize.checked_add(index.saturating_mul(stride)) else {
                self.print_metadata
                    .add_loss(PrintLossKind::InvalidPageBreak);
                break;
            };
            let Some(main) = u16le(data, offset) else {
                self.print_metadata
                    .add_loss(PrintLossKind::InvalidPageBreak);
                break;
            };
            if rows {
                self.print_metadata.push_manual_row_break(u32::from(main));
            } else if main <= 255 {
                self.print_metadata.push_manual_col_break(main);
            } else {
                self.print_metadata
                    .add_loss(PrintLossKind::InvalidPageBreak);
            }
        }
    }

    fn set_extended_header_footer(&mut self, data: &[u8], ctx: Ctx) {
        if !ctx.biff8 || data.len() < 38 {
            self.print_metadata
                .add_loss(PrintLossKind::MalformedHeaderFooter);
            return;
        }
        if u16le(data, 0) != Some(HEADERFOOTER) {
            self.print_metadata
                .add_loss(PrintLossKind::MalformedHeaderFooter);
        }
        let Some(guid) = data.get(12..28) else {
            self.print_metadata
                .add_loss(PrintLossKind::MalformedHeaderFooter);
            return;
        };
        if guid.iter().any(|byte| *byte != 0) {
            self.print_metadata
                .add_loss(PrintLossKind::UnsupportedProperty);
            return;
        }
        let Some(flags) = u16le(data, 28) else {
            self.print_metadata
                .add_loss(PrintLossKind::MalformedHeaderFooter);
            return;
        };
        self.print_metadata.set_header_footer_flag(
            Some(flags & 0x0001 != 0),
            Some(flags & 0x0002 != 0),
            Some(flags & 0x0004 != 0),
            Some(flags & 0x0008 != 0),
        );
        let counts = [
            u16le(data, 30),
            u16le(data, 32),
            u16le(data, 34),
            u16le(data, 36),
        ];
        let kinds = [
            HeaderFooterKind::EvenHeader,
            HeaderFooterKind::EvenFooter,
            HeaderFooterKind::FirstHeader,
            HeaderFooterKind::FirstFooter,
        ];
        let enabled = [
            flags & 0x0001 != 0,
            flags & 0x0001 != 0,
            flags & 0x0002 != 0,
            flags & 0x0002 != 0,
        ];
        let mut offset = 38usize;
        for ((count, kind), enabled) in counts.into_iter().zip(kinds).zip(enabled) {
            let Some(count) = count else {
                self.print_metadata
                    .add_loss(PrintLossKind::MalformedHeaderFooter);
                return;
            };
            if count > 255 {
                self.print_metadata
                    .add_loss(PrintLossKind::MalformedHeaderFooter);
                return;
            }
            if count == 0 {
                if enabled {
                    self.print_metadata.set_header_footer(kind, String::new());
                }
                continue;
            }
            if !enabled {
                self.print_metadata
                    .add_loss(PrintLossKind::MalformedHeaderFooter);
            }
            let Some((text, consumed)) = read_xl_unicode_string(data, offset, ctx) else {
                self.print_metadata
                    .add_loss(PrintLossKind::MalformedHeaderFooter);
                return;
            };
            if text.encode_utf16().count() != usize::from(count) {
                self.print_metadata
                    .add_loss(PrintLossKind::MalformedHeaderFooter);
            }
            self.print_metadata.set_header_footer(kind, text);
            let Some(next) = offset.checked_add(consumed) else {
                self.print_metadata
                    .add_loss(PrintLossKind::MalformedHeaderFooter);
                return;
            };
            offset = next;
        }
    }

    fn take_page_setup(&mut self) -> Option<PageSetup> {
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
        self.touched.then(|| std::mem::take(&mut self.setup))
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

pub(super) fn apply_sheet_page_setups(sheets: &mut [Sheet], setups: Vec<XlsPageSetup>) {
    for (sheet, mut setup) in sheets.iter_mut().zip(setups) {
        sheet.print_headings = setup.print_headings;
        sheet.print_gridlines = setup.print_gridlines;
        if let Some(page_setup) = setup.take_page_setup() {
            sheet.page_setup = Some(page_setup);
        }
        sheet.print_metadata = setup.print_metadata;
    }
}

pub(super) fn parse_sheet_ext_tab_color(data: &[u8], palette: &[Color; 56]) -> Option<Color> {
    if u16le(data, 0)? != SHEETEXT {
        return None;
    }
    let icv_plain = (u32le(data, 16)? & 0x7F) as u8;
    if icv_plain == 0x7F {
        return None;
    }
    biff_palette_color(icv_plain, palette)
}

pub(super) struct XlsNoteSh {
    pub(super) row: u32,
    pub(super) col: u16,
    pub(super) id_obj: u16,
    pub(super) author: Option<String>,
}

pub(super) fn parse_note_obj_id(data: &[u8]) -> Option<u16> {
    if u16le(data, 0)? != 0x0015 || u16le(data, 2)? != 0x0012 {
        return None;
    }
    let object_type = u16le(data, 4)?;
    (object_type == 0x0019).then(|| u16le(data, 6)).flatten()
}

pub(super) fn parse_txo_text(chunks: &[&[u8]], budget: &mut usize) -> Option<String> {
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

pub(super) fn parse_note_sh(data: &[u8], ctx: Ctx) -> Option<XlsNoteSh> {
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

pub(super) fn parse_dv(data: &[u8], ctx: Ctx) -> Vec<DataValidation> {
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

pub(super) fn parse_formula_definition(typ: u16, data: &[u8]) -> Option<FormulaDefinition> {
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

pub(super) fn formula_context<'a>(
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
pub(super) fn apply_formula_definition(
    sheet_idx: usize,
    definition: &FormulaDefinition,
    cells: &mut [CellEntry],
    last_formula: &mut Option<PendingFormula>,
    budget: &mut usize,
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
    if let Some((si, row, col, _ixfe, source)) = last_formula.as_mut() {
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
            } => {
                // A late ARRAY/SHRFMLA record can replace a formula source
                // after the cached cell was already retained. Charge any
                // growth before mutating so an exhausted budget leaves the
                // existing typed value and source intact.
                let growth = formula.capacity().saturating_sub(source.capacity());
                if growth > *budget {
                    *budget = 0;
                } else {
                    *budget -= growth;
                    *source = formula;
                }
            }
            cached => {
                // Moving the existing cached value into the Box avoids a
                // transient clone. Its heap payload was charged when the cell
                // was first retained, so only the formula source and Box<Cell>
                // allocation are additional retained storage.
                let growth = formula
                    .capacity()
                    .saturating_add(std::mem::size_of::<Cell>());
                if growth > *budget {
                    *budget = 0;
                } else {
                    *budget -= growth;
                    let cached = std::mem::replace(cached, Cell::Bool(false));
                    cell.value = Cell::Formula {
                        formula,
                        cached: Box::new(cached),
                    };
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_cell(
    typ: u16,
    data: &[u8],
    sst: &[String],
    sheet_idx: usize,
    cells: &mut Vec<CellEntry>,
    last_formula: &mut Option<PendingFormula>,
    formats: &Formats,
    budget: &mut usize,
    styles: &XlsStyles,
    style_budget: &mut usize,
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
                    push_text(
                        cells,
                        row,
                        col,
                        s.clone(),
                        ixfe,
                        formats,
                        styles,
                        style_budget,
                        budget,
                    );
                }
            }
        }
        // LABEL / RSTRING / STRING text payloads may span CONTINUE records, so
        // they are gathered and decoded in the main record loop via
        // `decode_string_cell`, not here.
        NUMBER => {
            if let Some(b) = data.get(6..14) {
                let f = f64::from_le_bytes(b.try_into().unwrap_or([0; 8]));
                push_number(
                    cells,
                    row,
                    col,
                    f,
                    ixfe,
                    formats,
                    styles,
                    style_budget,
                    budget,
                );
            }
        }
        RK => {
            if let Some(rk) = u32le(data, 6) {
                push_number(
                    cells,
                    row,
                    col,
                    rk_to_f64(rk),
                    ixfe,
                    formats,
                    styles,
                    style_budget,
                    budget,
                );
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
                        styles,
                        style_budget,
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
                    push_cell(
                        cells,
                        row,
                        col,
                        Cell::Bool(b),
                        text,
                        ixfe,
                        styles,
                        style_budget,
                        budget,
                    );
                } else {
                    let code = error_code(v).to_string();
                    push_cell(
                        cells,
                        row,
                        col,
                        Cell::Error(code.clone()),
                        code,
                        ixfe,
                        styles,
                        style_budget,
                        budget,
                    );
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
                        0x00 => *last_formula = Some((sheet_idx, row, col, ixfe, formula)),
                        0x01 => {
                            let b = res[2] != 0;
                            let text = if b { "TRUE" } else { "FALSE" }.to_string();
                            push_cell(
                                cells,
                                row,
                                col,
                                wrap_formula(&formula, Cell::Bool(b)),
                                text,
                                ixfe,
                                styles,
                                style_budget,
                                budget,
                            );
                        }
                        0x02 => {
                            let code = error_code(res[2]).to_string();
                            let cell = wrap_formula(&formula, Cell::Error(code.clone()));
                            push_cell(
                                cells,
                                row,
                                col,
                                cell,
                                code,
                                ixfe,
                                styles,
                                style_budget,
                                budget,
                            );
                        }
                        _ => {
                            // 0x03 empty cached result. Surface formula identity
                            // when rgce decompiled. `push_cell` charges an
                            // allocation budget even though the display text is
                            // empty.
                            if let Some(fs) = formula {
                                push_cell(
                                    cells,
                                    row,
                                    col,
                                    Cell::Formula {
                                        formula: fs,
                                        cached: Box::new(Cell::Text(String::new())),
                                    },
                                    String::new(),
                                    ixfe,
                                    styles,
                                    style_budget,
                                    budget,
                                );
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
                            push_cell(
                                cells,
                                row,
                                col,
                                cell,
                                text,
                                ixfe,
                                styles,
                                style_budget,
                                budget,
                            );
                        }
                        None => push_number(
                            cells,
                            row,
                            col,
                            f,
                            ixfe,
                            formats,
                            styles,
                            style_budget,
                            budget,
                        ),
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

pub(super) fn retain_blank_cell_styles(
    typ: u16,
    data: &[u8],
    blank_styles: &mut BTreeMap<(u32, u16), CellStyle>,
    styles: &XlsStyles,
    style_budget: &mut usize,
) {
    let (Some(row), Some(first_col)) = (u16le(data, 0), u16le(data, 2)) else {
        return;
    };
    let row = u32::from(row);
    match typ {
        BLANK => {
            if let Some(style) = u16le(data, 4).and_then(|ixfe| styles.clone_xf(ixfe, style_budget))
            {
                blank_styles.insert((row, first_col), style);
            }
        }
        MULBLANK => {
            // row, colFirst, rgixfe[2 bytes each], colLast. BIFF5/8 has at
            // most 256 columns; clamp both the declared range and body count.
            let Some(last_col) = u16le(data, data.len().wrapping_sub(2)) else {
                return;
            };
            if first_col > last_col || first_col > 255 {
                return;
            }
            let count = usize::from(last_col.min(255) - first_col + 1)
                .min(data.len().saturating_sub(6) / 2);
            for offset in 0..count {
                let Some(ixfe) = u16le(data, 4 + offset * 2) else {
                    break;
                };
                if let Some(style) = styles.clone_xf(ixfe, style_budget) {
                    blank_styles.insert((row, first_col + offset as u16), style);
                }
            }
        }
        _ => {}
    }
}

/// Decode a `LABEL` / `RSTRING` / `STRING` cell whose text may span CONTINUE
/// records. `chunks[0]` is the record body; `chunks[1..]` are the CONTINUE
/// bodies. This replaces the single-record arms once in `decode_cell`: the
/// payload is reassembled across the record boundary before decoding.
#[allow(clippy::too_many_arguments)]
pub(super) fn decode_string_cell(
    typ: u16,
    chunks: &[&[u8]],
    sheet_idx: usize,
    cells: &mut Vec<CellEntry>,
    rich: &mut BTreeMap<(u32, u16), Vec<crate::TextRun>>,
    last_formula: &mut Option<PendingFormula>,
    ctx: Ctx,
    budget: &mut usize,
    formats: &Formats,
    styles: &XlsStyles,
    style_budget: &mut usize,
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
                    let runs = parse_rstring_runs(first, 6, ctx, &s, styles);
                    if !runs.is_empty() {
                        rich.insert((u32::from(row), col), runs);
                    }
                }
                push_text(
                    cells,
                    u32::from(row),
                    col,
                    s,
                    u16le(first, 4).unwrap_or(0),
                    formats,
                    styles,
                    style_budget,
                    budget,
                );
            }
        }
        // STRING is the cached string result of the preceding FORMULA.
        STRING => {
            if let Some((si, r, c, ixfe, fs)) = last_formula.take() {
                if si == sheet_idx {
                    if let Some(s) = read_continued_xl_string(chunks, 0, ctx) {
                        match fs {
                            // Preserve formula identity: a string-result formula
                            // becomes `Cell::Formula { cached: Text }`, not bare text.
                            Some(fstr) => {
                                let display = formats.render_text(&s, ixfe);
                                let cell = Cell::Formula {
                                    formula: fstr,
                                    cached: Box::new(Cell::Text(s)),
                                };
                                push_cell(
                                    cells,
                                    r,
                                    c,
                                    cell,
                                    display,
                                    ixfe,
                                    styles,
                                    style_budget,
                                    budget,
                                );
                            }
                            None => push_text(
                                cells,
                                r,
                                c,
                                s,
                                ixfe,
                                formats,
                                styles,
                                style_budget,
                                budget,
                            ),
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn parse_rstring_runs(
    data: &[u8],
    off: usize,
    ctx: Ctx,
    text: &str,
    styles: &XlsStyles,
) -> Vec<crate::TextRun> {
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
        if let (Some(start), Some(font_index)) = (
            u16le(data, pos + index * 4),
            u16le(data, pos + index * 4 + 2),
        ) {
            starts.push((usize::from(start), font_index));
        }
    }
    starts.sort_unstable_by_key(|&(start, _)| start);
    starts.dedup_by_key(|entry| entry.0);

    let text_units = text.encode_utf16().count();
    let mut runs = Vec::with_capacity(starts.len());
    for (index, (start, font_index)) in starts.iter().copied().enumerate() {
        if start >= text_units {
            continue;
        }
        let end = starts
            .get(index + 1)
            .map(|&(next, _)| next)
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
            runs.push(crate::TextRun::new(
                fragment,
                styles.font(font_index).unwrap_or_default(),
            ));
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
pub(super) fn parse_mergecells(data: &[u8]) -> Vec<(u32, u16, u32, u16)> {
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

pub(super) struct XlsSheetView {
    pub(super) frozen: bool,
    pub(super) hide_gridlines: bool,
    pub(super) zoom: Option<u16>,
    pub(super) show_headers: Option<bool>,
    pub(super) right_to_left: bool,
    pub(super) selected: bool,
}

pub(super) fn parse_window2(data: &[u8]) -> Option<XlsSheetView> {
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

pub(super) fn parse_pane_freeze(data: &[u8]) -> Option<(u32, u16)> {
    let cols = u16le(data, 0)?;
    let rows = u16le(data, 2)?;
    if rows == 0 && cols == 0 {
        None
    } else {
        Some((u32::from(rows), cols))
    }
}

pub(super) fn apply_row_outline(
    data: &[u8],
    sheet: &mut Sheet,
    explicit_visible_rows: &mut BTreeSet<u32>,
    styles: &XlsStyles,
    style_budget: &mut usize,
    biff8: bool,
) {
    let (Some(row), Some(height_twips), Some(options)) =
        (u16le(data, 0), u16le(data, 6), u32le(data, 12))
    else {
        return;
    };
    let level = (options & 0x07) as u8;
    let row = u32::from(row);
    let maximum_row = if biff8 { 65_535 } else { 16_383 };
    if row > maximum_row {
        return;
    }
    // [MS-XLS] 2.4.221 Row: `fUnsynced` marks a manually assigned `miyRw`;
    // Calc retains that manual provenance monotonically across duplicate ROW
    // records while the most recent valid `miyRw` remains authoritative.
    if (MIN_BIFF_ROW_HEIGHT_TWIPS..=MAX_BIFF_ROW_HEIGHT_TWIPS).contains(&height_twips)
        && (options & BIFF_ROW_FLAG_UNSYNCED != 0 || sheet.row_heights.contains_key(&row))
    {
        sheet
            .row_heights
            .insert(row, f32::from(height_twips) / 20.0);
        sheet
            .imported_row_axis_measures
            .insert(row, ImportedAxisMeasure::Twips(u32::from(height_twips)));
    }
    if options & 0x20 != 0 {
        sheet.hidden_rows.insert(row);
        explicit_visible_rows.remove(&row);
    } else {
        explicit_visible_rows.insert(row);
    }
    if level > 0 {
        sheet.row_outline.insert(row, level);
    }
    if options & 0x10 != 0 {
        sheet.collapsed_rows.insert(row);
    }
    // `fGhostDirty` signals that the 12-bit row XF index is meaningful.
    if options & 0x80 != 0 {
        let ixfe = ((options >> 16) & 0x0FFF) as u16;
        if let Some(style) = styles.clone_xf(ixfe, style_budget) {
            sheet.row_formats.insert(row, style);
        }
    }
}

pub(super) fn apply_col_outline(
    data: &[u8],
    sheet: &mut Sheet,
    explicit_hidden_cols: &mut BTreeSet<u16>,
    styles: &XlsStyles,
    style_budget: &mut usize,
) {
    let (Some(first), Some(last), Some(width_256), Some(ixfe), Some(options)) = (
        u16le(data, 0),
        u16le(data, 2),
        u16le(data, 4),
        u16le(data, 6),
        u16le(data, 8),
    ) else {
        return;
    };
    // BIFF5/8 worksheets have exactly 256 columns. Clamp hostile or malformed
    // ranges before iterating or retaining per-column style clones.
    if first > last || first > 255 {
        return;
    }
    let level = ((options >> 8) & 0x07) as u8;
    for col in first..=last.min(255) {
        if width_256 > 0 {
            sheet.col_widths.insert(col, f32::from(width_256) / 256.0);
            sheet.imported_column_axis_measures.insert(
                col,
                ImportedAxisMeasure::CharacterWidth256(u32::from(width_256)),
            );
        } else {
            // Calc interprets a zero COLINFO width as a hidden column and
            // restores the sheet default when it is shown again.
            sheet.col_widths.remove(&col);
            sheet.imported_column_axis_measures.remove(&col);
        }
        if options & 0x01 != 0 {
            explicit_hidden_cols.insert(col);
        }
        if explicit_hidden_cols.contains(&col) || width_256 == 0 {
            sheet.hidden_cols.insert(col);
        } else {
            // Explicit fHidden is monotonic in Calc, while zero-width hiding
            // follows only the final effective COLINFO width.
            sheet.hidden_cols.remove(&col);
        }
        if level > 0 {
            sheet.col_outline.insert(col, level);
        }
        if let Some(style) = styles.clone_xf(ixfe, style_budget) {
            sheet.col_formats.insert(col, style);
        }
    }
}

pub(super) fn apply_wsbool_outline(data: &[u8], sheet: &mut Sheet) {
    let Some(flags) = u16le(data, 0) else {
        return;
    };
    sheet.outline_summary_below = flags & 0x0040 != 0;
    sheet.outline_summary_right = flags & 0x0080 != 0;
}

pub(super) fn parse_hlink(data: &[u8]) -> Vec<(u32, u16, String)> {
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

#[allow(clippy::too_many_arguments)]
fn push_text(
    cells: &mut Vec<CellEntry>,
    row: u32,
    col: u16,
    s: String,
    ixfe: u16,
    formats: &Formats,
    styles: &XlsStyles,
    style_budget: &mut usize,
    budget: &mut usize,
) {
    let text = formats.render_text(&s, ixfe);
    push_cell(
        cells,
        row,
        col,
        Cell::Text(s),
        text,
        ixfe,
        styles,
        style_budget,
        budget,
    );
}

#[allow(clippy::too_many_arguments)]
fn push_number(
    cells: &mut Vec<CellEntry>,
    row: u32,
    col: u16,
    value: f64,
    ixfe: u16,
    formats: &Formats,
    styles: &XlsStyles,
    style_budget: &mut usize,
    budget: &mut usize,
) {
    let text = formats.render(value, ixfe);
    let cell = if formats.is_datetime(ixfe) {
        Cell::Date(value)
    } else {
        Cell::Number(value)
    };
    push_cell(
        cells,
        row,
        col,
        cell,
        text,
        ixfe,
        styles,
        style_budget,
        budget,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn push_cell(
    cells: &mut Vec<CellEntry>,
    row: u32,
    col: u16,
    value: Cell,
    text: String,
    ixfe: u16,
    styles: &XlsStyles,
    style_budget: &mut usize,
    budget: &mut usize,
) {
    // Empty source text is not a value cell. Numeric, boolean, date, error, and
    // formula records remain semantically present even when their number format
    // deliberately renders no display text (for example zero under `# ?/?`).
    if matches!(&value, Cell::Text(source) if source.is_empty()) {
        return;
    }

    // Account for every retained allocation, independently of display text.
    // LABELSST can clone one pooled string into many cells, and a fourth number-
    // format section can hide every clone; charging only rendered bytes would
    // therefore turn a small SST into unbounded retained heap growth.
    let cost = retained_cell_cost(&value, &text);
    if cost > *budget {
        *budget = 0;
        return;
    }
    *budget -= cost;
    cells.push(CellEntry {
        row,
        col,
        value,
        text,
        style: styles.clone_xf(ixfe, style_budget),
        xlsx_font_size_pt: None,
        hyperlink: None,
    });
}

pub(super) fn retained_cell_cost(value: &Cell, text: &String) -> usize {
    std::mem::size_of::<CellEntry>()
        .saturating_add(text.capacity())
        .saturating_add(retained_cell_value_heap_bytes(value))
}

fn retained_cell_value_heap_bytes(mut value: &Cell) -> usize {
    let mut bytes = 0usize;
    loop {
        match value {
            Cell::Text(source) | Cell::Error(source) => {
                return bytes.saturating_add(source.capacity());
            }
            Cell::Formula { formula, cached } => {
                bytes = bytes
                    .saturating_add(formula.capacity())
                    .saturating_add(std::mem::size_of::<Cell>());
                value = cached;
            }
            Cell::Number(_) | Cell::Date(_) | Cell::Bool(_) => return bytes,
        }
    }
}
