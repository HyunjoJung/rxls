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

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::{Error, Result};
use crate::{
    format, Cell, CellEntry, CfRule, Chart, ChartKind, Color, Comment, CondFormat, DataValidation,
    DocProperties, DvKind, DvOp, Image, ImageFmt, PageSetup, ProtectionOptions, Series, Sheet,
    SheetType, Sparkline, SparklineKind, Table, Workbook,
};

/// Detect the ZIP/OOXML magic (`PK\x03\x04`).
pub(crate) fn is_xlsx(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
}

pub(crate) fn open(bytes: &[u8]) -> Result<Workbook> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|_| Error::Zip("not a valid spreadsheet ZIP container"))?;

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
    let shared =
        workbook_related_part(&mut zip, &workbook_path, &rels, &rel_types, "sharedStrings")
            .or_else(|| {
                part(
                    &mut zip,
                    &normalize_part_target(&workbook_path, "sharedStrings.xml"),
                )
            })
            .map(|s| parse_shared_strings(&s))
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
    let parsed = parse_workbook(&workbook_xml);
    let ParsedWorkbook {
        sheets: sheet_refs,
        date1904,
        structure_protected,
        active_sheet,
        defined_names,
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
            part(&mut zip, &path)
                .map(|s| parse_sheet(&s, &shared, &styles, &theme, date1904, &mut budget))
                .unwrap_or_default()
        } else {
            ParsedSheet::default()
        };
        let ParsedSheet {
            cells,
            merges,
            hyperlink_refs,
            freeze,
            mut autofilter,
            data_validations,
            cond_formats,
            mut page_setup,
            sparklines,
            tab_color,
            print_gridlines,
            print_headings,
            row_outline,
            col_outline,
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
        let tables = sheet_rels_xml
            .as_deref()
            .map(table_targets)
            .unwrap_or_default()
            .into_iter()
            .map(|target| normalize_part_target(&path, &target))
            .filter_map(|p| part(&mut zip, &p))
            .filter_map(|s| parse_table(&s))
            .collect();
        let images = read_drawing_images(&mut zip, &path, sheet_rels_xml.as_deref());
        let charts = read_drawing_charts(&mut zip, &path, sheet_rels_xml.as_deref());
        apply_sheet_defined_names(
            &mut page_setup,
            &mut autofilter,
            sheet_defined_names
                .iter()
                .filter(|name| name.local_sheet_id == sheet_idx),
        );
        sheets.push(Sheet {
            name,
            is_worksheet,
            sheet_type: Some(sheet_type),
            cells,
            read_merges: merges,
            read_hyperlinks,
            comments,
            tables,
            images,
            charts,
            freeze,
            autofilter,
            page_setup,
            data_validations,
            cond_formats,
            sparklines,
            tab_color,
            print_gridlines,
            print_headings,
            row_outline,
            col_outline,
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
        properties,
        defined_names,
        ..Default::default()
    })
}

/// Read a ZIP entry to a UTF-8 string, if present. Capped to guard against a
/// zip bomb (a tiny entry that decompresses to gigabytes).
fn part(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, name: &str) -> Option<String> {
    const MAX_PART: u64 = 256 << 20; // 256 MiB per entry
    let idx = part_index(zip, name)?;
    let f = zip.by_index(idx).ok()?;
    let mut s = String::new();
    f.take(MAX_PART).read_to_string(&mut s).ok()?;
    Some(s)
}

fn part_bytes(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, name: &str) -> Option<Vec<u8>> {
    const MAX_PART: u64 = 256 << 20; // 256 MiB per entry
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

/// Per-style number format, derived from `styles.xml`.
#[derive(Default)]
struct Styles {
    /// `numFmtId` per `cellXfs` style index.
    xf_numfmt: Vec<u16>,
    /// Custom `formatCode` strings keyed by `numFmtId`.
    custom: HashMap<u16, String>,
    /// Custom OOXML indexed color table from `<colors><indexedColors>`.
    indexed_colors: Vec<Color>,
    /// Solid fill color per `dxfs` index, used by conditional formatting rules.
    dxf_fills: Vec<Option<Color>>,
}

impl Styles {
    fn kind(&self, style_idx: usize) -> format::Kind {
        let numfmt_id = self.xf_numfmt.get(style_idx).copied().unwrap_or(0);
        format::classify(numfmt_id, self.custom.get(&numfmt_id).map(String::as_str))
    }

    fn dxf_fill(&self, dxf_id: usize) -> Option<Color> {
        self.dxf_fills.get(dxf_id).copied().flatten()
    }
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
            a.unescape_value().ok().map(|v| v.into_owned())
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

impl ThemeColors {
    fn color(&self, idx: usize, tint: Option<f64>) -> Option<Color> {
        let color = self.colors.get(idx).copied().flatten()?;
        Some(apply_optional_tint(color, tint))
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

fn parse_indexed_colors(xml: &str) -> Vec<Color> {
    let mut r = Reader::from_str(xml);
    let mut colors = Vec::new();
    let mut in_indexed_colors = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"indexedColors" => in_indexed_colors = true,
                b"rgbColor" if in_indexed_colors => {
                    if let Some(color) = attr(&e, b"rgb").as_deref().and_then(parse_color) {
                        colors.push(color);
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(e)) if in_indexed_colors && local(e.name().as_ref()) == b"rgbColor" => {
                if let Some(color) = attr(&e, b"rgb").as_deref().and_then(parse_color) {
                    colors.push(color);
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
    e.unescape().map(|c| c.into_owned()).unwrap_or_default()
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

/// `<sst><si>…<t>text</t>…</si>` — concatenate `<t>` runs within each `<si>`,
/// but skip `<rPh>` (East Asian phonetic / ruby guide) text, which is not part of
/// the displayed string.
fn parse_shared_strings(xml: &str) -> Vec<String> {
    let mut r = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_si = false;
    let mut in_t = false;
    let mut in_rph = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"si" => {
                    in_si = true;
                    cur.clear();
                }
                b"rPh" => in_rph = true,
                b"t" => in_t = true,
                _ => {}
            },
            // A self-closing `<si/>` is an empty string — it must still occupy an
            // index slot, or every later shared-string reference shifts.
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == b"si" => {
                out.push(String::new());
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"si" => {
                    in_si = false;
                    out.push(std::mem::take(&mut cur));
                }
                b"rPh" => in_rph = false,
                b"t" => in_t = false,
                _ => {}
            },
            Ok(Event::Text(t)) if in_si && in_t && !in_rph => cur.push_str(&text_of(&t)),
            Ok(Event::CData(t)) if in_si && in_t && !in_rph => {
                cur.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// `<styleSheet>`: `<numFmts><numFmt numFmtId formatCode/>` + the `<cellXfs>`
/// `<xf numFmtId/>` list (cell `s` indexes cellXfs).
fn parse_styles(xml: &str, theme: &ThemeColors) -> Styles {
    let mut r = Reader::from_str(xml);
    let mut styles = Styles {
        indexed_colors: parse_indexed_colors(xml),
        ..Styles::default()
    };
    let mut in_cell_xfs = false;
    let mut in_dxfs = false;
    let mut current_dxf_fill: Option<Color> = None;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"numFmt" => {
                    if let (Some(id), Some(code)) = (attr(&e, b"numFmtId"), attr(&e, b"formatCode"))
                    {
                        if let Ok(id) = id.parse::<u16>() {
                            styles.custom.insert(id, code);
                        }
                    }
                }
                b"cellXfs" => in_cell_xfs = true,
                b"dxfs" => in_dxfs = true,
                b"dxf" if in_dxfs => current_dxf_fill = None,
                b"xf" if in_cell_xfs => {
                    let id = attr(&e, b"numFmtId")
                        .and_then(|s| s.parse::<u16>().ok())
                        .unwrap_or(0);
                    styles.xf_numfmt.push(id);
                }
                b"fgColor" | b"bgColor" if in_dxfs && current_dxf_fill.is_none() => {
                    current_dxf_fill = color_attr(&e, theme, &styles.indexed_colors);
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"numFmt" => {
                    if let (Some(id), Some(code)) = (attr(&e, b"numFmtId"), attr(&e, b"formatCode"))
                    {
                        if let Ok(id) = id.parse::<u16>() {
                            styles.custom.insert(id, code);
                        }
                    }
                }
                b"xf" if in_cell_xfs => {
                    let id = attr(&e, b"numFmtId")
                        .and_then(|s| s.parse::<u16>().ok())
                        .unwrap_or(0);
                    styles.xf_numfmt.push(id);
                }
                b"dxf" if in_dxfs => styles.dxf_fills.push(None),
                b"fgColor" | b"bgColor" if in_dxfs && current_dxf_fill.is_none() => {
                    current_dxf_fill = color_attr(&e, theme, &styles.indexed_colors);
                }
                _ => {}
            },
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"cellXfs" => in_cell_xfs = false,
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"dxf" && in_dxfs => {
                styles.dxf_fills.push(current_dxf_fill.take());
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"dxfs" => in_dxfs = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    styles
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
    sheet_defined_names: Vec<SheetDefinedName>,
}

enum DefinedNameCapture {
    GlobalUser(String),
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
                        _ => None,
                    };
                    cur_refers.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if cur_name.is_some() => cur_refers.push_str(&text_of(&t)),
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"definedName" => {
                if let Some(name) = cur_name.take() {
                    match name {
                        DefinedNameCapture::GlobalUser(name) => {
                            defined_names.push((name, std::mem::take(&mut cur_refers)));
                        }
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
    ParsedWorkbook {
        sheets,
        date1904,
        structure_protected,
        active_sheet,
        defined_names,
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

struct DrawingRef {
    kind: DrawingRefKind,
    rid: String,
    from: (u32, u16),
    to: Option<(u32, u16)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DrawingRefKind {
    Image,
    Chart,
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
}

fn parse_drawing_refs(xml: &str) -> Vec<DrawingRef> {
    const XLSX_MAX_ROW: u32 = 1_048_575;
    const XLSX_MAX_COL: u16 = 16_383;

    let mut r = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut in_anchor = false;
    let mut section: Option<AnchorSection> = None;
    let mut field: Option<AnchorField> = None;
    let mut rid: Option<String> = None;
    let mut kind: Option<DrawingRefKind> = None;
    let mut from = (0u32, 0u16);
    let mut to_row: Option<u32> = None;
    let mut to_col: Option<u16> = None;

    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"twoCellAnchor" | b"oneCellAnchor" => {
                    in_anchor = true;
                    section = None;
                    field = None;
                    rid = None;
                    kind = None;
                    from = (0, 0);
                    to_row = None;
                    to_col = None;
                }
                b"from" if in_anchor => section = Some(AnchorSection::From),
                b"to" if in_anchor => section = Some(AnchorSection::To),
                b"row" if in_anchor => field = Some(AnchorField::Row),
                b"col" if in_anchor => field = Some(AnchorField::Col),
                b"blip" if in_anchor && rid.is_none() => {
                    rid = attr(&e, b"embed");
                    kind = Some(DrawingRefKind::Image);
                }
                b"chart" if in_anchor && rid.is_none() => {
                    rid = attr(&e, b"id");
                    kind = Some(DrawingRefKind::Chart);
                }
                _ => {}
            },
            Ok(Event::Empty(e)) if in_anchor && rid.is_none() => match local(e.name().as_ref()) {
                b"blip" => {
                    rid = attr(&e, b"embed");
                    kind = Some(DrawingRefKind::Image);
                }
                b"chart" => {
                    rid = attr(&e, b"id");
                    kind = Some(DrawingRefKind::Chart);
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_anchor => {
                if let (Some(section), Some(field), Ok(n)) =
                    (section, field, text_of(&t).parse::<u32>())
                {
                    match (section, field) {
                        (AnchorSection::From, AnchorField::Row) => from.0 = n.min(XLSX_MAX_ROW),
                        (AnchorSection::From, AnchorField::Col) => {
                            from.1 = (n.min(u32::from(XLSX_MAX_COL))) as u16;
                        }
                        (AnchorSection::To, AnchorField::Row) => to_row = Some(n.min(XLSX_MAX_ROW)),
                        (AnchorSection::To, AnchorField::Col) => {
                            to_col = Some((n.min(u32::from(XLSX_MAX_COL))) as u16);
                        }
                    }
                }
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"row" | b"col" => field = None,
                b"from" | b"to" => section = None,
                b"twoCellAnchor" | b"oneCellAnchor" if in_anchor => {
                    if let (Some(rid), Some(kind)) = (rid.take(), kind) {
                        out.push(DrawingRef {
                            kind,
                            rid,
                            from,
                            to: to_row.zip(to_col),
                        });
                    }
                    in_anchor = false;
                    section = None;
                    field = None;
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
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

fn read_drawing_images(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    sheet_path: &str,
    sheet_rels_xml: Option<&str>,
) -> Vec<Image> {
    let Some(drawing_target) = sheet_rels_xml.and_then(drawing_target) else {
        return Vec::new();
    };
    let drawing_path = normalize_part_target(sheet_path, &drawing_target);
    let Some(drawing_xml) = part(zip, &drawing_path) else {
        return Vec::new();
    };
    let refs = parse_drawing_refs(&drawing_xml);
    if refs.is_empty() {
        return Vec::new();
    }
    let drawing_rels = part(zip, &sheet_rels_path(&drawing_path))
        .map(|s| parse_rels(&s))
        .unwrap_or_default();

    refs.into_iter()
        .filter(|item| item.kind == DrawingRefKind::Image)
        .filter_map(|img| {
            let target = drawing_rels.get(&img.rid)?;
            let media_path = normalize_part_target(&drawing_path, target);
            let format = image_format(&media_path)?;
            let data = part_bytes(zip, &media_path)?;
            Some(Image {
                data,
                format,
                from: img.from,
                to: img.to,
            })
        })
        .collect()
}

fn read_drawing_charts(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    sheet_path: &str,
    sheet_rels_xml: Option<&str>,
) -> Vec<Chart> {
    let Some(drawing_target) = sheet_rels_xml.and_then(drawing_target) else {
        return Vec::new();
    };
    let drawing_path = normalize_part_target(sheet_path, &drawing_target);
    let Some(drawing_xml) = part(zip, &drawing_path) else {
        return Vec::new();
    };
    let refs = parse_drawing_refs(&drawing_xml);
    if refs.is_empty() {
        return Vec::new();
    }
    let drawing_rels = part(zip, &sheet_rels_path(&drawing_path))
        .map(|s| parse_rels(&s))
        .unwrap_or_default();

    refs.into_iter()
        .filter(|item| item.kind == DrawingRefKind::Chart)
        .filter_map(|chart_ref| {
            let target = drawing_rels.get(&chart_ref.rid)?;
            let chart_path = normalize_part_target(&drawing_path, target);
            let chart_xml = part(zip, &chart_path)?;
            parse_chart(
                &chart_xml,
                chart_ref.from,
                chart_ref.to.unwrap_or(chart_ref.from),
            )
        })
        .collect()
}

#[derive(Default)]
struct ParsedChartSeries {
    name: Option<String>,
    categories: Option<String>,
    values: Option<String>,
    bubble_sizes: Option<String>,
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

fn parse_chart(xml: &str, from: (u32, u16), to: (u32, u16)) -> Option<Chart> {
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
    let mut current_series: Option<ParsedChartSeries> = None;
    let mut series_field: Option<ChartSeriesField> = None;
    let mut capture_series_field: Option<ChartSeriesField> = None;
    let mut series_cache_depth = 0usize;
    let mut axis_context: Option<ChartAxisContext> = None;
    let mut val_axis_count = 0usize;

    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"barChart" => kind = Some(ChartKind::Bar),
                b"lineChart" => kind = Some(ChartKind::Line),
                b"pieChart" => kind = Some(ChartKind::Pie),
                b"scatterChart" => kind = Some(ChartKind::Scatter),
                b"areaChart" => kind = Some(ChartKind::Area),
                b"doughnutChart" => kind = Some(ChartKind::Doughnut),
                b"radarChart" => kind = Some(ChartKind::Radar),
                b"bubbleChart" => kind = Some(ChartKind::Bubble),
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
                b"strCache" | b"numCache" | b"multiLvlStrCache" if current_series.is_some() => {
                    series_cache_depth += 1;
                }
                b"f" if current_series.is_some() => {
                    capture_series_field = series_field;
                }
                b"v" if current_series.is_some() && series_cache_depth == 0 => {
                    capture_series_field = series_field;
                }
                b"t" | b"v" if title_target.is_some() => in_title_text = true,
                _ => {}
            },
            Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"barChart" => kind = Some(ChartKind::Bar),
                b"lineChart" => kind = Some(ChartKind::Line),
                b"pieChart" => kind = Some(ChartKind::Pie),
                b"scatterChart" => kind = Some(ChartKind::Scatter),
                b"areaChart" => kind = Some(ChartKind::Area),
                b"doughnutChart" => kind = Some(ChartKind::Doughnut),
                b"radarChart" => kind = Some(ChartKind::Radar),
                b"bubbleChart" => kind = Some(ChartKind::Bubble),
                b"legend" => legend = true,
                b"dLbls" => data_labels = true,
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if let Some(field) = capture_series_field {
                    if let Some(series) = current_series.as_mut() {
                        match field {
                            ChartSeriesField::Name => series.name = Some(text_of(&t)),
                            ChartSeriesField::Categories => series.categories = Some(text_of(&t)),
                            ChartSeriesField::Values => series.values = Some(text_of(&t)),
                            ChartSeriesField::BubbleSizes => {
                                series.bubble_sizes = Some(text_of(&t))
                            }
                        }
                    }
                } else if title_target.is_some() && in_title_text {
                    title_text.push_str(&text_of(&t));
                }
            }
            Ok(Event::CData(t)) => {
                let text = String::from_utf8_lossy(t.into_inner().as_ref()).into_owned();
                if let Some(field) = capture_series_field {
                    if let Some(series) = current_series.as_mut() {
                        match field {
                            ChartSeriesField::Name => series.name = Some(text),
                            ChartSeriesField::Categories => series.categories = Some(text),
                            ChartSeriesField::Values => series.values = Some(text),
                            ChartSeriesField::BubbleSizes => series.bubble_sizes = Some(text),
                        }
                    }
                } else if title_target.is_some() && in_title_text {
                    title_text.push_str(&text);
                }
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
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
                b"strCache" | b"numCache" | b"multiLvlStrCache" if series_cache_depth > 0 => {
                    series_cache_depth -= 1;
                }
                b"tx" | b"cat" | b"xVal" | b"val" | b"yVal" | b"bubbleSize"
                    if current_series.is_some() =>
                {
                    series_field = None;
                }
                b"ser" => {
                    if let Some(parsed) = current_series.take() {
                        if let Some(values) = parsed.values {
                            series.push(Series {
                                name: parsed.name,
                                categories: parsed.categories,
                                values,
                                bubble_sizes: parsed.bubble_sizes,
                            });
                        }
                    }
                    series_field = None;
                    capture_series_field = None;
                    series_cache_depth = 0;
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    Some(Chart {
        kind: kind?,
        title,
        series,
        legend,
        data_labels,
        x_axis_title,
        y_axis_title,
        from,
        to,
    })
}

fn parse_table(xml: &str) -> Option<Table> {
    let mut r = Reader::from_str(xml);
    let mut range: Option<(u32, u16, u32, u16)> = None;
    // Prefer `displayName`, falling back to `name`.
    let mut display_name: Option<String> = None;
    let mut name: Option<String> = None;
    let mut style: Option<String> = None;
    let mut columns: Vec<String> = Vec::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"table" => {
                    range = attr(&e, b"ref").as_deref().and_then(parse_range);
                    display_name = attr(&e, b"displayName");
                    name = attr(&e, b"name");
                }
                b"tableColumn" => {
                    if let Some(n) = attr(&e, b"name") {
                        columns.push(n);
                    }
                }
                b"tableStyleInfo" => style = attr(&e, b"name"),
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
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
    merges: Vec<(u32, u16, u32, u16)>,
    hyperlink_refs: Vec<(u32, u16, String)>,
    freeze: Option<(u32, u16)>,
    autofilter: Option<(u32, u16, u32, u16)>,
    data_validations: Vec<DataValidation>,
    cond_formats: Vec<CondFormat>,
    page_setup: Option<PageSetup>,
    sparklines: Vec<Sparkline>,
    tab_color: Option<Color>,
    print_gridlines: bool,
    print_headings: bool,
    row_outline: BTreeMap<u32, u8>,
    col_outline: BTreeMap<u16, u8>,
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
    shared: &[String],
    styles: &Styles,
    theme: &ThemeColors,
    date1904: bool,
    budget: &mut usize,
) -> ParsedSheet {
    let mut r = Reader::from_str(xml);
    let mut parsed = ParsedSheet::default();
    // Current cell state.
    let mut rc: Option<(u32, u16)> = None;
    let mut ctype = String::new();
    let mut style_idx = 0usize;
    let mut value = String::new();
    let mut inline_value = String::new();
    let mut inline_text_seen = false;
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
    let mut header_footer_capture: Option<HeaderFooterField> = None;
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
                if let Some((field, preferred)) = header_footer_field(local(e.name().as_ref())) {
                    begin_header_footer_capture(&mut parsed, field, preferred);
                }
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
                    if let Some(level) = attr(&e, b"outlineLevel")
                        .and_then(|s| s.parse::<u8>().ok())
                        .filter(|level| *level > 0)
                    {
                        let first = attr(&e, b"min")
                            .and_then(|s| s.parse::<u32>().ok())
                            .unwrap_or(1)
                            .max(1);
                        let last = attr(&e, b"max")
                            .and_then(|s| s.parse::<u32>().ok())
                            .unwrap_or(first)
                            .min(16_384);
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
                    if attr(&e, b"gridLines").as_deref().is_some_and(attr_true) {
                        parsed.print_gridlines = true;
                    }
                    if attr(&e, b"headings").as_deref().is_some_and(attr_true) {
                        parsed.print_headings = true;
                    }
                    if attr(&e, b"horizontalCentered")
                        .as_deref()
                        .is_some_and(attr_true)
                    {
                        page_setup_mut(&mut parsed).center_horizontally = true;
                    }
                    if attr(&e, b"verticalCentered")
                        .as_deref()
                        .is_some_and(attr_true)
                    {
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
                b"oddHeader" | b"firstHeader" | b"evenHeader" | b"oddFooter" | b"firstFooter"
                | b"evenFooter" => {
                    if let Some((field, preferred)) = header_footer_field(local(e.name().as_ref()))
                    {
                        header_footer_capture =
                            begin_header_footer_capture(&mut parsed, field, preferred);
                    }
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
                if let Some(field) = header_footer_capture {
                    append_header_footer_text(&mut parsed, field, &text_of(&t));
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
            Ok(Event::Text(t)) if in_is_t && !in_rph => inline_value.push_str(&text_of(&t)),
            Ok(Event::CData(t)) if header_footer_capture.is_some() => {
                if let Some(field) = header_footer_capture {
                    let bytes = t.into_inner();
                    let text = String::from_utf8_lossy(bytes.as_ref());
                    append_header_footer_text(&mut parsed, field, text.as_ref());
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
                inline_value.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"v" => in_v = false,
                b"f" if in_sparkline_formula => in_sparkline_formula = false,
                b"f" => in_f = false,
                b"rPh" => in_rph = false,
                b"t" => in_is_t = false,
                b"sheetView" => in_selected_sheet_view = false,
                b"oddHeader" | b"firstHeader" | b"evenHeader" | b"oddFooter" | b"firstFooter"
                | b"evenFooter" => header_footer_capture = None,
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

fn header_footer_field(name: &[u8]) -> Option<(HeaderFooterField, bool)> {
    match name {
        b"oddHeader" => Some((HeaderFooterField::Header, true)),
        b"firstHeader" | b"evenHeader" => Some((HeaderFooterField::Header, false)),
        b"oddFooter" => Some((HeaderFooterField::Footer, true)),
        b"firstFooter" | b"evenFooter" => Some((HeaderFooterField::Footer, false)),
        _ => None,
    }
}

fn begin_header_footer_capture(
    parsed: &mut ParsedSheet,
    field: HeaderFooterField,
    preferred: bool,
) -> Option<HeaderFooterField> {
    let page_setup = page_setup_mut(parsed);
    let slot = match field {
        HeaderFooterField::Header => &mut page_setup.header,
        HeaderFooterField::Footer => &mut page_setup.footer,
    };
    if preferred || slot.is_none() {
        *slot = Some(String::new());
        Some(field)
    } else {
        None
    }
}

fn append_header_footer_text(parsed: &mut ParsedSheet, field: HeaderFooterField, text: &str) {
    match field {
        HeaderFooterField::Header => {
            if let Some(header) = page_setup_mut(parsed).header.as_mut() {
                header.push_str(text);
            }
        }
        HeaderFooterField::Footer => {
            if let Some(footer) = page_setup_mut(parsed).footer.as_mut() {
                footer.push_str(text);
            }
        }
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
    autofilter: &mut Option<SheetRange>,
    names: I,
) where
    I: IntoIterator<Item = &'a SheetDefinedName>,
{
    for name in names {
        match name.name.as_str() {
            "_xlnm.Print_Area" => {
                if let Some(range) = parse_defined_name_range(&name.refers_to) {
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
    (1..=1_048_576).contains(&row).then_some(row - 1)
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

fn parse_dxf_fill(e: &quick_xml::events::BytesStart<'_>, styles: &Styles) -> Option<Color> {
    attr(e, b"dxfId")
        .and_then(|s| s.parse::<usize>().ok())
        .and_then(|id| styles.dxf_fill(id))
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
    let kind = match ty.as_str() {
        "cellIs" => PendingCfKind::CellIs {
            op: attr(e, b"operator")
                .as_deref()
                .and_then(parse_dv_op)
                .unwrap_or(DvOp::Between),
            fill: parse_dxf_fill(e, styles)?,
        },
        "colorScale" => PendingCfKind::ColorScale,
        "dataBar" => PendingCfKind::DataBar,
        "top10" => PendingCfKind::TopBottom {
            rank: attr(e, b"rank")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(10),
            bottom: attr(e, b"bottom").as_deref().is_some_and(attr_true),
            percent: attr(e, b"percent").as_deref().is_some_and(attr_true),
            fill: parse_dxf_fill(e, styles)?,
        },
        "aboveAverage" => PendingCfKind::AboveAverage {
            below: attr(e, b"aboveAverage").as_deref().is_some_and(attr_false),
            fill: parse_dxf_fill(e, styles)?,
        },
        "duplicateValues" => PendingCfKind::DuplicateValues {
            unique: false,
            fill: parse_dxf_fill(e, styles)?,
        },
        "uniqueValues" => PendingCfKind::DuplicateValues {
            unique: true,
            fill: parse_dxf_fill(e, styles)?,
        },
        "expression" => PendingCfKind::Expression {
            fill: parse_dxf_fill(e, styles)?,
        },
        _ => return None,
    };
    Some(PendingCfRule {
        ranges: ranges.to_vec(),
        kind,
        formulas: Vec::new(),
        colors: Vec::new(),
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
    shared: &[String],
    styles: &Styles,
    date1904: bool,
) -> Option<CellEntry> {
    // The cached value (the displayed result), if one is present and parseable.
    let cached: Option<(Cell, String)> = match ctype {
        "s" => value
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|idx| shared.get(idx).cloned())
            .map(|s| (Cell::Text(s.clone()), s)),
        "str" | "inlineStr" if !value.is_empty() => {
            Some((Cell::Text(value.to_string()), value.to_string()))
        }
        "b" if !value.trim().is_empty() => {
            let b = value.trim() == "1";
            Some((Cell::Bool(b), if b { "TRUE" } else { "FALSE" }.to_string()))
        }
        "e" if !value.is_empty() => Some((Cell::Error(value.to_string()), value.to_string())),
        // ISO-8601 date/time cell (`t="d"`, emitted by some non-Excel writers).
        "d" if !value.is_empty() => format::iso_date_to_serial(value).map(|serial| {
            let kind = styles.kind(style_idx);
            let display = if kind.is_datetime() {
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
            let display = format::render_value(f, kind, date1904);
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
        style: None,
        hyperlink: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let xml = r#"<sst><si><t>Hello</t></si><si><r><t>가</t></r><r><t>나</t></r></si></sst>"#;
        assert_eq!(parse_shared_strings(xml), vec!["Hello", "가나"]);
    }

    #[test]
    fn shared_strings_keep_empty_slots() {
        // A self-closing <si/> and an empty <si></si> must each occupy an index,
        // so later references don't shift.
        let xml = r#"<sst><si><t>품목</t></si><si/><si></si><si><t>가격</t></si></sst>"#;
        assert_eq!(parse_shared_strings(xml), vec!["품목", "", "", "가격"]);
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
        let shared = vec!["X".repeat(100)];
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
        let shared = vec!["X".repeat(100)];
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
        // Only the workbook-global user name `TaxRate` remains: the built-in
        // `_xlnm.Print_Area` and the sheet-local `LocalOnly` are both filtered out.
        assert_eq!(
            parsed.defined_names,
            vec![("TaxRate".to_string(), "Sheet1!$B$1".to_string())]
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
    fn chart_series_refs_ignore_cached_point_values() {
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
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
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
        let t = parse_table(xml).unwrap();
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
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
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
                r#"<table xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" id="1" name="Table1" displayName="가격표" ref="A1:B2"><tableColumns count="2"><tableColumn id="1" name="품목"/><tableColumn id="2" name="단가"/></tableColumns><tableStyleInfo name="TableStyleMedium2"/></table>"#,
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
        assert_eq!(parse_shared_strings(xml), vec!["東京"]);
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
}
