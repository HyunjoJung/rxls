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

mod cell;
mod drawing;
mod style;
mod workbook;

use cell::{apply_brt_formula_definition, decode_cell, parse_brt_formula_definition};
use drawing::read_sheet_drawings;
#[cfg(test)]
use drawing::{drawing_relationship_target, parse_xlsb_drawing_refs, XlsbDrawingKind};
use style::{add_style_loss, parse_styles, parse_xlsb_theme, Styles, XlsbTheme};
#[cfg(test)]
use style::{built_in_xlsb_num_fmt, verified_xlsb_collection_count};
#[cfg(test)]
use workbook::parse_brt_extern_sheets;
use workbook::{apply_xlsb_sheet_builtin_names, nullable_wide, parse_workbook};

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;

use crate::error::{Error, Result};
use crate::model::{
    ImportedAxisMeasure, OoxmlImplicitColumnWidth, OoxmlImplicitRowHeight, XlsbDefaultColumnWidth,
};
use crate::{
    CellEntry, CellStyle, Color, Comment, DataValidation, DvKind, DvOp, HeaderFooterKind,
    PageSetup, PrintLossKind, PrintMetadata, PrintPageOrder, ProtectionOptions, Sheet, SheetType,
    StyleFidelity, StyleLoss, StyleLossKind, Table, Workbook,
};

#[derive(Clone, Default)]
struct SharedString {
    text: String,
    runs: Vec<crate::TextRun>,
}

// BIFF12 record type ids ([MS-XLSB] 2.4).
const BRT_ROW_HDR: u32 = 0;
const BRT_CELL_BLANK: u32 = 1;
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
const BRT_AC_BEGIN: u32 = 37;
const BRT_AC_END: u32 = 38;
const BRT_FMT: u32 = 44;
const BRT_FONT: u32 = 43;
const BRT_FILL: u32 = 45;
const BRT_BORDER: u32 = 46;
const BRT_XF: u32 = 47;
const BRT_STYLE: u32 = 48;
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
const BRT_MARGINS: u32 = 476;
const BRT_PRINT_OPTIONS: u32 = 477;
const BRT_PAGE_SETUP: u32 = 478;
const BRT_BEGIN_HEADER_FOOTER: u32 = 479;
const BRT_WS_FMT_INFO: u32 = 0x01E5;
const BRT_BEGIN_RW_BRK: u32 = 392;
const BRT_END_RW_BRK: u32 = 393;
const BRT_BEGIN_COL_BRK: u32 = 394;
const BRT_END_COL_BRK: u32 = 395;
const BRT_BRK: u32 = 396;
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
const BRT_BEGIN_STYLES: u32 = 0x026B;
const BRT_END_STYLES: u32 = 0x026C;
const BRT_BEGIN_CELL_STYLE_XFS: u32 = 0x0272;
const BRT_END_CELL_STYLE_XFS: u32 = 0x0273;
const BRT_BEGIN_STYLE_SHEET: u32 = 0x0116;
const BRT_END_STYLE_SHEET: u32 = 0x0117;
const BRT_BEGIN_FONTS: u32 = 0x0263;
const BRT_END_FONTS: u32 = 0x0264;
const MAX_DVAL_RANGES: usize = 8192;
const MAX_TABLE_COLUMNS: usize = 16_384;
const MAX_XLSB_COL_INDEX: u32 = 16_383;
const MAX_XLSB_ROW_INDEX: u32 = 1_048_575;
const MAX_XLSB_ROW_HEIGHT_TWIPS: u16 = 8_192;
const XLSB_APPLICATION_DEFAULT_COL_WIDTH_256: u32 = 8 * 256 + 128;
const MAX_XLSB_SUPPORTING_LINKS: usize = 1 << 20;
const MAX_XLSB_EXTERNAL_NAMES: usize = 1 << 20;
const MAX_XLSB_STYLE_RECORDS: usize = 65_536;
// [MS-XLSB] §2.4.89 caps BrtBeginFonts.cfonts at 0xFFD3.
const MAX_XLSB_FONT_RECORDS: usize = 0xFFD3;
// [MS-XLSB] §§2.4.20, 2.4.22, and 2.4.232 require non-empty
// CellStyleXF, CellXF, and Style collections capped at 0xFF96 records.
const MAX_VERIFIED_XLSB_STYLE_RECORDS: usize = 0xFF96;
// Office accepts OOXML font sizes through 409.55 points. The renderer
// provenance sidecar is intentionally narrower because the public Font model
// stores whole points: only exact integral values through 409 are eligible.
const MAX_VERIFIED_XLSB_FONT_SIZE_POINTS: u16 = 409;
const MAX_XLSB_DRAWINGS: usize = 16_384;
const MAX_XLSB_DRAWING_TEXT: usize = 4_096;
const MAX_XLSB_BLANK_STYLES: usize = 1 << 20;

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
    let workbook_rels_xml =
        crate::xlsx::part_str(&mut zip, "xl/_rels/workbook.bin.rels").unwrap_or_default();
    let workbook_relationships = crate::xlsx::parse_ooxml_relationships(&workbook_rels_xml);
    let theme = match crate::xlsx::unique_internal_relationship_target(&workbook_rels_xml, "theme")
    {
        crate::xlsx::RelationshipTarget::Missing => {
            crate::xlsx::part_str(&mut zip, "xl/theme/theme1.xml")
                .map(|xml| parse_xlsb_theme(&xml))
                .unwrap_or_default()
        }
        crate::xlsx::RelationshipTarget::Internal(target) => {
            let path = normalize_part_target("xl/workbook.bin", &target);
            crate::xlsx::part_str(&mut zip, &path)
                .map(|xml| parse_xlsb_theme(&xml))
                .unwrap_or_else(XlsbTheme::invalid)
        }
        crate::xlsx::RelationshipTarget::Invalid => XlsbTheme::invalid(),
    };
    let styles = part(&mut zip, "xl/styles.bin")
        .map(|b| parse_styles(&b, &theme))
        .unwrap_or_default();
    let properties = crate::xlsx::parse_doc_properties(
        crate::xlsx::part_str(&mut zip, "docProps/core.xml").as_deref(),
        crate::xlsx::part_str(&mut zip, "docProps/app.xml").as_deref(),
    );
    let workbook_bin = part(&mut zip, "xl/workbook.bin").ok_or(Error::MissingWorkbook)?;
    let external_names = load_external_defined_names(
        &mut zip,
        &workbook_bin,
        workbook_relationships.as_deref().unwrap_or(&[]),
    );
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
    let mut chart_budget = crate::xlsx::ChartImportBudget::default();
    let mut sheets = Vec::with_capacity(names.len().min(1 << 16));
    let mut selected_sheet_fallback = None;
    for (sheet_index, (name, rid, hs_state)) in names.into_iter().enumerate() {
        let relationship = workbook_relationships.as_deref().and_then(|relationships| {
            relationships
                .iter()
                .find(|relationship| relationship.id == rid)
        });
        let path = relationship
            .filter(|relationship| !relationship.external)
            .and_then(|relationship| {
                crate::xlsx::resolve_internal_relationship_part(
                    "xl/workbook.bin",
                    &relationship.target,
                )
            })
            .unwrap_or_default();
        let sheet_type = relationship
            .filter(|relationship| {
                !relationship.external
                    && crate::xlsx::resolve_internal_relationship_part(
                        "xl/workbook.bin",
                        &relationship.target,
                    )
                    .is_some()
            })
            .map_or(SheetType::Vba, |relationship| {
                xlsb_sheet_type(&rid, hs_state, relationship.rel_type.as_deref())
            });
        let is_worksheet = sheet_type == SheetType::WorkSheet;
        let sheet_rels_xml = if is_worksheet {
            crate::xlsx::part_str(&mut zip, &sheet_rels_path(&path)).unwrap_or_default()
        } else {
            String::new()
        };
        let sheet_rels = if is_worksheet && !sheet_rels_xml.is_empty() {
            crate::xlsx::parse_ooxml_relationships(&sheet_rels_xml).unwrap_or_default()
        } else {
            Vec::new()
        };
        let comments = if is_worksheet {
            parse_sheet_comments(&mut zip, &path, &sheet_rels_xml)
        } else {
            Vec::new()
        };
        let (cells, merges, read_hyperlinks, mut metadata) = if is_worksheet {
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
            parse_sheet_tables(&mut zip, &path, &sheet_rels_xml, &metadata.table_rel_ids)
        } else {
            Vec::new()
        };
        let (images, charts, drawing_metadata, drawing_losses) = if is_worksheet {
            read_sheet_drawings(&mut zip, &path, &sheet_rels_xml, &theme, &mut chart_budget)
        } else {
            (Vec::new(), Vec::new(), Vec::new(), Vec::new())
        };
        let mut style_losses = styles.losses.clone();
        for loss in &metadata.style_losses {
            add_style_loss(&mut style_losses, loss.kind, loss.occurrences);
        }
        for loss in drawing_losses {
            add_style_loss(&mut style_losses, loss.kind, loss.occurrences);
        }
        let style_fidelity = if !styles.has_source_styles {
            StyleFidelity::Unavailable
        } else if style_losses.is_empty() {
            StyleFidelity::Retained
        } else {
            StyleFidelity::Partial
        };
        let ooxml_implicit_col_width = if !is_worksheet || metadata.default_col_width.is_some() {
            OoxmlImplicitColumnWidth::None
        } else if let Some(characters) = metadata.ooxml_base_col_width {
            OoxmlImplicitColumnWidth::BaseCharacters(f32::from(characters))
        } else {
            OoxmlImplicitColumnWidth::ApplicationDefault
        };
        let xlsb_default_col_width = if !is_worksheet {
            None
        } else if let Some(width_256) = metadata.default_col_width_256 {
            Some(XlsbDefaultColumnWidth::Digits256(width_256))
        } else if let Some(characters) = metadata.ooxml_base_col_width {
            Some(XlsbDefaultColumnWidth::BaseCharacters(characters))
        } else {
            Some(XlsbDefaultColumnWidth::ApplicationDefault)
        };
        let ooxml_implicit_row_height = if is_worksheet && metadata.default_row_height.is_none() {
            OoxmlImplicitRowHeight::XlsbApplicationDefault
        } else {
            OoxmlImplicitRowHeight::None
        };
        let imported_default_column_axis_measure = match xlsb_default_col_width {
            Some(XlsbDefaultColumnWidth::Digits256(width)) => {
                Some(ImportedAxisMeasure::DigitWidth256(width))
            }
            Some(XlsbDefaultColumnWidth::BaseCharacters(characters)) => u32::from(characters)
                .checked_mul(256)
                .map(ImportedAxisMeasure::DigitBaseWidth256),
            Some(XlsbDefaultColumnWidth::ApplicationDefault) => Some(
                ImportedAxisMeasure::DigitWidth256(XLSB_APPLICATION_DEFAULT_COL_WIDTH_256),
            ),
            None => None,
        };
        let default_hidden_row_exceptions = (is_worksheet && metadata.default_rows_hidden)
            .then_some(std::mem::take(&mut metadata.explicit_visible_rows));
        sheets.push(Sheet {
            name,
            is_worksheet,
            style_fidelity,
            sheet_type: Some(sheet_type),
            cells,
            default_format: styles.cell_styles.first().cloned(),
            col_formats: metadata.col_formats,
            row_formats: metadata.row_formats,
            blank_styles: metadata.blank_styles,
            read_merges: merges,
            read_hyperlinks,
            comments,
            tables,
            images,
            charts,
            drawing_metadata,
            style_losses,
            freeze: metadata.freeze,
            autofilter: metadata.autofilter,
            data_validations: metadata.data_validations,
            page_setup: metadata.page_setup,
            print_metadata: metadata.print_metadata,
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
            xlsb_col_widths_256: metadata.col_widths_256,
            xlsb_default_col_width,
            imported_row_axis_measures: metadata.imported_row_axis_measures,
            imported_default_row_axis_measure: metadata.imported_default_row_axis_measure,
            imported_column_axis_measures: metadata.imported_column_axis_measures,
            imported_default_column_axis_measure,
            default_row_height: metadata.default_row_height,
            default_col_width: metadata.default_col_width,
            ooxml_implicit_col_width,
            ooxml_implicit_row_height,
            xlsb_normal_font_size_pt: is_worksheet
                .then_some(styles.xlsb_normal_font_size_pt)
                .flatten(),
            xlsb_cell_font_sizes_pt: metadata.xlsb_cell_font_sizes_pt,
            xlsb_row_font_sizes_pt: metadata.xlsb_row_font_sizes_pt,
            xlsb_col_font_sizes_pt: metadata.xlsb_col_font_sizes_pt,
            hidden_rows: metadata.hidden_rows,
            hidden_cols: metadata.hidden_cols,
            default_hidden_row_exceptions,
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
        container_parse_mode: crate::ContainerParseMode::Primary,
        protect_structure,
        ..Default::default()
    })
}

fn xlsb_sheet_type(rid: &str, hs_state: u32, rel_type: Option<&str>) -> SheetType {
    if rid.is_empty() && hs_state == 2 {
        return SheetType::Vba;
    }
    match rel_type {
        None => SheetType::WorkSheet,
        Some(rel_type) if crate::xlsx::relationship_type_matches(rel_type, "worksheet") => {
            SheetType::WorkSheet
        }
        Some(rel_type) if crate::xlsx::relationship_type_matches(rel_type, "chartsheet") => {
            SheetType::ChartSheet
        }
        Some(rel_type) if crate::xlsx::relationship_type_matches(rel_type, "dialogsheet") => {
            SheetType::DialogSheet
        }
        Some(rel_type)
            if crate::xlsx::relationship_type_matches(rel_type, "macrosheet")
                || matches!(
                    rel_type,
                    "http://schemas.microsoft.com/office/2006/relationships/xlMacrosheet"
                        | "http://schemas.microsoft.com/office/2006/relationships/xlIntlMacrosheet"
                ) =>
        {
            SheetType::MacroSheet
        }
        Some(_) => SheetType::Vba,
    }
}

fn sheet_rels_path(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{path}.rels"),
    }
}

fn normalize_part_target(base: &str, target: &str) -> String {
    crate::xlsx::resolve_internal_relationship_part(base, target).unwrap_or_default()
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
    relationships: &[crate::xlsx::OoxmlRelationship],
) -> Vec<Vec<String>> {
    parse_supporting_link_rel_ids(workbook)
        .into_iter()
        .map(|rel_id| {
            let Some(relationship) = rel_id.as_ref().and_then(|id| {
                relationships.iter().find(|relationship| {
                    relationship.id == *id
                        && !relationship.external
                        && relationship.rel_type.as_deref().is_some_and(|rel_type| {
                            crate::xlsx::relationship_type_matches(rel_type, "externalLink")
                        })
                })
            }) else {
                return Vec::new();
            };
            let Some(path) = crate::xlsx::resolve_internal_relationship_part(
                "xl/workbook.bin",
                &relationship.target,
            ) else {
                return Vec::new();
            };
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
    page_setup_fit_counts: Option<(u32, u32)>,
    print_metadata: PrintMetadata,
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
    col_widths_256: BTreeMap<u16, u32>,
    imported_row_axis_measures: BTreeMap<u32, ImportedAxisMeasure>,
    imported_column_axis_measures: BTreeMap<u16, ImportedAxisMeasure>,
    imported_default_row_axis_measure: Option<ImportedAxisMeasure>,
    default_row_height: Option<f32>,
    default_col_width: Option<f32>,
    default_col_width_256: Option<u32>,
    ooxml_base_col_width: Option<u16>,
    row_formats: BTreeMap<u32, CellStyle>,
    col_formats: BTreeMap<u16, CellStyle>,
    blank_styles: BTreeMap<(u32, u16), CellStyle>,
    style_losses: Vec<StyleLoss>,
    hidden_rows: BTreeSet<u32>,
    hidden_cols: BTreeSet<u16>,
    default_rows_hidden: bool,
    explicit_visible_rows: BTreeSet<u32>,
    rich: BTreeMap<(u32, u16), Vec<crate::TextRun>>,
    /// Exact source font-size provenance parallel to `Sheet::cells`.
    xlsb_cell_font_sizes_pt: Vec<Option<u16>>,
    /// Exact source font-size provenance for retained row XF layers.
    xlsb_row_font_sizes_pt: BTreeMap<u32, u16>,
    /// Exact source font-size provenance for retained column XF layers.
    xlsb_col_font_sizes_pt: BTreeMap<u16, u16>,
}

#[derive(Clone, Copy)]
enum XlsbPageBreakAxis {
    Row,
    Column,
}

fn parse_sheet_comments(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    sheet_path: &str,
    sheet_rels_xml: &str,
) -> Vec<Comment> {
    if sheet_rels_xml.is_empty() {
        return Vec::new();
    }
    let target = match crate::xlsx::unique_internal_relationship_target(sheet_rels_xml, "comments")
    {
        crate::xlsx::RelationshipTarget::Internal(target) => target,
        crate::xlsx::RelationshipTarget::Missing | crate::xlsx::RelationshipTarget::Invalid => {
            return Vec::new()
        }
    };
    let path = normalize_part_target(sheet_path, &target);
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
    sheet_rels: &[crate::xlsx::OoxmlRelationship],
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
    let mut page_break_axis: Option<XlsbPageBreakAxis> = None;
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
                metadata.print_metadata.mark_source();
                if let Some(margins) = parse_page_margins(p) {
                    page_setup_mut(&mut metadata).margins = Some(margins);
                } else {
                    metadata
                        .print_metadata
                        .add_loss(PrintLossKind::UnsupportedProperty);
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
            BRT_BEGIN_RW_BRK => {
                metadata.print_metadata.mark_source();
                page_break_axis = Some(XlsbPageBreakAxis::Row);
            }
            BRT_END_RW_BRK => page_break_axis = None,
            BRT_BEGIN_COL_BRK => {
                metadata.print_metadata.mark_source();
                page_break_axis = Some(XlsbPageBreakAxis::Column);
            }
            BRT_END_COL_BRK => page_break_axis = None,
            BRT_BRK => parse_xlsb_page_break(p, page_break_axis, &mut metadata.print_metadata),
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
            BRT_WS_FMT_INFO => {
                apply_ws_fmt_info(p, &mut metadata);
            }
            BRT_COL_INFO => {
                apply_col_outline(p, &mut metadata, styles);
            }
            BRT_ROW_HDR => {
                if let Some(rr) = u32le(p, 0) {
                    row = rr;
                }
                apply_row_outline(p, &mut metadata, styles);
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
            BRT_CELL_BLANK => {
                let Some(col_u) = u32le(p, 0) else { continue };
                let col = col_u.min(MAX_XLSB_COL_INDEX) as u16;
                let style_idx = (u32::from(*p.get(4).unwrap_or(&0))
                    | (u32::from(*p.get(5).unwrap_or(&0)) << 8)
                    | (u32::from(*p.get(6).unwrap_or(&0)) << 16))
                    as usize;
                if metadata.blank_styles.len() < MAX_XLSB_BLANK_STYLES {
                    if let Some(style) = styles.cell_style(style_idx) {
                        if style != &CellStyle::default() {
                            metadata.blank_styles.insert((row, col), style.clone());
                        }
                    } else if styles.has_source_styles || style_idx != 0 {
                        add_style_loss(
                            &mut metadata.style_losses,
                            StyleLossKind::MissingReference,
                            1,
                        );
                    }
                } else {
                    add_style_loss(&mut metadata.style_losses, StyleLossKind::LimitExceeded, 1);
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
                    | (u32::from(*p.get(5).unwrap_or(&0)) << 8)
                    | (u32::from(*p.get(6).unwrap_or(&0)) << 16))
                    as usize;
                if styles.cell_style(style_idx).is_none()
                    && (styles.has_source_styles || style_idx != 0)
                {
                    add_style_loss(
                        &mut metadata.style_losses,
                        StyleLossKind::MissingReference,
                        1,
                    );
                }
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
                    &mut metadata.xlsb_cell_font_sizes_pt,
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
        // [MS-XLSB] BrtWsProp.fFitToPage is the authoritative selector between
        // percentage scaling and fit-to-pages; BrtPageSetup retains both sets
        // of values regardless of the selected mode.
        metadata.print_metadata.set_fit_to_page(flags & 0x0100 != 0);
        apply_page_setup_fit_counts(metadata);
    }
}

fn apply_row_outline(p: &[u8], metadata: &mut SheetReadMetadata, styles: &Styles) {
    let (Some(row), Some(height_twips), Some(flags)) = (u32le(p, 0), u16le(p, 8), u16le(p, 10))
    else {
        return;
    };
    if row > MAX_XLSB_ROW_INDEX {
        return;
    }
    // [MS-XLSB] §2.4.770: miyRw is authoritative only when fUnsynced is set.
    if (1..=MAX_XLSB_ROW_HEIGHT_TWIPS).contains(&height_twips) && flags & (1 << 13) != 0 {
        metadata
            .row_heights
            .insert(row, f32::from(height_twips) / 20.0);
        metadata
            .imported_row_axis_measures
            .insert(row, ImportedAxisMeasure::Twips(u32::from(height_twips)));
    }
    if flags & (1 << 12) != 0 {
        metadata.hidden_rows.insert(row);
        metadata.explicit_visible_rows.remove(&row);
    } else {
        metadata.hidden_rows.remove(&row);
        metadata.explicit_visible_rows.insert(row);
    }
    let level = ((flags >> 8) & 0x07) as u8;
    if level > 0 {
        metadata.row_outline.insert(row, level);
    }
    if flags & (1 << 11) != 0 {
        metadata.collapsed_rows.insert(row);
    }
    if flags & (1 << 14) != 0 {
        if let Some(style_index) = u32le(p, 4).and_then(|value| usize::try_from(value).ok()) {
            if let Some(style) = styles.cell_style(style_index) {
                metadata.row_formats.insert(row, style.clone());
                if let Some(points) = styles.xlsb_cell_font_size_pt(style_index) {
                    metadata.xlsb_row_font_sizes_pt.insert(row, points);
                } else {
                    metadata.xlsb_row_font_sizes_pt.remove(&row);
                }
            } else if styles.has_source_styles || style_index != 0 {
                metadata.xlsb_row_font_sizes_pt.remove(&row);
                add_style_loss(
                    &mut metadata.style_losses,
                    StyleLossKind::MissingReference,
                    1,
                );
            }
        }
    }
}

fn apply_col_outline(p: &[u8], metadata: &mut SheetReadMetadata, styles: &Styles) {
    let (Some(first), Some(last), Some(width_256), Some(style_index), Some(flags)) = (
        u32le(p, 0),
        u32le(p, 4),
        u32le(p, 8),
        u32le(p, 12).and_then(|value| usize::try_from(value).ok()),
        u16le(p, 16),
    ) else {
        return;
    };
    let level = ((flags >> 8) & 0x07) as u8;
    if first > last || first > MAX_XLSB_COL_INDEX {
        return;
    }
    let column_style = styles.cell_style(style_index).cloned();
    let column_font_size = styles.xlsb_cell_font_size_pt(style_index);
    if column_style.is_none() && (styles.has_source_styles || style_index != 0) {
        add_style_loss(
            &mut metadata.style_losses,
            StyleLossKind::MissingReference,
            1,
        );
    }
    for col in first..=last.min(MAX_XLSB_COL_INDEX) {
        let col = col as u16;
        if (1..=65_535).contains(&width_256) {
            metadata.col_widths.insert(col, width_256 as f32 / 256.0);
            metadata.col_widths_256.insert(col, width_256);
            metadata
                .imported_column_axis_measures
                .insert(col, ImportedAxisMeasure::DigitWidth256(width_256));
        }
        if flags & 0x01 != 0 {
            metadata.hidden_cols.insert(col);
        }
        if level > 0 {
            metadata.col_outline.insert(col, level);
        }
        if let Some(style) = column_style.as_ref() {
            metadata.col_formats.insert(col, style.clone());
            if let Some(points) = column_font_size {
                metadata.xlsb_col_font_sizes_pt.insert(col, points);
            } else {
                metadata.xlsb_col_font_sizes_pt.remove(&col);
            }
        }
    }
}

fn apply_ws_fmt_info(p: &[u8], metadata: &mut SheetReadMetadata) {
    let (
        Some(default_width_256),
        Some(base_characters),
        Some(default_row_height_twips),
        Some(flags),
    ) = (u32le(p, 0), u16le(p, 4), u16le(p, 6), u16le(p, 8))
    else {
        return;
    };
    metadata.default_col_width = None;
    metadata.default_col_width_256 = None;
    metadata.ooxml_base_col_width = None;
    metadata.default_row_height = None;
    metadata.imported_default_row_axis_measure = None;
    metadata.default_rows_hidden = flags & 0x0002 != 0;
    if default_width_256 == u32::MAX {
        if base_characters <= 255 {
            metadata.ooxml_base_col_width = Some(base_characters);
        }
    } else if default_width_256 <= 65_535 {
        metadata.default_col_width = Some(default_width_256 as f32 / 256.0);
        metadata.default_col_width_256 = Some(default_width_256);
    }
    // [MS-XLSB] §2.4.873: miyDefRwHeight is authoritative only when
    // fUnsynced is set. Unlike BrtRowHdr.miyRw, its unsigned two-byte field
    // has no 8192-twip ceiling and can retain an explicit zero.
    if flags & 0x0001 != 0 {
        metadata.default_row_height = Some(f32::from(default_row_height_twips) / 20.0);
        metadata.imported_default_row_axis_measure = Some(ImportedAxisMeasure::Twips(u32::from(
            default_row_height_twips,
        )));
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
    sheet_rels_xml: &str,
    table_rel_ids: &[String],
) -> Vec<Table> {
    if sheet_rels_xml.is_empty() {
        return Vec::new();
    }

    let Some(relationships) = crate::xlsx::parse_ooxml_relationships(sheet_rels_xml) else {
        return Vec::new();
    };
    let table_relationships: Vec<_> = relationships
        .iter()
        .filter(|relationship| {
            relationship
                .rel_type
                .as_deref()
                .is_some_and(|rel_type| crate::xlsx::relationship_type_matches(rel_type, "table"))
        })
        .collect();
    if table_relationships
        .iter()
        .any(|relationship| relationship.external)
    {
        return Vec::new();
    }
    let mut rel_ids: Vec<&str> = table_rel_ids
        .iter()
        .map(String::as_str)
        .filter(|id| {
            table_relationships
                .iter()
                .any(|relationship| relationship.id == *id)
        })
        .collect();
    if rel_ids.is_empty() {
        rel_ids.extend(
            table_relationships
                .iter()
                .map(|relationship| relationship.id.as_str()),
        );
    }
    let mut seen = HashSet::new();
    rel_ids
        .into_iter()
        .filter(|id| seen.insert(*id))
        .filter_map(|id| {
            table_relationships
                .iter()
                .find(|relationship| relationship.id == id)
                .map(|relationship| relationship.target.as_str())
        })
        .map(|target| normalize_part_target(sheet_path, target))
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
    let Some(flags) = u16le(p, 0) else {
        metadata
            .print_metadata
            .add_loss(PrintLossKind::UnsupportedProperty);
        return;
    };
    metadata
        .print_metadata
        .set_center_horizontally(flags & 0x0001 != 0);
    metadata
        .print_metadata
        .set_center_vertically(flags & 0x0002 != 0);
    metadata
        .print_metadata
        .set_print_headings(flags & 0x0004 != 0);
    metadata
        .print_metadata
        .set_print_gridlines(flags & 0x0008 != 0);
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
    let (Some(page_start), Some(fit_width), Some(fit_height), Some(flags)) =
        (i32le(p, 20), u32le(p, 24), u32le(p, 28), u16le(p, 32))
    else {
        metadata
            .print_metadata
            .add_loss(PrintLossKind::UnsupportedProperty);
        return;
    };

    metadata
        .print_metadata
        .set_page_order(if flags & 0x0001 != 0 {
            PrintPageOrder::OverThenDown
        } else {
            PrintPageOrder::DownThenOver
        });

    metadata.page_setup_fit_counts = Some((fit_width, fit_height));
    {
        let ps = page_setup_mut(metadata);
        ps.paper_size = nonzero_u32_as_u16(p, 0);
        ps.scale = nonzero_u32_as_u16(p, 4);
        if flags & 0x0040 == 0 {
            ps.landscape = flags & 0x0002 != 0;
        }
        if flags & 0x0080 != 0 && page_start > 0 {
            ps.first_page_number = u16::try_from(page_start).ok();
        }
    }
    apply_page_setup_fit_counts(metadata);
}

fn apply_page_setup_fit_counts(metadata: &mut SheetReadMetadata) {
    let Some((fit_width, fit_height)) = metadata.page_setup_fit_counts else {
        return;
    };
    let preserve_zero = metadata.print_metadata.fit_to_page() == Some(true);
    let ps = page_setup_mut(metadata);
    ps.fit_to_width = xlsb_fit_count_as_u16(fit_width, preserve_zero);
    ps.fit_to_height = xlsb_fit_count_as_u16(fit_height, preserve_zero);
}

fn xlsb_fit_count_as_u16(value: u32, preserve_zero: bool) -> Option<u16> {
    if value == 0 && !preserve_zero {
        return None;
    }
    u16::try_from(value).ok()
}

fn nonzero_u32_as_u16(p: &[u8], offset: usize) -> Option<u16> {
    let value = u32le(p, offset)?;
    if value == 0 {
        return None;
    }
    u16::try_from(value).ok()
}

fn parse_header_footer(p: &[u8], metadata: &mut SheetReadMetadata) {
    let Some(flags) = u16le(p, 0) else {
        metadata
            .print_metadata
            .add_loss(PrintLossKind::MalformedHeaderFooter);
        return;
    };
    metadata.print_metadata.set_header_footer_flag(
        Some(flags & 0x0001 != 0),
        Some(flags & 0x0002 != 0),
        Some(flags & 0x0004 != 0),
        Some(flags & 0x0008 != 0),
    );
    let mut offset = 2usize;
    let mut fields = Vec::with_capacity(6);
    for kind in [
        HeaderFooterKind::OddHeader,
        HeaderFooterKind::OddFooter,
        HeaderFooterKind::EvenHeader,
        HeaderFooterKind::EvenFooter,
        HeaderFooterKind::FirstHeader,
        HeaderFooterKind::FirstFooter,
    ] {
        let Some((value, used)) = nullable_wide_opt(p, offset) else {
            metadata
                .print_metadata
                .add_loss(PrintLossKind::MalformedHeaderFooter);
            return;
        };
        if let Some(value) = value.as_ref() {
            metadata
                .print_metadata
                .set_header_footer(kind, value.clone());
        }
        fields.push(value);
        let Some(next) = offset.checked_add(used) else {
            metadata
                .print_metadata
                .add_loss(PrintLossKind::MalformedHeaderFooter);
            return;
        };
        offset = next;
    }
    if let Some(header) = fields.first().and_then(|value| value.as_ref()) {
        if !header.is_empty() {
            page_setup_mut(metadata).header = Some(header.clone());
        }
    }
    if let Some(footer) = fields.get(1).and_then(|value| value.as_ref()) {
        if !footer.is_empty() {
            page_setup_mut(metadata).footer = Some(footer.clone());
        }
    }
}

fn parse_xlsb_page_break(p: &[u8], axis: Option<XlsbPageBreakAxis>, metadata: &mut PrintMetadata) {
    let (Some(index), Some(manual)) = (u32le(p, 0), u32le(p, 12)) else {
        metadata.add_loss(PrintLossKind::InvalidPageBreak);
        return;
    };
    match manual {
        0 => return,
        1 => {}
        _ => {
            metadata.add_loss(PrintLossKind::InvalidPageBreak);
            return;
        }
    }
    match axis {
        Some(XlsbPageBreakAxis::Row) => metadata.push_manual_row_break(index),
        Some(XlsbPageBreakAxis::Column) => match u16::try_from(index) {
            Ok(col) => metadata.push_manual_col_break(col),
            Err(_) => metadata.add_loss(PrintLossKind::InvalidPageBreak),
        },
        None => metadata.add_loss(PrintLossKind::InvalidPageBreak),
    }
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

fn parse_hlink(p: &[u8], sheet_rels: &[crate::xlsx::OoxmlRelationship]) -> Hyperlinks {
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

    let relationship = sheet_rels.iter().find(|relationship| {
        relationship.id == rel_id
            && relationship.rel_type.as_deref().is_some_and(|rel_type| {
                crate::xlsx::relationship_type_matches(rel_type, "hyperlink")
            })
    });
    let target = match (relationship, location.is_empty()) {
        (Some(relationship), true) => relationship.target.clone(),
        (Some(relationship), false) => format!("{}#{location}", relationship.target),
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

#[cfg(test)]
mod tests;
