//! `.xlsx` (SpreadsheetML / OOXML) reading.
//!
//! A `.xlsx` is a ZIP of XML parts: a workbook part (usually
//! `xl/workbook.xml`, but discoverable through `_rels/.rels`), workbook
//! relationships (relationship-id → worksheet path), shared strings, styles
//! (number formats, for date detection), and worksheet parts.
//!
//! The number-format classification and serial-date arithmetic are shared with
//! the `.xls` path ([`crate::format`]), so dates/percentages render identically.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;

use quick_xml::events::{BytesRef, Event};
use quick_xml::{Reader, XmlVersion};

use crate::error::{Error, Result};
use crate::model::{
    CellStyleOverlay, OoxmlImplicitColumnWidth, TableStyleApplication, TableStyleDefinition,
    TableStyleRegion,
};
use crate::{
    format, Alignment, Border, BorderStyle, Cell, CellEntry, CellProtection, CellStyle, CfRule,
    Chart, ChartBarDirection, ChartCachedPoint, ChartKind, ChartMarkerSymbol, ChartSeriesCache,
    ChartSeriesStyle, ChartSeriesStyleLossKind, ChartUnsupportedReason, Color, Comment, CondFormat,
    ConditionalFormatMetadata, DataValidation, DocProperties, DrawingAnchorBehavior, DrawingCrop,
    DrawingMetadata, DrawingObjectKind, DvKind, DvOp, Fill, Font, FormatPattern, FormatScript,
    HAlign, HeaderFooterKind, Image, ImageFmt, PageSetup, PrintLossKind, PrintMetadata,
    PrintPageOrder, ProtectionOptions, Series, Sheet, SheetType, Sparkline, SparklineKind,
    StyleFidelity, StyleLoss, StyleLossKind, Table, VAlign, Workbook,
};

/// Detect the ZIP/OOXML magic (`PK\x03\x04`).
pub(crate) fn is_xlsx(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
}

pub(crate) fn open(bytes: &[u8]) -> Result<Workbook> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|_| Error::Zip("not a valid spreadsheet ZIP container"))?;
    crate::ziputil::validate_compression(&mut zip)?;

    let mut workbook_path =
        office_document_path(&mut zip).unwrap_or_else(|| "xl/workbook.xml".to_string());
    let workbook_xml = match part(&mut zip, &workbook_path) {
        Some(xml) => xml,
        None if workbook_path != "xl/workbook.xml" => {
            workbook_path = "xl/workbook.xml".to_string();
            part(&mut zip, &workbook_path).ok_or(Error::MissingWorkbook)?
        }
        None => return Err(Error::MissingWorkbook),
    };
    let workbook_rels_xml = part(&mut zip, &sheet_rels_path(&workbook_path)).unwrap_or_default();
    let rels = parse_rels(&workbook_rels_xml);
    let rel_types = parse_rel_types(&workbook_rels_xml);
    let shared_xml =
        workbook_related_part(&mut zip, &workbook_path, &rels, &rel_types, "sharedStrings")
            .or_else(|| {
                part(
                    &mut zip,
                    &normalize_part_target(&workbook_path, "sharedStrings.xml"),
                )
            })
            .unwrap_or_default();
    let theme = workbook_related_part(&mut zip, &workbook_path, &rels, &rel_types, "theme")
        .or_else(|| {
            part(
                &mut zip,
                &normalize_part_target(&workbook_path, "theme/theme1.xml"),
            )
        })
        .map(|s| parse_theme(&s))
        .unwrap_or_default();
    let styles = workbook_related_part(&mut zip, &workbook_path, &rels, &rel_types, "styles")
        .or_else(|| {
            part(
                &mut zip,
                &normalize_part_target(&workbook_path, "styles.xml"),
            )
        })
        .map(|s| parse_styles(&s, &theme))
        .unwrap_or_default();
    let shared = parse_shared_strings(&shared_xml, &theme, &styles.indexed_colors);
    let parsed = parse_workbook(&workbook_xml);
    let ParsedWorkbook {
        sheets: sheet_refs,
        date1904,
        structure_protected,
        active_sheet,
        defined_names,
        local_defined_names,
        sheet_defined_names,
    } = parsed;
    let properties = parse_doc_properties(
        part(&mut zip, "docProps/core.xml").as_deref(),
        part(&mut zip, "docProps/app.xml").as_deref(),
    );

    // Per-workbook text budget (shared across sheets) — see MAX_TEXT_BYTES.
    let mut budget = crate::MAX_TEXT_BYTES;
    let mut sheets = Vec::with_capacity(sheet_refs.len().min(1 << 16));
    let mut tab_selected_sheet = None;
    for (
        sheet_idx,
        SheetRef {
            name,
            rid,
            visibility,
        },
    ) in sheet_refs.into_iter().enumerate()
    {
        let has_relationship = rels.contains_key(&rid);
        let target = rels.get(&rid).cloned().unwrap_or_default();
        let path = normalize_part_target(&workbook_path, &target);
        let sheet_type = if rid.is_empty() || !has_relationship {
            SheetType::Vba
        } else {
            let rel_kind = rel_types
                .get(&rid)
                .and_then(|t| t.rsplit('/').next())
                .unwrap_or("worksheet");
            match rel_kind.to_ascii_lowercase().as_str() {
                "chartsheet" => SheetType::ChartSheet,
                "dialogsheet" => SheetType::DialogSheet,
                "macrosheet" | "xlmacrosheet" | "xlintlmacrosheet" => SheetType::MacroSheet,
                _ => SheetType::WorkSheet,
            }
        };
        let is_worksheet = sheet_type == SheetType::WorkSheet;
        let parsed_sheet = if is_worksheet {
            part_raw(&mut zip, &path)
                .map(|s| parse_sheet(&s, &shared, &styles, &theme, date1904, &mut budget))
                .unwrap_or_default()
        } else {
            ParsedSheet::default()
        };
        let ParsedSheet {
            cells,
            direct_cell_formats,
            rich,
            merges,
            hyperlink_refs,
            freeze,
            mut autofilter,
            data_validations,
            cond_formats,
            cond_format_metadata,
            mut page_setup,
            mut print_metadata,
            sparklines,
            tab_color,
            print_gridlines,
            print_headings,
            row_outline,
            col_outline,
            col_widths,
            row_heights,
            col_formats,
            row_formats,
            hidden_cols,
            hidden_rows,
            default_row_height,
            default_col_width,
            base_col_width,
            collapsed_rows,
            outline_summary_below,
            outline_summary_right,
            protect,
            protect_options,
            hide_gridlines,
            zoom,
            show_headers,
            right_to_left,
            tab_selected,
        } = parsed_sheet;
        let ooxml_implicit_col_width = if !is_worksheet || default_col_width.is_some() {
            OoxmlImplicitColumnWidth::None
        } else if let Some(chars) = base_col_width {
            OoxmlImplicitColumnWidth::BaseCharacters(chars)
        } else {
            OoxmlImplicitColumnWidth::ApplicationDefault
        };
        if tab_selected && tab_selected_sheet.is_none() {
            tab_selected_sheet = Some(sheet_idx);
        }
        // Resolve each `<hyperlink ref r:id>` through the worksheet's own rels
        // (`xl/worksheets/_rels/sheetN.xml.rels`), where the relationship `Target`
        // is the external URL.
        // The worksheet rels (`xl/worksheets/_rels/sheetN.xml.rels`) carry both the
        // hyperlink URLs (resolved by `r:id`) and the `comments{N}.xml` target
        // (resolved by relationship Type). Read the part once and reuse it.
        let sheet_rels_xml = is_worksheet
            .then(|| part(&mut zip, &sheet_rels_path(&path)))
            .flatten();
        let read_hyperlinks = if hyperlink_refs.is_empty() {
            Vec::new()
        } else {
            let sheet_rels = sheet_rels_xml
                .as_deref()
                .map(parse_rels)
                .unwrap_or_default();
            hyperlink_refs
                .into_iter()
                .filter_map(|(row, col, rid)| {
                    sheet_rels.get(&rid).map(|url| (row, col, url.clone()))
                })
                .collect()
        };
        // Resolve a `comments{N}.xml` part (relationship Type `.../comments`) and
        // parse it into the authoring `comments` storage (round-trip friendly).
        let comments = sheet_rels_xml
            .as_deref()
            .and_then(comments_target)
            .map(|target| normalize_part_target(&path, &target))
            .and_then(|p| part(&mut zip, &p))
            .map(|s| parse_comments(&s))
            .unwrap_or_default();
        // Resolve every `table{N}.xml` part (relationship Type `.../table`) and
        // parse each into the authoring `tables` storage (round-trip friendly).
        let parsed_tables: Vec<ParsedTable> = sheet_rels_xml
            .as_deref()
            .map(table_targets)
            .unwrap_or_default()
            .into_iter()
            .map(|target| normalize_part_target(&path, &target))
            .filter_map(|p| part(&mut zip, &p))
            .filter_map(|s| parse_table(&s))
            .collect();
        let tables = parsed_tables
            .iter()
            .map(|parsed| parsed.table.clone())
            .collect::<Vec<_>>();
        let mut table_header_formats = BTreeMap::new();
        let mut table_region_formats = BTreeMap::new();
        let mut table_style_losses = styles.losses.clone();
        for parsed in parsed_tables {
            for loss in parsed.losses {
                add_drawing_loss(&mut table_style_losses, loss.kind, loss.occurrences);
            }
            let Some(style_name) = parsed.table.style.as_deref() else {
                continue;
            };
            let Some(table_style) = styles.table_style(style_name, &theme) else {
                add_drawing_loss(&mut table_style_losses, StyleLossKind::MissingReference, 1);
                continue;
            };
            for loss in table_style.losses {
                add_drawing_loss(&mut table_style_losses, loss.kind, loss.occurrences);
            }
            if let Some(header) = table_style
                .definition
                .get(TableStyleRegion::HeaderRow)
                .map(|element| element.style.clone())
            {
                table_header_formats.insert(parsed.table.name.clone(), header);
            }
            let mut application = parsed.application;
            application.definition = table_style.definition;
            table_region_formats.insert(parsed.table.name, application);
        }
        let (images, charts, drawing_metadata, mut drawing_losses) =
            read_sheet_drawings(&mut zip, &path, sheet_rels_xml.as_deref(), &theme);
        for loss in table_style_losses {
            add_drawing_loss(&mut drawing_losses, loss.kind, loss.occurrences);
        }
        apply_sheet_defined_names(
            &mut page_setup,
            &mut print_metadata,
            &mut autofilter,
            sheet_defined_names
                .iter()
                .filter(|name| name.local_sheet_id == sheet_idx),
        );
        sheets.push(Sheet {
            name,
            is_worksheet,
            style_fidelity: StyleFidelity::Partial,
            sheet_type: Some(sheet_type),
            cells,
            rich,
            read_merges: merges,
            read_hyperlinks,
            comments,
            tables,
            table_header_formats,
            table_region_formats,
            direct_cell_formats,
            images,
            charts,
            drawing_metadata,
            style_losses: drawing_losses,
            freeze,
            autofilter,
            page_setup,
            print_metadata,
            data_validations,
            cond_formats,
            cond_format_metadata,
            sparklines,
            tab_color,
            print_gridlines,
            print_headings,
            row_outline,
            col_outline,
            col_widths,
            row_heights,
            col_formats,
            row_formats,
            default_format: styles.cell_styles.first().cloned(),
            hidden_cols,
            hidden_rows,
            default_row_height,
            default_col_width,
            ooxml_implicit_col_width,
            collapsed_rows,
            outline_summary_below: outline_summary_below.unwrap_or(true),
            outline_summary_right: outline_summary_right.unwrap_or(true),
            protect,
            protect_options,
            hide_gridlines,
            zoom,
            show_headers,
            right_to_left,
            hidden: visibility == Visibility::Hidden,
            very_hidden: visibility == Visibility::VeryHidden,
            ..Default::default()
        });
    }
    Ok(Workbook {
        sheets,
        date1904,
        protect_structure: structure_protected,
        active_sheet: active_sheet.or(tab_selected_sheet).unwrap_or_default(),
        text_truncated: budget == 0,
        container_parse_mode: crate::ContainerParseMode::Primary,
        properties,
        defined_names,
        local_defined_names,
        ..Default::default()
    })
}

/// Read a ZIP entry to a UTF-8 string, if present. Capped to guard against a
/// zip bomb (a tiny entry that decompresses to gigabytes).
fn part(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, name: &str) -> Option<String> {
    let text = part_raw(zip, name)?;
    crate::xml_reference_work_within_budget(&text).then_some(text)
}

fn part_raw(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, name: &str) -> Option<String> {
    const MAX_PART: u64 = 256 << 20; // 256 MiB per entry
    let idx = part_index(zip, name)?;
    let f = zip.by_index(idx).ok()?;
    let mut s = String::new();
    f.take(MAX_PART).read_to_string(&mut s).ok()?;
    Some(s)
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
        if canonical_part_name(file.name()) == wanted {
            return Some(idx);
        }
    }
    None
}

fn canonical_part_name(name: &str) -> String {
    name.replace('\\', "/").trim_start_matches('/').to_string()
}

fn office_document_path(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>) -> Option<String> {
    let root_rels = part(zip, "_rels/.rels")?;
    let rels = parse_rels(&root_rels);
    let rel_types = parse_rel_types(&root_rels);
    rel_types.into_iter().find_map(|(id, ty)| {
        if ty.rsplit('/').next() == Some("officeDocument") {
            rels.get(&id).map(|target| canonical_part_name(target))
        } else {
            None
        }
    })
}

fn workbook_related_part(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    workbook_path: &str,
    rels: &HashMap<String, String>,
    rel_types: &HashMap<String, String>,
    rel_kind: &str,
) -> Option<String> {
    let path = rel_types.iter().find_map(|(id, ty)| {
        if ty.rsplit('/').next() == Some(rel_kind) {
            rels.get(id)
                .map(|target| normalize_part_target(workbook_path, target))
        } else {
            None
        }
    })?;
    part(zip, &path)
}

/// The rels path for a source part: `xl/worksheets/sheet1.xml` →
/// `xl/worksheets/_rels/sheet1.xml.rels`. Splits at the final `/` and inserts
/// the `_rels/` segment before the file name.
fn sheet_rels_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    match path.rfind('/') {
        Some(i) => format!("{}/_rels/{}.rels", &path[..i], &path[i + 1..]),
        None => format!("_rels/{path}.rels"),
    }
}

const MAX_XLSX_STYLE_RECORDS: usize = 65_536;
const MAX_XLSX_CUSTOM_NUMBER_FORMATS: usize = 65_536;
const MAX_XLSX_FORMAT_CODE_BYTES: usize = 4_096;
const MAX_XLSX_INDEXED_COLORS: usize = 256;

/// Per-style number format, derived from `styles.xml`.
#[derive(Default)]
struct Styles {
    /// `numFmtId` per `cellXfs` style index.
    xf_numfmt: Vec<u16>,
    /// Custom `formatCode` strings keyed by `numFmtId`.
    custom: HashMap<u16, String>,
    /// Custom OOXML indexed color table from `<colors><indexedColors>`.
    indexed_colors: Vec<Color>,
    /// Full differential styles and typed parse losses per `dxfs` index.
    differential_styles: Vec<DifferentialStyle>,
    /// Common public style subset per `cellXfs` index.
    cell_styles: Vec<CellStyle>,
    /// Sparse direct-format overlays per `cellXfs` style index.
    cell_style_overlays: Vec<CellStyleOverlay>,
    /// Imported custom table region styles keyed by `<tableStyle name>`.
    table_styles: HashMap<String, ParsedTableStyle>,
    /// Workbook-global style-table truncation and parse losses.
    losses: Vec<StyleLoss>,
}

impl Styles {
    fn format_id(&self, style_idx: usize) -> u16 {
        self.xf_numfmt.get(style_idx).copied().unwrap_or(0)
    }

    fn kind(&self, style_idx: usize) -> format::Kind {
        let numfmt_id = self.format_id(style_idx);
        format::classify(numfmt_id, self.custom.get(&numfmt_id).map(String::as_str))
    }

    fn custom_format(&self, style_idx: usize) -> Option<&str> {
        let numfmt_id = self.xf_numfmt.get(style_idx).copied()?;
        self.custom.get(&numfmt_id).map(String::as_str)
    }

    fn render_text(&self, style_idx: usize, value: &str) -> String {
        self.custom_format(style_idx).map_or_else(
            || value.to_string(),
            |code| format::render_text_format(value, code),
        )
    }

    fn differential_style(&self, dxf_id: usize) -> Option<&DifferentialStyle> {
        self.differential_styles.get(dxf_id)
    }

    fn cell_style(&self, style_idx: usize) -> Option<&CellStyle> {
        self.cell_styles.get(style_idx)
    }

    fn cell_style_overlay(&self, style_idx: usize) -> Option<&CellStyleOverlay> {
        self.cell_style_overlays.get(style_idx)
    }

    fn table_style(&self, name: &str, theme: &ThemeColors) -> Option<ParsedTableStyle> {
        self.table_styles
            .get(name)
            .cloned()
            .or_else(|| built_in_table_style(name, theme))
    }
}

#[derive(Debug, Clone, Default)]
struct DifferentialStyle {
    style: CellStyle,
    losses: Vec<StyleLoss>,
}

#[derive(Debug, Clone, Default)]
struct ParsedTableStyle {
    definition: TableStyleDefinition,
    losses: Vec<StyleLoss>,
}

fn local(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

fn attr(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if local(a.key.as_ref()) == key {
            a.decoded_and_normalized_value_with(
                XmlVersion::Implicit1_0,
                e.decoder(),
                1,
                quick_xml::escape::resolve_xml_entity,
            )
            .ok()
            .map(|v| v.into_owned())
        } else {
            None
        }
    })
}

fn parse_color(value: &str) -> Option<Color> {
    let rgb = value.trim().strip_prefix('#').unwrap_or(value.trim());
    let rgb = match rgb.len() {
        8 => &rgb[2..],
        6 => rgb,
        _ => return None,
    };
    if !rgb.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let red = u8::from_str_radix(&rgb[0..2], 16).ok()?;
    let green = u8::from_str_radix(&rgb[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&rgb[4..6], 16).ok()?;
    Some(Color::rgb(red, green, blue))
}

const OOXML_DEFAULT_INDEXED_COLORS: [Color; 64] = [
    Color::rgb(0x00, 0x00, 0x00),
    Color::rgb(0xFF, 0xFF, 0xFF),
    Color::rgb(0xFF, 0x00, 0x00),
    Color::rgb(0x00, 0xFF, 0x00),
    Color::rgb(0x00, 0x00, 0xFF),
    Color::rgb(0xFF, 0xFF, 0x00),
    Color::rgb(0xFF, 0x00, 0xFF),
    Color::rgb(0x00, 0xFF, 0xFF),
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

#[derive(Clone, Copy, Default)]
struct ThemeColors {
    colors: [Option<Color>; 12],
}

const OFFICE_CHART_ACCENTS: [Color; 6] = [
    Color::rgb(68, 114, 196),
    Color::rgb(237, 125, 49),
    Color::rgb(165, 165, 165),
    Color::rgb(255, 192, 0),
    Color::rgb(91, 155, 213),
    Color::rgb(112, 173, 71),
];

impl ThemeColors {
    fn color(&self, idx: usize, tint: Option<f64>) -> Option<Color> {
        let color = self.colors.get(idx).copied().flatten()?;
        Some(apply_optional_tint(color, tint))
    }

    fn chart_palette(&self) -> Vec<Color> {
        (0..OFFICE_CHART_ACCENTS.len())
            .map(|index| self.colors[index + 4].unwrap_or(OFFICE_CHART_ACCENTS[index]))
            .collect()
    }
}

fn theme_color_slot(name: &[u8]) -> Option<usize> {
    match name {
        b"lt1" => Some(0),
        b"dk1" => Some(1),
        b"lt2" => Some(2),
        b"dk2" => Some(3),
        b"accent1" => Some(4),
        b"accent2" => Some(5),
        b"accent3" => Some(6),
        b"accent4" => Some(7),
        b"accent5" => Some(8),
        b"accent6" => Some(9),
        b"hlink" => Some(10),
        b"folHlink" => Some(11),
        _ => None,
    }
}

fn parse_theme(xml: &str) -> ThemeColors {
    let mut r = Reader::from_str(xml);
    let mut theme = ThemeColors::default();
    let mut slot = None;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local(qname.as_ref());
                if let Some(next_slot) = theme_color_slot(name) {
                    slot = Some(next_slot);
                } else if name == b"srgbClr" {
                    if let (Some(slot), Some(color)) =
                        (slot, attr(&e, b"val").as_deref().and_then(parse_color))
                    {
                        theme.colors[slot] = Some(color);
                    }
                } else if name == b"sysClr" {
                    if let (Some(slot), Some(color)) =
                        (slot, attr(&e, b"lastClr").as_deref().and_then(parse_color))
                    {
                        theme.colors[slot] = Some(color);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                if theme_color_slot(local(qname.as_ref())).is_some() {
                    slot = None;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    theme
}

fn apply_tint(color: Color, tint: f64) -> Color {
    fn channel(value: u8, tint: f64) -> u8 {
        let value = f64::from(value);
        let tinted = if tint < 0.0 {
            value * (1.0 + tint)
        } else {
            value * (1.0 - tint) + 255.0 * tint
        };
        tinted.round().clamp(0.0, 255.0) as u8
    }

    let [red, green, blue] = color.as_rgb();
    Color::rgb(
        channel(red, tint),
        channel(green, tint),
        channel(blue, tint),
    )
}

fn apply_optional_tint(color: Color, tint: Option<f64>) -> Color {
    match tint {
        Some(tint) if tint.is_finite() => apply_tint(color, tint),
        _ => color,
    }
}

fn indexed_color(idx: usize, indexed_colors: &[Color], tint: Option<f64>) -> Option<Color> {
    let color = indexed_colors
        .get(idx)
        .copied()
        .or_else(|| OOXML_DEFAULT_INDEXED_COLORS.get(idx).copied())?;
    Some(apply_optional_tint(color, tint))
}

fn color_attr(
    e: &quick_xml::events::BytesStart<'_>,
    theme: &ThemeColors,
    indexed_colors: &[Color],
) -> Option<Color> {
    attr(e, b"rgb")
        .as_deref()
        .and_then(parse_color)
        .or_else(|| {
            let idx = attr(e, b"theme").and_then(|s| s.parse::<usize>().ok())?;
            let tint = attr(e, b"tint").and_then(|s| s.parse::<f64>().ok());
            theme.color(idx, tint)
        })
        .or_else(|| {
            let idx = attr(e, b"indexed").and_then(|s| s.parse::<usize>().ok())?;
            let tint = attr(e, b"tint").and_then(|s| s.parse::<f64>().ok());
            indexed_color(idx, indexed_colors, tint)
        })
}

fn parse_indexed_colors(xml: &str, losses: &mut Vec<StyleLoss>) -> Vec<Color> {
    let mut r = Reader::from_str(xml);
    let mut colors = Vec::new();
    let mut in_indexed_colors = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"indexedColors" => in_indexed_colors = true,
                b"rgbColor" if in_indexed_colors => {
                    if let Some(color) = attr(&e, b"rgb").as_deref().and_then(parse_color) {
                        if colors.len() < MAX_XLSX_INDEXED_COLORS {
                            colors.push(color);
                        } else {
                            add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(e)) if in_indexed_colors && local(e.name().as_ref()) == b"rgbColor" => {
                if let Some(color) = attr(&e, b"rgb").as_deref().and_then(parse_color) {
                    if colors.len() < MAX_XLSX_INDEXED_COLORS {
                        colors.push(color);
                    } else {
                        add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                    }
                }
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"indexedColors" => {
                in_indexed_colors = false;
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    colors
}

fn text_of(e: &quick_xml::events::BytesText<'_>) -> String {
    e.decode().map(|c| c.into_owned()).unwrap_or_default()
}

fn with_general_ref_text(reference: &BytesRef<'_>, mut append: impl FnMut(&str)) {
    match reference.resolve_char_ref() {
        Ok(Some(ch)) if is_xml_10_char(ch) => {
            let mut encoded = [0u8; 4];
            append(ch.encode_utf8(&mut encoded));
        }
        Ok(None) => {
            if let Ok(name) = reference.decode() {
                if let Some(value) = quick_xml::escape::resolve_xml_entity(&name) {
                    append(value);
                    return;
                }
            }
            append_raw_general_ref(reference, append);
        }
        Ok(Some(_)) | Err(_) => append_raw_general_ref(reference, append),
    }
}

fn append_raw_general_ref(reference: &BytesRef<'_>, mut append: impl FnMut(&str)) {
    if let Ok(raw) = std::str::from_utf8(reference.as_ref()) {
        append("&");
        append(raw);
        append(";");
    }
}

fn append_general_ref(out: &mut String, reference: &BytesRef<'_>) {
    with_general_ref_text(reference, |value| out.push_str(value));
}

fn is_xml_10_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}'
    ) || ('\u{10000}'..='\u{10FFFF}').contains(&ch)
}

fn assign_doc_property(props: &mut DocProperties, tag: &[u8], value: String) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    let value = value.to_string();
    match tag {
        b"title" => props.title = Some(value),
        b"subject" => props.subject = Some(value),
        b"creator" => props.creator = Some(value),
        b"keywords" => props.keywords = Some(value),
        b"description" => props.description = Some(value),
        b"lastModifiedBy" => props.last_modified_by = Some(value),
        b"created" => props.created = Some(value),
        b"modified" if props.created.is_none() => props.created = Some(value),
        b"Company" => props.company = Some(value),
        _ => {}
    }
}

pub(crate) fn parse_doc_properties(core_xml: Option<&str>, app_xml: Option<&str>) -> DocProperties {
    let mut props = DocProperties::default();
    for xml in [core_xml, app_xml].into_iter().flatten() {
        let mut r = Reader::from_str(xml);
        let mut current: Option<Vec<u8>> = None;
        let mut text = String::new();
        loop {
            match r.read_event() {
                Ok(Event::Start(e)) => {
                    current = Some(local(e.name().as_ref()).to_vec());
                    text.clear();
                }
                Ok(Event::Text(t)) if current.is_some() => text.push_str(&text_of(&t)),
                Ok(Event::GeneralRef(reference)) if current.is_some() => {
                    append_general_ref(&mut text, &reference);
                }
                Ok(Event::End(e)) => {
                    if let Some(tag) = current.take() {
                        if tag.as_slice() == local(e.name().as_ref()) {
                            assign_doc_property(&mut props, &tag, std::mem::take(&mut text));
                        }
                    }
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
    }
    props
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SharedString {
    text: String,
    runs: Vec<crate::TextRun>,
}

/// `<sst><si>…<t>text</t>…</si>` — concatenate `<t>` runs within each `<si>`,
/// but skip `<rPh>` (East Asian phonetic / ruby guide) text, which is not part of
/// the displayed string.
fn parse_shared_strings(xml: &str, theme: &ThemeColors, indexed: &[Color]) -> Vec<SharedString> {
    let mut r = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut cur = SharedString::default();
    let mut run: Option<crate::TextRun> = None;
    let mut in_si = false;
    let mut in_t = false;
    let mut in_rph = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"si" => {
                    in_si = true;
                    cur = SharedString::default();
                }
                b"r" if in_si && !in_rph => run = Some(crate::TextRun::default()),
                b"rPh" => in_rph = true,
                b"t" => in_t = true,
                b"rFont" if run.is_some() => {
                    run.as_mut().expect("run").font.name = attr(&e, b"val");
                }
                b"sz" if run.is_some() => {
                    run.as_mut().expect("run").font.size_pt = attr(&e, b"val")
                        .and_then(|value| value.parse::<f32>().ok())
                        .map(|value| value.round().clamp(1.0, f32::from(u16::MAX)) as u16);
                }
                b"color" if run.is_some() => {
                    run.as_mut().expect("run").font.color = color_attr(&e, theme, indexed);
                }
                b"b" if run.is_some() => run.as_mut().expect("run").font.bold = true,
                b"i" if run.is_some() => run.as_mut().expect("run").font.italic = true,
                b"u" if run.is_some() => run.as_mut().expect("run").font.underline = true,
                b"strike" if run.is_some() => {
                    run.as_mut().expect("run").font.strikethrough = true;
                }
                b"vertAlign" if run.is_some() => {
                    run.as_mut().expect("run").font.script = match attr(&e, b"val").as_deref() {
                        Some("superscript") => FormatScript::Superscript,
                        Some("subscript") => FormatScript::Subscript,
                        _ => FormatScript::None,
                    };
                }
                _ => {}
            },
            // A self-closing `<si/>` is an empty string — it must still occupy an
            // index slot, or every later shared-string reference shifts.
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == b"si" => {
                out.push(SharedString::default());
            }
            Ok(Event::Empty(e)) if in_si && run.is_some() => {
                let font = &mut run.as_mut().expect("run").font;
                match local(e.name().as_ref()) {
                    b"rFont" => font.name = attr(&e, b"val"),
                    b"sz" => {
                        font.size_pt = attr(&e, b"val")
                            .and_then(|value| value.parse::<f32>().ok())
                            .map(|value| value.round().clamp(1.0, f32::from(u16::MAX)) as u16);
                    }
                    b"color" => font.color = color_attr(&e, theme, indexed),
                    b"b" => font.bold = true,
                    b"i" => font.italic = true,
                    b"u" => font.underline = true,
                    b"strike" => font.strikethrough = true,
                    b"vertAlign" => {
                        font.script = match attr(&e, b"val").as_deref() {
                            Some("superscript") => FormatScript::Superscript,
                            Some("subscript") => FormatScript::Subscript,
                            _ => FormatScript::None,
                        };
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"si" => {
                    in_si = false;
                    out.push(std::mem::take(&mut cur));
                }
                b"r" if run.is_some() => {
                    let completed = run.take().expect("run");
                    if !completed.text.is_empty() {
                        cur.runs.push(completed);
                    }
                }
                b"rPh" => in_rph = false,
                b"t" => in_t = false,
                _ => {}
            },
            Ok(Event::Text(t)) if in_si && in_t && !in_rph => {
                let text = text_of(&t);
                cur.text.push_str(&text);
                if let Some(run) = run.as_mut() {
                    run.text.push_str(&text);
                }
            }
            Ok(Event::GeneralRef(reference)) if in_si && in_t && !in_rph => {
                with_general_ref_text(&reference, |text| {
                    cur.text.push_str(text);
                    if let Some(run) = run.as_mut() {
                        run.text.push_str(text);
                    }
                });
            }
            Ok(Event::CData(t)) if in_si && in_t && !in_rph => {
                let bytes = t.into_inner();
                let text = String::from_utf8_lossy(bytes.as_ref());
                cur.text.push_str(&text);
                if let Some(run) = run.as_mut() {
                    run.text.push_str(&text);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// `<styleSheet>`: `<numFmts><numFmt numFmtId formatCode/>` + the `<cellXfs>`
/// `<xf numFmtId/>` list (cell `s` indexes cellXfs).
fn retain_custom_number_format(styles: &mut Styles, e: &quick_xml::events::BytesStart<'_>) {
    let (Some(id), Some(code)) = (attr(e, b"numFmtId"), attr(e, b"formatCode")) else {
        return;
    };
    let Ok(id) = id.parse::<u16>() else {
        return;
    };
    if code.len() > MAX_XLSX_FORMAT_CODE_BYTES {
        add_differential_loss(&mut styles.losses, StyleLossKind::LimitExceeded, 1);
        return;
    }
    if !styles.custom.contains_key(&id) && styles.custom.len() >= MAX_XLSX_CUSTOM_NUMBER_FORMATS {
        add_differential_loss(&mut styles.losses, StyleLossKind::LimitExceeded, 1);
        return;
    }
    styles.custom.insert(id, code);
}

fn retain_cell_xf_number_format(styles: &mut Styles, e: &quick_xml::events::BytesStart<'_>) {
    if styles.xf_numfmt.len() >= MAX_XLSX_STYLE_RECORDS {
        add_differential_loss(&mut styles.losses, StyleLossKind::LimitExceeded, 1);
        return;
    }
    let id = attr(e, b"numFmtId")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    styles.xf_numfmt.push(id);
}

fn parse_styles(xml: &str, theme: &ThemeColors) -> Styles {
    let mut r = Reader::from_str(xml);
    let mut styles = Styles::default();
    styles.indexed_colors = parse_indexed_colors(xml, &mut styles.losses);
    let mut in_cell_xfs = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"numFmt" => retain_custom_number_format(&mut styles, &e),
                b"cellXfs" => in_cell_xfs = true,
                b"xf" if in_cell_xfs => retain_cell_xf_number_format(&mut styles, &e),
                _ => {}
            },
            Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"numFmt" => retain_custom_number_format(&mut styles, &e),
                b"xf" if in_cell_xfs => retain_cell_xf_number_format(&mut styles, &e),
                _ => {}
            },
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"cellXfs" => in_cell_xfs = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    let (cell_styles, cell_style_overlays) = parse_cell_styles(
        xml,
        theme,
        &styles.indexed_colors,
        &styles.custom,
        &mut styles.losses,
    );
    styles.cell_styles = cell_styles;
    styles.cell_style_overlays = cell_style_overlays;
    let differential_styles =
        parse_differential_styles(xml, theme, &styles.indexed_colors, &styles.custom);
    styles.table_styles = parse_table_styles(xml, &differential_styles);
    styles.differential_styles = differential_styles;
    styles
}

fn add_differential_loss(losses: &mut Vec<StyleLoss>, kind: StyleLossKind, occurrences: u32) {
    if occurrences == 0 {
        return;
    }
    if let Some(loss) = losses.iter_mut().find(|loss| loss.kind == kind) {
        loss.occurrences = loss.occurrences.saturating_add(occurrences);
    } else {
        losses.push(StyleLoss { kind, occurrences });
    }
}

fn retain_xlsx_style_record<T>(records: &mut Vec<T>, value: T, losses: &mut Vec<StyleLoss>) {
    if records.len() < MAX_XLSX_STYLE_RECORDS {
        records.push(value);
    } else {
        add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
    }
}

fn retain_cell_xf_style(
    styles: &mut Vec<CellStyle>,
    overlays: &mut Vec<CellStyleOverlay>,
    style: CellStyle,
    overlay: CellStyleOverlay,
    losses: &mut Vec<StyleLoss>,
) {
    if styles.len() < MAX_XLSX_STYLE_RECORDS {
        styles.push(style);
        overlays.push(overlay);
    } else {
        add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
    }
}

fn differential_alignment_is_lossy(e: &quick_xml::events::BytesStart<'_>) -> bool {
    let horizontal = attr(e, b"horizontal");
    let vertical = attr(e, b"vertical");
    let explicit_false = |name| {
        attr(e, name)
            .as_deref()
            .is_some_and(|value| !attr_true(value))
    };
    horizontal
        .as_deref()
        .is_some_and(|value| !matches!(value, "general" | "left" | "center" | "right"))
        || vertical
            .as_deref()
            .is_some_and(|value| !matches!(value, "top" | "center" | "bottom"))
        || attr(e, b"textRotation")
            .and_then(|value| value.parse::<i16>().ok())
            .is_some_and(|value| value > 180)
        || explicit_false(b"wrapText")
        || explicit_false(b"shrinkToFit")
        || attr(e, b"indent").as_deref() == Some("0")
        || [
            b"relativeIndent".as_slice(),
            b"justifyLastLine".as_slice(),
            b"readingOrder".as_slice(),
            b"mergeCell".as_slice(),
        ]
        .into_iter()
        .any(|name| attr(e, name).is_some())
}

fn parse_differential_styles(
    xml: &str,
    theme: &ThemeColors,
    indexed: &[Color],
    custom: &HashMap<u16, String>,
) -> Vec<DifferentialStyle> {
    const MAX_DXFS: usize = 4_096;
    let mut reader = Reader::from_str(xml);
    let mut in_dxfs = false;
    let mut current: Option<CellStyle> = None;
    let mut font: Option<Font> = None;
    let mut fill: Option<Fill> = None;
    let mut border: Option<Border> = None;
    let mut border_edge = None;
    let mut losses = Vec::<StyleLoss>::new();
    let mut styles = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qualified_name = e.name();
                let name = local(qualified_name.as_ref());
                if name == b"dxfs" {
                    in_dxfs = true;
                    continue;
                }
                if !in_dxfs {
                    continue;
                }
                match name {
                    b"dxf" => {
                        if current.is_some() && styles.len() < MAX_DXFS {
                            styles.push(DifferentialStyle {
                                style: current.take().unwrap_or_default(),
                                losses: std::mem::take(&mut losses),
                            });
                        }
                        current = Some(CellStyle::default());
                        losses.clear();
                        font = None;
                        fill = None;
                        border = None;
                        border_edge = None;
                        if e.is_empty() {
                            let style = current.take().unwrap_or_default();
                            if styles.len() < MAX_DXFS {
                                styles.push(DifferentialStyle {
                                    style,
                                    losses: std::mem::take(&mut losses),
                                });
                            }
                        }
                    }
                    b"font" if current.is_some() => {
                        font = (!e.is_empty()).then(Font::default);
                    }
                    b"fill" if current.is_some() => {
                        fill = (!e.is_empty()).then(Fill::default);
                    }
                    b"border" if current.is_some() => {
                        border = (!e.is_empty()).then(Border::default);
                    }
                    b"name" if font.is_some() => {
                        font.as_mut().expect("dxf font").name = attr(&e, b"val");
                    }
                    b"sz" if font.is_some() => {
                        font.as_mut().expect("dxf font").size_pt = attr(&e, b"val")
                            .and_then(|value| value.parse::<f32>().ok())
                            .map(|value| value.round().clamp(1.0, f32::from(u16::MAX)) as u16);
                    }
                    b"color" if font.is_some() => {
                        let color = color_attr(&e, theme, indexed);
                        if color.is_none() {
                            add_differential_loss(&mut losses, StyleLossKind::UnresolvedColor, 1);
                        }
                        font.as_mut().expect("dxf font").color = color;
                    }
                    b"b" if font.is_some() => {
                        let enabled = attr(&e, b"val").as_deref().is_none_or(attr_true);
                        if !enabled {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        font.as_mut().expect("dxf font").bold = enabled;
                    }
                    b"i" if font.is_some() => {
                        let enabled = attr(&e, b"val").as_deref().is_none_or(attr_true);
                        if !enabled {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        font.as_mut().expect("dxf font").italic = enabled;
                    }
                    b"u" if font.is_some() => {
                        let enabled = attr(&e, b"val").as_deref().is_none_or(attr_true);
                        if !enabled {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        font.as_mut().expect("dxf font").underline = enabled;
                    }
                    b"strike" if font.is_some() => {
                        let enabled = attr(&e, b"val").as_deref().is_none_or(attr_true);
                        if !enabled {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        font.as_mut().expect("dxf font").strikethrough = enabled;
                    }
                    b"vertAlign" if font.is_some() => {
                        font.as_mut().expect("dxf font").script = match attr(&e, b"val").as_deref()
                        {
                            Some("superscript") => FormatScript::Superscript,
                            Some("subscript") => FormatScript::Subscript,
                            _ => FormatScript::None,
                        };
                    }
                    b"patternFill" if fill.is_some() => {
                        let source_pattern = attr(&e, b"patternType");
                        let pattern = format_pattern(source_pattern.as_deref());
                        if pattern == FormatPattern::None
                            && source_pattern
                                .as_deref()
                                .is_some_and(|value| value != "none")
                        {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        fill.as_mut().expect("dxf fill").pattern = pattern;
                    }
                    b"fgColor" if fill.is_some() => {
                        let color = color_attr(&e, theme, indexed);
                        if color.is_none() {
                            add_differential_loss(&mut losses, StyleLossKind::UnresolvedColor, 1);
                        }
                        fill.as_mut().expect("dxf fill").foreground = color;
                    }
                    b"bgColor" if fill.is_some() => {
                        let color = color_attr(&e, theme, indexed);
                        if color.is_none() {
                            add_differential_loss(&mut losses, StyleLossKind::UnresolvedColor, 1);
                        }
                        fill.as_mut().expect("dxf fill").background = color;
                    }
                    b"left" | b"right" | b"top" | b"bottom" if border.is_some() => {
                        let edge = match name {
                            b"left" => BorderEdge::Left,
                            b"right" => BorderEdge::Right,
                            b"top" => BorderEdge::Top,
                            _ => BorderEdge::Bottom,
                        };
                        let source_style = attr(&e, b"style");
                        let parsed_style = border_style(source_style.as_deref());
                        if parsed_style == BorderStyle::None
                            && source_style.as_deref().is_some_and(|value| value != "none")
                        {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        set_border_edge(border.as_mut().expect("dxf border"), edge, parsed_style);
                        border_edge = (!e.is_empty()).then_some(edge);
                    }
                    b"color" if border.is_some() && border_edge.is_some() => {
                        if let Some(color) = color_attr(&e, theme, indexed) {
                            set_border_color(
                                border.as_mut().expect("dxf border"),
                                border_edge.expect("dxf border edge"),
                                color,
                            );
                        } else {
                            add_differential_loss(&mut losses, StyleLossKind::UnresolvedColor, 1);
                        }
                    }
                    b"gradientFill" if fill.is_some() => {
                        add_differential_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1)
                    }
                    b"diagonal" | b"vertical" | b"horizontal" | b"start" | b"end"
                        if border.is_some() =>
                    {
                        add_differential_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    }
                    b"numFmt" if current.is_some() => {
                        current.as_mut().expect("dxf").num_fmt =
                            attr(&e, b"formatCode").or_else(|| {
                                attr(&e, b"numFmtId")
                                    .and_then(|value| value.parse::<u16>().ok())
                                    .and_then(|id| {
                                        custom
                                            .get(&id)
                                            .cloned()
                                            .or_else(|| built_in_num_fmt(id).map(str::to_string))
                                    })
                            });
                    }
                    b"alignment" if current.is_some() => {
                        current.as_mut().expect("dxf").align = Some(parse_alignment(&e));
                        if differential_alignment_is_lossy(&e) {
                            add_differential_loss(
                                &mut losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                    }
                    b"protection" if current.is_some() => {
                        current.as_mut().expect("dxf").protection = Some(CellProtection {
                            locked: attr(&e, b"locked").as_deref().and_then(parse_bool_attr),
                            hidden: attr(&e, b"hidden").as_deref().is_some_and(attr_true),
                        });
                    }
                    _ if font.is_some() || fill.is_some() || border.is_some() => {
                        add_differential_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    }
                    b"extLst" if current.is_some() => {
                        add_differential_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1)
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"font" if current.is_some() => {
                    let value = font.take().unwrap_or_default();
                    if value != Font::default() {
                        current.as_mut().expect("dxf").font = Some(value);
                    }
                }
                b"fill" if current.is_some() => {
                    let value = fill.take().unwrap_or_default();
                    if value != Fill::default() {
                        if value.pattern == FormatPattern::Solid {
                            current.as_mut().expect("dxf").fill =
                                value.foreground.or(value.background);
                        }
                        current.as_mut().expect("dxf").pattern_fill = Some(value);
                    }
                }
                b"left" | b"right" | b"top" | b"bottom" => border_edge = None,
                b"border" if current.is_some() => {
                    let value = border.take().unwrap_or_default();
                    if value != Border::default() {
                        current.as_mut().expect("dxf").border = Some(value);
                    }
                }
                b"dxf" if current.is_some() => {
                    if styles.len() < MAX_DXFS {
                        styles.push(DifferentialStyle {
                            style: current.take().unwrap_or_default(),
                            losses: std::mem::take(&mut losses),
                        });
                    } else {
                        current = None;
                        losses.clear();
                    }
                    font = None;
                    fill = None;
                    border = None;
                    border_edge = None;
                }
                b"dxfs" => in_dxfs = false,
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    styles
}

fn table_style_region(value: &str) -> Option<TableStyleRegion> {
    match value {
        "wholeTable" => Some(TableStyleRegion::WholeTable),
        "firstColumnStripe" => Some(TableStyleRegion::FirstColumnStripe),
        "secondColumnStripe" => Some(TableStyleRegion::SecondColumnStripe),
        "firstRowStripe" => Some(TableStyleRegion::FirstRowStripe),
        "secondRowStripe" => Some(TableStyleRegion::SecondRowStripe),
        "firstColumn" => Some(TableStyleRegion::FirstColumn),
        "lastColumn" => Some(TableStyleRegion::LastColumn),
        "headerRow" => Some(TableStyleRegion::HeaderRow),
        "totalRow" => Some(TableStyleRegion::TotalRow),
        "firstHeaderCell" => Some(TableStyleRegion::FirstHeaderCell),
        "lastHeaderCell" => Some(TableStyleRegion::LastHeaderCell),
        "firstTotalCell" => Some(TableStyleRegion::FirstTotalCell),
        "lastTotalCell" => Some(TableStyleRegion::LastTotalCell),
        _ => None,
    }
}

fn table_style_region_is_stripe(region: TableStyleRegion) -> bool {
    matches!(
        region,
        TableStyleRegion::FirstColumnStripe
            | TableStyleRegion::SecondColumnStripe
            | TableStyleRegion::FirstRowStripe
            | TableStyleRegion::SecondRowStripe
    )
}

fn parse_table_styles(xml: &str, dxfs: &[DifferentialStyle]) -> HashMap<String, ParsedTableStyle> {
    const MAX_TABLE_STYLES: usize = 4_096;
    const MAX_ELEMENTS_PER_TABLE_STYLE: usize = 64;
    const MAX_TABLE_STRIPE_SIZE: u32 = 1_048_576;
    let mut reader = Reader::from_str(xml);
    let mut in_table_styles = false;
    let mut current_name: Option<String> = None;
    let mut current_elements = 0usize;
    let mut styles = HashMap::<String, ParsedTableStyle>::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"tableStyles" => in_table_styles = true,
                b"tableStyle" if in_table_styles => {
                    current_elements = 0;
                    current_name = attr(&e, b"name").filter(|name| !name.is_empty());
                    if let Some(name) = current_name.clone() {
                        if !styles.contains_key(&name) && styles.len() >= MAX_TABLE_STYLES {
                            current_name = None;
                        } else {
                            let duplicate = styles.contains_key(&name);
                            let parsed = styles.entry(name).or_default();
                            if duplicate {
                                add_differential_loss(
                                    &mut parsed.losses,
                                    StyleLossKind::UnsupportedProperty,
                                    1,
                                );
                            }
                        }
                    }
                    if e.is_empty() {
                        current_name = None;
                    }
                }
                b"tableStyleElement" if current_name.is_some() => {
                    current_elements = current_elements.saturating_add(1);
                    let parsed = styles
                        .get_mut(current_name.as_ref().expect("table style name"))
                        .expect("current table style");
                    if current_elements > MAX_ELEMENTS_PER_TABLE_STYLE {
                        add_differential_loss(&mut parsed.losses, StyleLossKind::LimitExceeded, 1);
                        continue;
                    }
                    let Some(region) = attr(&e, b"type").as_deref().and_then(table_style_region)
                    else {
                        add_differential_loss(
                            &mut parsed.losses,
                            StyleLossKind::UnsupportedProperty,
                            1,
                        );
                        continue;
                    };
                    let stripe_size = if table_style_region_is_stripe(region) {
                        match attr(&e, b"size") {
                            None => 1,
                            Some(value) => match value.parse::<u32>() {
                                Ok(size @ 1..=MAX_TABLE_STRIPE_SIZE) => size,
                                Ok(_) => {
                                    add_differential_loss(
                                        &mut parsed.losses,
                                        StyleLossKind::LimitExceeded,
                                        1,
                                    );
                                    1
                                }
                                Err(_) => {
                                    add_differential_loss(
                                        &mut parsed.losses,
                                        StyleLossKind::UnsupportedProperty,
                                        1,
                                    );
                                    1
                                }
                            },
                        }
                    } else {
                        if attr(&e, b"size").is_some() {
                            add_differential_loss(
                                &mut parsed.losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        }
                        1
                    };
                    let Some(dxf) = attr(&e, b"dxfId")
                        .and_then(|value| value.parse::<usize>().ok())
                        .and_then(|index| dxfs.get(index))
                    else {
                        add_differential_loss(
                            &mut parsed.losses,
                            StyleLossKind::MissingReference,
                            1,
                        );
                        continue;
                    };
                    for loss in &dxf.losses {
                        add_differential_loss(&mut parsed.losses, loss.kind, loss.occurrences);
                    }
                    if parsed
                        .definition
                        .insert(region, dxf.style.clone(), stripe_size)
                        .is_some()
                    {
                        add_differential_loss(
                            &mut parsed.losses,
                            StyleLossKind::UnsupportedProperty,
                            1,
                        );
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"tableStyle" => {
                    current_name = None;
                    current_elements = 0;
                }
                b"tableStyles" => in_table_styles = false,
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    styles
}

fn built_in_table_style(name: &str, theme: &ThemeColors) -> Option<ParsedTableStyle> {
    const OFFICE_ACCENTS: [Color; 6] = [
        Color::rgb(0x44, 0x72, 0xC4),
        Color::rgb(0xED, 0x7D, 0x31),
        Color::rgb(0xA5, 0xA5, 0xA5),
        Color::rgb(0xFF, 0xC0, 0x00),
        Color::rgb(0x5B, 0x9B, 0xD5),
        Color::rgb(0x70, 0xAD, 0x47),
    ];
    let (family, number) = ["TableStyleLight", "TableStyleMedium", "TableStyleDark"]
        .into_iter()
        .find_map(|prefix| {
            name.strip_prefix(prefix)
                .and_then(|suffix| suffix.parse::<usize>().ok())
                .map(|number| (prefix, number))
        })?;
    let valid = match family {
        "TableStyleLight" => (1..=21).contains(&number),
        "TableStyleMedium" => (1..=28).contains(&number),
        "TableStyleDark" => (1..=11).contains(&number),
        _ => false,
    };
    if !valid {
        return None;
    }
    let accent_index = match family {
        "TableStyleLight" => number.saturating_sub(2) % OFFICE_ACCENTS.len(),
        "TableStyleMedium" => number.saturating_sub(2) % OFFICE_ACCENTS.len(),
        "TableStyleDark" => number.saturating_sub(2) % OFFICE_ACCENTS.len(),
        _ => 0,
    };
    let accent = theme.colors[4 + accent_index].unwrap_or(OFFICE_ACCENTS[accent_index]);
    let white = Color::rgb(0xFF, 0xFF, 0xFF);
    let mut header = CellStyle {
        font: Some(Font::default().bold()),
        ..CellStyle::default()
    };
    match family {
        "TableStyleLight" => {
            header.font.as_mut().expect("table font").color = Some(accent);
            header.border = Some(
                Border::default()
                    .with_bottom(BorderStyle::Medium)
                    .with_color(accent),
            );
        }
        "TableStyleMedium" | "TableStyleDark" => {
            header.font.as_mut().expect("table font").color = Some(white);
            header.fill = Some(accent);
            header.pattern_fill = Some(Fill::solid(accent));
        }
        _ => unreachable!("validated table style family"),
    }
    let mut definition = TableStyleDefinition::default();
    definition.insert(TableStyleRegion::HeaderRow, header, 1);
    definition.insert(
        TableStyleRegion::TotalRow,
        CellStyle {
            font: Some(Font::default().bold()),
            border: Some(
                Border::default()
                    .with_top(BorderStyle::Medium)
                    .with_color(accent),
            ),
            ..CellStyle::default()
        },
        1,
    );
    let emphasis = CellStyle {
        font: Some(Font::default().bold()),
        ..CellStyle::default()
    };
    definition.insert(TableStyleRegion::FirstColumn, emphasis.clone(), 1);
    definition.insert(TableStyleRegion::LastColumn, emphasis, 1);

    let stripe = match family {
        "TableStyleLight" => apply_tint(accent, 0.90),
        "TableStyleMedium" => apply_tint(accent, 0.80),
        "TableStyleDark" => apply_tint(accent, -0.15),
        _ => unreachable!("validated table style family"),
    };
    let stripe_style = CellStyle {
        fill: Some(stripe),
        pattern_fill: Some(Fill::solid(stripe)),
        ..CellStyle::default()
    };
    definition.insert(TableStyleRegion::FirstRowStripe, stripe_style.clone(), 1);
    definition.insert(TableStyleRegion::FirstColumnStripe, stripe_style, 1);
    if family == "TableStyleDark" {
        let body = apply_tint(accent, -0.30);
        definition.insert(
            TableStyleRegion::WholeTable,
            CellStyle {
                font: Some(Font::default().with_color(white)),
                fill: Some(body),
                pattern_fill: Some(Fill::solid(body)),
                ..CellStyle::default()
            },
            1,
        );
    }
    Some(ParsedTableStyle {
        definition,
        // The built-in family recipes preserve the visible cascade regions,
        // but they do not yet encode every per-style Office border/fill
        // variation. Surface that approximation instead of presenting it as
        // exact source fidelity.
        losses: vec![StyleLoss {
            kind: StyleLossKind::UnsupportedProperty,
            occurrences: 1,
        }],
    })
}

#[cfg(test)]
fn built_in_table_header_style(name: &str, theme: &ThemeColors) -> Option<CellStyle> {
    built_in_table_style(name, theme).and_then(|style| {
        style
            .definition
            .get(TableStyleRegion::HeaderRow)
            .map(|element| element.style.clone())
    })
}

fn parse_font_table(
    xml: &str,
    theme: &ThemeColors,
    indexed: &[Color],
    losses: &mut Vec<StyleLoss>,
) -> Vec<Font> {
    let mut reader = Reader::from_str(xml);
    let mut in_fonts = false;
    let mut current: Option<Font> = None;
    let mut fonts = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"fonts" => in_fonts = true,
                b"font" if in_fonts => {
                    if current.is_some() {
                        retain_xlsx_style_record(
                            &mut fonts,
                            current.take().unwrap_or_default(),
                            losses,
                        );
                    }
                    if fonts.len() >= MAX_XLSX_STYLE_RECORDS {
                        add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                        current = None;
                        continue;
                    }
                    current = Some(Font::default());
                    if e.is_empty() {
                        retain_xlsx_style_record(
                            &mut fonts,
                            current.take().unwrap_or_default(),
                            losses,
                        );
                    }
                }
                b"name" if current.is_some() => {
                    current.as_mut().expect("font").name = attr(&e, b"val");
                }
                b"sz" if current.is_some() => {
                    current.as_mut().expect("font").size_pt = attr(&e, b"val")
                        .and_then(|value| value.parse::<f32>().ok())
                        .map(|value| value.round().clamp(1.0, f32::from(u16::MAX)) as u16);
                }
                b"color" if current.is_some() => {
                    current.as_mut().expect("font").color = color_attr(&e, theme, indexed);
                }
                b"b" if current.is_some() => current.as_mut().expect("font").bold = true,
                b"i" if current.is_some() => current.as_mut().expect("font").italic = true,
                b"u" if current.is_some() => current.as_mut().expect("font").underline = true,
                b"strike" if current.is_some() => {
                    current.as_mut().expect("font").strikethrough = true;
                }
                b"vertAlign" if current.is_some() => {
                    current.as_mut().expect("font").script = match attr(&e, b"val").as_deref() {
                        Some("superscript") => FormatScript::Superscript,
                        Some("subscript") => FormatScript::Subscript,
                        _ => FormatScript::None,
                    };
                }
                _ => {}
            },
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"font" && current.is_some() => {
                retain_xlsx_style_record(&mut fonts, current.take().unwrap_or_default(), losses);
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"fonts" => in_fonts = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    fonts
}

fn format_pattern(value: Option<&str>) -> FormatPattern {
    match value.unwrap_or("none") {
        "solid" => FormatPattern::Solid,
        "mediumGray" => FormatPattern::MediumGray,
        "darkGray" => FormatPattern::DarkGray,
        "lightGray" => FormatPattern::LightGray,
        "darkHorizontal" => FormatPattern::DarkHorizontal,
        "darkVertical" => FormatPattern::DarkVertical,
        "darkDown" => FormatPattern::DarkDown,
        "darkUp" => FormatPattern::DarkUp,
        "darkGrid" => FormatPattern::DarkGrid,
        "darkTrellis" => FormatPattern::DarkTrellis,
        "lightHorizontal" => FormatPattern::LightHorizontal,
        "lightVertical" => FormatPattern::LightVertical,
        "lightDown" => FormatPattern::LightDown,
        "lightUp" => FormatPattern::LightUp,
        "lightGrid" => FormatPattern::LightGrid,
        "lightTrellis" => FormatPattern::LightTrellis,
        "gray125" => FormatPattern::Gray125,
        "gray0625" => FormatPattern::Gray0625,
        _ => FormatPattern::None,
    }
}

fn parse_fill_table(
    xml: &str,
    theme: &ThemeColors,
    indexed: &[Color],
    losses: &mut Vec<StyleLoss>,
) -> Vec<Fill> {
    let mut reader = Reader::from_str(xml);
    let mut in_fills = false;
    let mut current: Option<Fill> = None;
    let mut fills = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"fills" => in_fills = true,
                b"fill" if in_fills => {
                    if let Some(previous) = current.take() {
                        retain_xlsx_style_record(&mut fills, previous, losses);
                    }
                    if fills.len() >= MAX_XLSX_STYLE_RECORDS {
                        add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                        current = None;
                        continue;
                    }
                    current = Some(Fill::default());
                    if e.is_empty() {
                        retain_xlsx_style_record(
                            &mut fills,
                            current.take().unwrap_or_default(),
                            losses,
                        );
                    }
                }
                b"patternFill" if current.is_some() => {
                    current.as_mut().expect("fill").pattern =
                        format_pattern(attr(&e, b"patternType").as_deref());
                }
                b"fgColor" if current.is_some() => {
                    current.as_mut().expect("fill").foreground = color_attr(&e, theme, indexed);
                }
                b"bgColor" if current.is_some() => {
                    current.as_mut().expect("fill").background = color_attr(&e, theme, indexed);
                }
                _ => {}
            },
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"fill" && current.is_some() => {
                retain_xlsx_style_record(&mut fills, current.take().unwrap_or_default(), losses);
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"fills" => in_fills = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    fills
}

#[derive(Clone, Copy)]
enum BorderEdge {
    Left,
    Right,
    Top,
    Bottom,
}

fn border_style(value: Option<&str>) -> BorderStyle {
    match value.unwrap_or("none") {
        "thin" | "hair" | "dotted" | "dashed" | "dashDot" | "dashDotDot" => BorderStyle::Thin,
        "medium" | "mediumDashed" | "mediumDashDot" | "mediumDashDotDot" => BorderStyle::Medium,
        "thick" | "slantDashDot" => BorderStyle::Thick,
        "double" => BorderStyle::Double,
        _ => BorderStyle::None,
    }
}

fn set_border_edge(border: &mut Border, edge: BorderEdge, style: BorderStyle) {
    match edge {
        BorderEdge::Left => border.left = style,
        BorderEdge::Right => border.right = style,
        BorderEdge::Top => border.top = style,
        BorderEdge::Bottom => border.bottom = style,
    }
}

fn set_border_color(border: &mut Border, edge: BorderEdge, color: Color) {
    match edge {
        BorderEdge::Left => border.left_color = Some(color),
        BorderEdge::Right => border.right_color = Some(color),
        BorderEdge::Top => border.top_color = Some(color),
        BorderEdge::Bottom => border.bottom_color = Some(color),
    }
}

fn parse_border_table(
    xml: &str,
    theme: &ThemeColors,
    indexed: &[Color],
    losses: &mut Vec<StyleLoss>,
) -> Vec<Border> {
    let mut reader = Reader::from_str(xml);
    let mut in_borders = false;
    let mut current: Option<Border> = None;
    let mut edge = None;
    let mut borders = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"borders" => in_borders = true,
                b"border" if in_borders => {
                    if let Some(previous) = current.take() {
                        retain_xlsx_style_record(&mut borders, previous, losses);
                    }
                    if borders.len() >= MAX_XLSX_STYLE_RECORDS {
                        add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                        current = None;
                        continue;
                    }
                    current = Some(Border::default());
                    if e.is_empty() {
                        retain_xlsx_style_record(
                            &mut borders,
                            current.take().unwrap_or_default(),
                            losses,
                        );
                    }
                }
                b"left" | b"right" | b"top" | b"bottom" if current.is_some() => {
                    let selected = match local(e.name().as_ref()) {
                        b"left" => BorderEdge::Left,
                        b"right" => BorderEdge::Right,
                        b"top" => BorderEdge::Top,
                        _ => BorderEdge::Bottom,
                    };
                    set_border_edge(
                        current.as_mut().expect("border"),
                        selected,
                        border_style(attr(&e, b"style").as_deref()),
                    );
                    edge = (!e.is_empty()).then_some(selected);
                }
                b"color" if current.is_some() && edge.is_some() => {
                    if let Some(color) = color_attr(&e, theme, indexed) {
                        set_border_color(
                            current.as_mut().expect("border"),
                            edge.expect("edge"),
                            color,
                        );
                    }
                }
                _ => {}
            },
            Ok(Event::End(e))
                if matches!(
                    local(e.name().as_ref()),
                    b"left" | b"right" | b"top" | b"bottom"
                ) =>
            {
                edge = None
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"border" && current.is_some() => {
                retain_xlsx_style_record(&mut borders, current.take().unwrap_or_default(), losses);
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"borders" => in_borders = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    borders
}

fn built_in_num_fmt(id: u16) -> Option<&'static str> {
    format::built_in_format_code(id)
}

fn parse_alignment(e: &quick_xml::events::BytesStart<'_>) -> Alignment {
    let horizontal = match attr(e, b"horizontal").as_deref() {
        Some("left") => Some(HAlign::Left),
        Some("center" | "centerContinuous" | "distributed") => Some(HAlign::Center),
        Some("right") => Some(HAlign::Right),
        _ => None,
    };
    let vertical = match attr(e, b"vertical").as_deref() {
        Some("top") => Some(VAlign::Top),
        Some("center" | "distributed" | "justify") => Some(VAlign::Middle),
        Some("bottom") => Some(VAlign::Bottom),
        _ => None,
    };
    let raw_rotation = attr(e, b"textRotation")
        .and_then(|value| value.parse::<i16>().ok())
        .unwrap_or(0);
    let rotation = if (91..=180).contains(&raw_rotation) {
        90 - raw_rotation
    } else if raw_rotation <= 90 {
        raw_rotation
    } else {
        0
    };
    Alignment {
        horizontal,
        vertical,
        wrap: attr(e, b"wrapText").as_deref().is_some_and(attr_true),
        rotation,
        indent: attr(e, b"indent")
            .and_then(|value| value.parse::<u8>().ok())
            .unwrap_or(0),
        shrink_to_fit: attr(e, b"shrinkToFit").as_deref().is_some_and(attr_true),
    }
}

fn cell_style_from_xf(
    e: &quick_xml::events::BytesStart<'_>,
    fonts: &[Font],
    fills: &[Fill],
    borders: &[Border],
    custom: &HashMap<u16, String>,
) -> CellStyle {
    let num_fmt_id = attr(e, b"numFmtId")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    let font_id = attr(e, b"fontId").and_then(|value| value.parse::<usize>().ok());
    let fill_id = attr(e, b"fillId").and_then(|value| value.parse::<usize>().ok());
    let border_id = attr(e, b"borderId").and_then(|value| value.parse::<usize>().ok());
    CellStyle {
        font: font_id.and_then(|id| fonts.get(id).cloned()),
        fill: None,
        pattern_fill: fill_id.and_then(|id| fills.get(id).copied()),
        border: border_id.and_then(|id| borders.get(id).cloned()),
        num_fmt: custom
            .get(&num_fmt_id)
            .cloned()
            .or_else(|| built_in_num_fmt(num_fmt_id).map(str::to_string)),
        align: None,
        protection: None,
    }
}

fn cell_style_overlay_from_xf(
    e: &quick_xml::events::BytesStart<'_>,
    fonts: &[Font],
    fills: &[Fill],
    borders: &[Border],
    custom: &HashMap<u16, String>,
) -> CellStyleOverlay {
    let num_fmt_id = attr(e, b"numFmtId")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    let font_id = attr(e, b"fontId").and_then(|value| value.parse::<usize>().ok());
    let fill_id = attr(e, b"fillId").and_then(|value| value.parse::<usize>().ok());
    let border_id = attr(e, b"borderId").and_then(|value| value.parse::<usize>().ok());
    let applies = |name: &[u8], fallback: bool| {
        attr(e, name)
            .as_deref()
            .and_then(parse_bool_attr)
            .unwrap_or(fallback)
    };
    let replace_font = applies(b"applyFont", font_id.is_some_and(|id| id != 0));
    let replace_fill = applies(b"applyFill", fill_id.is_some_and(|id| id != 0));
    let replace_border = applies(b"applyBorder", border_id.is_some_and(|id| id != 0));
    let replace_num_fmt = applies(b"applyNumberFormat", num_fmt_id != 0);
    CellStyleOverlay {
        style: CellStyle {
            font: replace_font
                .then(|| font_id.and_then(|id| fonts.get(id).cloned()))
                .flatten(),
            fill: None,
            pattern_fill: replace_fill
                .then(|| fill_id.and_then(|id| fills.get(id).copied()))
                .flatten(),
            border: replace_border
                .then(|| border_id.and_then(|id| borders.get(id).cloned()))
                .flatten(),
            num_fmt: replace_num_fmt
                .then(|| {
                    custom
                        .get(&num_fmt_id)
                        .cloned()
                        .or_else(|| built_in_num_fmt(num_fmt_id).map(str::to_string))
                })
                .flatten(),
            align: None,
            protection: None,
        },
        replace_font,
        replace_fill,
        replace_border,
        replace_num_fmt,
        replace_alignment: applies(b"applyAlignment", false),
        replace_protection: applies(b"applyProtection", false),
    }
}

fn parse_cell_styles(
    xml: &str,
    theme: &ThemeColors,
    indexed: &[Color],
    custom: &HashMap<u16, String>,
    losses: &mut Vec<StyleLoss>,
) -> (Vec<CellStyle>, Vec<CellStyleOverlay>) {
    let fonts = parse_font_table(xml, theme, indexed, losses);
    let fills = parse_fill_table(xml, theme, indexed, losses);
    let borders = parse_border_table(xml, theme, indexed, losses);
    let mut reader = Reader::from_str(xml);
    let mut in_cell_xfs = false;
    let mut current: Option<(CellStyle, CellStyleOverlay, Option<bool>, Option<bool>)> = None;
    let mut styles = Vec::new();
    let mut overlays = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"cellXfs" => in_cell_xfs = true,
                b"xf" if in_cell_xfs => {
                    if styles.len() >= MAX_XLSX_STYLE_RECORDS {
                        add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                        current = None;
                        continue;
                    }
                    current = Some((
                        cell_style_from_xf(&e, &fonts, &fills, &borders, custom),
                        cell_style_overlay_from_xf(&e, &fonts, &fills, &borders, custom),
                        attr(&e, b"applyAlignment")
                            .as_deref()
                            .and_then(parse_bool_attr),
                        attr(&e, b"applyProtection")
                            .as_deref()
                            .and_then(parse_bool_attr),
                    ));
                }
                b"alignment" if current.is_some() => {
                    let alignment = parse_alignment(&e);
                    let (resolved, overlay, apply_alignment, _) = current.as_mut().expect("xf");
                    resolved.align = Some(alignment.clone());
                    if *apply_alignment != Some(false) {
                        overlay.style.align = Some(alignment);
                        overlay.replace_alignment = true;
                    }
                }
                b"protection" if current.is_some() => {
                    let protection = CellProtection {
                        locked: attr(&e, b"locked").as_deref().and_then(parse_bool_attr),
                        hidden: attr(&e, b"hidden").as_deref().is_some_and(attr_true),
                    };
                    let (resolved, overlay, _, apply_protection) = current.as_mut().expect("xf");
                    resolved.protection = Some(protection.clone());
                    if *apply_protection != Some(false) {
                        overlay.style.protection = Some(protection);
                        overlay.replace_protection = true;
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"xf" if in_cell_xfs => {
                    retain_cell_xf_style(
                        &mut styles,
                        &mut overlays,
                        cell_style_from_xf(&e, &fonts, &fills, &borders, custom),
                        cell_style_overlay_from_xf(&e, &fonts, &fills, &borders, custom),
                        losses,
                    );
                }
                b"alignment" if current.is_some() => {
                    let alignment = parse_alignment(&e);
                    let (resolved, overlay, apply_alignment, _) = current.as_mut().expect("xf");
                    resolved.align = Some(alignment.clone());
                    if *apply_alignment != Some(false) {
                        overlay.style.align = Some(alignment);
                        overlay.replace_alignment = true;
                    }
                }
                b"protection" if current.is_some() => {
                    let protection = CellProtection {
                        locked: attr(&e, b"locked").as_deref().and_then(parse_bool_attr),
                        hidden: attr(&e, b"hidden").as_deref().is_some_and(attr_true),
                    };
                    let (resolved, overlay, _, apply_protection) = current.as_mut().expect("xf");
                    resolved.protection = Some(protection.clone());
                    if *apply_protection != Some(false) {
                        overlay.style.protection = Some(protection);
                        overlay.replace_protection = true;
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"xf" && current.is_some() => {
                let (style, overlay, _, _) = current.take().expect("xf");
                retain_cell_xf_style(&mut styles, &mut overlays, style, overlay, losses);
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"cellXfs" => in_cell_xfs = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    (styles, overlays)
}

/// A worksheet's visibility, from the `<sheet state>` attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Visibility {
    /// `state` absent or `"visible"`.
    #[default]
    Visible,
    /// `state="hidden"` — unhideable via the Excel UI.
    Hidden,
    /// `state="veryHidden"` — unhideable except via VBA.
    VeryHidden,
}

/// One `<sheet>` entry: its name, relationship id, and visibility.
struct SheetRef {
    name: String,
    rid: String,
    visibility: Visibility,
}

/// A sheet-local built-in defined name, such as print area, print titles, or
/// autofilter metadata.
struct SheetDefinedName {
    local_sheet_id: usize,
    name: String,
    refers_to: String,
}

/// The workbook-globals parse: ordered sheet refs, the 1904 flag, and the
/// workbook-global defined names.
struct ParsedWorkbook {
    sheets: Vec<SheetRef>,
    date1904: bool,
    structure_protected: bool,
    active_sheet: Option<usize>,
    defined_names: Vec<(String, String)>,
    local_defined_names: Vec<crate::LocalDefinedName>,
    sheet_defined_names: Vec<SheetDefinedName>,
}

enum DefinedNameCapture {
    GlobalUser(String),
    LocalUser { local_sheet_id: usize, name: String },
    LocalBuiltin { local_sheet_id: usize, name: String },
}

/// `<workbook>`: `<workbookPr date1904/>` + ordered `<sheet name state r:id/>` +
/// `<definedNames><definedName name>refers_to</definedName></definedNames>`. The
/// workbook-global user names are surfaced through [`Workbook::defined_names`].
/// Selected sheet-local built-ins are kept separately for sheet metadata; other
/// built-ins remain internal.
fn parse_workbook(xml: &str) -> ParsedWorkbook {
    let mut r = Reader::from_str(xml);
    let mut sheets = Vec::new();
    let mut date1904 = false;
    let mut structure_protected = false;
    let mut active_sheet = None;
    let mut defined_names = Vec::new();
    let mut raw_local_defined_names = Vec::new();
    let mut sheet_defined_names = Vec::new();
    // Open `<definedName>` capture: (name, accumulated refers-to text).
    let mut cur_name: Option<DefinedNameCapture> = None;
    let mut cur_refers = String::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"workbookPr" => {
                    if let Some(v) = attr(&e, b"date1904") {
                        date1904 = v == "1" || v.eq_ignore_ascii_case("true");
                    }
                }
                b"workbookProtection"
                    if attr(&e, b"lockStructure").as_deref().is_some_and(attr_true) =>
                {
                    structure_protected = true;
                }
                b"workbookView" if active_sheet.is_none() => {
                    active_sheet = attr(&e, b"activeTab").and_then(|s| s.parse::<usize>().ok());
                }
                b"sheet" => {
                    let name = attr(&e, b"name").unwrap_or_default();
                    let rid = attr(&e, b"id").unwrap_or_default(); // r:id → local "id"
                    let visibility = match attr(&e, b"state").as_deref() {
                        Some(s) if s.eq_ignore_ascii_case("hidden") => Visibility::Hidden,
                        Some(s) if s.eq_ignore_ascii_case("veryHidden") => Visibility::VeryHidden,
                        _ => Visibility::Visible,
                    };
                    sheets.push(SheetRef {
                        name,
                        rid,
                        visibility,
                    });
                }
                b"definedName" => {
                    // Capture workbook-global user names only: skip built-in
                    // `_xlnm.*` names from the workbook-global list. Selected
                    // sheet-local built-ins are kept for sheet metadata.
                    let local_sheet_id =
                        attr(&e, b"localSheetId").and_then(|s| s.parse::<usize>().ok());
                    cur_name = match (attr(&e, b"name"), local_sheet_id) {
                        (Some(n), None) if !n.starts_with("_xlnm.") => {
                            Some(DefinedNameCapture::GlobalUser(n))
                        }
                        (Some(n), Some(local_sheet_id))
                            if matches!(
                                n.as_str(),
                                "_xlnm.Print_Area" | "_xlnm.Print_Titles" | "_xlnm._FilterDatabase"
                            ) =>
                        {
                            Some(DefinedNameCapture::LocalBuiltin {
                                local_sheet_id,
                                name: n,
                            })
                        }
                        (Some(n), Some(local_sheet_id)) if !n.starts_with("_xlnm.") => {
                            Some(DefinedNameCapture::LocalUser {
                                local_sheet_id,
                                name: n,
                            })
                        }
                        _ => None,
                    };
                    cur_refers.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if cur_name.is_some() => cur_refers.push_str(&text_of(&t)),
            Ok(Event::GeneralRef(reference)) if cur_name.is_some() => {
                append_general_ref(&mut cur_refers, &reference);
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"definedName" => {
                if let Some(name) = cur_name.take() {
                    match name {
                        DefinedNameCapture::GlobalUser(name) => {
                            defined_names.push((name, std::mem::take(&mut cur_refers)));
                        }
                        DefinedNameCapture::LocalUser {
                            local_sheet_id,
                            name,
                        } => raw_local_defined_names.push((
                            local_sheet_id,
                            name,
                            std::mem::take(&mut cur_refers),
                        )),
                        DefinedNameCapture::LocalBuiltin {
                            local_sheet_id,
                            name,
                        } => {
                            sheet_defined_names.push(SheetDefinedName {
                                local_sheet_id,
                                name,
                                refers_to: std::mem::take(&mut cur_refers),
                            });
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    let local_defined_names = raw_local_defined_names
        .into_iter()
        .filter_map(|(sheet_index, name, refers_to)| {
            sheets
                .get(sheet_index)
                .map(|sheet| crate::LocalDefinedName {
                    sheet: sheet.name.clone(),
                    name,
                    refers_to,
                })
        })
        .collect();
    ParsedWorkbook {
        sheets,
        date1904,
        structure_protected,
        active_sheet,
        defined_names,
        local_defined_names,
        sheet_defined_names,
    }
}

/// Read a ZIP entry to a UTF-8 string (shared with the `.xlsb` reader).
#[cfg(feature = "xlsb")]
pub(crate) fn part_str(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    name: &str,
) -> Option<String> {
    part(zip, name)
}

/// `xl/_rels/workbook.xml.rels`: `<Relationship Id Target/>`. Shared with `.xlsb`.
pub(crate) fn parse_rels(xml: &str) -> HashMap<String, String> {
    if !crate::xml_reference_work_within_budget(xml) {
        return HashMap::new();
    }
    let mut r = Reader::from_str(xml);
    let mut map = HashMap::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if local(e.name().as_ref()) == b"Relationship" =>
            {
                if let (Some(id), Some(target)) = (attr(&e, b"Id"), attr(&e, b"Target")) {
                    map.insert(id, target);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    map
}

pub(crate) fn parse_rel_types(xml: &str) -> HashMap<String, String> {
    if !crate::xml_reference_work_within_budget(xml) {
        return HashMap::new();
    }
    let mut r = Reader::from_str(xml);
    let mut map = HashMap::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if local(e.name().as_ref()) == b"Relationship" =>
            {
                if let (Some(id), Some(ty)) = (attr(&e, b"Id"), attr(&e, b"Type")) {
                    map.insert(id, ty);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    map
}

/// Find the `commentsN.xml` target in a worksheet's rels by its relationship
/// `Type` (`.../officeDocument/2006/relationships/comments`). Returns the raw
/// `Target` (typically `../comments1.xml`), to be resolved against the worksheet
/// path by [`normalize_part_target`].
fn comments_target(xml: &str) -> Option<String> {
    let mut r = Reader::from_str(xml);
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if local(e.name().as_ref()) == b"Relationship" =>
            {
                let is_comments =
                    attr(&e, b"Type").is_some_and(|t| t.rsplit('/').next() == Some("comments"));
                if is_comments {
                    if let Some(target) = attr(&e, b"Target") {
                        return Some(target);
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// Collect every `table{N}.xml` target in a worksheet's rels by its relationship
/// `Type` (`.../officeDocument/2006/relationships/table`). Returns the raw
/// `Target`s (typically `../tables/table1.xml`), each to be resolved against the
/// worksheet path by [`normalize_part_target`].
fn table_targets(xml: &str) -> Vec<String> {
    let mut r = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if local(e.name().as_ref()) == b"Relationship" =>
            {
                let is_table =
                    attr(&e, b"Type").is_some_and(|t| t.rsplit('/').next() == Some("table"));
                if is_table {
                    if let Some(target) = attr(&e, b"Target") {
                        out.push(target);
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// `xl/tables/table{N}.xml`: `<table name displayName ref="A1:C3">` with a
/// `<tableColumns><tableColumn name/>…>` list. Parses into a [`Table`] (range,
/// name, header column names, style). Returns `None` if the `ref` is unparseable.
fn drawing_target(xml: &str) -> Option<String> {
    let mut r = Reader::from_str(xml);
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if local(e.name().as_ref()) == b"Relationship" =>
            {
                let is_drawing =
                    attr(&e, b"Type").is_some_and(|t| t.rsplit('/').next() == Some("drawing"));
                if is_drawing {
                    if let Some(target) = attr(&e, b"Target") {
                        return Some(target);
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

const MAX_XLSX_DRAWINGS: usize = 16_384;
const MAX_XLSX_DRAWING_TEXT: usize = 4_096;
const MAX_XLSX_DRAWING_NUMBER_TEXT: usize = 128;

struct DrawingRef {
    kind: DrawingRefKind,
    rid: Option<String>,
    from: (u32, u16),
    to: Option<(u32, u16)>,
    metadata: DrawingMetadata,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DrawingRefKind {
    Image,
    Chart,
    Shape,
}

#[derive(Clone, Copy)]
enum AnchorSection {
    From,
    To,
}

#[derive(Clone, Copy)]
enum AnchorField {
    Row,
    Col,
    RowOffset,
    ColOffset,
}

fn add_drawing_loss(losses: &mut Vec<StyleLoss>, kind: StyleLossKind, occurrences: u32) {
    if occurrences == 0 {
        return;
    }
    if let Some(loss) = losses.iter_mut().find(|loss| loss.kind == kind) {
        loss.occurrences = loss.occurrences.saturating_add(occurrences);
    } else {
        losses.push(StyleLoss { kind, occurrences });
    }
}

fn truncate_drawing_text(value: &mut String, max: usize) -> bool {
    if value.len() <= max {
        return false;
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    true
}

fn bounded_drawing_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    losses: &mut Vec<StyleLoss>,
) -> Option<String> {
    attr(e, key).map(|mut value| {
        if truncate_drawing_text(&mut value, MAX_XLSX_DRAWING_TEXT) {
            add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
        }
        value
    })
}

fn append_bounded_drawing_ref(
    out: &mut String,
    reference: &BytesRef<'_>,
    max: usize,
    losses: &mut Vec<StyleLoss>,
) {
    if out.len() >= max {
        add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
        return;
    }
    match reference.resolve_char_ref() {
        Ok(Some(ch)) => out.push(ch),
        Ok(None) => {
            if let Ok(name) = reference.decode() {
                if let Some(value) = quick_xml::escape::resolve_xml_entity(&name) {
                    out.push_str(value);
                }
            }
        }
        Err(_) => {}
    }
    if truncate_drawing_text(out, max) {
        add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
    }
}

fn drawing_anchor_behavior(
    element: &[u8],
    e: &quick_xml::events::BytesStart<'_>,
) -> DrawingAnchorBehavior {
    match element {
        b"absoluteAnchor" => DrawingAnchorBehavior::Absolute,
        b"oneCellAnchor" => DrawingAnchorBehavior::MoveOnly,
        b"twoCellAnchor" => match attr(e, b"editAs").as_deref() {
            Some("absolute") => DrawingAnchorBehavior::Absolute,
            Some("oneCell") => DrawingAnchorBehavior::MoveOnly,
            _ => DrawingAnchorBehavior::MoveAndSize,
        },
        _ => DrawingAnchorBehavior::MoveAndSize,
    }
}

fn drawing_crop(e: &quick_xml::events::BytesStart<'_>) -> DrawingCrop {
    let edge = |name| {
        attr(e, name)
            .and_then(|value| value.parse::<u32>().ok())
            .map(|value| value.saturating_mul(10).min(1_000_000))
            .unwrap_or(0)
    };
    DrawingCrop {
        left_ppm: edge(b"l"),
        top_ppm: edge(b"t"),
        right_ppm: edge(b"r"),
        bottom_ppm: edge(b"b"),
    }
}

fn parse_drawing_refs_bounded(xml: &str, losses: &mut Vec<StyleLoss>) -> Vec<DrawingRef> {
    const XLSX_MAX_ROW: i64 = 1_048_575;
    const XLSX_MAX_COL: i64 = 16_383;

    let mut r = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut current: Option<DrawingRef> = None;
    let mut anchor_depth = 0usize;
    let mut anchor_requires_from = false;
    let mut anchor_requires_to = false;
    let mut section: Option<AnchorSection> = None;
    let mut field: Option<AnchorField> = None;
    let mut field_text = String::new();
    let mut from_row_seen = false;
    let mut from_col_seen = false;
    let mut to_row_seen = false;
    let mut to_col_seen = false;
    let mut from_offset = (0i64, 0i64);
    let mut to_offset = (0i64, 0i64);
    let mut from_row_offset_seen = false;
    let mut from_col_offset_seen = false;
    let mut to_row_offset_seen = false;
    let mut to_col_offset_seen = false;
    let mut desc_depth = 0usize;
    let mut desc_text = String::new();

    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let local_name = local(name.as_ref());
                if matches!(
                    local_name,
                    b"twoCellAnchor" | b"oneCellAnchor" | b"absoluteAnchor"
                ) {
                    if current.is_some() {
                        anchor_depth = anchor_depth.saturating_add(1);
                        add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        continue;
                    }
                    if out.len() >= MAX_XLSX_DRAWINGS {
                        add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                        break;
                    }
                    anchor_depth = 1;
                    anchor_requires_from = local_name != b"absoluteAnchor";
                    anchor_requires_to = local_name == b"twoCellAnchor";
                    current = Some(DrawingRef {
                        kind: DrawingRefKind::Shape,
                        rid: None,
                        from: (0, 0),
                        to: None,
                        metadata: DrawingMetadata {
                            behavior: drawing_anchor_behavior(local_name, &e),
                            z_order: Some(out.len().min(i32::MAX as usize) as i32),
                            ..Default::default()
                        },
                    });
                    section = None;
                    field = None;
                    field_text.clear();
                    from_row_seen = false;
                    from_col_seen = false;
                    to_row_seen = false;
                    to_col_seen = false;
                    from_offset = (0, 0);
                    to_offset = (0, 0);
                    from_row_offset_seen = false;
                    from_col_offset_seen = false;
                    to_row_offset_seen = false;
                    to_col_offset_seen = false;
                    desc_depth = 0;
                    desc_text.clear();
                    continue;
                }
                if current.is_none() || anchor_depth > 1 {
                    continue;
                }
                match local_name {
                    b"from" => section = Some(AnchorSection::From),
                    b"to" => section = Some(AnchorSection::To),
                    b"row" => {
                        field = Some(AnchorField::Row);
                        field_text.clear();
                    }
                    b"col" => {
                        field = Some(AnchorField::Col);
                        field_text.clear();
                    }
                    b"rowOff" => {
                        field = Some(AnchorField::RowOffset);
                        field_text.clear();
                    }
                    b"colOff" => {
                        field = Some(AnchorField::ColOffset);
                        field_text.clear();
                    }
                    b"pic" => current.as_mut().expect("drawing").kind = DrawingRefKind::Image,
                    b"blip" => {
                        let item = current.as_mut().expect("drawing");
                        if item.kind == DrawingRefKind::Image && item.rid.is_none() {
                            item.rid = bounded_drawing_attr(&e, b"embed", losses);
                        }
                    }
                    b"chart" => {
                        let item = current.as_mut().expect("drawing");
                        item.kind = DrawingRefKind::Chart;
                        if item.rid.is_none() {
                            item.rid = bounded_drawing_attr(&e, b"id", losses);
                        }
                    }
                    b"cNvPr" => {
                        let item = current.as_mut().expect("drawing");
                        if item.metadata.name.is_none() {
                            item.metadata.name = bounded_drawing_attr(&e, b"name", losses);
                        }
                        if item.metadata.alt_text.is_none() {
                            item.metadata.alt_text = bounded_drawing_attr(&e, b"descr", losses)
                                .or_else(|| bounded_drawing_attr(&e, b"title", losses));
                        }
                    }
                    b"xfrm" => {
                        current.as_mut().expect("drawing").metadata.rotation_mdeg =
                            attr(&e, b"rot")
                                .and_then(|value| value.parse::<i32>().ok())
                                .map(|value| value / 60);
                    }
                    b"ext"
                        if !anchor_requires_to
                            || current.as_ref().is_some_and(|item| {
                                item.metadata.behavior != DrawingAnchorBehavior::MoveAndSize
                            }) =>
                    {
                        let width = attr(&e, b"cx").and_then(|value| value.parse::<u64>().ok());
                        let height = attr(&e, b"cy").and_then(|value| value.parse::<u64>().ok());
                        let item = current.as_mut().expect("drawing");
                        if item.metadata.absolute_size_emu.is_none() {
                            item.metadata.absolute_size_emu = width.zip(height);
                        }
                        if width.is_some() ^ height.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                    }
                    b"pos" => {
                        let x = attr(&e, b"x").and_then(|value| value.parse::<i64>().ok());
                        let y = attr(&e, b"y").and_then(|value| value.parse::<i64>().ok());
                        current.as_mut().expect("drawing").metadata.from_offset_emu = x.zip(y);
                        if x.is_some() ^ y.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                    }
                    b"srcRect" => {
                        current.as_mut().expect("drawing").metadata.crop = Some(drawing_crop(&e));
                    }
                    b"desc" => {
                        desc_depth = 1;
                        desc_text.clear();
                    }
                    _ if desc_depth > 0 => desc_depth += 1,
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) if current.is_some() && anchor_depth == 1 => {
                match local(e.name().as_ref()) {
                    b"pic" => current.as_mut().expect("drawing").kind = DrawingRefKind::Image,
                    b"blip" => {
                        let item = current.as_mut().expect("drawing");
                        if item.kind == DrawingRefKind::Image && item.rid.is_none() {
                            item.rid = bounded_drawing_attr(&e, b"embed", losses);
                        }
                    }
                    b"chart" => {
                        let item = current.as_mut().expect("drawing");
                        item.kind = DrawingRefKind::Chart;
                        if item.rid.is_none() {
                            item.rid = bounded_drawing_attr(&e, b"id", losses);
                        }
                    }
                    b"cNvPr" => {
                        let item = current.as_mut().expect("drawing");
                        if item.metadata.name.is_none() {
                            item.metadata.name = bounded_drawing_attr(&e, b"name", losses);
                        }
                        if item.metadata.alt_text.is_none() {
                            item.metadata.alt_text = bounded_drawing_attr(&e, b"descr", losses)
                                .or_else(|| bounded_drawing_attr(&e, b"title", losses));
                        }
                    }
                    b"xfrm" => {
                        current.as_mut().expect("drawing").metadata.rotation_mdeg =
                            attr(&e, b"rot")
                                .and_then(|value| value.parse::<i32>().ok())
                                .map(|value| value / 60);
                    }
                    b"ext"
                        if !anchor_requires_to
                            || current.as_ref().is_some_and(|item| {
                                item.metadata.behavior != DrawingAnchorBehavior::MoveAndSize
                            }) =>
                    {
                        let width = attr(&e, b"cx").and_then(|value| value.parse::<u64>().ok());
                        let height = attr(&e, b"cy").and_then(|value| value.parse::<u64>().ok());
                        let item = current.as_mut().expect("drawing");
                        if item.metadata.absolute_size_emu.is_none() {
                            item.metadata.absolute_size_emu = width.zip(height);
                        }
                        if width.is_some() ^ height.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                    }
                    b"pos" => {
                        let x = attr(&e, b"x").and_then(|value| value.parse::<i64>().ok());
                        let y = attr(&e, b"y").and_then(|value| value.parse::<i64>().ok());
                        current.as_mut().expect("drawing").metadata.from_offset_emu = x.zip(y);
                        if x.is_some() ^ y.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                    }
                    b"srcRect" => {
                        current.as_mut().expect("drawing").metadata.crop = Some(drawing_crop(&e));
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) if field.is_some() => {
                field_text.push_str(&text_of(&t));
                if truncate_drawing_text(&mut field_text, MAX_XLSX_DRAWING_NUMBER_TEXT) {
                    add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            Ok(Event::Text(t)) if desc_depth > 0 => {
                desc_text.push_str(&text_of(&t));
                if truncate_drawing_text(&mut desc_text, MAX_XLSX_DRAWING_TEXT) {
                    add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            Ok(Event::GeneralRef(reference)) if field.is_some() => {
                append_bounded_drawing_ref(
                    &mut field_text,
                    &reference,
                    MAX_XLSX_DRAWING_NUMBER_TEXT,
                    losses,
                );
            }
            Ok(Event::GeneralRef(reference)) if desc_depth > 0 => {
                append_bounded_drawing_ref(
                    &mut desc_text,
                    &reference,
                    MAX_XLSX_DRAWING_TEXT,
                    losses,
                );
            }
            Ok(Event::CData(t)) if field.is_some() => {
                field_text.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                if truncate_drawing_text(&mut field_text, MAX_XLSX_DRAWING_NUMBER_TEXT) {
                    add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            Ok(Event::CData(t)) if desc_depth > 0 => {
                desc_text.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                if truncate_drawing_text(&mut desc_text, MAX_XLSX_DRAWING_TEXT) {
                    add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let local_name = local(name.as_ref());
                if matches!(
                    local_name,
                    b"twoCellAnchor" | b"oneCellAnchor" | b"absoluteAnchor"
                ) && current.is_some()
                {
                    if anchor_depth > 1 {
                        anchor_depth -= 1;
                        continue;
                    }
                    if let Some(mut item) = current.take() {
                        if from_row_offset_seen || from_col_offset_seen {
                            item.metadata.from_offset_emu = Some(from_offset);
                            if from_row_offset_seen ^ from_col_offset_seen {
                                add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                            }
                        }
                        if to_row_offset_seen || to_col_offset_seen {
                            item.metadata.to_offset_emu = Some(to_offset);
                            if to_row_offset_seen ^ to_col_offset_seen {
                                add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                            }
                        }
                        if anchor_requires_from && !(from_row_seen && from_col_seen) {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                        if anchor_requires_to && !(to_row_seen && to_col_seen) {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                        if from_row_seen && from_col_seen {
                            item.metadata.from_cell = Some(item.from);
                        }
                        if to_row_seen && to_col_seen {
                            item.metadata.to_cell = item.to;
                        }
                        out.push(item);
                    }
                    anchor_depth = 0;
                    section = None;
                    field = None;
                    desc_depth = 0;
                    continue;
                }
                if current.is_none() || anchor_depth > 1 {
                    continue;
                }
                match local_name {
                    b"row" | b"col" | b"rowOff" | b"colOff" => {
                        if let (Some(section), Some(field), Ok(value)) =
                            (section, field, field_text.trim().parse::<i64>())
                        {
                            let item = current.as_mut().expect("drawing");
                            match (section, field) {
                                (AnchorSection::From, AnchorField::Row) => {
                                    item.from.0 = value.clamp(0, XLSX_MAX_ROW) as u32;
                                    from_row_seen = true;
                                }
                                (AnchorSection::From, AnchorField::Col) => {
                                    item.from.1 = value.clamp(0, XLSX_MAX_COL) as u16;
                                    from_col_seen = true;
                                }
                                (AnchorSection::To, AnchorField::Row) => {
                                    item.to.get_or_insert((0, 0)).0 =
                                        value.clamp(0, XLSX_MAX_ROW) as u32;
                                    to_row_seen = true;
                                }
                                (AnchorSection::To, AnchorField::Col) => {
                                    item.to.get_or_insert((0, 0)).1 =
                                        value.clamp(0, XLSX_MAX_COL) as u16;
                                    to_col_seen = true;
                                }
                                (AnchorSection::From, AnchorField::RowOffset) => {
                                    from_offset.1 = value;
                                    from_row_offset_seen = true;
                                }
                                (AnchorSection::From, AnchorField::ColOffset) => {
                                    from_offset.0 = value;
                                    from_col_offset_seen = true;
                                }
                                (AnchorSection::To, AnchorField::RowOffset) => {
                                    to_offset.1 = value;
                                    to_row_offset_seen = true;
                                }
                                (AnchorSection::To, AnchorField::ColOffset) => {
                                    to_offset.0 = value;
                                    to_col_offset_seen = true;
                                }
                            }
                        } else if field.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                        field = None;
                        field_text.clear();
                    }
                    b"from" | b"to" => section = None,
                    b"desc" if desc_depth > 0 => {
                        if current
                            .as_ref()
                            .expect("drawing")
                            .metadata
                            .alt_text
                            .is_none()
                            && !desc_text.trim().is_empty()
                        {
                            current.as_mut().expect("drawing").metadata.alt_text =
                                Some(desc_text.trim().to_string());
                        }
                        desc_depth = 0;
                    }
                    _ if desc_depth > 0 => desc_depth -= 1,
                    _ => {}
                }
            }
            Ok(Event::Eof) => {
                if current.is_some() {
                    add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                }
                break;
            }
            Err(_) => {
                add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                break;
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
fn parse_drawing_refs(xml: &str) -> Vec<DrawingRef> {
    parse_drawing_refs_bounded(xml, &mut Vec::new())
}

fn image_format(path: &str) -> Option<ImageFmt> {
    match path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some(ImageFmt::Png),
        Some("jpg" | "jpeg") => Some(ImageFmt::Jpeg),
        _ => None,
    }
}

enum DrawingPartRead {
    Missing,
    LimitExceeded,
    Data(Vec<u8>),
}

fn drawing_part_bytes(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    path: &str,
    max: u64,
) -> DrawingPartRead {
    let Some(index) = part_index(zip, path) else {
        return DrawingPartRead::Missing;
    };
    let Ok(file) = zip.by_index(index) else {
        return DrawingPartRead::Missing;
    };
    if file.size() > max {
        return DrawingPartRead::LimitExceeded;
    }
    let mut data = Vec::new();
    if file
        .take(max.saturating_add(1))
        .read_to_end(&mut data)
        .is_err()
    {
        return DrawingPartRead::Missing;
    }
    if data.len() as u64 > max {
        DrawingPartRead::LimitExceeded
    } else {
        DrawingPartRead::Data(data)
    }
}

fn retain_unrepresented_drawing(mut sidecar: DrawingMetadata, metadata: &mut Vec<DrawingMetadata>) {
    sidecar.kind = DrawingObjectKind::Shape;
    sidecar.object_index = 0;
    metadata.push(sidecar);
}

type DrawingReadResult = (Vec<Image>, Vec<Chart>, Vec<DrawingMetadata>, Vec<StyleLoss>);

fn read_sheet_drawings(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    sheet_path: &str,
    sheet_rels_xml: Option<&str>,
    theme: &ThemeColors,
) -> DrawingReadResult {
    const MAX_IMAGE_PART: u64 = 64 << 20;
    const MAX_IMAGE_TOTAL: usize = 256 << 20;
    let Some(drawing_target) = sheet_rels_xml.and_then(drawing_target) else {
        return (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    };
    let drawing_path = normalize_part_target(sheet_path, &drawing_target);
    let Some(drawing_xml) = part(zip, &drawing_path) else {
        return (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![StyleLoss {
                kind: StyleLossKind::DrawingMetadataPartial,
                occurrences: 1,
            }],
        );
    };
    let mut losses = Vec::new();
    let refs = parse_drawing_refs_bounded(&drawing_xml, &mut losses);
    let drawing_rels = part(zip, &sheet_rels_path(&drawing_path))
        .map(|s| parse_rels(&s))
        .unwrap_or_default();
    let mut images = Vec::new();
    let mut charts = Vec::new();
    let mut metadata = Vec::new();
    let mut image_bytes = 0usize;
    let mut chart_cache_points_remaining = MAX_XLSX_CHART_CACHE_POINTS_PER_SHEET;
    let mut chart_series_remaining = MAX_XLSX_CHART_SERIES_PER_SHEET;

    for drawing in refs {
        match drawing.kind {
            DrawingRefKind::Image => {
                let Some(target) = drawing.rid.as_ref().and_then(|rid| drawing_rels.get(rid))
                else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                    continue;
                };
                let media_path = normalize_part_target(&drawing_path, target);
                let Some(format) = image_format(&media_path) else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    continue;
                };
                let data = match drawing_part_bytes(zip, &media_path, MAX_IMAGE_PART) {
                    DrawingPartRead::Data(data) => data,
                    DrawingPartRead::Missing => {
                        retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                        add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                        continue;
                    }
                    DrawingPartRead::LimitExceeded => {
                        retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                        add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                        continue;
                    }
                };
                if image_bytes.saturating_add(data.len()) > MAX_IMAGE_TOTAL {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                image_bytes += data.len();
                let index = images.len();
                images.push(Image {
                    data,
                    format,
                    from: drawing.from,
                    to: drawing.to,
                });
                let mut sidecar = drawing.metadata;
                sidecar.kind = DrawingObjectKind::Image;
                sidecar.object_index = index;
                metadata.push(sidecar);
            }
            DrawingRefKind::Chart => {
                let Some(target) = drawing.rid.as_ref().and_then(|rid| drawing_rels.get(rid))
                else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                    continue;
                };
                let chart_path = normalize_part_target(&drawing_path, target);
                let Some(chart_xml) = part(zip, &chart_path) else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                    continue;
                };
                let Some(parsed) = parse_chart_with_theme(
                    &chart_xml,
                    drawing.from,
                    drawing.to.unwrap_or(drawing.from),
                    &mut chart_cache_points_remaining,
                    &mut chart_series_remaining,
                    theme,
                ) else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    continue;
                };
                let has_unsupported_chart_content = !parsed.unsupported_reasons.is_empty()
                    || parsed
                        .series_styles
                        .iter()
                        .any(|style| !style.losses.is_empty());
                let index = charts.len();
                charts.push(parsed.chart);
                let mut sidecar = drawing.metadata;
                sidecar.kind = DrawingObjectKind::Chart;
                sidecar.object_index = index;
                sidecar.chart_palette = theme.chart_palette();
                sidecar.chart_series_caches = parsed.series_caches;
                sidecar.chart_series_styles = parsed.series_styles;
                sidecar.chart_unsupported_reasons = parsed.unsupported_reasons;
                sidecar.chart_bar_direction = parsed.bar_direction;
                metadata.push(sidecar);
                if parsed.limit_exceeded {
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                }
                if has_unsupported_chart_content {
                    add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                }
            }
            DrawingRefKind::Shape => {
                retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
            }
        }
    }

    (images, charts, metadata, losses)
}

#[derive(Default)]
struct ParsedChartSeries {
    name: Option<String>,
    categories: Option<String>,
    values: Option<String>,
    bubble_sizes: Option<String>,
    cache: ChartSeriesCache,
    style: ChartSeriesStyle,
}

const MAX_XLSX_CHART_SERIES_PER_SHEET: usize = 4_096;
const MAX_XLSX_CHART_CACHE_POINTS_PER_SHEET: usize = 1_000_000;
const MAX_XLSX_CHART_CACHE_VALUE_BYTES: usize = 4_096;

pub(crate) struct ParsedChart {
    pub(crate) chart: Chart,
    pub(crate) series_caches: Vec<ChartSeriesCache>,
    pub(crate) series_styles: Vec<ChartSeriesStyle>,
    pub(crate) limit_exceeded: bool,
    pub(crate) unsupported_reasons: Vec<ChartUnsupportedReason>,
    pub(crate) bar_direction: ChartBarDirection,
}

#[derive(Clone, Copy)]
enum ChartSeriesField {
    Name,
    Categories,
    Values,
    BubbleSizes,
}

#[derive(Clone, Copy)]
enum ChartAxisContext {
    Category,
    Value,
}

#[derive(Clone, Copy)]
enum ChartTitleTarget {
    Main,
    XAxis,
    YAxis,
}

#[allow(clippy::too_many_arguments)]
fn append_chart_text(
    current_series: &mut Option<ParsedChartSeries>,
    capture_series_field: Option<ChartSeriesField>,
    capture_cache_value: bool,
    cache_value: &mut String,
    title_target: Option<ChartTitleTarget>,
    in_title_text: bool,
    title_text: &mut String,
    text: &str,
    limit_exceeded: &mut bool,
    cache_value_valid: &mut bool,
) {
    if capture_cache_value {
        let remaining = MAX_XLSX_CHART_CACHE_VALUE_BYTES.saturating_sub(cache_value.len());
        if text.len() <= remaining {
            cache_value.push_str(text);
        } else {
            *limit_exceeded = true;
            *cache_value_valid = false;
        }
    } else if let Some(field) = capture_series_field {
        if let Some(series) = current_series.as_mut() {
            let slot = match field {
                ChartSeriesField::Name => &mut series.name,
                ChartSeriesField::Categories => &mut series.categories,
                ChartSeriesField::Values => &mut series.values,
                ChartSeriesField::BubbleSizes => &mut series.bubble_sizes,
            };
            slot.get_or_insert_with(String::new).push_str(text);
        }
    } else if title_target.is_some() && in_title_text {
        title_text.push_str(text);
    }
}

fn chart_cache_points_mut(
    cache: &mut ChartSeriesCache,
    field: ChartSeriesField,
) -> &mut Vec<ChartCachedPoint> {
    match field {
        ChartSeriesField::Name => &mut cache.name,
        ChartSeriesField::Categories => &mut cache.categories,
        ChartSeriesField::Values => &mut cache.values,
        ChartSeriesField::BubbleSizes => &mut cache.bubble_sizes,
    }
}

fn chart_kind_element(name: &[u8]) -> Option<ChartKind> {
    match name {
        b"barChart" => Some(ChartKind::Bar),
        b"lineChart" => Some(ChartKind::Line),
        b"pieChart" => Some(ChartKind::Pie),
        b"scatterChart" => Some(ChartKind::Scatter),
        b"areaChart" => Some(ChartKind::Area),
        b"doughnutChart" => Some(ChartKind::Doughnut),
        b"radarChart" => Some(ChartKind::Radar),
        b"bubbleChart" => Some(ChartKind::Bubble),
        _ => None,
    }
}

fn chart_3d_kind_element(name: &[u8]) -> Option<ChartKind> {
    match name {
        b"bar3DChart" => Some(ChartKind::Bar),
        b"line3DChart" => Some(ChartKind::Line),
        b"pie3DChart" => Some(ChartKind::Pie),
        b"area3DChart" => Some(ChartKind::Area),
        _ => None,
    }
}

fn add_chart_unsupported(
    reasons: &mut Vec<ChartUnsupportedReason>,
    reason: ChartUnsupportedReason,
) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

fn add_chart_series_style_loss(style: &mut ChartSeriesStyle, loss: ChartSeriesStyleLossKind) {
    if !style.losses.contains(&loss) {
        style.losses.push(loss);
    }
}

fn retain_chart_marker_symbol(style: &mut ChartSeriesStyle, value: Option<&str>) {
    style.marker = match value {
        Some("none") => ChartMarkerSymbol::None,
        Some("circle") => ChartMarkerSymbol::Circle,
        Some("square") => ChartMarkerSymbol::Square,
        Some("diamond") => ChartMarkerSymbol::Diamond,
        Some("triangle") => ChartMarkerSymbol::Triangle,
        Some("auto") | None => ChartMarkerSymbol::Automatic,
        Some(_) => {
            add_chart_series_style_loss(style, ChartSeriesStyleLossKind::UnsupportedMarkerSymbol);
            ChartMarkerSymbol::Automatic
        }
    };
}

fn retain_chart_marker_size(style: &mut ChartSeriesStyle, value: Option<&str>) {
    match value.and_then(|value| value.parse::<u8>().ok()) {
        Some(size @ 2..=72) => style.marker_size = Some(size),
        _ => add_chart_series_style_loss(style, ChartSeriesStyleLossKind::InvalidMarkerSize),
    }
}

fn chart_series_line_color(
    element: &[u8],
    value: Option<&str>,
    theme: &ThemeColors,
) -> Option<Color> {
    match element {
        b"srgbClr" => value.and_then(parse_color),
        b"sysClr" => value.and_then(parse_color),
        element => {
            let name = if element == b"schemeClr" {
                value?.as_bytes()
            } else {
                element
            };
            theme_color_slot(name)
                .and_then(|slot| theme.color(slot, None))
                .or_else(|| {
                    let index = match name {
                        b"accent1" => 0,
                        b"accent2" => 1,
                        b"accent3" => 2,
                        b"accent4" => 3,
                        b"accent5" => 4,
                        b"accent6" => 5,
                        _ => return None,
                    };
                    theme.chart_palette().get(index).copied()
                })
        }
    }
}

fn observe_chart_kind(
    kind: &mut Option<ChartKind>,
    next: ChartKind,
    reasons: &mut Vec<ChartUnsupportedReason>,
) {
    match *kind {
        Some(previous) if previous != next => {
            add_chart_unsupported(reasons, ChartUnsupportedReason::Combo);
        }
        None => *kind = Some(next),
        _ => {}
    }
}

fn is_external_chart_reference(reference: &str) -> bool {
    let Some(open) = reference.find('[') else {
        return false;
    };
    let Some(close) = reference[open + 1..]
        .find(']')
        .map(|index| index + open + 1)
    else {
        return false;
    };
    reference[close + 1..].contains('!')
}

#[cfg(any(test, feature = "xlsb"))]
pub(crate) fn parse_chart(
    xml: &str,
    from: (u32, u16),
    to: (u32, u16),
    chart_cache_points_remaining: &mut usize,
    chart_series_remaining: &mut usize,
) -> Option<ParsedChart> {
    parse_chart_with_theme(
        xml,
        from,
        to,
        chart_cache_points_remaining,
        chart_series_remaining,
        &ThemeColors::default(),
    )
}

fn parse_chart_with_theme(
    xml: &str,
    from: (u32, u16),
    to: (u32, u16),
    chart_cache_points_remaining: &mut usize,
    chart_series_remaining: &mut usize,
    theme: &ThemeColors,
) -> Option<ParsedChart> {
    let mut r = Reader::from_str(xml);
    let mut kind: Option<ChartKind> = None;
    let mut title: Option<String> = None;
    let mut x_axis_title: Option<String> = None;
    let mut y_axis_title: Option<String> = None;
    let mut title_text = String::new();
    let mut title_target: Option<ChartTitleTarget> = None;
    let mut in_title_text = false;
    let mut legend = false;
    let mut data_labels = false;
    let mut series = Vec::new();
    let mut series_caches = Vec::new();
    let mut series_styles = Vec::new();
    let mut current_series: Option<ParsedChartSeries> = None;
    let mut series_field: Option<ChartSeriesField> = None;
    let mut capture_series_field: Option<ChartSeriesField> = None;
    let mut series_cache_depth = 0usize;
    let mut cache_field: Option<ChartSeriesField> = None;
    let mut cache_point_index: Option<u32> = None;
    let mut cache_value = String::new();
    let mut cache_value_valid = true;
    let mut capture_cache_value = false;
    let mut limit_exceeded = false;
    let mut unsupported_reasons = Vec::new();
    let mut bar_direction = ChartBarDirection::Column;
    let mut bar_chart_depth = 0usize;
    let mut axis_context: Option<ChartAxisContext> = None;
    let mut val_axis_count = 0usize;
    let mut marker_depth = 0usize;
    let mut data_point_depth = 0usize;
    let mut trendline_depth = 0usize;
    let mut error_bars_depth = 0usize;
    let mut series_shape_depth = 0usize;
    let mut series_line_depth = 0usize;
    let mut series_line_solid_fill_depth = 0usize;

    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                name if chart_kind_element(name).is_some() => {
                    let observed = chart_kind_element(name).expect("guarded chart kind");
                    observe_chart_kind(&mut kind, observed, &mut unsupported_reasons);
                    if observed == ChartKind::Bar {
                        bar_chart_depth = bar_chart_depth.saturating_add(1);
                    }
                }
                name if chart_3d_kind_element(name).is_some() => {
                    observe_chart_kind(
                        &mut kind,
                        chart_3d_kind_element(name).expect("guarded 3-D chart kind"),
                        &mut unsupported_reasons,
                    );
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::ThreeDimensional,
                    );
                }
                b"stockChart" | b"surfaceChart" | b"surface3DChart" | b"ofPieChart" => {
                    let fallback = match local(e.name().as_ref()) {
                        b"stockChart" => ChartKind::Line,
                        b"ofPieChart" => ChartKind::Pie,
                        _ => ChartKind::Area,
                    };
                    observe_chart_kind(&mut kind, fallback, &mut unsupported_reasons);
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedKind,
                    );
                    if local(e.name().as_ref()) == b"surface3DChart" {
                        add_chart_unsupported(
                            &mut unsupported_reasons,
                            ChartUnsupportedReason::ThreeDimensional,
                        );
                    }
                }
                b"view3D" | b"bubble3D" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::ThreeDimensional,
                ),
                b"pivotSource" => {
                    add_chart_unsupported(&mut unsupported_reasons, ChartUnsupportedReason::Pivot)
                }
                b"externalData" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::ExternalData,
                ),
                b"barDir" if bar_chart_depth > 0 => {
                    bar_direction = if attr(&e, b"val").as_deref() == Some("bar") {
                        ChartBarDirection::Horizontal
                    } else {
                        ChartBarDirection::Column
                    };
                }
                b"catAx" | b"dateAx" => axis_context = Some(ChartAxisContext::Category),
                b"valAx" => {
                    val_axis_count += 1;
                    axis_context = if matches!(kind, Some(ChartKind::Scatter | ChartKind::Bubble))
                        && val_axis_count == 1
                    {
                        Some(ChartAxisContext::Category)
                    } else {
                        Some(ChartAxisContext::Value)
                    };
                }
                b"title" if current_series.is_none() => {
                    let target = match axis_context {
                        Some(ChartAxisContext::Category) if x_axis_title.is_none() => {
                            Some(ChartTitleTarget::XAxis)
                        }
                        Some(ChartAxisContext::Value) if y_axis_title.is_none() => {
                            Some(ChartTitleTarget::YAxis)
                        }
                        None if title.is_none() => Some(ChartTitleTarget::Main),
                        _ => None,
                    };
                    if let Some(target) = target {
                        title_target = Some(target);
                        title_text.clear();
                    }
                }
                b"legend" => legend = true,
                b"dLbls" => data_labels = true,
                b"ser" => {
                    current_series = Some(ParsedChartSeries::default());
                    series_field = None;
                    capture_series_field = None;
                    series_cache_depth = 0;
                    marker_depth = 0;
                    data_point_depth = 0;
                    trendline_depth = 0;
                    error_bars_depth = 0;
                    series_shape_depth = 0;
                    series_line_depth = 0;
                    series_line_solid_fill_depth = 0;
                }
                b"marker" if current_series.is_some() => marker_depth = 1,
                b"dPt" if current_series.is_some() => data_point_depth = 1,
                b"trendline" if current_series.is_some() => trendline_depth = 1,
                b"errBars" if current_series.is_some() => error_bars_depth = 1,
                b"spPr"
                    if current_series.is_some()
                        && marker_depth == 0
                        && data_point_depth == 0
                        && trendline_depth == 0
                        && error_bars_depth == 0
                        && series_shape_depth == 0 =>
                {
                    series_shape_depth = 1;
                }
                b"ln" if series_shape_depth > 0 => series_line_depth = 1,
                b"solidFill" if series_line_depth > 0 => {
                    series_line_solid_fill_depth = 1;
                    if let Some(series) = current_series.as_mut() {
                        series.style.line_visible = true;
                    }
                }
                b"noFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        series.style.line_visible = false;
                        series.style.line_color = None;
                    }
                }
                b"srgbClr" | b"schemeClr" if series_line_solid_fill_depth > 0 => {
                    let qualified_name = e.name();
                    let name = local(qualified_name.as_ref());
                    if let Some(series) = current_series.as_mut() {
                        let value = attr(&e, b"val");
                        if let Some(color) = chart_series_line_color(name, value.as_deref(), theme)
                        {
                            series.style.line_color = Some(color);
                        } else {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"sysClr" if series_line_solid_fill_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        let value = attr(&e, b"lastClr");
                        if let Some(color) =
                            chart_series_line_color(b"sysClr", value.as_deref(), theme)
                        {
                            series.style.line_color = Some(color);
                        } else {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"gradFill" | b"pattFill" | b"blipFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"prstDash" if series_line_depth > 0 => {
                    if attr(&e, b"val").as_deref() != Some("solid") {
                        if let Some(series) = current_series.as_mut() {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"tint" | b"shade" | b"lumMod" | b"lumOff" if series_line_solid_fill_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"symbol" if marker_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        retain_chart_marker_symbol(&mut series.style, attr(&e, b"val").as_deref());
                    }
                }
                b"size" if marker_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        retain_chart_marker_size(&mut series.style, attr(&e, b"val").as_deref());
                    }
                }
                b"tx" if current_series.is_some() => series_field = Some(ChartSeriesField::Name),
                b"cat" | b"xVal" if current_series.is_some() => {
                    series_field = Some(ChartSeriesField::Categories);
                }
                b"val" | b"yVal" if current_series.is_some() => {
                    series_field = Some(ChartSeriesField::Values);
                }
                b"bubbleSize" if current_series.is_some() => {
                    series_field = Some(ChartSeriesField::BubbleSizes);
                }
                b"strCache" | b"numCache" | b"strLit" | b"numLit" if current_series.is_some() => {
                    if series_cache_depth == 0 {
                        cache_field = series_field;
                    }
                    series_cache_depth += 1;
                }
                b"multiLvlStrCache" if current_series.is_some() => {
                    // Multi-level categories cannot be represented faithfully by
                    // the flat public Series API. Keep the A1 reference and
                    // deliberately leave this cache unusable.
                    if series_cache_depth == 0 {
                        cache_field = None;
                    }
                    series_cache_depth += 1;
                }
                b"pt" if current_series.is_some() && series_cache_depth > 0 => {
                    cache_point_index = attr(&e, b"idx").and_then(|value| value.parse().ok());
                    cache_value.clear();
                    cache_value_valid = true;
                }
                b"f" if current_series.is_some() => {
                    capture_series_field = series_field;
                }
                b"v" if current_series.is_some()
                    && series_cache_depth > 0
                    && cache_point_index.is_some() =>
                {
                    capture_cache_value = true;
                }
                b"v" if current_series.is_some() && series_cache_depth == 0 => {
                    capture_series_field = series_field;
                }
                b"t" | b"v" if title_target.is_some() => in_title_text = true,
                _ => {}
            },
            Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                name if chart_kind_element(name).is_some() => observe_chart_kind(
                    &mut kind,
                    chart_kind_element(name).expect("guarded chart kind"),
                    &mut unsupported_reasons,
                ),
                name if chart_3d_kind_element(name).is_some() => {
                    observe_chart_kind(
                        &mut kind,
                        chart_3d_kind_element(name).expect("guarded 3-D chart kind"),
                        &mut unsupported_reasons,
                    );
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::ThreeDimensional,
                    );
                }
                b"stockChart" | b"surfaceChart" | b"surface3DChart" | b"ofPieChart" => {
                    let qualified_name = e.name();
                    let name = local(qualified_name.as_ref());
                    let fallback = match name {
                        b"stockChart" => ChartKind::Line,
                        b"ofPieChart" => ChartKind::Pie,
                        _ => ChartKind::Area,
                    };
                    observe_chart_kind(&mut kind, fallback, &mut unsupported_reasons);
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedKind,
                    );
                    if name == b"surface3DChart" {
                        add_chart_unsupported(
                            &mut unsupported_reasons,
                            ChartUnsupportedReason::ThreeDimensional,
                        );
                    }
                }
                b"view3D" | b"bubble3D" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::ThreeDimensional,
                ),
                b"pivotSource" => {
                    add_chart_unsupported(&mut unsupported_reasons, ChartUnsupportedReason::Pivot)
                }
                b"externalData" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::ExternalData,
                ),
                b"barDir" if bar_chart_depth > 0 => {
                    bar_direction = if attr(&e, b"val").as_deref() == Some("bar") {
                        ChartBarDirection::Horizontal
                    } else {
                        ChartBarDirection::Column
                    };
                }
                b"legend" => legend = true,
                b"dLbls" => data_labels = true,
                b"symbol" if marker_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        retain_chart_marker_symbol(&mut series.style, attr(&e, b"val").as_deref());
                    }
                }
                b"size" if marker_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        retain_chart_marker_size(&mut series.style, attr(&e, b"val").as_deref());
                    }
                }
                b"noFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        series.style.line_visible = false;
                        series.style.line_color = None;
                    }
                }
                b"srgbClr" | b"schemeClr" if series_line_solid_fill_depth > 0 => {
                    let qualified_name = e.name();
                    let name = local(qualified_name.as_ref());
                    if let Some(series) = current_series.as_mut() {
                        let value = attr(&e, b"val");
                        if let Some(color) = chart_series_line_color(name, value.as_deref(), theme)
                        {
                            series.style.line_color = Some(color);
                        } else {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"sysClr" if series_line_solid_fill_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        let value = attr(&e, b"lastClr");
                        if let Some(color) =
                            chart_series_line_color(b"sysClr", value.as_deref(), theme)
                        {
                            series.style.line_color = Some(color);
                        } else {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"gradFill" | b"pattFill" | b"blipFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"prstDash" if series_line_depth > 0 => {
                    if attr(&e, b"val").as_deref() != Some("solid") {
                        if let Some(series) = current_series.as_mut() {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"tint" | b"shade" | b"lumMod" | b"lumOff" if series_line_solid_fill_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                _ => {}
            },
            Ok(Event::Text(t)) => {
                append_chart_text(
                    &mut current_series,
                    capture_series_field,
                    capture_cache_value,
                    &mut cache_value,
                    title_target,
                    in_title_text,
                    &mut title_text,
                    &text_of(&t),
                    &mut limit_exceeded,
                    &mut cache_value_valid,
                );
            }
            Ok(Event::GeneralRef(reference)) => {
                with_general_ref_text(&reference, |text| {
                    append_chart_text(
                        &mut current_series,
                        capture_series_field,
                        capture_cache_value,
                        &mut cache_value,
                        title_target,
                        in_title_text,
                        &mut title_text,
                        text,
                        &mut limit_exceeded,
                        &mut cache_value_valid,
                    );
                });
            }
            Ok(Event::CData(t)) => {
                let text = String::from_utf8_lossy(t.into_inner().as_ref()).into_owned();
                append_chart_text(
                    &mut current_series,
                    capture_series_field,
                    capture_cache_value,
                    &mut cache_value,
                    title_target,
                    in_title_text,
                    &mut title_text,
                    &text,
                    &mut limit_exceeded,
                    &mut cache_value_valid,
                );
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"barChart" if bar_chart_depth > 0 => {
                    bar_chart_depth -= 1;
                }
                b"marker" if marker_depth > 0 => marker_depth = 0,
                b"dPt" if data_point_depth > 0 => data_point_depth = 0,
                b"trendline" if trendline_depth > 0 => trendline_depth = 0,
                b"errBars" if error_bars_depth > 0 => error_bars_depth = 0,
                b"solidFill" if series_line_solid_fill_depth > 0 => {
                    series_line_solid_fill_depth = 0;
                }
                b"ln" if series_line_depth > 0 => {
                    series_line_depth = 0;
                    series_line_solid_fill_depth = 0;
                }
                b"spPr" if series_shape_depth > 0 => {
                    series_shape_depth = 0;
                    series_line_depth = 0;
                    series_line_solid_fill_depth = 0;
                }
                b"v" if capture_cache_value => capture_cache_value = false,
                b"t" | b"v" if in_title_text => in_title_text = false,
                b"title" if title_target.is_some() => {
                    let text = title_text.trim();
                    if !text.is_empty() {
                        match title_target.expect("title target checked above") {
                            ChartTitleTarget::Main => title = Some(text.to_string()),
                            ChartTitleTarget::XAxis => x_axis_title = Some(text.to_string()),
                            ChartTitleTarget::YAxis => y_axis_title = Some(text.to_string()),
                        }
                    }
                    title_target = None;
                    in_title_text = false;
                    title_text.clear();
                }
                b"catAx" | b"dateAx" | b"valAx" => axis_context = None,
                b"f" | b"v" if capture_series_field.is_some() => capture_series_field = None,
                b"pt" if series_cache_depth > 0 => {
                    if cache_value_valid {
                        if let (Some(field), Some(index), Some(parsed)) =
                            (cache_field, cache_point_index, current_series.as_mut())
                        {
                            if *chart_cache_points_remaining == 0 {
                                limit_exceeded = true;
                            } else {
                                chart_cache_points_mut(&mut parsed.cache, field).push(
                                    ChartCachedPoint {
                                        index,
                                        value: std::mem::take(&mut cache_value),
                                    },
                                );
                                *chart_cache_points_remaining -= 1;
                            }
                        }
                    }
                    cache_point_index = None;
                    cache_value.clear();
                    cache_value_valid = true;
                    capture_cache_value = false;
                }
                b"strCache" | b"numCache" | b"strLit" | b"numLit" | b"multiLvlStrCache"
                    if series_cache_depth > 0 =>
                {
                    series_cache_depth -= 1;
                    if series_cache_depth == 0 {
                        cache_field = None;
                        cache_point_index = None;
                        cache_value.clear();
                        cache_value_valid = true;
                        capture_cache_value = false;
                    }
                }
                b"tx" | b"cat" | b"xVal" | b"val" | b"yVal" | b"bubbleSize"
                    if current_series.is_some() =>
                {
                    series_field = None;
                }
                b"ser" => {
                    if let Some(parsed) = current_series.take() {
                        if [
                            parsed.name.as_deref(),
                            parsed.categories.as_deref(),
                            parsed.values.as_deref(),
                            parsed.bubble_sizes.as_deref(),
                        ]
                        .into_iter()
                        .flatten()
                        .any(is_external_chart_reference)
                        {
                            add_chart_unsupported(
                                &mut unsupported_reasons,
                                ChartUnsupportedReason::ExternalData,
                            );
                        }
                        if let Some(values) = parsed.values {
                            if *chart_series_remaining > 0 {
                                series.push(Series {
                                    name: parsed.name,
                                    categories: parsed.categories,
                                    values,
                                    bubble_sizes: parsed.bubble_sizes,
                                });
                                series_caches.push(parsed.cache);
                                series_styles.push(parsed.style);
                                *chart_series_remaining -= 1;
                            } else {
                                limit_exceeded = true;
                            }
                        }
                    }
                    series_field = None;
                    capture_series_field = None;
                    series_cache_depth = 0;
                    cache_field = None;
                    cache_point_index = None;
                    cache_value.clear();
                    cache_value_valid = true;
                    capture_cache_value = false;
                    marker_depth = 0;
                    data_point_depth = 0;
                    trendline_depth = 0;
                    error_bars_depth = 0;
                    series_shape_depth = 0;
                    series_line_depth = 0;
                    series_line_solid_fill_depth = 0;
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    Some(ParsedChart {
        chart: Chart {
            kind: kind?,
            title,
            series,
            legend,
            data_labels,
            x_axis_title,
            y_axis_title,
            from,
            to,
        },
        series_caches,
        series_styles,
        limit_exceeded,
        unsupported_reasons,
        bar_direction,
    })
}

#[derive(Debug)]
struct ParsedTable {
    table: Table,
    application: TableStyleApplication,
    losses: Vec<StyleLoss>,
}

fn table_bool_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    default: bool,
    losses: &mut Vec<StyleLoss>,
) -> bool {
    match attr(e, key) {
        Some(value) => parse_bool_attr(&value).unwrap_or_else(|| {
            add_differential_loss(losses, StyleLossKind::UnsupportedProperty, 1);
            default
        }),
        None => default,
    }
}

fn table_single_row_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    default: bool,
    losses: &mut Vec<StyleLoss>,
) -> bool {
    match attr(e, key) {
        None => default,
        Some(value) => match value.parse::<u32>() {
            Ok(0) => false,
            Ok(1) => true,
            Ok(_) => {
                add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                true
            }
            Err(_) => {
                add_differential_loss(losses, StyleLossKind::UnsupportedProperty, 1);
                default
            }
        },
    }
}

fn parse_table(xml: &str) -> Option<ParsedTable> {
    const MAX_TABLE_COLUMNS: usize = 16_384;
    let mut r = Reader::from_str(xml);
    let mut range: Option<(u32, u16, u32, u16)> = None;
    // Prefer `displayName`, falling back to `name`.
    let mut display_name: Option<String> = None;
    let mut name: Option<String> = None;
    let mut style: Option<String> = None;
    let mut columns: Vec<String> = Vec::new();
    let mut application = TableStyleApplication::default();
    let mut losses = Vec::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"table" => {
                    range = attr(&e, b"ref").as_deref().and_then(parse_range);
                    display_name = attr(&e, b"displayName");
                    name = attr(&e, b"name");
                    application.header_row =
                        table_single_row_attr(&e, b"headerRowCount", true, &mut losses);
                    let totals_count =
                        table_single_row_attr(&e, b"totalsRowCount", false, &mut losses);
                    let totals_shown = table_bool_attr(&e, b"totalsRowShown", false, &mut losses);
                    application.totals_row = totals_count || totals_shown;
                }
                b"tableColumn" => {
                    if let Some(n) = attr(&e, b"name") {
                        if columns.len() < MAX_TABLE_COLUMNS {
                            columns.push(n);
                        } else {
                            add_differential_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                        }
                    }
                }
                b"tableStyleInfo" => {
                    style = attr(&e, b"name");
                    application.show_first_column =
                        table_bool_attr(&e, b"showFirstColumn", false, &mut losses);
                    application.show_last_column =
                        table_bool_attr(&e, b"showLastColumn", false, &mut losses);
                    application.show_row_stripes =
                        table_bool_attr(&e, b"showRowStripes", false, &mut losses);
                    application.show_column_stripes =
                        table_bool_attr(&e, b"showColumnStripes", false, &mut losses);
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    Some(ParsedTable {
        table: Table {
            range: range?,
            name: display_name.or(name).unwrap_or_default(),
            columns,
            style,
        },
        application,
        losses,
    })
}

/// Resolve a rels `Target` (relative to the source part's directory) to a ZIP
/// part path. A leading `/` is workbook-root-absolute; otherwise the target is
/// relative to the directory of `base` (the worksheet path), resolving any
/// leading `../` segments. E.g. base `xl/worksheets/sheet1.xml` + target
/// `../comments1.xml` → `xl/comments1.xml`.
fn normalize_part_target(base: &str, target: &str) -> String {
    let base = base.replace('\\', "/");
    let target = target.replace('\\', "/");
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    // Directory segments of the base part (everything before the final `/`).
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

/// `xl/comments{N}.xml`: an `<authors><author>` table followed by a
/// `<commentList>` of `<comment ref authorId>` notes, whose body is the
/// concatenated `<text>…<t>text</t>…</text>` runs. Resolves each comment's
/// `authorId` against the authors table.
fn parse_comments(xml: &str) -> Vec<Comment> {
    let mut r = Reader::from_str(xml);
    let mut authors: Vec<String> = Vec::new();
    let mut out: Vec<Comment> = Vec::new();
    let mut in_authors = false;
    let mut in_author = false;
    let mut cur_author = String::new();
    // Current `<comment>` capture.
    let mut cur_rc: Option<(u32, u16)> = None;
    let mut cur_author_id: usize = 0;
    let mut cur_text = String::new();
    let mut in_t = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"authors" => in_authors = true,
                b"author" => {
                    in_author = true;
                    cur_author.clear();
                }
                b"comment" => {
                    cur_rc = attr(&e, b"ref").as_deref().and_then(parse_ref);
                    cur_author_id = attr(&e, b"authorId")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    cur_text.clear();
                }
                // `<t>` runs only count inside a `<comment>`'s `<text>`; the
                // authors table has no `<t>`, so a plain `in_t` flag suffices.
                b"t" if cur_rc.is_some() => in_t = true,
                _ => {}
            },
            Ok(Event::Text(t)) if in_author => cur_author.push_str(&text_of(&t)),
            Ok(Event::Text(t)) if in_t => cur_text.push_str(&text_of(&t)),
            Ok(Event::GeneralRef(reference)) if in_author => {
                append_general_ref(&mut cur_author, &reference);
            }
            Ok(Event::GeneralRef(reference)) if in_t => {
                append_general_ref(&mut cur_text, &reference);
            }
            Ok(Event::CData(t)) if in_t => {
                cur_text.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"authors" => in_authors = false,
                b"author" => {
                    if in_authors {
                        authors.push(std::mem::take(&mut cur_author));
                    }
                    in_author = false;
                }
                b"t" => in_t = false,
                b"comment" => {
                    if let Some((row, col)) = cur_rc.take() {
                        let author = authors
                            .get(cur_author_id)
                            .filter(|a| !a.is_empty())
                            .cloned();
                        out.push(Comment {
                            row,
                            col,
                            text: std::mem::take(&mut cur_text),
                            author,
                        });
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// A1-style reference → 0-based `(row, col)`.
fn parse_ref(r: &str) -> Option<(u32, u16)> {
    let mut col: u32 = 0;
    let mut row: u32 = 0;
    let mut seen_col = false;
    let mut seen_row = false;
    for c in r.chars() {
        if c.is_ascii_alphabetic() {
            if seen_row {
                return None;
            }
            // Checked arithmetic: a hostile ref like `ZZZZZZZ1` must not overflow
            // (the crate's panic-free contract).
            col = col
                .checked_mul(26)?
                .checked_add(c.to_ascii_uppercase() as u32 - 'A' as u32 + 1)?;
            seen_col = true;
        } else if c.is_ascii_digit() {
            row = row.checked_mul(10)?.checked_add(c as u32 - '0' as u32)?;
            seen_row = true;
        }
    }
    // Reject anything past Excel's grid (XFD = col 16384 1-based, 1048576 rows).
    if !seen_col || !seen_row || col == 0 || row == 0 || col > 16384 || row > 1_048_576 {
        return None;
    }
    Some((row - 1, u16::try_from(col - 1).ok()?))
}

/// `A1:C3` (or a lone `A1`) → `(first_row, first_col, last_row, last_col)`.
fn parse_range(s: &str) -> Option<(u32, u16, u32, u16)> {
    let mut it = s.split(':');
    let first = parse_ref(it.next()?)?;
    let last = match it.next() {
        Some(r) => parse_ref(r)?,
        None => first,
    };
    Some((first.0, first.1, last.0, last.1))
}

/// Convert a 0-based column index to A1 letters (0→`A`, 25→`Z`, 26→`AA`).
fn col_letters(mut idx: u32) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (idx % 26) as u8);
        if idx < 26 {
            break;
        }
        idx = idx / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

/// Parse 1–3 A1 column letters to a 0-based index.
fn letters_col(s: &[char]) -> Option<u32> {
    if s.is_empty() || s.len() > 3 {
        return None;
    }
    let mut idx: u32 = 0;
    for &c in s {
        if !c.is_ascii_uppercase() {
            return None;
        }
        idx = idx
            .checked_mul(26)?
            .checked_add(c as u32 - 'A' as u32 + 1)?;
    }
    Some(idx - 1)
}

/// Try to read an A1 cell reference at `ch[start]` and return `(chars_consumed,
/// shifted_ref)` after shifting its relative parts by `(drow, dcol)`. `$`-absolute
/// parts are unchanged; a ref shifted off-grid becomes `#REF!`. Returns `None` if
/// there is no reference here (identifier char before, `(`/alnum after → a function
/// name like `LOG10(` or a larger token, not a reference).
fn try_shift_ref(ch: &[char], start: usize, drow: i64, dcol: i64) -> Option<(usize, String)> {
    if !token_boundary_before(ch, start) {
        return None;
    }
    let mut i = start;
    let col_abs = ch.get(i) == Some(&'$');
    if col_abs {
        i += 1;
    }
    let lstart = i;
    while i < ch.len() && ch[i].is_ascii_uppercase() && i - lstart < 3 {
        i += 1;
    }
    let letters = &ch[lstart..i];
    if letters.is_empty() {
        return None;
    }
    let row_abs = ch.get(i) == Some(&'$');
    if row_abs {
        i += 1;
    }
    let dstart = i;
    while i < ch.len() && ch[i].is_ascii_digit() {
        i += 1;
    }
    if i == dstart {
        return None;
    }
    if !token_boundary_after(ch, i) {
        return None;
    }
    let col = letters_col(letters)?;
    let row: u32 = ch[dstart..i].iter().collect::<String>().parse().ok()?;
    // The *original* token must itself be an in-grid reference; an A1-shaped name
    // outside the grid (e.g. `XFE1`, col > XFD) is not a cell ref — leave it verbatim.
    if row == 0 || col > 16383 || row > 1_048_576 {
        return None;
    }
    let new_col = if col_abs {
        col as i64
    } else {
        col as i64 + dcol
    };
    let new_row = if row_abs {
        row as i64
    } else {
        row as i64 + drow
    };
    if !(0..=16383).contains(&new_col) || !(1..=1_048_576).contains(&new_row) {
        return Some((i - start, "#REF!".to_string()));
    }
    let mut out = String::new();
    if col_abs {
        out.push('$');
    }
    out.push_str(&col_letters(new_col as u32));
    if row_abs {
        out.push('$');
    }
    out.push_str(&new_row.to_string());
    Some((i - start, out))
}

fn token_boundary_before(ch: &[char], start: usize) -> bool {
    start == 0 || {
        let p = ch[start - 1];
        !(p.is_ascii_alphanumeric() || p == '_')
    }
}

fn token_boundary_after(ch: &[char], end: usize) -> bool {
    !matches!(
        ch.get(end),
        Some(after) if *after == '(' || after.is_ascii_alphanumeric() || *after == '_'
    )
}

fn parse_whole_row_part(ch: &[char], mut i: usize) -> Option<(bool, u32, usize)> {
    let abs = ch.get(i) == Some(&'$');
    if abs {
        i += 1;
    }
    let start = i;
    while i < ch.len() && ch[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    let row: u32 = ch[start..i].iter().collect::<String>().parse().ok()?;
    if row == 0 || row > 1_048_576 {
        return None;
    }
    Some((abs, row, i))
}

fn shift_row_part(row: u32, abs: bool, drow: i64) -> Option<u32> {
    let shifted = if abs { row as i64 } else { row as i64 + drow };
    (1..=1_048_576).contains(&shifted).then_some(shifted as u32)
}

fn try_shift_whole_row_ref(ch: &[char], start: usize, drow: i64) -> Option<(usize, String)> {
    if !token_boundary_before(ch, start) {
        return None;
    }
    let (first_abs, first, mut i) = parse_whole_row_part(ch, start)?;
    if ch.get(i) != Some(&':') {
        return None;
    }
    i += 1;
    let (last_abs, last, end) = parse_whole_row_part(ch, i)?;
    if !token_boundary_after(ch, end) {
        return None;
    }
    let (Some(first), Some(last)) = (
        shift_row_part(first, first_abs, drow),
        shift_row_part(last, last_abs, drow),
    ) else {
        return Some((end - start, "#REF!".to_string()));
    };
    let mut out = String::new();
    if first_abs {
        out.push('$');
    }
    out.push_str(&first.to_string());
    out.push(':');
    if last_abs {
        out.push('$');
    }
    out.push_str(&last.to_string());
    Some((end - start, out))
}

fn parse_whole_col_part(ch: &[char], mut i: usize) -> Option<(bool, u32, usize)> {
    let abs = ch.get(i) == Some(&'$');
    if abs {
        i += 1;
    }
    let start = i;
    while i < ch.len() && ch[i].is_ascii_uppercase() && i - start < 3 {
        i += 1;
    }
    if i == start {
        return None;
    }
    let col = letters_col(&ch[start..i])?;
    if col > 16_383 {
        return None;
    }
    Some((abs, col, i))
}

fn shift_col_part(col: u32, abs: bool, dcol: i64) -> Option<u32> {
    let shifted = if abs { col as i64 } else { col as i64 + dcol };
    (0..=16_383).contains(&shifted).then_some(shifted as u32)
}

fn try_shift_whole_col_ref(ch: &[char], start: usize, dcol: i64) -> Option<(usize, String)> {
    if !token_boundary_before(ch, start) {
        return None;
    }
    let (first_abs, first, mut i) = parse_whole_col_part(ch, start)?;
    if ch.get(i) != Some(&':') {
        return None;
    }
    i += 1;
    let (last_abs, last, end) = parse_whole_col_part(ch, i)?;
    if !token_boundary_after(ch, end) {
        return None;
    }
    let (Some(first), Some(last)) = (
        shift_col_part(first, first_abs, dcol),
        shift_col_part(last, last_abs, dcol),
    ) else {
        return Some((end - start, "#REF!".to_string()));
    };
    let mut out = String::new();
    if first_abs {
        out.push('$');
    }
    out.push_str(&col_letters(first));
    out.push(':');
    if last_abs {
        out.push('$');
    }
    out.push_str(&col_letters(last));
    Some((end - start, out))
}

/// Shift the relative A1 references in a formula by `(drow, dcol)` — the core of
/// reconstructing a shared-formula follower from its master. References inside
/// `"…"` string literals and `'…'` quoted sheet names, and `$`-absolute parts, are
/// left unchanged; off-grid shifts become `#REF!`.
fn shift_formula(f: &str, drow: i64, dcol: i64) -> String {
    let ch: Vec<char> = f.chars().collect();
    let mut out = String::with_capacity(f.len());
    let mut i = 0;
    let mut in_string = false; // "…" string literal
    let mut in_quote = false; // '…' quoted sheet name
    while i < ch.len() {
        let c = ch[i];
        if c == '"' && !in_quote {
            in_string = !in_string;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\'' && !in_string {
            in_quote = !in_quote;
            out.push(c);
            i += 1;
            continue;
        }
        if !in_string && !in_quote {
            if let Some((consumed, shifted)) = try_shift_whole_row_ref(&ch, i, drow) {
                out.push_str(&shifted);
                i += consumed;
                continue;
            }
            if let Some((consumed, shifted)) = try_shift_whole_col_ref(&ch, i, dcol) {
                out.push_str(&shifted);
                i += consumed;
                continue;
            }
            if let Some((consumed, shifted)) = try_shift_ref(&ch, i, drow, dcol) {
                out.push_str(&shifted);
                i += consumed;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `<sheetData><row><c r t s><f>formula</f><v>…</v>|<is><t>…</t></is></c>` →
/// typed cells, plus the sheet's `<mergeCells>` ranges and the unresolved
/// `<hyperlinks>` as `(row, col, r:id)` (the caller resolves each `r:id` via the
/// worksheet rels).
#[derive(Debug, Default)]
struct ParsedSheet {
    cells: Vec<CellEntry>,
    direct_cell_formats: BTreeMap<(u32, u16), CellStyleOverlay>,
    rich: BTreeMap<(u32, u16), Vec<crate::TextRun>>,
    merges: Vec<(u32, u16, u32, u16)>,
    hyperlink_refs: Vec<(u32, u16, String)>,
    freeze: Option<(u32, u16)>,
    autofilter: Option<(u32, u16, u32, u16)>,
    data_validations: Vec<DataValidation>,
    cond_formats: Vec<CondFormat>,
    cond_format_metadata: Vec<ConditionalFormatMetadata>,
    page_setup: Option<PageSetup>,
    print_metadata: PrintMetadata,
    sparklines: Vec<Sparkline>,
    tab_color: Option<Color>,
    print_gridlines: bool,
    print_headings: bool,
    row_outline: BTreeMap<u32, u8>,
    col_outline: BTreeMap<u16, u8>,
    col_widths: BTreeMap<u16, f32>,
    row_heights: BTreeMap<u32, f32>,
    col_formats: BTreeMap<u16, CellStyle>,
    row_formats: BTreeMap<u32, CellStyle>,
    hidden_cols: BTreeSet<u16>,
    hidden_rows: BTreeSet<u32>,
    default_row_height: Option<f32>,
    default_col_width: Option<f32>,
    base_col_width: Option<f32>,
    collapsed_rows: BTreeSet<u32>,
    outline_summary_below: Option<bool>,
    outline_summary_right: Option<bool>,
    protect: bool,
    protect_options: Option<ProtectionOptions>,
    hide_gridlines: bool,
    zoom: Option<u16>,
    show_headers: Option<bool>,
    right_to_left: bool,
    tab_selected: bool,
}

type SheetRange = (u32, u16, u32, u16);
type ParsedDataValidation = (DataValidation, Vec<SheetRange>);

#[derive(Clone, Copy, Debug)]
enum HeaderFooterField {
    Header,
    Footer,
}

#[derive(Clone, Copy, Debug)]
struct HeaderFooterCapture {
    kind: HeaderFooterKind,
    legacy: Option<HeaderFooterField>,
}

#[derive(Clone, Copy, Debug)]
enum PageBreakAxis {
    Row,
    Column,
}

#[derive(Debug)]
enum PendingCfKind {
    CellIs {
        op: DvOp,
        fill: Color,
    },
    ColorScale,
    DataBar,
    TopBottom {
        rank: u32,
        bottom: bool,
        percent: bool,
        fill: Color,
    },
    AboveAverage {
        below: bool,
        fill: Color,
    },
    DuplicateValues {
        unique: bool,
        fill: Color,
    },
    Expression {
        fill: Color,
    },
}

#[derive(Debug)]
struct PendingCfRule {
    ranges: Vec<SheetRange>,
    kind: PendingCfKind,
    formulas: Vec<String>,
    colors: Vec<Color>,
    metadata: ConditionalFormatMetadata,
}

impl PendingCfRule {
    fn build_rule(&self) -> Option<CfRule> {
        match &self.kind {
            PendingCfKind::CellIs { op, fill } => Some(CfRule::CellIs {
                op: *op,
                formula1: self.formulas.first()?.clone(),
                formula2: self.formulas.get(1).filter(|s| !s.is_empty()).cloned(),
                fill: *fill,
            }),
            PendingCfKind::ColorScale => match self.colors.as_slice() {
                [min, max] => Some(CfRule::ColorScale2 {
                    min: *min,
                    max: *max,
                }),
                [min, mid, max, ..] => Some(CfRule::ColorScale3 {
                    min: *min,
                    mid: *mid,
                    max: *max,
                }),
                _ => None,
            },
            PendingCfKind::DataBar => self
                .colors
                .first()
                .copied()
                .map(|color| CfRule::DataBar { color }),
            PendingCfKind::TopBottom {
                rank,
                bottom,
                percent,
                fill,
            } => Some(CfRule::TopBottom {
                rank: *rank,
                bottom: *bottom,
                percent: *percent,
                fill: *fill,
            }),
            PendingCfKind::AboveAverage { below, fill } => Some(CfRule::AboveAverage {
                below: *below,
                fill: *fill,
            }),
            PendingCfKind::DuplicateValues { unique, fill } => Some(CfRule::DuplicateValues {
                unique: *unique,
                fill: *fill,
            }),
            PendingCfKind::Expression { fill } => Some(CfRule::Expression {
                formula: self.formulas.first()?.clone(),
                fill: *fill,
            }),
        }
    }
}

fn parse_sheet(
    xml: &str,
    shared: &[SharedString],
    styles: &Styles,
    theme: &ThemeColors,
    date1904: bool,
    budget: &mut usize,
) -> ParsedSheet {
    if !crate::xml_reference_work_within_budget(xml) {
        *budget = 0;
        return ParsedSheet::default();
    }
    let mut r = Reader::from_str(xml);
    let mut parsed = ParsedSheet::default();
    // Current cell state.
    let mut rc: Option<(u32, u16)> = None;
    let mut ctype = String::new();
    let mut style_idx = 0usize;
    let mut value = String::new();
    let mut inline_value = String::new();
    let mut inline_text_seen = false;
    let mut inline_run: Option<crate::TextRun> = None;
    let mut inline_runs = Vec::<crate::TextRun>::new();
    let mut formula = String::new();
    // Shared-formula state: si → (master formula text, base row, base col).
    let mut shared_masters: HashMap<u32, (String, u32, u16)> = HashMap::new();
    // Array-formula state: declared rectangular range → anchor formula text.
    let mut array_formulas: Vec<(SheetRange, String)> = Vec::new();
    let mut f_si: Option<u32> = None;
    let mut f_array_ref: Option<SheetRange> = None;
    let mut in_v = false;
    let mut in_f = false;
    let mut in_is_t = false;
    let mut in_rph = false; // East Asian phonetic (ruby) guide — excluded from value
    let mut current_dv: Option<DataValidation> = None;
    let mut current_dv_extra_ranges: Vec<SheetRange> = Vec::new();
    let mut in_dv_formula1 = false;
    let mut in_dv_formula2 = false;
    let mut current_cf_ranges: Vec<SheetRange> = Vec::new();
    let mut current_cf: Option<PendingCfRule> = None;
    let mut in_cf_formula = false;
    let mut header_footer_capture: Option<HeaderFooterCapture> = None;
    let mut page_break_axis: Option<PageBreakAxis> = None;
    let mut current_sparkline_kind = SparklineKind::Line;
    let mut current_sparkline_range = String::new();
    let mut current_sparkline_location = String::new();
    let mut in_sparkline = false;
    let mut in_sparkline_formula = false;
    let mut in_sparkline_sqref = false;
    // Implicit position tracking: the `r` attribute on `<row>`/`<c>` is optional
    // in [ISO/IEC 29500]; when omitted, position is implicit (cells fill
    // left-to-right, rows top-to-bottom). Some writers (LibreOffice, EPPlus, …)
    // omit it. Without this, every `r`-less cell would be dropped.
    let mut cur_row: u32 = 0;
    let mut cur_col: u16 = 0;
    let mut row_started = false;
    let mut selected_sheet_view_rank = 0u8;
    let mut in_selected_sheet_view = false;
    loop {
        match r.read_event() {
            // A self-closing `<f/>` (a shared-formula follower has no formula text)
            // must NOT open formula-text capture: otherwise pretty-printing
            // whitespace between `<f/>` and `<v>` is captured as the formula and the
            // follower is mis-registered as a master. Capture only formula metadata.
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == b"f" => {
                let formula_kind = attr(&e, b"t");
                f_si = if formula_kind.as_deref() == Some("shared") {
                    attr(&e, b"si").and_then(|s| s.parse::<u32>().ok())
                } else {
                    None
                };
                f_array_ref = if formula_kind.as_deref() == Some("array") {
                    attr(&e, b"ref").as_deref().and_then(parse_range)
                } else {
                    None
                };
                in_f = false;
            }
            Ok(Event::Empty(e))
                if matches!(
                    local(e.name().as_ref()),
                    b"dataValidation" | b"formula1" | b"formula2"
                ) => {}
            Ok(Event::Empty(e)) if header_footer_field(local(e.name().as_ref())).is_some() => {
                if let Some((kind, field, preferred)) =
                    header_footer_field(local(e.name().as_ref()))
                {
                    begin_header_footer_capture(&mut parsed, kind, field, preferred);
                }
            }
            Ok(Event::Empty(e))
                if matches!(local(e.name().as_ref()), b"rowBreaks" | b"colBreaks") =>
            {
                parsed.print_metadata.mark_source();
                page_break_axis = None;
            }
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == b"conditionalFormatting" => {
                push_current_conditional_format(&mut parsed, current_cf.take());
                current_cf_ranges.clear();
            }
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == b"cfRule" => {
                push_current_conditional_format(&mut parsed, current_cf.take());
                let current = parse_conditional_rule(&e, &current_cf_ranges, styles);
                push_current_conditional_format(&mut parsed, current);
            }
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"row" => {
                    match attr(&e, b"r").and_then(|s| s.parse::<u32>().ok()) {
                        Some(n) if n >= 1 => cur_row = n - 1,
                        _ if row_started => cur_row = cur_row.saturating_add(1),
                        _ => {}
                    }
                    row_started = true;
                    cur_col = 0;
                    if let Some(level) = attr(&e, b"outlineLevel")
                        .and_then(|s| s.parse::<u8>().ok())
                        .filter(|level| *level > 0)
                    {
                        parsed.row_outline.insert(cur_row, level);
                    }
                    if attr(&e, b"collapsed").as_deref().is_some_and(attr_true) {
                        parsed.collapsed_rows.insert(cur_row);
                    }
                    if let Some(height) = attr(&e, b"ht").and_then(|s| s.parse::<f32>().ok()) {
                        parsed.row_heights.insert(cur_row, height);
                    }
                    if attr(&e, b"hidden").as_deref().is_some_and(attr_true) {
                        parsed.hidden_rows.insert(cur_row);
                    }
                    if let Some(style) = attr(&e, b"s")
                        .and_then(|value| value.parse::<usize>().ok())
                        .and_then(|index| styles.cell_style(index))
                    {
                        parsed.row_formats.insert(cur_row, style.clone());
                    }
                }
                b"sheetFormatPr" => {
                    parsed.default_row_height =
                        attr(&e, b"defaultRowHeight").and_then(|s| s.parse::<f32>().ok());
                    parsed.default_col_width =
                        attr(&e, b"defaultColWidth").and_then(|s| s.parse::<f32>().ok());
                    parsed.base_col_width =
                        attr(&e, b"baseColWidth").and_then(|s| s.parse::<f32>().ok());
                }
                b"outlinePr" => {
                    if let Some(value) = attr(&e, b"summaryBelow")
                        .as_deref()
                        .and_then(parse_bool_attr)
                    {
                        parsed.outline_summary_below = Some(value);
                    }
                    if let Some(value) = attr(&e, b"summaryRight")
                        .as_deref()
                        .and_then(parse_bool_attr)
                    {
                        parsed.outline_summary_right = Some(value);
                    }
                }
                b"col" => {
                    let first = attr(&e, b"min")
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(1)
                        .max(1);
                    let last = attr(&e, b"max")
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(first)
                        .min(16_384);
                    if first <= last {
                        let width = attr(&e, b"width").and_then(|s| s.parse::<f32>().ok());
                        let hidden = attr(&e, b"hidden").as_deref().is_some_and(attr_true);
                        let style = attr(&e, b"style")
                            .and_then(|value| value.parse::<usize>().ok())
                            .and_then(|index| styles.cell_style(index));
                        for col in first..=last {
                            if let Ok(col) = u16::try_from(col - 1) {
                                if let Some(width) = width {
                                    parsed.col_widths.insert(col, width);
                                }
                                if hidden {
                                    parsed.hidden_cols.insert(col);
                                }
                                if let Some(style) = style {
                                    parsed.col_formats.insert(col, style.clone());
                                }
                            }
                        }
                    }
                    if let Some(level) = attr(&e, b"outlineLevel")
                        .and_then(|s| s.parse::<u8>().ok())
                        .filter(|level| *level > 0)
                    {
                        if first <= last {
                            for col in first..=last {
                                if let Ok(col) = u16::try_from(col - 1) {
                                    parsed.col_outline.insert(col, level);
                                }
                            }
                        }
                    }
                }
                b"sheetProtection" => {
                    if let Some(options) = parse_sheet_protection(&e) {
                        parsed.protect = true;
                        parsed.protect_options = options;
                    }
                }
                b"sheetView" => {
                    if attr(&e, b"tabSelected").as_deref().is_some_and(attr_true) {
                        parsed.tab_selected = true;
                    }
                    let rank = sheet_view_rank(&e);
                    in_selected_sheet_view = rank > selected_sheet_view_rank;
                    if in_selected_sheet_view {
                        selected_sheet_view_rank = rank;
                        clear_sheet_view_metadata(&mut parsed);
                        if attr(&e, b"showGridLines")
                            .as_deref()
                            .is_some_and(attr_false)
                        {
                            parsed.hide_gridlines = true;
                        }
                        if let Some(show_headers) = attr(&e, b"showRowColHeaders")
                            .as_deref()
                            .and_then(parse_bool_attr)
                        {
                            parsed.show_headers = Some(show_headers);
                        }
                        if attr(&e, b"rightToLeft").as_deref().is_some_and(attr_true) {
                            parsed.right_to_left = true;
                        }
                        if let Some(zoom) =
                            attr(&e, b"zoomScale").and_then(|s| s.parse::<u16>().ok())
                        {
                            parsed.zoom = Some(zoom);
                        }
                    }
                }
                b"pane"
                    if in_selected_sheet_view
                        && attr(&e, b"state")
                            .as_deref()
                            .is_some_and(|state| matches!(state, "frozen" | "frozenSplit")) =>
                {
                    let row = attr(&e, b"ySplit")
                        .as_deref()
                        .and_then(parse_split_u32)
                        .unwrap_or(0);
                    let col = attr(&e, b"xSplit")
                        .as_deref()
                        .and_then(parse_split_u16)
                        .unwrap_or(0);
                    if row > 0 || col > 0 {
                        parsed.freeze = Some((row, col));
                    }
                }
                b"tabColor" => {
                    parsed.tab_color = color_attr(&e, theme, &styles.indexed_colors);
                }
                b"c" => {
                    // Use the explicit `r` when present (and resync the implicit
                    // column to it); otherwise fall back to the running position.
                    let pos = match attr(&e, b"r").as_deref().and_then(parse_ref) {
                        Some((row, col)) => {
                            cur_row = row;
                            cur_col = col;
                            (row, col)
                        }
                        None => (cur_row, cur_col),
                    };
                    cur_col = cur_col.saturating_add(1);
                    rc = Some(pos);
                    ctype = attr(&e, b"t").unwrap_or_default();
                    style_idx = attr(&e, b"s").and_then(|s| s.parse().ok()).unwrap_or(0);
                    value.clear();
                    inline_value.clear();
                    inline_text_seen = false;
                    inline_run = None;
                    inline_runs.clear();
                    formula.clear();
                    f_si = None;
                    f_array_ref = None;
                    // Reset text-capture flags so a stray one (e.g. a self-closing
                    // `<f/>` that never fires an End) cannot leak into this cell.
                    (in_v, in_f, in_is_t, in_rph) = (false, false, false, false);
                }
                // A `<v>` is a sibling of `<f>`, never inside it: entering `<v>`
                // clears `in_f` so a self-closing `<f/>` (shared-formula follower,
                // no End event) can't capture the value text as formula text.
                b"v" => (in_v, in_f) = (true, false),
                b"f" if in_sparkline => in_sparkline_formula = true,
                b"f" => {
                    in_f = true; // formula text (sibling of <v>)
                                 // A shared formula carries `t="shared" si="N"`; the master also
                                 // has the formula text + a `ref`, followers are empty `<f/>`.
                    let formula_kind = attr(&e, b"t");
                    f_si = if formula_kind.as_deref() == Some("shared") {
                        attr(&e, b"si").and_then(|s| s.parse::<u32>().ok())
                    } else {
                        None
                    };
                    f_array_ref = if formula_kind.as_deref() == Some("array") {
                        attr(&e, b"ref").as_deref().and_then(parse_range)
                    } else {
                        None
                    };
                }
                b"rPh" => in_rph = true, // phonetic/ruby guide in an inline string
                b"r" if ctype == "inlineStr" && rc.is_some() && !in_rph => {
                    inline_run = Some(crate::TextRun::default());
                }
                b"rFont" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.name = attr(&e, b"val");
                }
                b"sz" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.size_pt = attr(&e, b"val")
                        .and_then(|value| value.parse::<f32>().ok())
                        .map(|value| value.round().clamp(1.0, f32::from(u16::MAX)) as u16);
                }
                b"color" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.color =
                        color_attr(&e, theme, &styles.indexed_colors);
                }
                b"b" if inline_run.is_some() => inline_run.as_mut().expect("run").font.bold = true,
                b"i" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.italic = true;
                }
                b"u" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.underline = true;
                }
                b"strike" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.strikethrough = true;
                }
                b"vertAlign" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.script =
                        match attr(&e, b"val").as_deref() {
                            Some("superscript") => FormatScript::Superscript,
                            Some("subscript") => FormatScript::Subscript,
                            _ => FormatScript::None,
                        };
                }
                b"t" => {
                    in_is_t = true; // inline-string text (within <is>)
                    if !in_rph {
                        inline_text_seen = true;
                    }
                }
                // `<mergeCell ref="A1:C3"/>` — usually self-closing (Empty), but
                // accept Start too.
                b"mergeCell" => {
                    if let Some(rng) = attr(&e, b"ref").as_deref().and_then(parse_range) {
                        parsed.merges.push(rng);
                    }
                }
                b"autoFilter" => {
                    if let Some(rng) = attr(&e, b"ref").as_deref().and_then(parse_range) {
                        parsed.autofilter = Some(rng);
                    }
                }
                b"printOptions" => {
                    let gridlines = print_bool_attr(&e, b"gridLines", &mut parsed.print_metadata);
                    let headings = print_bool_attr(&e, b"headings", &mut parsed.print_metadata);
                    let horizontal =
                        print_bool_attr(&e, b"horizontalCentered", &mut parsed.print_metadata);
                    let vertical =
                        print_bool_attr(&e, b"verticalCentered", &mut parsed.print_metadata);
                    parsed.print_metadata.set_print_gridlines(gridlines);
                    parsed.print_metadata.set_print_headings(headings);
                    parsed.print_metadata.set_center_horizontally(horizontal);
                    parsed.print_metadata.set_center_vertically(vertical);
                    if gridlines {
                        parsed.print_gridlines = true;
                    }
                    if headings {
                        parsed.print_headings = true;
                    }
                    if horizontal {
                        page_setup_mut(&mut parsed).center_horizontally = true;
                    }
                    if vertical {
                        page_setup_mut(&mut parsed).center_vertically = true;
                    }
                }
                b"pageMargins" => {
                    let margins = (
                        attr_f64(&e, b"left"),
                        attr_f64(&e, b"right"),
                        attr_f64(&e, b"top"),
                        attr_f64(&e, b"bottom"),
                        attr_f64(&e, b"header"),
                        attr_f64(&e, b"footer"),
                    );
                    if let (
                        Some(left),
                        Some(right),
                        Some(top),
                        Some(bottom),
                        Some(header),
                        Some(footer),
                    ) = margins
                    {
                        page_setup_mut(&mut parsed).margins =
                            Some((left, right, top, bottom, header, footer));
                    }
                }
                b"pageSetup" => {
                    match attr(&e, b"pageOrder").as_deref() {
                        Some("overThenDown") => parsed
                            .print_metadata
                            .set_page_order(PrintPageOrder::OverThenDown),
                        Some("downThenOver") | None => parsed
                            .print_metadata
                            .set_page_order(PrintPageOrder::DownThenOver),
                        Some(_) => parsed
                            .print_metadata
                            .add_loss(PrintLossKind::UnsupportedProperty),
                    }
                    let ps = page_setup_mut(&mut parsed);
                    ps.landscape = attr(&e, b"orientation")
                        .as_deref()
                        .is_some_and(|orientation| orientation.eq_ignore_ascii_case("landscape"));
                    ps.paper_size = attr_u16(&e, b"paperSize");
                    ps.scale = attr_u16(&e, b"scale");
                    ps.fit_to_width = attr_u16(&e, b"fitToWidth");
                    ps.fit_to_height = attr_u16(&e, b"fitToHeight");
                    ps.first_page_number = attr(&e, b"useFirstPageNumber")
                        .as_deref()
                        .is_some_and(attr_true)
                        .then(|| attr_u16(&e, b"firstPageNumber"))
                        .flatten();
                }
                b"headerFooter" => {
                    let different_odd_even = header_footer_bool_attr(
                        &e,
                        b"differentOddEven",
                        false,
                        &mut parsed.print_metadata,
                    );
                    let different_first = header_footer_bool_attr(
                        &e,
                        b"differentFirst",
                        false,
                        &mut parsed.print_metadata,
                    );
                    let scale_with_document = header_footer_bool_attr(
                        &e,
                        b"scaleWithDoc",
                        true,
                        &mut parsed.print_metadata,
                    );
                    let align_with_margins = header_footer_bool_attr(
                        &e,
                        b"alignWithMargins",
                        true,
                        &mut parsed.print_metadata,
                    );
                    parsed.print_metadata.set_header_footer_flag(
                        Some(different_odd_even),
                        Some(different_first),
                        Some(scale_with_document),
                        Some(align_with_margins),
                    );
                }
                b"oddHeader" | b"firstHeader" | b"evenHeader" | b"oddFooter" | b"firstFooter"
                | b"evenFooter" => {
                    if let Some((kind, field, preferred)) =
                        header_footer_field(local(e.name().as_ref()))
                    {
                        header_footer_capture = Some(begin_header_footer_capture(
                            &mut parsed,
                            kind,
                            field,
                            preferred,
                        ));
                    }
                }
                b"rowBreaks" => page_break_axis = Some(PageBreakAxis::Row),
                b"colBreaks" => page_break_axis = Some(PageBreakAxis::Column),
                b"brk" if page_break_axis.is_some() => {
                    parse_manual_page_break(&e, page_break_axis, &mut parsed.print_metadata);
                }
                b"sparklineGroup" => {
                    current_sparkline_kind = attr(&e, b"type")
                        .as_deref()
                        .map(parse_sparkline_kind)
                        .unwrap_or(SparklineKind::Line);
                }
                b"sparkline" => {
                    in_sparkline = true;
                    current_sparkline_range.clear();
                    current_sparkline_location.clear();
                }
                b"dataValidation" => {
                    push_current_data_validation(
                        &mut parsed,
                        current_dv.take(),
                        &mut current_dv_extra_ranges,
                    );
                    if let Some((dv, ranges)) = parse_data_validation(&e) {
                        current_dv = Some(dv);
                        current_dv_extra_ranges = ranges;
                    }
                }
                b"conditionalFormatting" => {
                    push_current_conditional_format(&mut parsed, current_cf.take());
                    current_cf_ranges = attr(&e, b"sqref")
                        .map(|sqref| sqref.split_whitespace().filter_map(parse_range).collect())
                        .unwrap_or_default();
                }
                b"cfRule" => {
                    push_current_conditional_format(&mut parsed, current_cf.take());
                    current_cf = parse_conditional_rule(&e, &current_cf_ranges, styles);
                }
                b"formula" if current_cf.is_some() => {
                    if let Some(cf) = current_cf.as_mut() {
                        cf.formulas.push(String::new());
                    }
                    in_cf_formula = true;
                }
                b"sqref" if in_sparkline => in_sparkline_sqref = true,
                b"color" if current_cf.is_some() => {
                    if let (Some(cf), Some(color)) = (
                        current_cf.as_mut(),
                        color_attr(&e, theme, &styles.indexed_colors),
                    ) {
                        cf.colors.push(color);
                    }
                }
                b"formula1" if current_dv.is_some() => in_dv_formula1 = true,
                b"formula2" if current_dv.is_some() => {
                    if let Some(dv) = current_dv.as_mut() {
                        dv.formula2.get_or_insert_with(String::new);
                    }
                    in_dv_formula2 = true;
                }
                // `<hyperlink ref="A1" r:id="rIdN"/>` — the `ref` may be a single
                // cell or a range; anchor at its top-left. The URL lives in the
                // worksheet rels (`r:id` → local "id"), resolved by the caller.
                b"hyperlink" => {
                    if let (Some((r0, c0, r1, c1)), Some(rid)) = (
                        attr(&e, b"ref").as_deref().and_then(parse_range),
                        attr(&e, b"id"),
                    ) {
                        // A `ref` may be a range (`A1:A3`) — surface every cell, not
                        // just the top-left, bounded so a whole-column ref can't
                        // amplify into millions of entries.
                        let mut n = 0usize;
                        'hl: for row in r0..=r1 {
                            for col in c0..=c1 {
                                if n >= (1 << 16) {
                                    break 'hl;
                                }
                                parsed.hyperlink_refs.push((row, col, rid.clone()));
                                n += 1;
                            }
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Text(t)) if header_footer_capture.is_some() => {
                if let Some(capture) = header_footer_capture {
                    append_header_footer_text(&mut parsed, capture, &text_of(&t));
                }
            }
            Ok(Event::Text(t)) if in_sparkline_formula => {
                current_sparkline_range.push_str(&text_of(&t));
            }
            Ok(Event::Text(t)) if in_sparkline_sqref => {
                current_sparkline_location.push_str(&text_of(&t));
            }
            Ok(Event::Text(t)) if in_cf_formula => {
                if let Some(formula) = current_cf.as_mut().and_then(|cf| cf.formulas.last_mut()) {
                    formula.push_str(&text_of(&t));
                }
            }
            Ok(Event::Text(t)) if in_dv_formula1 => {
                if let Some(dv) = current_dv.as_mut() {
                    dv.formula1.push_str(&text_of(&t));
                }
            }
            Ok(Event::Text(t)) if in_dv_formula2 => {
                if let Some(dv) = current_dv.as_mut() {
                    if let Some(formula2) = dv.formula2.as_mut() {
                        formula2.push_str(&text_of(&t));
                    }
                }
            }
            Ok(Event::Text(t)) if in_f => formula.push_str(&text_of(&t)),
            Ok(Event::Text(t)) if in_v => value.push_str(&text_of(&t)),
            Ok(Event::Text(t)) if in_is_t && !in_rph => {
                let text = text_of(&t);
                inline_value.push_str(&text);
                if let Some(run) = inline_run.as_mut() {
                    run.text.push_str(&text);
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                with_general_ref_text(&reference, |text| {
                    if let Some(capture) = header_footer_capture {
                        append_header_footer_text(&mut parsed, capture, text);
                    } else if in_sparkline_formula {
                        current_sparkline_range.push_str(text);
                    } else if in_sparkline_sqref {
                        current_sparkline_location.push_str(text);
                    } else if in_cf_formula {
                        if let Some(formula) =
                            current_cf.as_mut().and_then(|cf| cf.formulas.last_mut())
                        {
                            formula.push_str(text);
                        }
                    } else if in_dv_formula1 {
                        if let Some(dv) = current_dv.as_mut() {
                            dv.formula1.push_str(text);
                        }
                    } else if in_dv_formula2 {
                        if let Some(formula2) =
                            current_dv.as_mut().and_then(|dv| dv.formula2.as_mut())
                        {
                            formula2.push_str(text);
                        }
                    } else if in_f {
                        formula.push_str(text);
                    } else if in_v {
                        value.push_str(text);
                    } else if in_is_t && !in_rph {
                        inline_value.push_str(text);
                        if let Some(run) = inline_run.as_mut() {
                            run.text.push_str(text);
                        }
                    }
                });
            }
            Ok(Event::CData(t)) if header_footer_capture.is_some() => {
                if let Some(capture) = header_footer_capture {
                    let bytes = t.into_inner();
                    let text = String::from_utf8_lossy(bytes.as_ref());
                    append_header_footer_text(&mut parsed, capture, text.as_ref());
                }
            }
            Ok(Event::CData(t)) if in_sparkline_formula => {
                current_sparkline_range.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::CData(t)) if in_sparkline_sqref => {
                current_sparkline_location
                    .push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::CData(t)) if in_cf_formula => {
                if let Some(formula) = current_cf.as_mut().and_then(|cf| cf.formulas.last_mut()) {
                    formula.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                }
            }
            Ok(Event::CData(t)) if in_dv_formula1 => {
                if let Some(dv) = current_dv.as_mut() {
                    dv.formula1
                        .push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                }
            }
            Ok(Event::CData(t)) if in_dv_formula2 => {
                if let Some(dv) = current_dv.as_mut() {
                    if let Some(formula2) = dv.formula2.as_mut() {
                        formula2.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                    }
                }
            }
            Ok(Event::CData(t)) if in_v => {
                value.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::CData(t)) if in_is_t && !in_rph => {
                let bytes = t.into_inner();
                let text = String::from_utf8_lossy(bytes.as_ref());
                inline_value.push_str(&text);
                if let Some(run) = inline_run.as_mut() {
                    run.text.push_str(&text);
                }
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"v" => in_v = false,
                b"f" if in_sparkline_formula => in_sparkline_formula = false,
                b"f" => in_f = false,
                b"rPh" => in_rph = false,
                b"t" => in_is_t = false,
                b"r" if inline_run.is_some() => {
                    let completed = inline_run.take().expect("run");
                    if !completed.text.is_empty() {
                        inline_runs.push(completed);
                    }
                }
                b"sheetView" => in_selected_sheet_view = false,
                b"oddHeader" | b"firstHeader" | b"evenHeader" | b"oddFooter" | b"firstFooter"
                | b"evenFooter" => header_footer_capture = None,
                b"rowBreaks" | b"colBreaks" => page_break_axis = None,
                b"sqref" if in_sparkline_sqref => in_sparkline_sqref = false,
                b"sparkline" => {
                    in_sparkline = false;
                    if let Some(sparkline) = parse_sparkline(
                        current_sparkline_kind,
                        &current_sparkline_range,
                        &current_sparkline_location,
                    ) {
                        parsed.sparklines.push(sparkline);
                    }
                    current_sparkline_range.clear();
                    current_sparkline_location.clear();
                }
                b"sparklineGroup" => current_sparkline_kind = SparklineKind::Line,
                b"formula" => in_cf_formula = false,
                b"cfRule" => {
                    in_cf_formula = false;
                    push_current_conditional_format(&mut parsed, current_cf.take());
                }
                b"conditionalFormatting" => {
                    push_current_conditional_format(&mut parsed, current_cf.take());
                    current_cf_ranges.clear();
                }
                b"formula1" => in_dv_formula1 = false,
                b"formula2" => in_dv_formula2 = false,
                b"dataValidation" => {
                    in_dv_formula1 = false;
                    in_dv_formula2 = false;
                    push_current_data_validation(
                        &mut parsed,
                        current_dv.take(),
                        &mut current_dv_extra_ranges,
                    );
                }
                b"c" => {
                    if let Some((row, col)) = rc.take() {
                        let cell_value = if ctype == "inlineStr" && inline_text_seen {
                            inline_value.as_str()
                        } else {
                            value.as_str()
                        };
                        // Resolve a shared formula: a master (`si` + formula text)
                        // registers itself; a follower (`si`, empty text) rebuilds the
                        // formula by shifting the master's relative refs to this cell.
                        let mut resolved = match f_si {
                            Some(si) if !formula.is_empty() => {
                                shared_masters.insert(si, (formula.clone(), row, col));
                                formula.clone()
                            }
                            Some(si) => match shared_masters.get(&si) {
                                Some((mf, br, bc)) => shift_formula(
                                    mf,
                                    i64::from(row) - i64::from(*br),
                                    i64::from(col) - i64::from(*bc),
                                ),
                                None => formula.clone(),
                            },
                            None => formula.clone(),
                        };
                        if resolved.is_empty() {
                            if let Some((_, array_formula)) =
                                array_formulas.iter().rev().find(|((r0, c0, r1, c1), _)| {
                                    row >= *r0 && row <= *r1 && col >= *c0 && col <= *c1
                                })
                            {
                                resolved = array_formula.clone();
                            }
                        }
                        if !formula.is_empty() {
                            if let Some(array_ref) = f_array_ref.take() {
                                array_formulas.push((array_ref, resolved.clone()));
                            }
                        } else {
                            f_array_ref = None;
                        }
                        if let Some(entry) = build_cell(
                            row, col, &ctype, style_idx, cell_value, &resolved, shared, styles,
                            date1904,
                        ) {
                            // Bound accumulated text so a small file that clones a
                            // large shared string into many cells cannot exhaust
                            // memory; stop the sheet once the budget is spent.
                            if entry.text.len() > *budget {
                                *budget = 0;
                                break;
                            }
                            *budget -= entry.text.len();
                            if ctype == "s" {
                                if let Some(runs) = value
                                    .trim()
                                    .parse::<usize>()
                                    .ok()
                                    .and_then(|index| shared.get(index))
                                    .map(|shared| shared.runs.clone())
                                    .filter(|runs| !runs.is_empty())
                                {
                                    parsed.rich.insert((row, col), runs);
                                }
                            } else if ctype == "inlineStr" && !inline_runs.is_empty() {
                                parsed.rich.insert((row, col), inline_runs.clone());
                            }
                            if style_idx != 0 {
                                if let Some(overlay) = styles.cell_style_overlay(style_idx) {
                                    parsed
                                        .direct_cell_formats
                                        .insert((row, col), overlay.clone());
                                }
                            }
                            parsed.cells.push(entry);
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    push_current_conditional_format(&mut parsed, current_cf.take());
    parsed
}

fn attr_true(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true")
}

fn attr_false(value: &str) -> bool {
    value == "0" || value.eq_ignore_ascii_case("false")
}

fn parse_bool_attr(value: &str) -> Option<bool> {
    if attr_true(value) {
        Some(true)
    } else if attr_false(value) {
        Some(false)
    } else {
        None
    }
}

fn print_bool_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    metadata: &mut PrintMetadata,
) -> bool {
    match attr(e, key).as_deref() {
        Some(value) => parse_bool_attr(value).unwrap_or_else(|| {
            metadata.add_loss(PrintLossKind::UnsupportedProperty);
            false
        }),
        None => false,
    }
}

fn header_footer_bool_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    default: bool,
    metadata: &mut PrintMetadata,
) -> bool {
    match attr(e, key).as_deref() {
        Some(value) => parse_bool_attr(value).unwrap_or_else(|| {
            metadata.add_loss(PrintLossKind::MalformedHeaderFooter);
            default
        }),
        None => default,
    }
}

fn parse_sheet_protection(
    e: &quick_xml::events::BytesStart<'_>,
) -> Option<Option<ProtectionOptions>> {
    if attr(e, b"sheet").as_deref().is_some_and(attr_false) {
        return None;
    }

    let mut options = ProtectionOptions::default();
    for (key, field) in [
        (b"sort".as_slice(), &mut options.sort),
        (b"autoFilter".as_slice(), &mut options.auto_filter),
        (b"formatCells".as_slice(), &mut options.format_cells),
        (b"formatColumns".as_slice(), &mut options.format_columns),
        (b"formatRows".as_slice(), &mut options.format_rows),
        (b"insertColumns".as_slice(), &mut options.insert_columns),
        (b"insertRows".as_slice(), &mut options.insert_rows),
        (
            b"insertHyperlinks".as_slice(),
            &mut options.insert_hyperlinks,
        ),
        (b"deleteColumns".as_slice(), &mut options.delete_columns),
        (b"deleteRows".as_slice(), &mut options.delete_rows),
        (b"pivotTables".as_slice(), &mut options.pivot_tables),
    ] {
        if attr(e, key).as_deref().is_some_and(attr_false) {
            *field = true;
        }
    }

    if options == ProtectionOptions::default() {
        Some(None)
    } else {
        Some(Some(options))
    }
}

fn sheet_view_rank(e: &quick_xml::events::BytesStart<'_>) -> u8 {
    match attr(e, b"workbookViewId").as_deref() {
        Some("0") | None => 2,
        Some(_) => 1,
    }
}

fn clear_sheet_view_metadata(parsed: &mut ParsedSheet) {
    parsed.freeze = None;
    parsed.hide_gridlines = false;
    parsed.zoom = None;
    parsed.show_headers = None;
    parsed.right_to_left = false;
}

fn page_setup_mut(parsed: &mut ParsedSheet) -> &mut PageSetup {
    parsed.page_setup.get_or_insert_with(PageSetup::default)
}

fn header_footer_field(name: &[u8]) -> Option<(HeaderFooterKind, HeaderFooterField, bool)> {
    match name {
        b"oddHeader" => Some((HeaderFooterKind::OddHeader, HeaderFooterField::Header, true)),
        b"firstHeader" => Some((
            HeaderFooterKind::FirstHeader,
            HeaderFooterField::Header,
            false,
        )),
        b"evenHeader" => Some((
            HeaderFooterKind::EvenHeader,
            HeaderFooterField::Header,
            false,
        )),
        b"oddFooter" => Some((HeaderFooterKind::OddFooter, HeaderFooterField::Footer, true)),
        b"firstFooter" => Some((
            HeaderFooterKind::FirstFooter,
            HeaderFooterField::Footer,
            false,
        )),
        b"evenFooter" => Some((
            HeaderFooterKind::EvenFooter,
            HeaderFooterField::Footer,
            false,
        )),
        _ => None,
    }
}

fn begin_header_footer_capture(
    parsed: &mut ParsedSheet,
    kind: HeaderFooterKind,
    field: HeaderFooterField,
    preferred: bool,
) -> HeaderFooterCapture {
    parsed.print_metadata.set_header_footer(kind, String::new());
    let page_setup = page_setup_mut(parsed);
    let slot = match field {
        HeaderFooterField::Header => &mut page_setup.header,
        HeaderFooterField::Footer => &mut page_setup.footer,
    };
    let legacy = if preferred || slot.is_none() {
        *slot = Some(String::new());
        Some(field)
    } else {
        None
    };
    HeaderFooterCapture { kind, legacy }
}

fn append_header_footer_text(parsed: &mut ParsedSheet, capture: HeaderFooterCapture, text: &str) {
    parsed
        .print_metadata
        .append_header_footer(capture.kind, text);
    match capture.legacy {
        Some(HeaderFooterField::Header) => {
            if let Some(header) = page_setup_mut(parsed).header.as_mut() {
                header.push_str(text);
            }
        }
        Some(HeaderFooterField::Footer) => {
            if let Some(footer) = page_setup_mut(parsed).footer.as_mut() {
                footer.push_str(text);
            }
        }
        None => {}
    }
}

fn parse_manual_page_break(
    e: &quick_xml::events::BytesStart<'_>,
    axis: Option<PageBreakAxis>,
    metadata: &mut PrintMetadata,
) {
    let manual = match attr(e, b"man").as_deref() {
        Some(value) => match parse_bool_attr(value) {
            Some(value) => value,
            None => {
                metadata.add_loss(PrintLossKind::InvalidPageBreak);
                return;
            }
        },
        None => false,
    };
    if !manual {
        return;
    }
    let Some(id) = attr(e, b"id").and_then(|value| value.parse::<u32>().ok()) else {
        metadata.add_loss(PrintLossKind::InvalidPageBreak);
        return;
    };
    match axis {
        Some(PageBreakAxis::Row) => metadata.push_manual_row_break(id),
        Some(PageBreakAxis::Column) => match u16::try_from(id) {
            Ok(col) => metadata.push_manual_col_break(col),
            Err(_) => metadata.add_loss(PrintLossKind::InvalidPageBreak),
        },
        None => metadata.add_loss(PrintLossKind::InvalidPageBreak),
    }
}

fn attr_f64(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<f64> {
    attr(e, key).and_then(|s| s.parse::<f64>().ok())
}

fn attr_u16(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<u16> {
    attr(e, key).and_then(|s| s.parse::<u16>().ok())
}

fn apply_sheet_defined_names<'a, I>(
    page_setup: &mut Option<PageSetup>,
    print_metadata: &mut PrintMetadata,
    autofilter: &mut Option<SheetRange>,
    names: I,
) where
    I: IntoIterator<Item = &'a SheetDefinedName>,
{
    for name in names {
        match name.name.as_str() {
            "_xlnm.Print_Area" => {
                let mut first = None;
                for part in split_defined_name_refs(&name.refers_to) {
                    if let Some(range) = parse_defined_name_range(part) {
                        first.get_or_insert(range);
                        print_metadata.push_print_area(range);
                    } else if part.contains("#REF!") {
                        print_metadata.add_loss(PrintLossKind::MissingReference);
                    } else {
                        print_metadata.add_loss(PrintLossKind::InvalidPrintArea);
                    }
                }
                if let Some(range) = first {
                    page_setup.get_or_insert_with(PageSetup::default).print_area = Some(range);
                }
            }
            "_xlnm.Print_Titles" => {
                for part in split_defined_name_refs(&name.refers_to) {
                    let body = strip_sheet_prefix(part);
                    if let Some(rows) = parse_repeat_rows(body) {
                        page_setup
                            .get_or_insert_with(PageSetup::default)
                            .repeat_rows = Some(rows);
                    } else if let Some(cols) = parse_repeat_cols(body) {
                        page_setup
                            .get_or_insert_with(PageSetup::default)
                            .repeat_cols = Some(cols);
                    }
                }
            }
            "_xlnm._FilterDatabase" => {
                if let Some(range) = parse_defined_name_range(&name.refers_to) {
                    *autofilter = Some(range);
                }
            }
            _ => {}
        }
    }
}

fn split_defined_name_refs(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_quote = false;
    let mut chars = value.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '\'' => {
                if in_quote && chars.peek().is_some_and(|(_, next)| *next == '\'') {
                    chars.next();
                } else {
                    in_quote = !in_quote;
                }
            }
            ',' if !in_quote => {
                out.push(value[start..idx].trim());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    out.push(value[start..].trim());
    out
}

fn strip_sheet_prefix(value: &str) -> &str {
    value
        .rsplit_once('!')
        .map(|(_, reference)| reference.trim())
        .unwrap_or_else(|| value.trim())
}

fn parse_defined_name_range(value: &str) -> Option<SheetRange> {
    parse_range(strip_sheet_prefix(value))
}

fn parse_repeat_rows(value: &str) -> Option<(u32, u32)> {
    let (first, last) = value.split_once(':')?;
    let first = parse_one_based_row(first)?;
    let last = parse_one_based_row(last)?;
    Some((first.min(last), first.max(last)))
}

fn parse_one_based_row(value: &str) -> Option<u32> {
    let row = value.trim().trim_start_matches('$').parse::<u32>().ok()?;
    if !(1..=1_048_576).contains(&row) {
        return None;
    }
    Some(row - 1)
}

fn parse_repeat_cols(value: &str) -> Option<(u16, u16)> {
    let (first, last) = value.split_once(':')?;
    let first = parse_col_ref(first)?;
    let last = parse_col_ref(last)?;
    Some((first.min(last), first.max(last)))
}

fn parse_col_ref(value: &str) -> Option<u16> {
    let letters: Vec<char> = value
        .trim()
        .trim_start_matches('$')
        .chars()
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let col = letters_col(&letters)?;
    (col <= 16_383).then(|| u16::try_from(col).ok()).flatten()
}

fn parse_sparkline_kind(value: &str) -> SparklineKind {
    match value {
        "column" => SparklineKind::Column,
        "stacked" => SparklineKind::WinLoss,
        _ => SparklineKind::Line,
    }
}

fn parse_sparkline(kind: SparklineKind, range: &str, location: &str) -> Option<Sparkline> {
    let range = range.trim();
    if range.is_empty() {
        return None;
    }
    let (row, col) = location.split_whitespace().next().and_then(parse_ref)?;
    Some(Sparkline {
        location: (row, col),
        range: range.to_string(),
        kind,
    })
}

fn parse_conditional_metadata(
    e: &quick_xml::events::BytesStart<'_>,
    styles: &Styles,
) -> ConditionalFormatMetadata {
    let mut metadata = ConditionalFormatMetadata {
        priority: attr(e, b"priority")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|priority| *priority != 0),
        stop_if_true: attr(e, b"stopIfTrue").as_deref().is_some_and(attr_true),
        ..ConditionalFormatMetadata::default()
    };
    let Some(dxf_id) = attr(e, b"dxfId") else {
        return metadata;
    };
    let Some(dxf) = dxf_id
        .parse::<usize>()
        .ok()
        .and_then(|id| styles.differential_style(id))
    else {
        metadata.style_losses.push(StyleLoss {
            kind: StyleLossKind::MissingReference,
            occurrences: 1,
        });
        return metadata;
    };
    metadata.differential_style = Some(dxf.style.clone());
    metadata.style_losses = dxf.losses.clone();
    metadata
}

fn conditional_compatibility_fill(metadata: &ConditionalFormatMetadata) -> Color {
    metadata
        .differential_style
        .as_ref()
        .and_then(|style| {
            style.fill.or_else(|| {
                style.pattern_fill.and_then(|fill| {
                    (fill.pattern == FormatPattern::Solid)
                        .then(|| fill.foreground.or(fill.background))
                        .flatten()
                })
            })
        })
        .unwrap_or_default()
}

fn parse_conditional_rule(
    e: &quick_xml::events::BytesStart<'_>,
    ranges: &[SheetRange],
    styles: &Styles,
) -> Option<PendingCfRule> {
    if ranges.is_empty() {
        return None;
    }
    let ty = attr(e, b"type")?;
    let metadata = parse_conditional_metadata(e, styles);
    let compatibility_fill = conditional_compatibility_fill(&metadata);
    let kind = match ty.as_str() {
        "cellIs" => PendingCfKind::CellIs {
            op: attr(e, b"operator")
                .as_deref()
                .and_then(parse_dv_op)
                .unwrap_or(DvOp::Between),
            fill: compatibility_fill,
        },
        "colorScale" => PendingCfKind::ColorScale,
        "dataBar" => PendingCfKind::DataBar,
        "top10" => PendingCfKind::TopBottom {
            rank: attr(e, b"rank")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(10),
            bottom: attr(e, b"bottom").as_deref().is_some_and(attr_true),
            percent: attr(e, b"percent").as_deref().is_some_and(attr_true),
            fill: compatibility_fill,
        },
        "aboveAverage" => PendingCfKind::AboveAverage {
            below: attr(e, b"aboveAverage").as_deref().is_some_and(attr_false),
            fill: compatibility_fill,
        },
        "duplicateValues" => PendingCfKind::DuplicateValues {
            unique: false,
            fill: compatibility_fill,
        },
        "uniqueValues" => PendingCfKind::DuplicateValues {
            unique: true,
            fill: compatibility_fill,
        },
        "expression" => PendingCfKind::Expression {
            fill: compatibility_fill,
        },
        _ => return None,
    };
    Some(PendingCfRule {
        ranges: ranges.to_vec(),
        kind,
        formulas: Vec::new(),
        colors: Vec::new(),
        metadata,
    })
}

fn push_current_conditional_format(parsed: &mut ParsedSheet, current: Option<PendingCfRule>) {
    let Some(current) = current else {
        return;
    };
    let Some(rule) = current.build_rule() else {
        return;
    };
    for sqref in current.ranges.into_iter().take(1 << 16) {
        parsed.cond_formats.push(CondFormat {
            sqref,
            rule: rule.clone(),
        });
        parsed.cond_format_metadata.push(current.metadata.clone());
    }
}

fn parse_data_validation(e: &quick_xml::events::BytesStart<'_>) -> Option<ParsedDataValidation> {
    let ranges: Vec<_> = attr(e, b"sqref")?
        .split_whitespace()
        .filter_map(parse_range)
        .collect();
    let (&sqref, rest) = ranges.split_first()?;
    let kind = attr(e, b"type").as_deref().and_then(parse_dv_kind)?;
    let operator = attr(e, b"operator")
        .as_deref()
        .and_then(parse_dv_op)
        .unwrap_or(DvOp::Between);
    let allow_blank = attr(e, b"allowBlank")
        .as_deref()
        .map(attr_true)
        .unwrap_or(false);
    let show_input_message = attr(e, b"showInputMessage")
        .as_deref()
        .map(attr_true)
        .unwrap_or(false);
    let show_error_message = attr(e, b"showErrorMessage")
        .as_deref()
        .map(attr_true)
        .unwrap_or(false);
    let prompt = match (attr(e, b"promptTitle"), attr(e, b"prompt")) {
        (None, None) => None,
        (title, message) => Some((title.unwrap_or_default(), message.unwrap_or_default())),
    };
    let error = match (attr(e, b"errorTitle"), attr(e, b"error")) {
        (None, None) => None,
        (title, message) => Some((title.unwrap_or_default(), message.unwrap_or_default())),
    };
    Some((
        DataValidation {
            sqref,
            kind,
            operator,
            formula1: String::new(),
            formula2: None,
            allow_blank,
            show_input_message,
            show_error_message,
            prompt,
            error,
        },
        rest.to_vec(),
    ))
}

fn push_current_data_validation(
    parsed: &mut ParsedSheet,
    current: Option<DataValidation>,
    extra_ranges: &mut Vec<SheetRange>,
) {
    let Some(mut dv) = current else {
        extra_ranges.clear();
        return;
    };
    if dv.formula1.is_empty() {
        extra_ranges.clear();
        return;
    }
    if dv.formula2.as_deref() == Some("") {
        dv.formula2 = None;
    }
    parsed.data_validations.push(dv.clone());
    for sqref in extra_ranges.drain(..) {
        let mut clone = dv.clone();
        clone.sqref = sqref;
        parsed.data_validations.push(clone);
    }
}

fn parse_dv_kind(value: &str) -> Option<DvKind> {
    match value {
        "list" => Some(DvKind::List),
        "whole" => Some(DvKind::Whole),
        "decimal" => Some(DvKind::Decimal),
        "date" => Some(DvKind::Date),
        "time" => Some(DvKind::Time),
        "textLength" => Some(DvKind::TextLength),
        "custom" => Some(DvKind::Custom),
        _ => None,
    }
}

fn parse_dv_op(value: &str) -> Option<DvOp> {
    match value {
        "between" => Some(DvOp::Between),
        "notBetween" => Some(DvOp::NotBetween),
        "equal" => Some(DvOp::Equal),
        "notEqual" => Some(DvOp::NotEqual),
        "greaterThan" => Some(DvOp::GreaterThan),
        "lessThan" => Some(DvOp::LessThan),
        "greaterThanOrEqual" => Some(DvOp::GreaterThanOrEqual),
        "lessThanOrEqual" => Some(DvOp::LessThanOrEqual),
        _ => None,
    }
}

fn parse_split_u32(value: &str) -> Option<u32> {
    let n = value.parse::<f64>().ok()?;
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > f64::from(u32::MAX) {
        return None;
    }
    Some(n as u32)
}

fn parse_split_u16(value: &str) -> Option<u16> {
    u16::try_from(parse_split_u32(value)?).ok()
}

#[allow(clippy::too_many_arguments)]
fn build_cell(
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
    // Drop a value-only cell with no display text (keeps the grid sparse); a
    // formula cell is always surfaced.
    if text.is_empty() && formula.is_empty() {
        return None;
    }
    Some(CellEntry {
        row,
        col,
        value,
        text,
        style: styles.cell_style(style_idx).cloned(),
        hyperlink: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay_is_empty(overlay: &CellStyleOverlay) -> bool {
        !overlay.replace_font
            && !overlay.replace_fill
            && !overlay.replace_border
            && !overlay.replace_num_fmt
            && !overlay.replace_alignment
            && !overlay.replace_protection
    }

    fn shared_texts(xml: &str) -> Vec<String> {
        parse_shared_strings(xml, &ThemeColors::default(), &[])
            .into_iter()
            .map(|shared| shared.text)
            .collect()
    }

    #[test]
    fn cell_ref_parsing() {
        assert_eq!(parse_ref("A1"), Some((0, 0)));
        assert_eq!(parse_ref("B2"), Some((1, 1)));
        assert_eq!(parse_ref("Z1"), Some((0, 25)));
        assert_eq!(parse_ref("AA1"), Some((0, 26)));
        assert_eq!(parse_ref("XFD1048576"), Some((1_048_575, 16_383))); // Excel max
        assert_eq!(parse_ref("A"), None);
        assert_eq!(parse_ref("XFE1"), None); // past the last column
        assert_eq!(parse_ref("ZZZZZZZ1"), None); // overflow → None, NOT a panic
    }

    #[test]
    fn shared_strings_concatenate_runs() {
        let xml = r#"<sst><si><t>Hello</t></si><si><r><rPr><b/><color rgb="FF112233"/></rPr><t>가</t></r><r><rPr><i/></rPr><t>나</t></r></si></sst>"#;
        assert_eq!(shared_texts(xml), vec!["Hello", "가나"]);
        let parsed = parse_shared_strings(xml, &ThemeColors::default(), &[]);
        assert_eq!(parsed[1].runs.len(), 2);
        assert!(parsed[1].runs[0].font.bold);
        assert_eq!(
            parsed[1].runs[0].font.color,
            Some(Color::rgb(0x11, 0x22, 0x33))
        );
        assert!(parsed[1].runs[1].font.italic);
    }

    #[test]
    fn general_refs_are_reassembled_across_xlsx_text_surfaces() {
        assert_eq!(
            shared_texts("<sst><si><t>A&amp;B&#33;</t></si></sst>"),
            vec!["A&B!"]
        );

        let props = parse_doc_properties(
            Some("<coreProperties><title>A&amp;B&#33;</title></coreProperties>"),
            None,
        );
        assert_eq!(props.title.as_deref(), Some("A&B!"));

        let comments = parse_comments(
            r#"<comments><authors><author>R&amp;D</author></authors><commentList><comment ref="A1" authorId="0"><text><t>Check &lt;now&gt;</t></text></comment></commentList></comments>"#,
        );
        assert_eq!(comments[0].author.as_deref(), Some("R&D"));
        assert_eq!(comments[0].text, "Check <now>");

        let xml = r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>A&amp;B&#33;</t></is></c><c r="B1"><f>A1&amp;"!"</f><v>1&#48;</v></c></row></sheetData></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        assert_eq!(cells[0].value, Cell::Text("A&B!".to_string()));
        match &cells[1].value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "A1&\"!\"");
                assert_eq!(**cached, Cell::Number(10.0));
            }
            other => panic!("expected formula cell, got {other:?}"),
        }
    }

    #[test]
    fn unknown_and_illegal_general_refs_are_preserved_lexically_on_read() {
        assert_eq!(
            shared_texts("<sst><si><t>A&bogus;&#x1;</t></si></sst>"),
            vec!["A&bogus;&#x1;"]
        );
    }

    #[test]
    fn attributes_accept_only_xml_predefined_entities() {
        let mut reader = Reader::from_str(r#"<x value="a&nbsp;b"/>"#);
        let Event::Empty(element) = reader.read_event().unwrap() else {
            panic!("expected empty element");
        };
        assert_eq!(attr(&element, b"value"), None);
    }

    #[test]
    fn general_refs_are_reassembled_in_drawing_coordinates_and_chart_refs() {
        let drawing = r#"<wsDr><twoCellAnchor><from><col>1&#48;</col><row>2&#48;</row></from><to><col>3&#48;</col><row>4&#48;</row></to><graphicFrame><chart r:id="rId&amp;Chart"/></graphicFrame></twoCellAnchor></wsDr>"#;
        let refs = parse_drawing_refs(drawing);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].rid.as_deref(), Some("rId&Chart"));
        assert_eq!(refs[0].from, (20, 10));
        assert_eq!(refs[0].to, Some((40, 30)));

        let mut cache_points = 16;
        let mut chart_series = 16;
        let chart = parse_chart(
            r#"<chartSpace><chart><plotArea><lineChart><ser><tx><strRef><f>Data&amp;More!$A$1</f></strRef></tx><cat><strRef><f>Data!$A$2:$A$3</f></strRef></cat><val><numRef><f>Data!$B$2:$B$3</f></numRef></val></ser></lineChart></plotArea></chart></chartSpace>"#,
            (20, 10),
            (40, 30),
            &mut cache_points,
            &mut chart_series,
        )
        .unwrap()
        .chart;
        assert_eq!(chart.series[0].name.as_deref(), Some("Data&More!$A$1"));
        assert_eq!(
            chart.series[0].categories.as_deref(),
            Some("Data!$A$2:$A$3")
        );
        assert_eq!(chart.series[0].values, "Data!$B$2:$B$3");
    }

    #[test]
    fn drawing_sidecars_retain_all_anchor_geometry_and_unsupported_shapes() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let drawing = r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"
                xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"
                xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
            <xdr:twoCellAnchor editAs="oneCell">
                <xdr:from><xdr:col>1</xdr:col><xdr:colOff>123</xdr:colOff><xdr:row>2</xdr:row><xdr:rowOff>456</xdr:rowOff></xdr:from>
                <xdr:to><xdr:col>4</xdr:col><xdr:colOff>789</xdr:colOff><xdr:row>5</xdr:row><xdr:rowOff>1011</xdr:rowOff></xdr:to>
                <xdr:pic>
                    <xdr:nvPicPr><xdr:cNvPr id="2" name="Logo &amp; mark" descr="Accessible logo"/></xdr:nvPicPr>
                    <xdr:blipFill><a:blip r:embed="rIdImage"/><a:srcRect l="1000" t="2000" r="3000" b="4000"/></xdr:blipFill>
                    <xdr:spPr><a:xfrm rot="60000"><a:ext cx="914400" cy="457200"/></a:xfrm></xdr:spPr>
                </xdr:pic>
            </xdr:twoCellAnchor>
            <xdr:oneCellAnchor>
                <xdr:from><xdr:col>6</xdr:col><xdr:colOff>-5</xdr:colOff><xdr:row>7</xdr:row><xdr:rowOff>6</xdr:rowOff></xdr:from>
                <xdr:ext cx="1828800" cy="914400"/>
                <xdr:graphicFrame>
                    <xdr:nvGraphicFramePr><xdr:cNvPr id="3" name="Sales chart" title="Chart fallback text"/></xdr:nvGraphicFramePr>
                    <a:graphic><a:graphicData><c:chart r:id="rIdChart"/></a:graphicData></a:graphic>
                </xdr:graphicFrame>
            </xdr:oneCellAnchor>
            <xdr:absoluteAnchor>
                <xdr:pos x="1234" y="5678"/><xdr:ext cx="777" cy="888"/>
                <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="4" name="Callout" descr="Unsupported callout"/></xdr:nvSpPr>
                    <xdr:spPr><a:xfrm rot="-120000"/></xdr:spPr>
                </xdr:sp>
            </xdr:absoluteAnchor>
        </xdr:wsDr>"#;
        let parts = [
            (
                "xl/workbook.xml",
                br#"<workbook><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#.as_slice(),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#.as_slice(),
            ),
            (
                "xl/worksheets/sheet1.xml",
                br#"<worksheet><sheetData/><drawing r:id="rIdDrawing"/></worksheet>"#.as_slice(),
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                br#"<Relationships><Relationship Id="rIdDrawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#.as_slice(),
            ),
            ("xl/drawings/drawing1.xml", drawing.as_bytes()),
            (
                "xl/drawings/_rels/drawing1.xml.rels",
                br#"<Relationships><Relationship Id="rIdImage" Target="../media/image1.png"/><Relationship Id="rIdChart" Target="../charts/chart1.xml"/></Relationships>"#.as_slice(),
            ),
            (
                "xl/charts/chart1.xml",
                br#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart><c:plotArea><c:lineChart/></c:plotArea></c:chart></c:chartSpace>"#.as_slice(),
            ),
            ("xl/media/image1.png", b"\x89PNG\r\n\x1a\n".as_slice()),
        ];
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in parts {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();

        let workbook = Workbook::open(&bytes).unwrap();
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.images().len(), 1);
        assert_eq!(sheet.images()[0].from, (2, 1));
        assert_eq!(sheet.images()[0].to, Some((5, 4)));
        assert_eq!(sheet.charts().len(), 1);
        assert_eq!(sheet.charts()[0].from, (7, 6));
        assert_eq!(sheet.charts()[0].to, (7, 6));

        let metadata = sheet.drawing_metadata();
        assert_eq!(metadata.len(), 3);
        assert_eq!(metadata[0].kind, DrawingObjectKind::Image);
        assert_eq!(metadata[0].object_index, 0);
        assert_eq!(metadata[0].from_cell, Some((2, 1)));
        assert_eq!(metadata[0].to_cell, Some((5, 4)));
        assert_eq!(metadata[0].from_offset_emu, Some((123, 456)));
        assert_eq!(metadata[0].to_offset_emu, Some((789, 1011)));
        assert_eq!(metadata[0].absolute_size_emu, Some((914400, 457200)));
        assert_eq!(
            metadata[0].crop,
            Some(DrawingCrop {
                left_ppm: 10_000,
                top_ppm: 20_000,
                right_ppm: 30_000,
                bottom_ppm: 40_000,
            })
        );
        assert_eq!(metadata[0].rotation_mdeg, Some(1000));
        assert_eq!(metadata[0].z_order, Some(0));
        assert_eq!(metadata[0].name.as_deref(), Some("Logo & mark"));
        assert_eq!(metadata[0].alt_text.as_deref(), Some("Accessible logo"));
        assert_eq!(metadata[0].behavior, DrawingAnchorBehavior::MoveOnly);

        assert_eq!(metadata[1].kind, DrawingObjectKind::Chart);
        assert_eq!(metadata[1].object_index, 0);
        assert_eq!(metadata[1].from_cell, Some((7, 6)));
        assert_eq!(metadata[1].to_cell, None);
        assert_eq!(metadata[1].from_offset_emu, Some((-5, 6)));
        assert_eq!(metadata[1].to_offset_emu, None);
        assert_eq!(metadata[1].absolute_size_emu, Some((1_828_800, 914_400)));
        assert_eq!(metadata[1].z_order, Some(1));
        assert_eq!(metadata[1].name.as_deref(), Some("Sales chart"));
        assert_eq!(metadata[1].alt_text.as_deref(), Some("Chart fallback text"));
        assert_eq!(metadata[1].behavior, DrawingAnchorBehavior::MoveOnly);

        assert_eq!(metadata[2].kind, DrawingObjectKind::Shape);
        assert_eq!(metadata[2].from_cell, None);
        assert_eq!(metadata[2].to_cell, None);
        assert_eq!(metadata[2].from_offset_emu, Some((1234, 5678)));
        assert_eq!(metadata[2].absolute_size_emu, Some((777, 888)));
        assert_eq!(metadata[2].rotation_mdeg, Some(-2000));
        assert_eq!(metadata[2].z_order, Some(2));
        assert_eq!(metadata[2].name.as_deref(), Some("Callout"));
        assert_eq!(metadata[2].alt_text.as_deref(), Some("Unsupported callout"));
        assert_eq!(metadata[2].behavior, DrawingAnchorBehavior::Absolute);
        assert_eq!(
            sheet.style_losses(),
            &[StyleLoss {
                kind: StyleLossKind::UnsupportedProperty,
                occurrences: 1,
            }]
        );
    }

    #[test]
    fn drawing_sidecar_strings_are_utf8_bounded_and_loss_aware() {
        let long_name = format!("{}한", "a".repeat(MAX_XLSX_DRAWING_TEXT));
        let xml = format!(
            r#"<wsDr><absoluteAnchor><pos x="1" y="2"/><ext cx="3" cy="4"/><sp><nvSpPr><cNvPr name="{long_name}"/></nvSpPr></sp></absoluteAnchor></wsDr>"#
        );
        let mut losses = Vec::new();
        let refs = parse_drawing_refs_bounded(&xml, &mut losses);

        assert_eq!(refs.len(), 1);
        let name = refs[0].metadata.name.as_deref().unwrap();
        assert_eq!(name.len(), MAX_XLSX_DRAWING_TEXT);
        assert!(name.is_char_boundary(name.len()));
        assert_eq!(
            losses,
            vec![StyleLoss {
                kind: StyleLossKind::LimitExceeded,
                occurrences: 1,
            }]
        );
    }

    #[test]
    fn drawing_anchor_behavior_matrix_and_zero_offsets_are_exact() {
        let cases = [
            (None, DrawingAnchorBehavior::MoveAndSize),
            (Some("twoCell"), DrawingAnchorBehavior::MoveAndSize),
            (Some("oneCell"), DrawingAnchorBehavior::MoveOnly),
            (Some("absolute"), DrawingAnchorBehavior::Absolute),
        ];
        for (edit_as, expected) in cases {
            let edit_as = edit_as
                .map(|value| format!(r#" editAs="{value}""#))
                .unwrap_or_default();
            let xml = format!(
                r#"<wsDr><twoCellAnchor{edit_as}><from><col>0</col><colOff>0</colOff><row>0</row><rowOff>0</rowOff></from><to><col>1</col><colOff>0</colOff><row>1</row><rowOff>0</rowOff></to><sp/></twoCellAnchor></wsDr>"#
            );
            let refs = parse_drawing_refs(&xml);
            assert_eq!(refs.len(), 1);
            assert_eq!(refs[0].metadata.behavior, expected);
            assert_eq!(refs[0].metadata.from_offset_emu, Some((0, 0)));
            assert_eq!(refs[0].metadata.to_offset_emu, Some((0, 0)));
        }

        let one_cell = parse_drawing_refs(
            "<wsDr><oneCellAnchor><from><col>0</col><row>0</row></from><sp/></oneCellAnchor></wsDr>",
        );
        assert_eq!(
            one_cell[0].metadata.behavior,
            DrawingAnchorBehavior::MoveOnly
        );
        let absolute = parse_drawing_refs(
            "<wsDr><absoluteAnchor><pos x=\"0\" y=\"0\"/><ext cx=\"1\" cy=\"1\"/><sp/></absoluteAnchor></wsDr>",
        );
        assert_eq!(
            absolute[0].metadata.behavior,
            DrawingAnchorBehavior::Absolute
        );
    }

    #[test]
    fn drawing_anchor_count_is_bounded_and_reports_the_limit() {
        let anchor =
            "<absoluteAnchor><pos x=\"0\" y=\"0\"/><ext cx=\"1\" cy=\"1\"/><sp/></absoluteAnchor>";
        let xml = format!("<wsDr>{}</wsDr>", anchor.repeat(MAX_XLSX_DRAWINGS + 1));
        let mut losses = Vec::new();
        let refs = parse_drawing_refs_bounded(&xml, &mut losses);

        assert_eq!(refs.len(), MAX_XLSX_DRAWINGS);
        assert_eq!(
            losses,
            vec![StyleLoss {
                kind: StyleLossKind::LimitExceeded,
                occurrences: 1,
            }]
        );
    }

    #[test]
    fn shared_strings_keep_empty_slots() {
        // A self-closing <si/> and an empty <si></si> must each occupy an index,
        // so later references don't shift.
        let xml = r#"<sst><si><t>품목</t></si><si/><si></si><si><t>가격</t></si></sst>"#;
        assert_eq!(shared_texts(xml), vec!["품목", "", "", "가격"]);
    }

    #[test]
    fn implicit_cell_positions() {
        // No `r` on <row>/<c>: position is implicit (col by order, row by order).
        // Some writers (LibreOffice, EPPlus) emit this; every cell would be lost
        // without implicit-position tracking.
        let xml = "<worksheet><sheetData>\
            <row><c t=\"inlineStr\"><is><t>A</t></is></c><c t=\"inlineStr\"><is><t>B</t></is></c></row>\
            <row><c t=\"inlineStr\"><is><t>C</t></is></c></row></sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        let got: Vec<_> = cells
            .iter()
            .map(|c| (c.row, c.col, c.text.as_str()))
            .collect();
        assert_eq!(got, vec![(0, 0, "A"), (0, 1, "B"), (1, 0, "C")]);
    }

    #[test]
    fn mixed_explicit_and_implicit_positions() {
        // An explicit `r` resyncs the running position; following r-less cells
        // continue from there.
        let xml = "<worksheet><sheetData>\
            <row r=\"5\"><c r=\"C5\" t=\"inlineStr\"><is><t>X</t></is></c>\
            <c t=\"inlineStr\"><is><t>Y</t></is></c></row></sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        let got: Vec<_> = cells
            .iter()
            .map(|c| (c.row, c.col, c.text.as_str()))
            .collect();
        assert_eq!(got, vec![(4, 2, "X"), (4, 3, "Y")]);
    }

    #[test]
    fn inline_string_with_cached_value_uses_inline_text_not_concatenation() {
        let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\" t=\"inlineStr\"><v>1.0</v><is><t>1.</t></is></c>\
            </row></sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;

        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].value, Cell::Text("1.".to_string()));
        assert_eq!(cells[0].text, "1.");
    }

    #[test]
    fn text_budget_caps_shared_string_amplification() {
        // The shared-string DoS: one large pooled string referenced by very many
        // cells. Accumulated text must stay within the budget (here, a small one).
        let shared = vec![SharedString {
            text: "X".repeat(100),
            runs: Vec::new(),
        }];
        let mut xml = String::from("<worksheet><sheetData><row>");
        for _ in 0..1000 {
            xml.push_str("<c t=\"s\"><v>0</v></c>");
        }
        xml.push_str("</row></sheetData></worksheet>");
        let mut budget = 250usize;
        let cells = parse_sheet(
            &xml,
            &shared,
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        let total: usize = cells.iter().map(|c| c.text.len()).sum();
        assert!(
            total <= 250,
            "accumulated {total} bytes exceeded the 250 budget"
        );
        assert!(!cells.is_empty(), "should still extract up to the cap");
    }

    #[test]
    fn text_budget_exhaustion_leaves_zero_budget_signal() {
        let shared = vec![SharedString {
            text: "X".repeat(100),
            runs: Vec::new(),
        }];
        let xml =
            "<worksheet><sheetData><row><c t=\"s\"><v>0</v></c></row></sheetData></worksheet>";
        let mut budget = 50usize;
        let cells = parse_sheet(
            xml,
            &shared,
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;

        assert!(cells.is_empty());
        assert_eq!(budget, 0);
    }

    /// Build a minimal `.xlsx` in memory and read it end-to-end.
    #[test]
    fn custom_number_formats_are_applied_to_xlsx_display_text() {
        let styles = parse_styles(
            r#"<styleSheet><numFmts count="3"><numFmt numFmtId="164" formatCode="[$₩-412]#,##0.00"/><numFmt numFmtId="165" formatCode="yyyy&quot;년&quot; m&quot;월&quot; d&quot;일&quot;"/><numFmt numFmtId="166" formatCode="0;[Red](0);0;&quot;값: &quot;@"/></numFmts><cellXfs count="3"><xf numFmtId="164"/><xf numFmtId="165"/><xf numFmtId="166"/></cellXfs></styleSheet>"#,
            &ThemeColors::default(),
        );
        let xml = r#"<worksheet><sheetData><row r="1"><c r="A1" s="0"><v>1234.5</v></c><c r="B1" s="1"><v>45366</v></c><c r="C1" s="2" t="inlineStr"><is><t>한글</t></is></c></row></sheetData></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &styles,
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.cells[0].text, "₩1,234.50");
        assert_eq!(parsed.cells[1].text, "2024년 3월 15일");
        assert!(matches!(parsed.cells[1].value, Cell::Date(45_366.0)));
        assert_eq!(parsed.cells[2].text, "값: 한글");
    }

    /// Build a minimal `.xlsx` in memory and read it end-to-end.
    #[test]
    fn reads_a_minimal_xlsx() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="가격" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            ("xl/sharedStrings.xml", r#"<sst><si><t>품목</t></si></sst>"#),
            (
                "xl/styles.xml",
                r#"<styleSheet><cellXfs><xf numFmtId="0"/><xf numFmtId="14"/></cellXfs></styleSheet>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1"><v>42</v></c><c r="C1" s="1"><v>45366</v></c><c r="D1" t="b"><v>1</v></c></row></sheetData></worksheet>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        let s = &wb.sheets[0];
        assert_eq!(s.name, "가격");
        assert_eq!(s.cell(0, 0), Some(&Cell::Text("품목".to_string())));
        assert_eq!(s.cell(0, 1), Some(&Cell::Number(42.0)));
        assert_eq!(s.cell(0, 2), Some(&Cell::Date(45366.0))); // numFmt 14 → date
        assert_eq!(s.cell(0, 3), Some(&Cell::Bool(true)));
        assert!(s.to_text().contains("2024-03-15"));
        assert_eq!(s.default_column_width(), None);
        assert_eq!(s.implicit_ooxml_column_width(), Some(None));
    }

    #[test]
    fn sheet_format_retains_explicit_and_base_column_width_provenance() {
        let parse = |format: &str| {
            let xml = format!(
                r#"<worksheet>{format}<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#
            );
            let mut budget = crate::MAX_TEXT_BYTES;
            parse_sheet(
                &xml,
                &[],
                &Styles::default(),
                &ThemeColors::default(),
                false,
                &mut budget,
            )
        };

        let absent = parse("");
        assert_eq!(absent.default_col_width, None);
        assert_eq!(absent.base_col_width, None);

        let explicit = parse(r#"<sheetFormatPr baseColWidth="8" defaultColWidth="8.43"/>"#);
        assert_eq!(explicit.default_col_width, Some(8.43));
        assert_eq!(explicit.base_col_width, Some(8.0));

        let base = parse(r#"<sheetFormatPr baseColWidth="10"/>"#);
        assert_eq!(base.default_col_width, None);
        assert_eq!(base.base_col_width, Some(10.0));
    }

    #[test]
    fn malformed_zip_container_reports_zip_error_not_biff() {
        let err = Workbook::open(b"PK\x03\x04 truncated").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid ZIP package: not a valid spreadsheet ZIP container"
        );
    }

    #[test]
    fn reads_xlsx_with_backslash_package_paths() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl\\workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl\\_rels\\workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets\sheet1.xml"/></Relationships>"#,
            ),
            ("xl\\sharedStrings.xml", r#"<sst><si><t>ok</t></si></sst>"#),
            (
                "xl\\worksheets\\sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData></worksheet>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        let s = &wb.sheets[0];
        assert_eq!(s.name, "Data");
        assert_eq!(s.cell(0, 0), Some(&Cell::Text("ok".to_string())));
    }

    #[test]
    fn reads_xlsx_with_root_office_document_part() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "_rels/.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="workbook.xml"/></Relationships>"#,
            ),
            (
                "workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Root" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="sheet1.xml"/></Relationships>"#,
            ),
            (
                "sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>root</t></is></c></row></sheetData></worksheet>"#,
            ),
            ("styles.xml", r#"<styleSheet/>"#),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        let s = &wb.sheets[0];
        assert_eq!(s.name, "Root");
        assert_eq!(s.cell(0, 0), Some(&Cell::Text("root".to_string())));
    }

    #[test]
    fn workbook_sheet_visibility_and_defined_names() {
        // `<sheet state>` carries visibility; `<definedNames>` carry workbook-global
        // names. A built-in `_xlnm.*` name is skipped; a user name is kept.
        let xml = r#"<workbook>
            <sheets>
                <sheet name="Vis" r:id="rId1"/>
                <sheet name="Hid" state="hidden" r:id="rId2"/>
                <sheet name="VHid" state="veryHidden" r:id="rId3"/>
            </sheets>
            <definedNames>
                <definedName name="TaxRate">Sheet1!$B$1</definedName>
                <definedName name="_xlnm.Print_Area" localSheetId="0">Sheet1!$A$1:$C$3</definedName>
                <definedName name="LocalOnly" localSheetId="1">Sheet2!$A$1</definedName>
            </definedNames>
        </workbook>"#;
        let parsed = parse_workbook(xml);
        assert_eq!(parsed.sheets.len(), 3);
        assert_eq!(parsed.sheets[0].visibility, Visibility::Visible);
        assert_eq!(parsed.sheets[1].visibility, Visibility::Hidden);
        assert_eq!(parsed.sheets[2].visibility, Visibility::VeryHidden);
        // Global and local user names remain distinct; the built-in print area
        // stays in the sheet-metadata path.
        assert_eq!(
            parsed.defined_names,
            vec![("TaxRate".to_string(), "Sheet1!$B$1".to_string())]
        );
        assert_eq!(
            parsed.local_defined_names,
            vec![crate::LocalDefinedName {
                sheet: "Hid".to_string(),
                name: "LocalOnly".to_string(),
                refers_to: "Sheet2!$A$1".to_string(),
            }]
        );
    }

    /// End-to-end `.xlsx` read: a hidden sheet + a defined name surface via the
    /// public `is_hidden()` / `defined_names()` accessors.
    #[test]
    fn hidden_sheet_and_defined_name_end_to_end() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/><sheet name="Secret" state="hidden" r:id="rId2"/></sheets><definedNames><definedName name="TaxRate">Data!$A$1</definedName></definedNames></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="worksheets/sheet2.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData/></worksheet>"#,
            ),
            (
                "xl/worksheets/sheet2.xml",
                r#"<worksheet><sheetData/></worksheet>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 2);
        assert!(!wb.sheets[0].is_hidden(), "Data is visible");
        assert!(wb.sheets[1].is_hidden(), "Secret is hidden");
        assert!(!wb.sheets[1].is_very_hidden());
        assert_eq!(
            wb.defined_names(),
            &[("TaxRate".to_string(), "Data!$A$1".to_string())]
        );
    }

    #[test]
    fn chart_series_refs_retain_bounded_caches_and_theme_palette_sidecar() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rIdTheme" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/></Relationships>"#,
            ),
            (
                "xl/theme/theme1.xml",
                r#"<theme><themeElements><clrScheme><accent1><srgbClr val="010203"/></accent1><accent2><srgbClr val="A0B0C0"/></accent2></clrScheme></themeElements></theme>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData/><drawing r:id="rIdDraw"/></worksheet>"#,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rIdDraw" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#,
            ),
            (
                "xl/drawings/drawing1.xml",
                r#"<wsDr>
                    <twoCellAnchor>
                        <from><col>2</col><row>4</row></from>
                        <to><col>8</col><row>16</row></to>
                        <graphicFrame>
                            <graphic>
                                <graphicData>
                                    <chart r:id="rIdChart"/>
                                </graphicData>
                            </graphic>
                        </graphicFrame>
                    </twoCellAnchor>
                </wsDr>"#,
            ),
            (
                "xl/drawings/_rels/drawing1.xml.rels",
                r#"<Relationships><Relationship Id="rIdChart" Target="../charts/chart1.xml"/></Relationships>"#,
            ),
            (
                "xl/charts/chart1.xml",
                r#"<chartSpace><chart><plotArea><lineChart><ser>
                    <tx><strRef><f>Data!$C$1</f><strCache><pt idx="0"><v>Cached Series</v></pt></strCache></strRef></tx>
                    <marker><symbol val="circle"/><size val="5"/></marker>
                    <spPr><a:ln><a:solidFill><a:schemeClr val="accent2"/></a:solidFill></a:ln></spPr>
                    <cat><strRef><f>Data!$A$2:$A$4</f><strCache><pt idx="0"><v>Q1</v></pt><pt idx="1"><v>Q2</v></pt><pt idx="2"><v>Q3</v></pt></strCache></strRef></cat>
                    <val><numRef><f>Data!$B$2:$B$4</f><numCache><pt idx="0"><v>10</v></pt><pt idx="1"><v>20</v></pt><pt idx="2"><v>30</v></pt></numCache></numRef></val>
                </ser></lineChart></plotArea></chart></chartSpace>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        let charts = wb.sheets[0].charts();

        assert_eq!(charts.len(), 1);
        assert_eq!(charts[0].kind, ChartKind::Line);
        assert_eq!(charts[0].from, (4, 2));
        assert_eq!(charts[0].to, (16, 8));
        assert_eq!(charts[0].series.len(), 1);
        assert_eq!(charts[0].series[0].name.as_deref(), Some("Data!$C$1"));
        assert_eq!(
            charts[0].series[0].categories.as_deref(),
            Some("Data!$A$2:$A$4")
        );
        assert_eq!(charts[0].series[0].values, "Data!$B$2:$B$4");
        let sidecar = wb.sheets[0]
            .drawing_metadata()
            .iter()
            .find(|metadata| metadata.kind == DrawingObjectKind::Chart)
            .expect("chart rendering sidecar");
        assert_eq!(sidecar.chart_palette[0], Color::rgb(1, 2, 3));
        assert_eq!(sidecar.chart_palette[1], Color::rgb(160, 176, 192));
        assert_eq!(sidecar.chart_series_caches.len(), 1);
        assert_eq!(sidecar.chart_series_styles.len(), 1);
        assert_eq!(
            sidecar.chart_series_styles[0].marker,
            ChartMarkerSymbol::Circle
        );
        assert_eq!(sidecar.chart_series_styles[0].marker_size, Some(5));
        assert!(sidecar.chart_series_styles[0].line_visible);
        assert_eq!(
            sidecar.chart_series_styles[0].line_color,
            Some(Color::rgb(160, 176, 192))
        );
        assert!(sidecar.chart_series_styles[0].losses.is_empty());
        let cache = &sidecar.chart_series_caches[0];
        assert_eq!(cache.name[0].value, "Cached Series");
        assert_eq!(
            cache
                .categories
                .iter()
                .map(|point| point.value.as_str())
                .collect::<Vec<_>>(),
            ["Q1", "Q2", "Q3"]
        );
        assert_eq!(
            cache
                .values
                .iter()
                .map(|point| point.value.as_str())
                .collect::<Vec<_>>(),
            ["10", "20", "30"]
        );
    }

    #[test]
    fn chart_cache_and_series_sidecars_stop_at_exact_budgets() {
        let xml = r#"<chartSpace><chart><plotArea><lineChart>
            <ser><val><numRef><f>S!$A$1:$A$2</f><numCache>
                <pt idx="0"><v>1</v></pt><pt idx="1"><v>2</v></pt>
            </numCache></numRef></val></ser>
            <ser><val><numRef><f>S!$B$1:$B$2</f><numCache>
                <pt idx="0"><v>3</v></pt><pt idx="1"><v>4</v></pt>
            </numCache></numRef></val></ser>
        </lineChart></plotArea></chart></chartSpace>"#;
        let mut cache_points = 2;
        let mut chart_series = 1;
        let parsed =
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        assert_eq!(cache_points, 0);
        assert_eq!(chart_series, 0);
        assert!(parsed.limit_exceeded);
        assert_eq!(parsed.chart.series.len(), 1);
        assert_eq!(parsed.series_caches.len(), 1);
        assert_eq!(parsed.series_styles.len(), 1);
        assert_eq!(parsed.series_caches[0].values.len(), 2);
    }

    #[test]
    fn unsupported_chart_series_style_metadata_is_typed_and_bounded() {
        let xml = r#"<chartSpace><chart><plotArea><lineChart><ser>
            <marker><symbol val="picture"/><size val="255"/></marker>
            <spPr><a:ln><a:gradFill/><a:prstDash val="dash"/></a:ln></spPr>
            <val><numRef><f>S!$A$1:$A$2</f></numRef></val>
        </ser></lineChart></plotArea></chart></chartSpace>"#;
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        assert_eq!(parsed.series_styles.len(), 1);
        let style = &parsed.series_styles[0];
        assert_eq!(style.marker, ChartMarkerSymbol::Automatic);
        assert_eq!(style.marker_size, None);
        assert_eq!(
            style.losses,
            [
                ChartSeriesStyleLossKind::UnsupportedMarkerSymbol,
                ChartSeriesStyleLossKind::InvalidMarkerSize,
                ChartSeriesStyleLossKind::UnsupportedLinePaint,
            ]
        );
    }

    #[test]
    fn bar_chart_direction_is_retained_without_changing_chart_kind() {
        for (value, expected) in [
            ("col", ChartBarDirection::Column),
            ("bar", ChartBarDirection::Horizontal),
        ] {
            let xml = format!(
                r#"<chartSpace><chart><plotArea><barChart><barDir val="{value}"/><ser><val><numRef><f>Data!$A$1:$A$2</f></numRef></val></ser></barChart></plotArea></chart></chartSpace>"#
            );
            let mut cache_points = 16;
            let mut chart_series = 16;
            let parsed =
                parse_chart(&xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
            assert_eq!(parsed.chart.kind, ChartKind::Bar);
            assert_eq!(parsed.bar_direction, expected);
        }
    }

    #[test]
    fn unsupported_combo_3d_pivot_and_external_charts_are_explicit() {
        let xml = r#"<chartSpace><pivotSource/><externalData/><chart><view3D/><plotArea>
            <barChart><ser><val><numRef><f>Data!$A$1:$A$2</f></numRef></val></ser></barChart>
            <lineChart><ser><val><numRef><f>'[Other.xlsx]Data'!$B$1:$B$2</f></numRef></val></ser></lineChart>
        </plotArea></chart></chartSpace>"#;
        let mut cache_points = 16;
        let mut chart_series = 16;
        let parsed =
            parse_chart(xml, (0, 0), (10, 5), &mut cache_points, &mut chart_series).unwrap();
        assert_eq!(parsed.chart.kind, ChartKind::Bar);
        assert_eq!(parsed.chart.series.len(), 2);
        for reason in [
            ChartUnsupportedReason::Combo,
            ChartUnsupportedReason::ThreeDimensional,
            ChartUnsupportedReason::Pivot,
            ChartUnsupportedReason::ExternalData,
        ] {
            assert!(parsed.unsupported_reasons.contains(&reason), "{reason:?}");
        }

        let mut cache_points = 16;
        let mut chart_series = 16;
        let surface = parse_chart(
            r#"<chartSpace><chart><plotArea><surface3DChart><ser><val><numRef><f>Data!$A$1:$A$2</f></numRef></val></ser></surface3DChart></plotArea></chart></chartSpace>"#,
            (0, 0),
            (10, 5),
            &mut cache_points,
            &mut chart_series,
        )
        .unwrap();
        assert_eq!(surface.chart.kind, ChartKind::Area);
        assert!(surface
            .unsupported_reasons
            .contains(&ChartUnsupportedReason::ThreeDimensional));
        assert!(surface
            .unsupported_reasons
            .contains(&ChartUnsupportedReason::UnsupportedKind));
    }

    #[test]
    fn sheet_local_filter_database_defined_name_surfaces_autofilter() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/></sheets><definedNames><definedName name="_xlnm._FilterDatabase" localSheetId="0">Data!$B$3:$E$10</definedName></definedNames></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData/></worksheet>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();

        assert!(wb.defined_names().is_empty());
        assert_eq!(wb.sheets[0].autofilter_range(), Some((2, 1, 9, 4)));
        assert_eq!(wb.sheets[0].page_setup(), None);
    }

    /// End-to-end `.xlsx` read: package document properties surface through the
    /// public `Workbook::properties` field instead of remaining writer-only.
    #[test]
    fn reads_xlsx_doc_properties_end_to_end() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData/></worksheet>"#,
            ),
            (
                "docProps/core.xml",
                r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/"><dc:title>Quarterly Report</dc:title><dc:subject>Procurement</dc:subject><dc:creator>rxls reader</dc:creator><cp:keywords>bid,report</cp:keywords><dc:description>Public bid report</dc:description><cp:lastModifiedBy>reviewer</cp:lastModifiedBy><dcterms:created>2024-01-02T03:04:05Z</dcterms:created></cp:coreProperties>"#,
            ),
            (
                "docProps/app.xml",
                r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>Excel</Application><Company>ACME</Company></Properties>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();

        assert_eq!(wb.properties.title.as_deref(), Some("Quarterly Report"));
        assert_eq!(wb.properties.subject.as_deref(), Some("Procurement"));
        assert_eq!(wb.properties.creator.as_deref(), Some("rxls reader"));
        assert_eq!(wb.properties.keywords.as_deref(), Some("bid,report"));
        assert_eq!(
            wb.properties.description.as_deref(),
            Some("Public bid report")
        );
        assert_eq!(wb.properties.last_modified_by.as_deref(), Some("reviewer"));
        assert_eq!(
            wb.properties.created.as_deref(),
            Some("2024-01-02T03:04:05Z")
        );
        assert_eq!(wb.properties.company.as_deref(), Some("ACME"));
    }

    #[test]
    fn chartsheet_is_not_marked_as_worksheet_end_to_end() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Data" r:id="rId1"/><sheet name="Chart" r:id="rId2"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>ok</t></is></c></row></sheetData></worksheet>"#,
            ),
            ("xl/chartsheets/sheet1.xml", r#"<chartsheet/>"#),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();

        assert_eq!(wb.sheets.len(), 2);
        assert!(wb.sheets[0].is_worksheet);
        assert!(!wb.sheets[1].is_worksheet);
        assert_eq!(wb.sheets[0].cell(0, 0), Some(&Cell::Text("ok".into())));
        assert_eq!(wb.text(), "# Data\nok\n");
    }

    #[test]
    fn dangling_sheet_ref_without_relationship_is_not_marked_as_worksheet() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/><sheet name="Module" r:id=""/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData/></worksheet>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();

        assert_eq!(wb.sheets.len(), 2);
        assert!(wb.sheets[0].is_worksheet);
        assert!(!wb.sheets[1].is_worksheet);
        assert_eq!(wb.sheets[1].sheet_type(), SheetType::Vba);
        assert_eq!(wb.text(), "# Sheet1\n\n");
    }

    #[test]
    fn sheet_rels_path_inserts_rels_segment() {
        assert_eq!(
            sheet_rels_path("xl/worksheets/sheet1.xml"),
            "xl/worksheets/_rels/sheet1.xml.rels"
        );
        assert_eq!(sheet_rels_path("sheet1.xml"), "_rels/sheet1.xml.rels");
    }

    /// End-to-end `.xlsx` read: a worksheet `<hyperlink>` whose `r:id` resolves
    /// through the worksheet rels surfaces via the public `hyperlinks()` accessor
    /// as `(row, col, url)`.
    #[test]
    fn reads_xlsx_hyperlinks_end_to_end() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Links" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="2"><c r="B2" t="inlineStr"><is><t>click</t></is></c></row></sheetData><hyperlinks><hyperlink ref="B2:B4" r:id="rId1"/></hyperlinks></worksheet>"#,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="https://example.com/" TargetMode="External"/></Relationships>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        // The `B2:B4` range expands to every cell (0-based rows 1..=3, col 1), each
        // resolved to the URL from the worksheet rels.
        let url = "https://example.com/".to_string();
        assert_eq!(
            wb.sheets[0].hyperlinks(),
            &[(1u32, 1u16, url.clone()), (2, 1, url.clone()), (3, 1, url)]
        );
    }

    #[test]
    fn normalize_part_target_resolves_relative() {
        assert_eq!(
            normalize_part_target("xl/worksheets/sheet1.xml", "../comments1.xml"),
            "xl/comments1.xml"
        );
        assert_eq!(
            normalize_part_target("xl/worksheets/sheet1.xml", "comments1.xml"),
            "xl/worksheets/comments1.xml"
        );
        assert_eq!(
            normalize_part_target("xl/worksheets/sheet1.xml", "/xl/comments1.xml"),
            "xl/comments1.xml"
        );
    }

    #[test]
    fn parse_comments_resolves_author_and_ref() {
        let xml = r#"<comments>
            <authors><author>Alice</author><author>Bob</author></authors>
            <commentList>
                <comment ref="B2" authorId="1"><text><t>hello </t><t>world</t></text></comment>
                <comment ref="A1" authorId="0"><text><r><t>note</t></r></text></comment>
            </commentList>
        </comments>"#;
        let cs = parse_comments(xml);
        assert_eq!(cs.len(), 2);
        assert_eq!((cs[0].row, cs[0].col), (1, 1)); // B2
        assert_eq!(cs[0].text, "hello world");
        assert_eq!(cs[0].author.as_deref(), Some("Bob"));
        assert_eq!((cs[1].row, cs[1].col), (0, 0)); // A1
        assert_eq!(cs[1].text, "note");
        assert_eq!(cs[1].author.as_deref(), Some("Alice"));
    }

    /// End-to-end `.xlsx` read: a worksheet referencing a `comments1.xml` part
    /// via its rels (relationship Type `.../comments`) surfaces the notes via the
    /// public `comments()` accessor as `(row, col, text, author)`.
    #[test]
    fn reads_xlsx_comments_end_to_end() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Notes" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="3"><c r="C3" t="inlineStr"><is><t>x</t></is></c></row></sheetData></worksheet>"#,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="../comments1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/vmlDrawing" Target="../drawings/vmlDrawing1.vml"/></Relationships>"#,
            ),
            (
                "xl/comments1.xml",
                r#"<comments><authors><author>심사위원</author></authors><commentList><comment ref="C3" authorId="0"><text><t>검토 필요</t></text></comment></commentList></comments>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        let cs = wb.sheets[0].comments();
        assert_eq!(cs.len(), 1);
        // C3 → 0-based (row 2, col 2).
        assert_eq!((cs[0].row, cs[0].col), (2, 2));
        assert_eq!(cs[0].text, "검토 필요");
        assert_eq!(cs[0].author.as_deref(), Some("심사위원"));
    }

    #[test]
    fn parse_table_reads_name_range_columns() {
        // `displayName` is preferred over `name`; `ref` → 0-based inclusive range;
        // `<tableColumn name>` list → header columns; `<tableStyleInfo name>` → style.
        let xml = r#"<table xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" id="1" name="Table1" displayName="가격표" ref="A1:C3"><autoFilter ref="A1:C3"/><tableColumns count="3"><tableColumn id="1" name="품목"/><tableColumn id="2" name="단가"/><tableColumn id="3" name="수량"/></tableColumns><tableStyleInfo name="TableStyleMedium2"/></table>"#;
        let parsed = parse_table(xml).unwrap();
        let t = parsed.table;
        assert_eq!(t.name, "가격표");
        assert_eq!(t.range, (0, 0, 2, 2)); // A1:C3
        assert_eq!(t.columns, vec!["품목", "단가", "수량"]);
        assert_eq!(t.style.as_deref(), Some("TableStyleMedium2"));
    }

    /// End-to-end `.xlsx` read: a worksheet referencing a `tables/table1.xml` part
    /// via its rels (relationship Type `.../table`) surfaces the table via the
    /// public `tables()` accessor with its name, range, and header columns.
    #[test]
    fn reads_xlsx_tables_end_to_end() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        let parts = [
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr/><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rIdStyles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
            ),
            (
                "xl/styles.xml",
                r#"<styleSheet><dxfs count="1"><dxf><font><b/><color rgb="FFFFFFFF"/></font><fill><patternFill patternType="solid"><fgColor rgb="FF123456"/></patternFill></fill><border><bottom style="medium"><color rgb="FFABCDEF"/></bottom></border><alignment horizontal="center" wrapText="1"/></dxf></dxfs><tableStyles count="1" defaultTableStyle="NamedBlue"><tableStyle name="NamedBlue" pivot="0" count="1"><tableStyleElement type="headerRow" dxfId="0"/></tableStyle></tableStyles></styleSheet>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>품목</t></is></c></row></sheetData></worksheet>"#,
            ),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/></Relationships>"#,
            ),
            (
                "xl/tables/table1.xml",
                r#"<table xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" id="1" name="Table1" displayName="가격표" ref="A1:B2"><tableColumns count="2"><tableColumn id="1" name="품목"/><tableColumn id="2" name="단가"/></tableColumns><tableStyleInfo name="NamedBlue"/></table>"#,
            ),
        ];
        for (name, body) in parts {
            zw.start_file(name, opt).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();

        let wb = Workbook::open(&bytes).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        let tables = wb.sheets[0].tables();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "가격표");
        assert_eq!(tables[0].range, (0, 0, 1, 1)); // A1:B2
        assert_eq!(tables[0].columns, vec!["품목", "단가"]);
        let header = wb.sheets[0]
            .table_header_styles()
            .get("가격표")
            .expect("imported named table header style");
        assert_eq!(header.fill, Some(Color::rgb(0x12, 0x34, 0x56)));
        assert_eq!(
            header.font.as_ref().and_then(|font| font.color),
            Some(Color::rgb(0xFF, 0xFF, 0xFF))
        );
        assert!(header.font.as_ref().is_some_and(|font| font.bold));
        assert_eq!(
            header.border.as_ref().map(|border| border.bottom),
            Some(BorderStyle::Medium)
        );
        assert_eq!(
            header
                .border
                .as_ref()
                .and_then(|border| border.bottom_color),
            Some(Color::rgb(0xAB, 0xCD, 0xEF))
        );
        assert_eq!(
            header.align.as_ref().and_then(|align| align.horizontal),
            Some(HAlign::Center)
        );
        assert!(header.align.as_ref().is_some_and(|align| align.wrap));
    }

    #[test]
    fn built_in_table_style_header_uses_theme_accent() {
        let mut theme = ThemeColors::default();
        theme.colors[4] = Some(Color::rgb(1, 2, 3));
        let built_in = built_in_table_style("TableStyleMedium2", &theme).unwrap();
        let style = built_in_table_header_style("TableStyleMedium2", &theme).unwrap();
        assert_eq!(style.fill, Some(Color::rgb(1, 2, 3)));
        assert_eq!(
            style.font.as_ref().and_then(|font| font.color),
            Some(Color::rgb(0xFF, 0xFF, 0xFF))
        );
        assert!(style.font.as_ref().is_some_and(|font| font.bold));
        for region in [
            TableStyleRegion::HeaderRow,
            TableStyleRegion::TotalRow,
            TableStyleRegion::FirstColumn,
            TableStyleRegion::LastColumn,
            TableStyleRegion::FirstRowStripe,
            TableStyleRegion::FirstColumnStripe,
        ] {
            assert!(built_in.definition.get(region).is_some(), "{region:?}");
        }
        assert!(built_in_table_header_style("TableStyleMedium29", &theme).is_none());
    }

    #[test]
    fn direct_xf_masks_retain_explicit_resets_and_complete_builtin_formats() {
        let xml = r#"<styleSheet>
            <fonts count="2"><font><name val="Base"/></font><font><b/></font></fonts>
            <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
            <borders count="1"><border/></borders>
            <cellXfs count="3">
                <xf numFmtId="0" fontId="1" fillId="0" borderId="0" applyFont="0"/>
                <xf numFmtId="0" fontId="0" fillId="0" borderId="0"
                    applyFont="1" applyBorder="1" applyNumberFormat="1"
                    applyAlignment="1"><alignment wrapText="0"/></xf>
                <xf numFmtId="46" fontId="0" fillId="0" borderId="0"
                    applyNumberFormat="1"/>
            </cellXfs>
        </styleSheet>"#;
        let styles = parse_styles(xml, &ThemeColors::default());

        let disabled = styles.cell_style_overlay(0).expect("disabled overlay");
        assert!(!disabled.replace_font, "explicit applyFont=0 must win");
        assert!(overlay_is_empty(disabled));

        let reset = styles.cell_style_overlay(1).expect("reset overlay");
        assert!(reset.replace_font);
        assert!(reset.replace_border);
        assert!(reset.replace_num_fmt);
        assert!(reset.replace_alignment);
        assert!(reset
            .style
            .font
            .as_ref()
            .is_some_and(|font| font.name.as_deref() == Some("Base") && !font.bold));
        assert_eq!(reset.style.border, None, "borderId=0 clears the border");
        assert_eq!(reset.style.num_fmt, None, "numFmtId=0 means General");
        assert_eq!(reset.style.align, Some(Alignment::default()));

        assert_eq!(styles.cell_styles[2].num_fmt.as_deref(), Some("[h]:mm:ss"));
        assert_eq!(
            styles.cell_style_overlays[2].style.num_fmt.as_deref(),
            Some("[h]:mm:ss")
        );
    }

    #[test]
    fn xlsx_style_table_limits_are_bounded_and_typed() {
        let colors = r#"<rgbColor rgb="FF010203"/>"#.repeat(MAX_XLSX_INDEXED_COLORS + 1);
        let overlong_format = "0".repeat(MAX_XLSX_FORMAT_CODE_BYTES + 1);
        let xml = format!(
            r#"<styleSheet><numFmts count="1"><numFmt numFmtId="164" formatCode="{overlong_format}"/></numFmts><colors><indexedColors>{colors}</indexedColors></colors><cellXfs count="1"><xf numFmtId="0"/></cellXfs></styleSheet>"#
        );
        let styles = parse_styles(&xml, &ThemeColors::default());
        assert!(styles.custom.is_empty());
        assert_eq!(styles.indexed_colors.len(), MAX_XLSX_INDEXED_COLORS);
        assert!(styles
            .losses
            .iter()
            .any(|loss| loss.kind == StyleLossKind::LimitExceeded && loss.occurrences >= 2));

        let mut records = vec![(); MAX_XLSX_STYLE_RECORDS];
        let mut losses = Vec::new();
        retain_xlsx_style_record(&mut records, (), &mut losses);
        assert_eq!(records.len(), MAX_XLSX_STYLE_RECORDS);
        assert_eq!(
            losses,
            vec![StyleLoss {
                kind: StyleLossKind::LimitExceeded,
                occurrences: 1,
            }]
        );
    }

    #[test]
    fn empty_direct_xf_mask_still_prevents_full_style_fallback() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="styles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
            ),
            (
                "xl/styles.xml",
                r#"<styleSheet><fonts count="2"><font><name val="Base"/></font><font><b/></font></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="0" applyFont="0"/></cellXfs></styleSheet>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" s="1"><v>1</v></c></row></sheetData></worksheet>"#,
            ),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        let workbook = Workbook::open(&zip.finish().unwrap().into_inner()).unwrap();
        let sheet = &workbook.sheets[0];
        assert!(sheet
            .direct_cell_formats
            .get(&(0, 0))
            .is_some_and(overlay_is_empty));
        let font = sheet
            .resolved_cell_style(0, 0)
            .and_then(|style| style.font)
            .expect("resolved base font");
        assert_eq!(font.name.as_deref(), Some("Base"));
        assert!(!font.bold, "explicit applyFont=0 must not use fontId=1");
    }

    #[test]
    fn custom_table_style_parser_retains_regions_sizes_and_typed_losses() {
        let regions = [
            TableStyleRegion::WholeTable,
            TableStyleRegion::FirstColumnStripe,
            TableStyleRegion::SecondColumnStripe,
            TableStyleRegion::FirstRowStripe,
            TableStyleRegion::SecondRowStripe,
            TableStyleRegion::FirstColumn,
            TableStyleRegion::LastColumn,
            TableStyleRegion::HeaderRow,
            TableStyleRegion::TotalRow,
            TableStyleRegion::FirstHeaderCell,
            TableStyleRegion::LastHeaderCell,
            TableStyleRegion::FirstTotalCell,
            TableStyleRegion::LastTotalCell,
        ];
        let dxfs = regions
            .iter()
            .enumerate()
            .map(|(index, _)| DifferentialStyle {
                style: CellStyle::new().background_color([index as u8, 1, 2]),
                losses: (index == 0)
                    .then_some(StyleLoss {
                        kind: StyleLossKind::UnresolvedColor,
                        occurrences: 1,
                    })
                    .into_iter()
                    .collect(),
            })
            .collect::<Vec<_>>();
        let xml = r#"<styleSheet><tableStyles count="1"><tableStyle name="AllRegions" count="17">
            <tableStyleElement type="wholeTable" dxfId="0"/>
            <tableStyleElement type="firstColumnStripe" size="3" dxfId="1"/>
            <tableStyleElement type="secondColumnStripe" size="9999999" dxfId="2"/>
            <tableStyleElement type="firstRowStripe" size="2" dxfId="3"/>
            <tableStyleElement type="secondRowStripe" dxfId="4"/>
            <tableStyleElement type="firstColumn" dxfId="5"/>
            <tableStyleElement type="lastColumn" dxfId="6"/>
            <tableStyleElement type="headerRow" dxfId="7"/>
            <tableStyleElement type="totalRow" dxfId="8"/>
            <tableStyleElement type="firstHeaderCell" dxfId="9"/>
            <tableStyleElement type="lastHeaderCell" dxfId="10"/>
            <tableStyleElement type="firstTotalCell" dxfId="11"/>
            <tableStyleElement type="lastTotalCell" dxfId="12"/>
            <tableStyleElement type="pageFieldLabels" dxfId="0"/>
            <tableStyleElement type="headerRow" dxfId="999"/>
            <tableStyleElement type="wholeTable" dxfId="1"/>
        </tableStyle></tableStyles></styleSheet>"#;
        let parsed = parse_table_styles(xml, &dxfs)
            .remove("AllRegions")
            .expect("parsed table style");

        for region in regions {
            assert!(
                parsed.definition.get(region).is_some(),
                "missing region {region:?}"
            );
        }
        assert_eq!(
            parsed
                .definition
                .get(TableStyleRegion::FirstColumnStripe)
                .map(|style| style.stripe_size),
            Some(3)
        );
        assert_eq!(
            parsed
                .definition
                .get(TableStyleRegion::FirstRowStripe)
                .map(|style| style.stripe_size),
            Some(2)
        );
        for kind in [
            StyleLossKind::UnsupportedProperty,
            StyleLossKind::MissingReference,
            StyleLossKind::LimitExceeded,
            StyleLossKind::UnresolvedColor,
        ] {
            assert!(
                parsed.losses.iter().any(|loss| loss.kind == kind),
                "missing typed loss {kind:?}: {:?}",
                parsed.losses
            );
        }
    }

    #[test]
    fn xlsx_table_regions_compose_with_sheet_column_row_and_direct_cell_styles() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let styles = r#"<styleSheet>
            <fonts count="3"><font><name val="Base"/></font><font><b/></font><font><i/></font></fonts>
            <fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="solid"><fgColor rgb="FF636363"/></patternFill></fill></fills>
            <borders count="1"><border/></borders>
            <cellXfs count="4">
                <xf numFmtId="2" fontId="0" fillId="0" borderId="0"/>
                <xf numFmtId="2" fontId="1" fillId="0" borderId="0" applyFont="1"/>
                <xf numFmtId="2" fontId="2" fillId="0" borderId="0" applyFont="1"/>
                <xf numFmtId="2" fontId="0" fillId="1" borderId="0" applyFill="1"/>
            </cellXfs>
            <dxfs count="11">
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF0A0A0A"/></patternFill></fill></dxf>
                <dxf><font><b/><color rgb="FFFFFFFF"/></font><fill><patternFill patternType="solid"><fgColor rgb="FF141414"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF1E1E1E"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF282828"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF323232"/></patternFill></fill></dxf>
                <dxf><font><color rgb="FF3C3C3C"/></font></dxf>
                <dxf><font><i/></font></dxf>
                <dxf><font><color rgb="FF505050"/></font></dxf>
                <dxf><font><color rgb="FF5A5A5A"/></font></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF464646"/></patternFill></fill></dxf>
                <dxf><fill><patternFill patternType="solid"><fgColor rgb="FF484848"/></patternFill></fill></dxf>
            </dxfs>
            <tableStyles count="1"><tableStyle name="Layered" count="12">
                <tableStyleElement type="wholeTable" dxfId="0"/>
                <tableStyleElement type="headerRow" dxfId="1"/>
                <tableStyleElement type="totalRow" dxfId="2"/>
                <tableStyleElement type="firstRowStripe" size="2" dxfId="3"/>
                <tableStyleElement type="secondRowStripe" dxfId="4"/>
                <tableStyleElement type="firstColumn" dxfId="5"/>
                <tableStyleElement type="lastColumn" dxfId="6"/>
                <tableStyleElement type="firstHeaderCell" dxfId="7"/>
                <tableStyleElement type="lastTotalCell" dxfId="8"/>
                <tableStyleElement type="firstColumnStripe" dxfId="9"/>
                <tableStyleElement type="secondColumnStripe" dxfId="10"/>
                <tableStyleElement type="pageFieldLabels" dxfId="0"/>
            </tableStyle></tableStyles>
        </styleSheet>"#;
        let worksheet = r#"<worksheet><cols><col min="1" max="1" style="1"/></cols><sheetData>
            <row r="1"><c r="A1" t="inlineStr"><is><t>H1</t></is></c><c r="B1" t="inlineStr"><is><t>H2</t></is></c><c r="C1" t="inlineStr"><is><t>H3</t></is></c></row>
            <row r="2" s="2" customFormat="1"><c r="A2"><v>1</v></c><c r="B2" s="3"><v>2</v></c><c r="C2"><v>3</v></c></row>
            <row r="3"><c r="A3"><v>4</v></c><c r="B3"><v>5</v></c><c r="C3"><v>6</v></c></row>
            <row r="4"><c r="A4"><v>7</v></c><c r="B4"><v>8</v></c><c r="C4"><v>9</v></c></row>
            <row r="5"><c r="A5"><v>10</v></c><c r="B5"><v>11</v></c><c r="C5"><v>12</v></c></row>
        </sheetData><tableParts count="1"><tablePart r:id="rIdTable"/></tableParts></worksheet>"#;
        let table = r#"<table id="1" name="LayeredTable" displayName="LayeredTable" ref="A1:C5" headerRowCount="1" totalsRowCount="1"><tableColumns count="3"><tableColumn id="1" name="H1"/><tableColumn id="2" name="H2"/><tableColumn id="3" name="H3"/></tableColumns><tableStyleInfo name="Layered" showFirstColumn="1" showLastColumn="1" showRowStripes="1" showColumnStripes="1"/></table>"#;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="Sheet1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="styles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
            ),
            ("xl/styles.xml", styles),
            ("xl/worksheets/sheet1.xml", worksheet),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rIdTable" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/></Relationships>"#,
            ),
            ("xl/tables/table1.xml", table),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        let workbook = Workbook::open(&zip.finish().unwrap().into_inner()).unwrap();
        let sheet = &workbook.sheets[0];
        let effective_fill = |row, col| {
            let style = sheet.resolved_cell_style(row, col)?;
            style
                .pattern_fill
                .and_then(|fill| fill.foreground.or(fill.background))
                .or(style.fill)
        };

        assert_eq!(effective_fill(0, 0), Some(Color::rgb(0x14, 0x14, 0x14)));
        assert_eq!(
            sheet
                .resolved_cell_style(0, 0)
                .and_then(|style| style.font)
                .and_then(|font| font.color),
            Some(Color::rgb(0x50, 0x50, 0x50))
        );
        assert_eq!(
            effective_fill(1, 1),
            Some(Color::rgb(0x63, 0x63, 0x63)),
            "direct cell fill must win over both row and column banding"
        );
        let direct = sheet.resolved_cell_style(1, 1).expect("direct style");
        assert!(direct.font.as_ref().is_some_and(|font| font.italic));
        assert_eq!(direct.num_fmt.as_deref(), Some("0.00"));
        assert_eq!(effective_fill(2, 1), Some(Color::rgb(0x28, 0x28, 0x28)));
        assert_eq!(effective_fill(3, 1), Some(Color::rgb(0x32, 0x32, 0x32)));
        assert_eq!(effective_fill(4, 2), Some(Color::rgb(0x1E, 0x1E, 0x1E)));
        assert_eq!(
            sheet
                .resolved_cell_style(4, 2)
                .and_then(|style| style.font)
                .and_then(|font| font.color),
            Some(Color::rgb(0x5A, 0x5A, 0x5A))
        );
        assert!(sheet.style_losses().iter().any(|loss| {
            loss.kind == StyleLossKind::UnsupportedProperty && loss.occurrences == 1
        }));
        assert_eq!(
            sheet.resolved_cell_style(2, 1),
            sheet.resolved_cell_style(2, 1)
        );
    }

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("A1:C3"), Some((0, 0, 2, 2)));
        assert_eq!(parse_range("B2"), Some((1, 1, 1, 1))); // lone ref = 1×1
        assert_eq!(parse_range("A1:"), None);
        assert_eq!(parse_range("junk"), None);
    }

    #[test]
    fn sheet_view_and_autofilter_metadata_is_parsed() {
        let xml = r#"<worksheet>
            <sheetViews>
                <sheetView showGridLines="0" showRowColHeaders="0" rightToLeft="1" zoomScale="125">
                    <pane xSplit="2" ySplit="1" state="frozen"/>
                </sheetView>
            </sheetViews>
            <sheetData/>
            <autoFilter ref="A1:C10"/>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.freeze, Some((1, 2)));
        assert_eq!(parsed.autofilter, Some((0, 0, 9, 2)));
        assert!(parsed.hide_gridlines);
        assert_eq!(parsed.show_headers, Some(false));
        assert!(parsed.right_to_left);
        assert_eq!(parsed.zoom, Some(125));
    }

    #[test]
    fn sheet_view_metadata_uses_primary_view_only() {
        let xml = r#"<worksheet>
            <sheetViews>
                <sheetView workbookViewId="0" zoomScale="110"/>
                <sheetView workbookViewId="1" showGridLines="0" showRowColHeaders="0" rightToLeft="1" zoomScale="125">
                    <pane xSplit="2" ySplit="1" state="frozen"/>
                </sheetView>
            </sheetViews>
            <sheetData/>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.freeze, None);
        assert!(!parsed.hide_gridlines);
        assert_eq!(parsed.show_headers, None);
        assert!(!parsed.right_to_left);
        assert_eq!(parsed.zoom, Some(110));
    }

    #[test]
    fn sheet_view_explicit_visible_headers_are_preserved() {
        let xml = r#"<worksheet>
            <sheetViews>
                <sheetView showRowColHeaders="1"/>
            </sheetViews>
            <sheetData/>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.show_headers, Some(true));
    }

    #[test]
    fn page_setup_first_page_number_requires_use_flag() {
        for (attrs, expected) in [
            (r#"firstPageNumber="7""#, None),
            (r#"firstPageNumber="7" useFirstPageNumber="0""#, None),
            (r#"firstPageNumber="7" useFirstPageNumber="1""#, Some(7)),
        ] {
            let xml = format!(r#"<worksheet><sheetData/><pageSetup {attrs}/></worksheet>"#);
            let mut budget = crate::MAX_TEXT_BYTES;
            let parsed = parse_sheet(
                &xml,
                &[],
                &Styles::default(),
                &ThemeColors::default(),
                false,
                &mut budget,
            );

            assert_eq!(
                parsed
                    .page_setup
                    .as_ref()
                    .and_then(|setup| setup.first_page_number),
                expected,
                "unexpected first_page_number for pageSetup attrs {attrs}"
            );
        }
    }

    #[test]
    fn first_header_footer_falls_back_to_page_setup_metadata() {
        let xml = r#"<worksheet>
            <sheetData/>
            <headerFooter>
                <firstHeader>&amp;CFirst page</firstHeader>
                <firstFooter>&amp;RFirst footer</firstFooter>
            </headerFooter>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        let page_setup = parsed.page_setup.expect("page setup metadata");
        assert_eq!(page_setup.header.as_deref(), Some("&CFirst page"));
        assert_eq!(page_setup.footer.as_deref(), Some("&RFirst footer"));
    }

    #[test]
    fn even_header_footer_falls_back_to_page_setup_metadata() {
        let xml = r#"<worksheet>
            <sheetData/>
            <headerFooter>
                <evenHeader>&amp;LEven pages</evenHeader>
                <evenFooter>&amp;REven footer</evenFooter>
            </headerFooter>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        let page_setup = parsed.page_setup.expect("page setup metadata");
        assert_eq!(page_setup.header.as_deref(), Some("&LEven pages"));
        assert_eq!(page_setup.footer.as_deref(), Some("&REven footer"));
    }

    #[test]
    fn odd_header_footer_overrides_first_even_fallback_metadata() {
        let xml = r#"<worksheet>
            <sheetData/>
            <headerFooter>
                <firstHeader>&amp;CFirst page</firstHeader>
                <evenHeader>&amp;LEven pages</evenHeader>
                <oddHeader>&amp;COdd pages</oddHeader>
                <firstFooter>&amp;RFirst footer</firstFooter>
                <evenFooter>&amp;REven footer</evenFooter>
                <oddFooter>&amp;COdd footer</oddFooter>
            </headerFooter>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        let page_setup = parsed.page_setup.expect("page setup metadata");
        assert_eq!(page_setup.header.as_deref(), Some("&COdd pages"));
        assert_eq!(page_setup.footer.as_deref(), Some("&COdd footer"));
    }

    #[test]
    fn print_sidecar_retains_exact_ooxml_source_metadata() {
        let xml = r#"<worksheet>
            <sheetData/>
            <printOptions gridLines="0" headings="1" horizontalCentered="1" verticalCentered="0"/>
            <pageSetup pageOrder="overThenDown"/>
            <headerFooter differentOddEven="1" differentFirst="1" scaleWithDoc="0" alignWithMargins="1">
                <oddHeader>&amp;COdd</oddHeader><oddFooter>&amp;LOddF</oddFooter>
                <evenHeader>&amp;CEven</evenHeader><evenFooter>&amp;LEvenF</evenFooter>
                <firstHeader>&amp;CFirst</firstHeader><firstFooter>&amp;LFirstF</firstFooter>
            </headerFooter>
            <rowBreaks count="3" manualBreakCount="2">
                <brk id="20" min="0" max="16383" man="1"/>
                <brk id="5" min="0" max="16383" man="1"/>
                <brk id="8" min="0" max="16383" man="0"/>
            </rowBreaks>
            <colBreaks count="2" manualBreakCount="2">
                <brk id="7" min="0" max="1048575" man="1"/>
                <brk id="3" min="0" max="1048575" man="1"/>
            </colBreaks>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let mut parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );
        let names = [SheetDefinedName {
            local_sheet_id: 0,
            name: "_xlnm.Print_Area".to_string(),
            refers_to: "'Print Sheet'!$A$1:$B$2,'Print Sheet'!$D$4:$F$9".to_string(),
        }];
        apply_sheet_defined_names(
            &mut parsed.page_setup,
            &mut parsed.print_metadata,
            &mut parsed.autofilter,
            names.iter(),
        );

        let metadata = &parsed.print_metadata;
        assert_eq!(metadata.fidelity(), crate::PrintFidelity::Retained);
        assert_eq!(metadata.print_areas(), &[(0, 0, 1, 1), (3, 3, 8, 5)]);
        assert_eq!(metadata.manual_row_breaks(), &[5, 20]);
        assert_eq!(metadata.manual_col_breaks(), &[3, 7]);
        assert_eq!(metadata.page_order(), Some(PrintPageOrder::OverThenDown));
        assert_eq!(metadata.print_gridlines(), Some(false));
        assert_eq!(metadata.print_headings(), Some(true));
        assert_eq!(metadata.center_horizontally(), Some(true));
        assert_eq!(metadata.center_vertically(), Some(false));
        let header_footer = metadata.header_footer();
        assert_eq!(header_footer.odd_header(), Some("&COdd"));
        assert_eq!(header_footer.odd_footer(), Some("&LOddF"));
        assert_eq!(header_footer.even_header(), Some("&CEven"));
        assert_eq!(header_footer.even_footer(), Some("&LEvenF"));
        assert_eq!(header_footer.first_header(), Some("&CFirst"));
        assert_eq!(header_footer.first_footer(), Some("&LFirstF"));
        assert_eq!(header_footer.different_odd_even(), Some(true));
        assert_eq!(header_footer.different_first(), Some(true));
        assert_eq!(header_footer.scale_with_document(), Some(false));
        assert_eq!(header_footer.align_with_margins(), Some(true));
        assert_eq!(
            parsed
                .page_setup
                .as_ref()
                .and_then(|setup| setup.print_area),
            Some((0, 0, 1, 1))
        );
    }

    #[test]
    fn malformed_ooxml_print_state_is_typed_not_flattened() {
        let xml = r#"<worksheet><sheetData/><pageSetup pageOrder="sideways"/>
            <rowBreaks><brk id="bad" man="1"/></rowBreaks>
            <headerFooter differentFirst="maybe"><firstHeader>first</firstHeader></headerFooter>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );
        assert_eq!(
            parsed.print_metadata.fidelity(),
            crate::PrintFidelity::Partial
        );
        assert!(parsed
            .print_metadata
            .losses()
            .iter()
            .any(|loss| loss.kind == PrintLossKind::InvalidPageBreak));
        assert!(parsed
            .print_metadata
            .losses()
            .iter()
            .any(|loss| loss.kind == PrintLossKind::MalformedHeaderFooter));
        assert!(parsed
            .print_metadata
            .losses()
            .iter()
            .any(|loss| loss.kind == PrintLossKind::UnsupportedProperty));
    }

    #[test]
    fn self_closing_ooxml_break_container_does_not_capture_stray_breaks() {
        let xml = r#"<worksheet><sheetData/><rowBreaks/><brk id="9" man="1"/></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );
        assert!(parsed.print_metadata.manual_row_breaks().is_empty());
        assert!(parsed.print_metadata.manual_col_breaks().is_empty());
    }

    #[test]
    fn data_validations_metadata_is_parsed() {
        let xml = r#"<worksheet>
            <sheetData/>
            <dataValidations count="3">
                <dataValidation type="list" allowBlank="1" showInputMessage="1" sqref="A1 A3:A4" promptTitle="Pick" prompt="Choose one">
                    <formula1>"Yes,No"</formula1>
                </dataValidation>
                <dataValidation type="whole" operator="between" allowBlank="0" showErrorMessage="1" sqref="B1:B2" errorTitle="Bounds" error="1..9 only">
                    <formula1>1</formula1><formula2>9</formula2>
                </dataValidation>
                <dataValidation type="custom" sqref="C1"><formula1>ISNUMBER(C1)</formula1></dataValidation>
            </dataValidations>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.data_validations.len(), 4);
        assert_eq!(parsed.data_validations[0].sqref, (0, 0, 0, 0));
        assert_eq!(parsed.data_validations[1].sqref, (2, 0, 3, 0));
        assert_eq!(parsed.data_validations[0].kind, DvKind::List);
        assert_eq!(parsed.data_validations[0].formula1, "\"Yes,No\"");
        assert_eq!(
            parsed.data_validations[0].prompt.as_ref(),
            Some(&("Pick".to_string(), "Choose one".to_string()))
        );
        assert!(parsed.data_validations[0].show_input_message);
        assert!(!parsed.data_validations[0].show_error_message);
        let whole = &parsed.data_validations[2];
        assert_eq!(whole.kind, DvKind::Whole);
        assert_eq!(whole.operator, DvOp::Between);
        assert!(!whole.allow_blank);
        assert!(!whole.show_input_message);
        assert!(whole.show_error_message);
        assert_eq!(whole.formula1, "1");
        assert_eq!(whole.formula2.as_deref(), Some("9"));
        assert_eq!(
            whole.error.as_ref(),
            Some(&("Bounds".to_string(), "1..9 only".to_string()))
        );
        assert_eq!(parsed.data_validations[3].kind, DvKind::Custom);
        assert_eq!(parsed.data_validations[3].formula1, "ISNUMBER(C1)");
    }

    #[test]
    fn data_validation_missing_allow_blank_defaults_false() {
        let xml = r#"<worksheet><sheetData/>
            <dataValidations count="1">
                <dataValidation type="whole" sqref="A1" promptTitle="Prompt" prompt="Text" errorTitle="Error" error="Text"><formula1>1</formula1></dataValidation>
            </dataValidations>
        </worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );

        assert_eq!(parsed.data_validations.len(), 1);
        assert!(!parsed.data_validations[0].allow_blank);
        assert!(!parsed.data_validations[0].show_input_message);
        assert!(!parsed.data_validations[0].show_error_message);
        assert_eq!(
            parsed.data_validations[0].prompt.as_ref(),
            Some(&("Prompt".to_string(), "Text".to_string()))
        );
        assert_eq!(
            parsed.data_validations[0].error.as_ref(),
            Some(&("Error".to_string(), "Text".to_string()))
        );
    }

    #[test]
    fn shared_string_excludes_phonetic_ruby() {
        // `<rPh>` carries East Asian ruby (furigana) guide text, not part of the
        // displayed string — it must not be concatenated into the value.
        let xml = r#"<sst><si><t>東京</t><rPh sb="0" eb="2"><t>とうきょう</t></rPh></si></sst>"#;
        assert_eq!(shared_texts(xml), vec!["東京"]);
    }

    #[test]
    fn formula_without_cached_value_is_surfaced() {
        // An uncalculated formula (`<f>` but no `<v>`) must still surface its
        // source as Cell::Formula, not be silently dropped.
        let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\"><f>SUM(B1:B2)</f></c></row></sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        assert_eq!(cells.len(), 1);
        match &cells[0].value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "SUM(B1:B2)");
                assert_eq!(**cached, Cell::Text(String::new()));
            }
            other => panic!("expected a formula cell, got {other:?}"),
        }
    }

    #[test]
    fn self_closing_formula_keeps_cached_value() {
        // A self-closing `<f/>` (e.g. a shared-formula follower) has no formula
        // text and no End event; the following `<v>` must be read as the value,
        // not captured as formula text (which would surface an empty-cached
        // formula and swallow the 42).
        let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\"><f t=\"shared\" si=\"0\"/><v>42</v></c></row></sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].value, Cell::Number(42.0));
    }

    #[test]
    fn shift_formula_engine() {
        assert_eq!(shift_formula("A1+B1", 1, 0), "A2+B2");
        assert_eq!(shift_formula("$A$1+B1", 1, 1), "$A$1+C2");
        assert_eq!(shift_formula("SUM(A1:A3)", 2, 0), "SUM(A3:A5)");
        assert_eq!(shift_formula("LOG10(A1)", 1, 0), "LOG10(A2)"); // function, not a ref
        assert_eq!(shift_formula("\"A1\"&B2", 1, 0), "\"A1\"&B3"); // string literal untouched
        assert_eq!(shift_formula("$A1", 5, 9), "$A6"); // col absolute, row shifts
        assert_eq!(shift_formula("A1", 0, -1), "#REF!"); // shifted off-grid
        assert_eq!(shift_formula("Z9", 1, 1), "AA10"); // column carry
        assert_eq!(shift_formula("'My Sheet'!A1", 1, 0), "'My Sheet'!A2"); // sheet name kept
        assert_eq!(shift_formula("'A1'!B1", 1, 0), "'A1'!B2"); // ref inside '…' not shifted
        assert_eq!(shift_formula("XFE1+1", 1, 0), "XFE1+1"); // off-grid A1-shaped name kept
        assert_eq!(shift_formula("SUM(1:2)", 1, 0), "SUM(2:3)");
        assert_eq!(shift_formula("SUM($1:2)", 1, 0), "SUM($1:3)");
        assert_eq!(shift_formula("SUM(A:B)", 0, 1), "SUM(B:C)");
        assert_eq!(shift_formula("SUM($A:B)", 0, 1), "SUM($A:C)");
    }

    #[test]
    fn shared_formula_follower_with_whitespace() {
        // A pretty-printed follower (whitespace between the self-closing `<f/>` and
        // `<v>`) must not capture that whitespace as formula text and be mis-registered
        // as a master.
        let xml = "<worksheet><sheetData>\
            <row r=\"1\"><c r=\"A1\"><f t=\"shared\" ref=\"A1:A2\" si=\"0\">B1*2</f><v>2</v></c></row>\
            <row r=\"2\"><c r=\"A2\"><f t=\"shared\" si=\"0\"/>\n            <v>4</v></c></row>\
            </sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        match &cells[1].value {
            Cell::Formula { formula, .. } => assert_eq!(formula, "B2*2"),
            o => panic!("whitespace follower not reconstructed: {o:?}"),
        }
    }

    #[test]
    fn shared_formula_follower_is_reconstructed() {
        // Master at A1 defines si=0; follower at A2 must surface the relative-shifted
        // formula (B1 -> B2), not a bare cached value.
        let xml = "<worksheet><sheetData>\
            <row r=\"1\"><c r=\"A1\"><f t=\"shared\" ref=\"A1:A2\" si=\"0\">B1*2</f><v>2</v></c></row>\
            <row r=\"2\"><c r=\"A2\"><f t=\"shared\" si=\"0\"/><v>4</v></c></row>\
            </sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        assert_eq!(cells.len(), 2);
        match &cells[0].value {
            Cell::Formula { formula, .. } => assert_eq!(formula, "B1*2"),
            o => panic!("master not a formula: {o:?}"),
        }
        match &cells[1].value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "B2*2");
                assert_eq!(**cached, Cell::Number(4.0));
            }
            o => panic!("follower not reconstructed: {o:?}"),
        }
    }

    #[test]
    fn iso_date_cell_t_d() {
        // A `t="d"` ISO date cell (emitted by some non-Excel writers) must read as
        // a Date, not be dropped by the numeric fallback.
        let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\" t=\"d\"><v>2024-03-15</v></c></row></sheetData></worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].value, Cell::Date(45366.0));
    }

    #[test]
    fn iso_date_cell_t_d_renders_datetime_and_time_only_values() {
        let xml = "<worksheet><sheetData><row r=\"1\">\
            <c r=\"A1\" s=\"1\" t=\"d\"><v>2021-01-01T10:10:10</v></c>\
            <c r=\"A2\" s=\"2\" t=\"d\"><v>10:10:10</v></c>\
            </row></sheetData></worksheet>";
        let styles = Styles {
            xf_numfmt: vec![0, 22, 20],
            ..Default::default()
        };
        let mut budget = crate::MAX_TEXT_BYTES;
        let cells = parse_sheet(
            xml,
            &[],
            &styles,
            &ThemeColors::default(),
            false,
            &mut budget,
        )
        .cells;

        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].text, "2021-01-01 10:10:10");
        assert_eq!(cells[1].text, "10:10:10");
    }

    #[test]
    fn reads_merged_ranges_and_formula() {
        let xml = "<worksheet><sheetData>\
            <row r=\"1\"><c r=\"A1\"><f>SUM(B1:B2)</f><v>30</v></c>\
            <c r=\"B1\"><v>10</v></c></row>\
            <row r=\"2\"><c r=\"B2\"><v>20</v></c></row>\
            </sheetData>\
            <mergeCells count=\"1\"><mergeCell ref=\"A1:C1\"/></mergeCells>\
            </worksheet>";
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(
            xml,
            &[],
            &Styles::default(),
            &ThemeColors::default(),
            false,
            &mut budget,
        );
        let cells = parsed.cells;
        assert_eq!(parsed.merges, vec![(0, 0, 0, 2)]); // A1:C1

        // The formula cell exposes both the source text and the cached value.
        let a1 = cells.iter().find(|c| c.row == 0 && c.col == 0).unwrap();
        match &a1.value {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "SUM(B1:B2)");
                assert_eq!(**cached, Cell::Number(30.0));
            }
            other => panic!("expected a formula cell, got {other:?}"),
        }
        assert_eq!(a1.text, "30"); // display text is the cached value
    }

    #[test]
    fn conditional_metadata_retains_priority_stop_full_dxf_and_losses() {
        let styles_xml = r#"<styleSheet>
            <dxfs count="1"><dxf>
                <font><b/><color rgb="FF112233"/><outline/></font>
                <fill><patternFill patternType="solid"><fgColor rgb="FF445566"/></patternFill></fill>
                <border><left style="thin"><color rgb="FF778899"/></left><diagonal style="thin"/></border>
                <numFmt numFmtId="166" formatCode="0.000"/>
                <alignment horizontal="center" wrapText="1" readingOrder="2"/>
                <protection locked="0" hidden="1"/>
            </dxf></dxfs>
        </styleSheet>"#;
        let theme = ThemeColors::default();
        let styles = parse_styles(styles_xml, &theme);
        let xml = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>2</v></c></row></sheetData>
            <conditionalFormatting sqref="A1">
                <cfRule type="cellIs" dxfId="0" priority="7" stopIfTrue="1" operator="greaterThan"><formula>1</formula></cfRule>
            </conditionalFormatting></worksheet>"#;
        let mut budget = crate::MAX_TEXT_BYTES;
        let parsed = parse_sheet(xml, &[], &styles, &theme, false, &mut budget);

        assert_eq!(parsed.cond_formats.len(), 1);
        assert_eq!(parsed.cond_format_metadata.len(), 1);
        let metadata = &parsed.cond_format_metadata[0];
        assert_eq!(metadata.priority, Some(7));
        assert!(metadata.stop_if_true);
        let dxf = metadata
            .differential_style
            .as_ref()
            .expect("retained differential style");
        assert_eq!(dxf.fill, Some(Color::rgb(0x44, 0x55, 0x66)));
        assert_eq!(
            dxf.font.as_ref().and_then(|font| font.color),
            Some(Color::rgb(0x11, 0x22, 0x33))
        );
        assert!(dxf.font.as_ref().is_some_and(|font| font.bold));
        assert_eq!(
            dxf.border.as_ref().map(|border| border.left),
            Some(BorderStyle::Thin)
        );
        assert_eq!(dxf.num_fmt.as_deref(), Some("0.000"));
        assert!(dxf.align.as_ref().is_some_and(|align| align.wrap));
        assert!(dxf
            .protection
            .as_ref()
            .is_some_and(|protection| protection.locked == Some(false) && protection.hidden));
        assert!(metadata.style_losses.iter().any(|loss| {
            loss.kind == StyleLossKind::UnsupportedProperty && loss.occurrences >= 2
        }));
    }
}
#[test]
fn zero_print_title_rows_are_rejected_without_panicking() {
    assert_eq!(parse_repeat_rows("0:1"), None);
    assert_eq!(parse_repeat_rows("$0:$1"), None);
    assert_eq!(parse_repeat_rows("1:1048577"), None);
}
