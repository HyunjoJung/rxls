//! `.ods` (OpenDocument Spreadsheet / ODF) reading.
//!
//! An `.ods` is a ZIP whose `content.xml` holds the cells under
//! `office:spreadsheet` → `table:table` → `table:table-row` → `table:table-cell`.
//! Unlike OOXML this is the OASIS ODF namespace, but the parse reuses the same
//! `quick_xml` setup. Cells carry an `office:value-type` (float / percentage /
//! currency / date / time / boolean / string); the value is in an `office:*-value`
//! attribute or the child `<text:p>`. `table:number-columns-repeated` /
//! `…-rows-repeated` expand runs (clamped), and `…-columns-spanned` /
//! `…-rows-spanned` give merged ranges. Panic-free / bounds-checked.

mod style;

use std::collections::{BTreeMap, HashMap};
use std::io::Read;

use quick_xml::events::{BytesRef, Event};
use quick_xml::{Reader, XmlVersion};

use crate::error::{Error, Result};
use crate::model::ImportedAxisMeasure;
use crate::{
    Cell, CellEntry, CellStyle, Color, Comment, DataValidation, DocProperties,
    DrawingAnchorBehavior, DrawingCrop, DrawingMetadata, DrawingObjectKind, DvKind, DvOp, Font,
    Image, ImageFmt, PageSetup, PrintMetadata, Sheet, StyleFidelity, StyleLoss, StyleLossKind,
    Table, Workbook,
};

use self::style::{
    add_ods_style_loss, apply_ods_column_style, apply_ods_row_style, flush_ods_run,
    ods_cell_base_font, ods_cell_number_format, ods_frame, ods_named_cell_number_format_state,
    ods_named_cell_style, ods_number_format_state, ods_table_default_cell_style, ods_text_font,
    read_ods_style_definitions, record_missing_ods_style, record_ods_cell_style_reference,
    record_ods_manual_breaks, table_page_setup, table_print_metadata, table_protected,
    table_style_options, OdsCellNumberFormat, OdsNumberFormatState, OdsResolvedStyles,
    OdsStyleDefinitions, MAX_ODS_LAYOUT_ENTRIES, MAX_ODS_STYLE_NAME,
};

const ODS_MIME: &str = "application/vnd.oasis.opendocument.spreadsheet";
/// Cap a `number-*-repeated` run so a hostile `repeated="1000000000"` cannot
/// drive an unbounded allocation; trailing empty spacers just advance the cursor.
/// Column repeat ceiling — a column index is a `u16`, so this is the grid bound.
const MAX_REPEAT: u32 = 1 << 16;
/// Row repeat ceiling — the spreadsheet row grid (`MAX_ROW + 1`), so a legitimate
/// large `number-rows-repeated` is replicated rather than truncated at 64k. The
/// real bound on output is the text budget below.
const MAX_ROW_REPEAT: u32 = 1 << 20;
/// Per-replicated-cell budget charge on top of its text length, so a flood of
/// *empty-text* valued cells (`<table-cell><text:p/></table-cell>` repeated many
/// times) still consumes the allocation budget and cannot blow memory/CPU at
/// near-zero text cost. This is intentionally conservative: ODS repeat counts
/// can describe billions of cells in a tiny ZIP part.
const CELL_COST: usize = 2048;
const MAX_TABLE_COLUMNS: usize = 16_384;
const MAX_IMAGE_PART: u64 = 64 << 20;
const MAX_IMAGE_BYTES: usize = crate::MAX_TEXT_BYTES;
const MAX_ODS_DRAWINGS: usize = 16_384;
const MAX_ODS_DRAWING_TEXT: usize = 4_096;

/// Detect `.ods` by the ODF spreadsheet mimetype (or an `office:spreadsheet`
/// `content.xml`).
pub(crate) fn is_ods(bytes: &[u8]) -> bool {
    let Ok(mut zip) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) else {
        return false;
    };
    if let Ok(f) = zip.by_name("mimetype") {
        let mut s = String::new();
        if f.take(256).read_to_string(&mut s).is_ok() && s.trim() == ODS_MIME {
            return true;
        }
    }
    if let Ok(f) = zip.by_name("content.xml") {
        let mut s = String::new();
        if f.take(4096).read_to_string(&mut s).is_ok() && s.contains("office:spreadsheet") {
            return true;
        }
    }
    false
}

fn has_encrypted_manifest(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>) -> bool {
    let Ok(f) = zip.by_name("META-INF/manifest.xml") else {
        return false;
    };
    let mut manifest = String::new();
    f.take(16 << 20).read_to_string(&mut manifest).is_ok() && manifest.contains("encryption-data")
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

fn normalize_package_path(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_start_matches('/');
    let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
    let mut parts = Vec::new();
    for segment in trimmed.split('/') {
        match segment {
            "" | "." => {}
            ".." => return None,
            other => parts.push(other),
        }
    }
    let normalized = parts.join("/");
    (!normalized.is_empty()).then_some(normalized)
}

fn read_image_parts(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>) -> ImageParts {
    read_image_parts_with_limits(zip, MAX_IMAGE_PART, MAX_IMAGE_BYTES)
}

fn read_image_parts_with_limits(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    max_part: u64,
    max_total: usize,
) -> ImageParts {
    let mut images = HashMap::new();
    let mut remaining = max_total;
    for idx in 0..zip.len() {
        let Ok(file) = zip.by_index(idx) else {
            continue;
        };
        let size = file.size();
        if size > max_part {
            continue;
        }
        let Ok(size) = usize::try_from(size) else {
            continue;
        };
        if size > remaining {
            continue;
        }
        let Some(path) = normalize_package_path(file.name()) else {
            continue;
        };
        let Some(format) = image_format(&path) else {
            continue;
        };
        let mut data = Vec::new();
        if file.take(max_part).read_to_end(&mut data).is_ok() && data.len() <= size {
            remaining -= size;
            images.insert(path, (format, data));
        }
    }
    images
}

pub(crate) fn open(bytes: &[u8]) -> Result<Workbook> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|_| Error::Zip("not a valid .ods ZIP container"))?;
    crate::ziputil::validate_compression(&mut zip)?;
    if has_encrypted_manifest(&mut zip) {
        return Err(Error::EncryptedOpenDocument);
    }
    let mut content = String::new();
    zip.by_name("content.xml")
        .map_err(|_| Error::MissingWorkbook)?
        .take(256 << 20)
        .read_to_string(&mut content)
        .map_err(|_| Error::MissingWorkbook)?;
    let mut styles_xml = String::new();
    if let Ok(f) = zip.by_name("styles.xml") {
        let _ = f.take(256 << 20).read_to_string(&mut styles_xml);
    }
    let mut meta_xml = String::new();
    if let Ok(f) = zip.by_name("meta.xml") {
        let _ = f.take(16 << 20).read_to_string(&mut meta_xml);
    }
    let mut settings_xml = String::new();
    if let Ok(f) = zip.by_name("settings.xml") {
        let _ = f.take(16 << 20).read_to_string(&mut settings_xml);
    }
    if [&content, &styles_xml, &meta_xml, &settings_xml]
        .into_iter()
        .any(|xml| !crate::xml_reference_work_within_budget(xml))
    {
        return Err(Error::Xml("xml has too many entity references"));
    }
    let image_parts = read_image_parts(&mut zip);
    let mut style_definitions = OdsStyleDefinitions::default();
    read_ods_style_definitions(&styles_xml, &mut style_definitions);
    read_ods_style_definitions(&content, &mut style_definitions);
    let styles = style_definitions.into_resolved();
    let mut workbook = parse_content(&content, &styles, &image_parts);
    workbook.properties = parse_meta_properties(&meta_xml);
    apply_ods_settings(&mut workbook, parse_settings(&settings_xml));
    Ok(workbook)
}

type Merges = Vec<(u32, u16, u32, u16)>;
type Hyperlinks = Vec<(u32, u16, String)>;
type Comments = Vec<Comment>;
type DataValidations = Vec<DataValidation>;
type Images = Vec<Image>;
type AutoFilters = HashMap<String, (u32, u16, u32, u16)>;
type ValidationRules = HashMap<String, DataValidation>;
type ImageParts = HashMap<String, (ImageFmt, Vec<u8>)>;

#[derive(Clone)]
struct DatabaseRange {
    name: String,
    sheet: String,
    range: (u32, u16, u32, u16),
    display_filter_buttons: bool,
}

/// Cell attributes read from a `<table-cell>`: value type, value attr, and the
/// repeat / span counts.
struct CellAttrs {
    vtype: String,
    val: Option<String>,
    formula: Option<String>,
    validation_name: Option<String>,
    style_name: Option<String>,
    style_name_invalid: bool,
    col_rep: u32,
    col_span: u16,
    row_span: u32,
}

#[derive(Clone)]
struct PendingComment {
    text: String,
    author: Option<String>,
}

#[derive(Clone)]
struct PendingImage {
    data: Vec<u8>,
    format: ImageFmt,
    to: Option<(u32, u16)>,
    metadata: DrawingMetadata,
}

#[derive(Clone)]
struct PendingFrame {
    image: Option<PendingImage>,
    to: Option<(u32, u16)>,
    metadata: DrawingMetadata,
    description: String,
    clip_points: Option<[f64; 4]>,
}

struct CellMetadata<'a> {
    hyperlink: Option<&'a str>,
    comment: Option<&'a PendingComment>,
    validation: Option<&'a DataValidation>,
    images: &'a [PendingImage],
    style: Option<&'a CellStyle>,
    number_format_state: Option<OdsNumberFormatState>,
    row_formats: &'a BTreeMap<u32, CellStyle>,
    row_number_format_states: &'a BTreeMap<u32, OdsNumberFormatState>,
    col_formats: &'a BTreeMap<u16, CellStyle>,
    col_number_format_states: &'a BTreeMap<u16, OdsNumberFormatState>,
    default_format: Option<&'a CellStyle>,
    default_number_format_state: Option<OdsNumberFormatState>,
}

fn read_cell_attrs(e: &quick_xml::events::BytesStart<'_>) -> CellAttrs {
    let raw_style_name = attr(e, b"style-name");
    let style_name = raw_style_name
        .as_ref()
        .filter(|name| name.len() <= MAX_ODS_STYLE_NAME)
        .cloned();
    CellAttrs {
        vtype: attr(e, b"value-type").unwrap_or_default(),
        val: attr(e, b"value")
            .or_else(|| attr(e, b"date-value"))
            .or_else(|| attr(e, b"boolean-value"))
            .or_else(|| attr(e, b"time-value")),
        formula: attr(e, b"formula").map(normalize_formula),
        validation_name: attr(e, b"content-validation-name").filter(|name| !name.trim().is_empty()),
        style_name,
        style_name_invalid: raw_style_name.is_some()
            && raw_style_name
                .as_ref()
                .is_some_and(|name| name.len() > MAX_ODS_STYLE_NAME),
        col_rep: attr(e, b"number-columns-repeated")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
            .min(MAX_REPEAT),
        col_span: attr(e, b"number-columns-spanned")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1),
        row_span: attr(e, b"number-rows-spanned")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1),
    }
}

fn text_of(e: &quick_xml::events::BytesText<'_>) -> String {
    e.decode().map(|c| c.into_owned()).unwrap_or_default()
}

fn append_general_ref(out: &mut String, reference: &BytesRef<'_>) {
    match reference.resolve_char_ref() {
        Ok(Some(ch)) if is_xml_10_char(ch) => out.push(ch),
        Ok(None) => {
            if let Ok(name) = reference.decode() {
                if let Some(value) = quick_xml::escape::resolve_xml_entity(&name) {
                    out.push_str(value);
                    return;
                }
            }
            append_raw_general_ref(out, reference);
        }
        Ok(Some(_)) | Err(_) => append_raw_general_ref(out, reference),
    }
}

fn append_raw_general_ref(out: &mut String, reference: &BytesRef<'_>) {
    if let Ok(raw) = std::str::from_utf8(reference.as_ref()) {
        out.push('&');
        out.push_str(raw);
        out.push(';');
    }
}

fn is_xml_10_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}'
    ) || ('\u{10000}'..='\u{10FFFF}').contains(&ch)
}

fn append_odf_text_empty(e: &quick_xml::events::BytesStart<'_>, out: &mut String) {
    match local(e.name().as_ref()) {
        b"s" => {
            let count = attr(e, b"c")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(1)
                .min(MAX_REPEAT as usize);
            out.extend(std::iter::repeat_n(' ', count));
        }
        b"tab" => out.push('\t'),
        b"line-break" => out.push('\n'),
        _ => {}
    }
}

fn assign_meta_property(
    props: &mut DocProperties,
    keywords: &mut Vec<String>,
    tag: &[u8],
    attr_name: Option<&str>,
    value: String,
) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    let value = value.to_string();
    match tag {
        b"title" => props.title = Some(value),
        b"subject" => props.subject = Some(value),
        b"initial-creator" => props.creator = Some(value),
        b"creator" => {
            if props.creator.is_none() {
                props.creator = Some(value.clone());
            }
            props.last_modified_by = Some(value);
        }
        b"keyword" => keywords.push(value),
        b"description" => props.description = Some(value),
        b"creation-date" => props.created = Some(value),
        b"date" if props.created.is_none() => props.created = Some(value),
        b"user-defined" if attr_name == Some("Company") => props.company = Some(value),
        _ => {}
    }
}

fn parse_meta_properties(xml: &str) -> DocProperties {
    let mut props = DocProperties::default();
    let mut keywords = Vec::new();
    let mut r = Reader::from_str(xml);
    let mut current: Option<(Vec<u8>, Option<String>)> = None;
    let mut text = String::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                current = Some((local(e.name().as_ref()).to_vec(), attr(&e, b"name")));
                text.clear();
            }
            Ok(Event::Text(t)) if current.is_some() => text.push_str(&text_of(&t)),
            Ok(Event::GeneralRef(reference)) if current.is_some() => {
                append_general_ref(&mut text, &reference);
            }
            Ok(Event::End(e)) => {
                if let Some((tag, attr_name)) = current.take() {
                    if tag.as_slice() == local(e.name().as_ref()) {
                        assign_meta_property(
                            &mut props,
                            &mut keywords,
                            &tag,
                            attr_name.as_deref(),
                            std::mem::take(&mut text),
                        );
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    if !keywords.is_empty() {
        props.keywords = Some(keywords.join(","));
    }
    props
}

#[derive(Debug, Default)]
struct OdsSettings {
    active_table: Option<String>,
    global_show_grid: Option<bool>,
    global_show_headers: Option<bool>,
    global_zoom: Option<u16>,
    sheet_views: HashMap<String, OdsSheetViewSettings>,
}

#[derive(Clone, Copy, Debug, Default)]
struct OdsSheetViewSettings {
    horizontal_split_mode: Option<u16>,
    vertical_split_mode: Option<u16>,
    horizontal_split_position: Option<u32>,
    vertical_split_position: Option<u32>,
    show_grid: Option<bool>,
    show_headers: Option<bool>,
    zoom: Option<u16>,
}

fn apply_ods_settings(workbook: &mut Workbook, settings: OdsSettings) {
    if let Some(active_table) = settings.active_table {
        if let Some(index) = workbook
            .sheets
            .iter()
            .position(|sheet| sheet.name == active_table)
        {
            workbook.active_sheet = index;
        }
    }

    for sheet in &mut workbook.sheets {
        if !sheet.is_worksheet {
            continue;
        }
        if let Some(view) = settings.sheet_views.get(&sheet.name) {
            let rows = if view.vertical_split_mode == Some(2) {
                view.vertical_split_position.unwrap_or(0)
            } else {
                0
            };
            let cols = if view.horizontal_split_mode == Some(2) {
                view.horizontal_split_position
                    .unwrap_or(0)
                    .min(u32::from(u16::MAX)) as u16
            } else {
                0
            };
            if rows > 0 || cols > 0 {
                sheet.freeze = Some((rows, cols));
            }
            if let Some(show_grid) = view.show_grid.or(settings.global_show_grid) {
                sheet.hide_gridlines = !show_grid;
            }
            sheet.show_headers = view.show_headers.or(settings.global_show_headers);
            sheet.zoom = view.zoom.or(settings.global_zoom).filter(|&zoom| zoom != 0);
        } else {
            if let Some(show_grid) = settings.global_show_grid {
                sheet.hide_gridlines = !show_grid;
            }
            sheet.show_headers = settings.global_show_headers;
            sheet.zoom = settings.global_zoom.filter(|&zoom| zoom != 0);
        }
    }
}

fn parse_settings(xml: &str) -> OdsSettings {
    let mut settings = OdsSettings::default();
    let mut r = Reader::from_str(xml);
    let mut in_tables_map = false;
    let mut current_table: Option<(String, OdsSheetViewSettings)> = None;
    let mut current_item: Option<String> = None;
    let mut text = String::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e))
                if local(e.name().as_ref()) == b"config-item-map-named"
                    && attr(&e, b"name").as_deref() == Some("Tables") =>
            {
                in_tables_map = true;
            }
            Ok(Event::Start(e))
                if in_tables_map
                    && current_table.is_none()
                    && local(e.name().as_ref()) == b"config-item-map-entry" =>
            {
                if let Some(name) = attr(&e, b"name").filter(|name| !name.trim().is_empty()) {
                    current_table = Some((name, OdsSheetViewSettings::default()));
                }
            }
            Ok(Event::Start(e)) if local(e.name().as_ref()) == b"config-item" => {
                current_item = attr(&e, b"name");
                text.clear();
            }
            Ok(Event::Text(t)) if current_item.is_some() => text.push_str(&text_of(&t)),
            Ok(Event::GeneralRef(reference)) if current_item.is_some() => {
                append_general_ref(&mut text, &reference);
            }
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"config-item" => {
                if let Some(name) = current_item.take() {
                    assign_settings_item(
                        &mut settings,
                        current_table.as_mut().map(|(_, view)| view),
                        &name,
                        std::mem::take(&mut text),
                    );
                }
            }
            Ok(Event::End(e))
                if local(e.name().as_ref()) == b"config-item-map-entry"
                    && current_table.is_some() =>
            {
                if let Some((name, view)) = current_table.take() {
                    settings.sheet_views.entry(name).or_insert(view);
                }
            }
            Ok(Event::End(e))
                if local(e.name().as_ref()) == b"config-item-map-named"
                    && in_tables_map
                    && current_table.is_none() =>
            {
                in_tables_map = false;
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    settings
}

fn assign_settings_item(
    settings: &mut OdsSettings,
    table_view: Option<&mut OdsSheetViewSettings>,
    name: &str,
    value: String,
) {
    if let Some(view) = table_view {
        match name {
            "HorizontalSplitMode" => view.horizontal_split_mode = parse_settings_u16(&value),
            "VerticalSplitMode" => view.vertical_split_mode = parse_settings_u16(&value),
            "HorizontalSplitPosition" => {
                view.horizontal_split_position = parse_settings_u32(&value)
            }
            "VerticalSplitPosition" => view.vertical_split_position = parse_settings_u32(&value),
            "ShowGrid" => view.show_grid = parse_settings_bool(&value),
            "HasColumnRowHeaders" => view.show_headers = parse_settings_bool(&value),
            "ZoomValue" => view.zoom = parse_settings_u16(&value),
            _ => {}
        }
        return;
    }

    match name {
        "ActiveTable" if settings.active_table.is_none() => {
            let active_table = value.trim();
            if !active_table.is_empty() {
                settings.active_table = Some(active_table.to_string());
            }
        }
        "ShowGrid" if settings.global_show_grid.is_none() => {
            settings.global_show_grid = parse_settings_bool(&value);
        }
        "HasColumnRowHeaders" if settings.global_show_headers.is_none() => {
            settings.global_show_headers = parse_settings_bool(&value);
        }
        "ZoomValue" if settings.global_zoom.is_none() => {
            settings.global_zoom = parse_settings_u16(&value);
        }
        _ => {}
    }
}

fn parse_settings_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn parse_settings_u16(value: &str) -> Option<u16> {
    value.trim().parse().ok()
}

fn parse_settings_u32(value: &str) -> Option<u32> {
    value.trim().parse().ok()
}

fn read_named_range(e: &quick_xml::events::BytesStart<'_>) -> Option<(String, String)> {
    let name = attr(e, b"name")?;
    if name.trim().is_empty() {
        return None;
    }
    let address = attr(e, b"cell-range-address")?;
    let refers_to = normalize_ods_cell_range_address(&address);
    if refers_to.is_empty() {
        return None;
    }
    Some((name, refers_to))
}

fn normalize_ods_cell_range_address(address: &str) -> String {
    let address = address.trim();
    address
        .strip_prefix("of:=")
        .unwrap_or(address)
        .trim_matches(|c| c == '[' || c == ']')
        .split(':')
        .map(normalize_ods_cell_reference)
        .collect::<Vec<_>>()
        .join(":")
}

fn normalize_ods_cell_reference(reference: &str) -> String {
    let reference = reference.trim();
    let reference = reference.trim_matches(|c| c == '[' || c == ']');
    if let Some(cell) = reference.strip_prefix('.') {
        return cell.to_string();
    }

    let reference = reference.strip_prefix('$').unwrap_or(reference);
    if let Some(rest) = reference.strip_prefix('\'') {
        if let Some(end) = rest.find("'.") {
            let sheet = &reference[..end + 2];
            let cell = &rest[end + 2..];
            if !cell.is_empty() {
                return format!("{sheet}!{cell}");
            }
        }
    }

    if let Some((sheet, cell)) = reference.split_once('.') {
        if !sheet.is_empty() && !cell.is_empty() {
            return format!("{sheet}!{cell}");
        }
    }
    reference.to_string()
}

fn read_database_range(e: &quick_xml::events::BytesStart<'_>) -> Option<DatabaseRange> {
    let name = attr(e, b"name").unwrap_or_default();
    let address = attr(e, b"target-range-address")?;
    let (sheet, range) = parse_ods_cell_range(&address)?;
    let display_filter_buttons = attr(e, b"display-filter-buttons")
        .as_deref()
        .map(attr_true)
        .unwrap_or(true);
    Some(DatabaseRange {
        name,
        sheet,
        range,
        display_filter_buttons,
    })
}

fn read_table_print_area(
    e: &quick_xml::events::BytesStart<'_>,
    default_sheet: &str,
) -> Option<(u32, u16, u32, u16)> {
    let ranges = attr(e, b"print-ranges")?;
    split_ods_reference_list(&ranges)
        .into_iter()
        .find_map(|range| {
            let (sheet, parsed) = parse_ods_cell_range_with_default(range, Some(default_sheet))?;
            (sheet == default_sheet).then_some(parsed)
        })
}

fn split_ods_reference_list(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut in_quote = false;
    let mut chars = value.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if start.is_none() {
            if ch.is_ascii_whitespace() {
                continue;
            }
            start = Some(idx);
        }
        if ch == '\'' {
            if in_quote && chars.peek().is_some_and(|(_, next)| *next == '\'') {
                chars.next();
            } else {
                in_quote = !in_quote;
            }
        } else if ch.is_ascii_whitespace() && !in_quote {
            if let Some(begin) = start.take() {
                out.push(&value[begin..idx]);
            }
        }
    }
    if let Some(begin) = start {
        out.push(&value[begin..]);
    }
    out
}

fn read_column_repeat(e: &quick_xml::events::BytesStart<'_>) -> u32 {
    attr(e, b"number-columns-repeated")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .min(MAX_REPEAT)
}

fn record_row_outline(row_outline: &mut BTreeMap<u32, u8>, first_row: u32, repeat: u32, level: u8) {
    if level == 0 {
        return;
    }
    for offset in 0..repeat.clamp(1, MAX_ROW_REPEAT) {
        let row = first_row.saturating_add(offset).min(MAX_ROW_REPEAT - 1);
        row_outline
            .entry(row)
            .and_modify(|existing| *existing = (*existing).max(level))
            .or_insert(level);
    }
}

fn record_col_outline(col_outline: &mut BTreeMap<u16, u8>, first_col: u32, repeat: u32, level: u8) {
    if level == 0 {
        return;
    }
    for offset in 0..repeat.clamp(1, MAX_REPEAT) {
        let col = first_col.saturating_add(offset);
        if let Ok(col) = u16::try_from(col) {
            col_outline
                .entry(col)
                .and_modify(|existing| *existing = (*existing).max(level))
                .or_insert(level);
        }
    }
}

fn read_content_validation(
    e: &quick_xml::events::BytesStart<'_>,
) -> Option<(String, DataValidation)> {
    let name = attr(e, b"name")?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let condition = attr(e, b"condition")?;
    let condition = normalize_ods_validation_condition(&condition);
    if condition.is_empty() {
        return None;
    }
    let allow_blank = attr(e, b"allow-empty-cell")
        .as_deref()
        .map(attr_true)
        .unwrap_or(true);
    Some((
        name.to_string(),
        DataValidation {
            sqref: (0, 0, 0, 0),
            kind: DvKind::Custom,
            operator: DvOp::Between,
            formula1: condition,
            formula2: None,
            allow_blank,
            show_input_message: false,
            show_error_message: false,
            prompt: None,
            error: None,
        },
    ))
}

fn parse_ods_cell_range(address: &str) -> Option<(String, (u32, u16, u32, u16))> {
    parse_ods_cell_range_with_default(address, None)
}

fn parse_ods_cell_range_with_default(
    address: &str,
    default_sheet: Option<&str>,
) -> Option<(String, (u32, u16, u32, u16))> {
    let address = address.trim();
    let address = address
        .strip_prefix("of:=")
        .unwrap_or(address)
        .trim_matches(|c| c == '[' || c == ']');
    let (first, last) = address.split_once(':').unwrap_or((address, address));
    let (sheet, r0, c0) = parse_ods_cell_ref(first, default_sheet)?;
    let (_, r1, c1) = parse_ods_cell_ref(last, Some(&sheet))?;
    Some((sheet, (r0.min(r1), c0.min(c1), r0.max(r1), c0.max(c1))))
}

fn parse_ods_cell_ref(reference: &str, default_sheet: Option<&str>) -> Option<(String, u32, u16)> {
    let reference = reference.trim().trim_matches(|c| c == '[' || c == ']');
    let reference = reference.strip_prefix('$').unwrap_or(reference);
    let (sheet, cell) = if let Some(cell) = reference.strip_prefix('.') {
        (default_sheet?.to_string(), cell)
    } else if let Some(rest) = reference.strip_prefix('\'') {
        let end = rest.find("'.")?;
        (rest[..end].replace("''", "'"), &rest[end + 2..])
    } else if let Some((sheet, cell)) = reference.split_once('.') {
        (sheet.trim_start_matches('$').to_string(), cell)
    } else {
        (default_sheet?.to_string(), reference)
    };
    let (row, col) = parse_a1_cell(cell)?;
    Some((sheet, row, col))
}

fn parse_a1_cell(cell: &str) -> Option<(u32, u16)> {
    let mut col: u32 = 0;
    let mut row = String::new();
    let mut saw_col = false;
    let mut saw_row = false;
    for ch in cell.chars().filter(|ch| *ch != '$') {
        if ch.is_ascii_alphabetic() && !saw_row {
            saw_col = true;
            col = col
                .checked_mul(26)?
                .checked_add(u32::from(ch.to_ascii_uppercase() as u8 - b'A' + 1))?;
        } else if ch.is_ascii_digit() {
            saw_row = true;
            row.push(ch);
        } else {
            return None;
        }
    }
    if !saw_col || !saw_row || col == 0 {
        return None;
    }
    let row: u32 = row.parse().ok()?;
    if row == 0 {
        return None;
    }
    let col = col.checked_sub(1)?;
    if col > u32::from(u16::MAX) {
        return None;
    }
    Some((row - 1, col as u16))
}

fn table_from_database_range(sheet: &Sheet, db: &DatabaseRange) -> Option<Table> {
    let (r0, c0, r1, c1) = db.range;
    if db.name.is_empty() || c0 > c1 || r0 > r1 {
        return None;
    }
    let width = usize::from(c1 - c0) + 1;
    if width > MAX_TABLE_COLUMNS {
        return None;
    }
    let mut columns = Vec::with_capacity(width);
    for (idx, col) in (c0..=c1).enumerate() {
        let header = sheet
            .cells
            .iter()
            .find(|cell| cell.row == r0 && cell.col == col)
            .map(|cell| cell.text.trim())
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Column{}", idx + 1));
        columns.push(header);
    }
    Some(Table {
        range: db.range,
        name: db.name.clone(),
        columns,
        style: None,
    })
}

fn read_draw_image(
    e: &quick_xml::events::BytesStart<'_>,
    image_parts: &ImageParts,
) -> Option<PendingImage> {
    let href = attr(e, b"href")?;
    let path = normalize_package_path(&href)?;
    let (format, data) = image_parts.get(&path)?;
    Some(PendingImage {
        data: data.clone(),
        format: *format,
        to: None,
        metadata: DrawingMetadata {
            kind: DrawingObjectKind::Image,
            ..Default::default()
        },
    })
}

fn ods_points_to_emu(points: f64) -> Option<u64> {
    let value = points * 12_700.0;
    (value.is_finite() && value >= 0.0 && value <= u64::MAX as f64).then_some(value.round() as u64)
}

fn ods_signed_points_to_emu(points: f64) -> Option<i64> {
    let value = points * 12_700.0;
    (value.is_finite() && value >= i64::MIN as f64 && value <= i64::MAX as f64)
        .then_some(value.round() as i64)
}

fn png_physical_size_points(data: &[u8]) -> Option<(f64, f64)> {
    if data.get(..8)? != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let mut cursor = 8usize;
    let mut dimensions = None;
    let mut pixels_per_meter = None;
    while cursor.checked_add(12)? <= data.len() {
        let length = u32::from_be_bytes(data.get(cursor..cursor + 4)?.try_into().ok()?) as usize;
        let kind = data.get(cursor + 4..cursor + 8)?;
        let payload_start = cursor.checked_add(8)?;
        let payload_end = payload_start.checked_add(length)?;
        let chunk_end = payload_end.checked_add(4)?;
        if chunk_end > data.len() {
            return None;
        }
        let payload = &data[payload_start..payload_end];
        match kind {
            b"IHDR" if payload.len() >= 8 => {
                let width = u32::from_be_bytes(payload[0..4].try_into().ok()?);
                let height = u32::from_be_bytes(payload[4..8].try_into().ok()?);
                if width == 0 || height == 0 {
                    return None;
                }
                dimensions = Some((width, height));
            }
            b"pHYs" if payload.len() == 9 && payload[8] == 1 => {
                let x = u32::from_be_bytes(payload[0..4].try_into().ok()?);
                let y = u32::from_be_bytes(payload[4..8].try_into().ok()?);
                if x > 0 && y > 0 {
                    pixels_per_meter = Some((x, y));
                }
            }
            b"IEND" => break,
            _ => {}
        }
        cursor = chunk_end;
    }
    let ((width, height), (x_density, y_density)) = (dimensions?, pixels_per_meter?);
    let points_per_meter = 72.0 / 0.0254;
    Some((
        f64::from(width) * points_per_meter / f64::from(x_density),
        f64::from(height) * points_per_meter / f64::from(y_density),
    ))
}

fn jpeg_physical_size_points(data: &[u8]) -> Option<(f64, f64)> {
    if data.get(..2)? != b"\xff\xd8" {
        return None;
    }
    let mut cursor = 2usize;
    let mut dimensions = None;
    let mut density = None;
    while cursor < data.len() {
        while cursor < data.len() && data[cursor] != 0xff {
            cursor += 1;
        }
        while cursor < data.len() && data[cursor] == 0xff {
            cursor += 1;
        }
        let marker = *data.get(cursor)?;
        cursor += 1;
        if marker == 0xd9 || marker == 0xda {
            break;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let length = u16::from_be_bytes(data.get(cursor..cursor + 2)?.try_into().ok()?) as usize;
        if length < 2 {
            return None;
        }
        let payload_start = cursor.checked_add(2)?;
        let payload_end = cursor.checked_add(length)?;
        let payload = data.get(payload_start..payload_end)?;
        if marker == 0xe0 && payload.len() >= 12 && payload.get(..5) == Some(b"JFIF\0") {
            let units = payload[7];
            let x = u16::from_be_bytes(payload[8..10].try_into().ok()?);
            let y = u16::from_be_bytes(payload[10..12].try_into().ok()?);
            if x > 0 && y > 0 && matches!(units, 1 | 2) {
                density = Some((units, x, y));
            }
        }
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) && payload.len() >= 5
        {
            let height = u16::from_be_bytes(payload[1..3].try_into().ok()?);
            let width = u16::from_be_bytes(payload[3..5].try_into().ok()?);
            if width > 0 && height > 0 {
                dimensions = Some((width, height));
            }
        }
        cursor = payload_end;
    }
    let ((width, height), (units, x_density, y_density)) = (dimensions?, density?);
    let density_scale = if units == 1 { 1.0 } else { 2.54 };
    Some((
        f64::from(width) * 72.0 / (f64::from(x_density) * density_scale),
        f64::from(height) * 72.0 / (f64::from(y_density) * density_scale),
    ))
}

fn ods_image_physical_size_points(data: &[u8], format: ImageFmt) -> Option<(f64, f64)> {
    match format {
        ImageFmt::Png => png_physical_size_points(data),
        ImageFmt::Jpeg => jpeg_physical_size_points(data),
    }
}

fn ods_crop_ppm(distance: f64, extent: f64) -> std::result::Result<u32, ()> {
    if !distance.is_finite()
        || !extent.is_finite()
        || distance < 0.0
        || extent <= 0.0
        || distance > extent * (1.0 + 1e-9)
    {
        return Err(());
    }
    Ok(((distance / extent) * 1_000_000.0)
        .round()
        .clamp(0.0, 1_000_000.0) as u32)
}

fn normalize_ods_image_crop(
    clip_points: [f64; 4],
    data: &[u8],
    format: ImageFmt,
) -> std::result::Result<Option<DrawingCrop>, ()> {
    if clip_points.iter().all(|value| value.abs() <= f64::EPSILON) {
        return Ok(None);
    }
    let (width_points, height_points) = ods_image_physical_size_points(data, format).ok_or(())?;
    let top_ppm = ods_crop_ppm(clip_points[0], height_points)?;
    let right_ppm = ods_crop_ppm(clip_points[1], width_points)?;
    let bottom_ppm = ods_crop_ppm(clip_points[2], height_points)?;
    let left_ppm = ods_crop_ppm(clip_points[3], width_points)?;
    if u64::from(left_ppm) + u64::from(right_ppm) >= 1_000_000
        || u64::from(top_ppm) + u64::from(bottom_ppm) >= 1_000_000
    {
        return Err(());
    }
    Ok(Some(DrawingCrop {
        left_ppm,
        top_ppm,
        right_ppm,
        bottom_ppm,
    }))
}

fn parse_content(xml: &str, styles: &OdsResolvedStyles, image_parts: &ImageParts) -> Workbook {
    let mut r = Reader::from_str(xml);
    let mut sheets: Vec<Sheet> = Vec::new();
    let mut defined_names: Vec<(String, String)> = Vec::new();
    let mut validation_rules: ValidationRules = HashMap::new();
    let mut autofilters: AutoFilters = HashMap::new();
    let mut database_ranges: Vec<DatabaseRange> = Vec::new();
    let mut budget = crate::MAX_TEXT_BYTES;

    // Per-sheet state.
    let mut cells: Vec<CellEntry> = Vec::new();
    let mut merges: Merges = Vec::new();
    let mut read_hyperlinks: Hyperlinks = Vec::new();
    let mut read_comments: Comments = Vec::new();
    let mut read_data_validations: DataValidations = Vec::new();
    let mut read_images: Images = Vec::new();
    let mut drawing_metadata: Vec<DrawingMetadata> = Vec::new();
    let mut default_format: Option<CellStyle> = None;
    let mut default_number_format_state: Option<OdsNumberFormatState> = None;
    let mut row_formats: BTreeMap<u32, CellStyle> = BTreeMap::new();
    let mut row_number_format_states: BTreeMap<u32, OdsNumberFormatState> = BTreeMap::new();
    let mut col_formats: BTreeMap<u16, CellStyle> = BTreeMap::new();
    let mut col_number_format_states: BTreeMap<u16, OdsNumberFormatState> = BTreeMap::new();
    let mut blank_styles: BTreeMap<(u32, u16), CellStyle> = BTreeMap::new();
    let mut row_heights: BTreeMap<u32, f32> = BTreeMap::new();
    let mut automatic_row_height_candidates = std::collections::BTreeSet::new();
    let mut col_widths: BTreeMap<u16, f32> = BTreeMap::new();
    let mut physical_col_widths: BTreeMap<u16, f32> = BTreeMap::new();
    let mut imported_row_axis_measures: BTreeMap<u32, ImportedAxisMeasure> = BTreeMap::new();
    let mut imported_column_axis_measures: BTreeMap<u16, ImportedAxisMeasure> = BTreeMap::new();
    let mut hidden_rows = std::collections::BTreeSet::new();
    let mut hidden_cols = std::collections::BTreeSet::new();
    let mut style_losses = styles.losses.clone();
    let mut row_outline: BTreeMap<u32, u8> = BTreeMap::new();
    let mut col_outline: BTreeMap<u16, u8> = BTreeMap::new();
    let mut rich: BTreeMap<(u32, u16), Vec<crate::TextRun>> = BTreeMap::new();
    let mut page_setup: Option<PageSetup> = None;
    let mut print_metadata = PrintMetadata::default();
    let mut name = String::new();
    let mut tab_color: Option<Color> = None;
    let mut right_to_left = false;
    let mut hidden = false;
    let mut protected = false;
    let mut print_gridlines = false;
    let mut print_headings = false;
    let mut row: u32 = 0;
    let mut col: u16 = 0;
    let mut table_column: u32 = 0;
    let mut row_rep: u32 = 1;
    let mut row_start = 0usize; // index in `cells` where the current row began
    let mut row_hyperlink_start = 0usize;
    let mut row_comment_start = 0usize;
    let mut row_validation_start = 0usize;
    let mut row_image_start = 0usize;
    let mut row_drawing_start = 0usize;
    let mut in_table = false;
    let mut row_group_depth: u8 = 0;
    let mut col_group_depth: u8 = 0;
    let mut in_table_header_rows = false;
    let mut table_header_row_start: Option<u32> = None;
    let mut in_table_header_columns = false;
    let mut table_header_column_count: u32 = 0;

    // Open-cell state (for a `<table-cell>` with a text body).
    let mut cur: Option<CellAttrs> = None;
    let mut text = String::new();
    let mut cell_hyperlink: Option<String> = None;
    let mut cell_comment_text = String::new();
    let mut cell_comment_author: Option<String> = None;
    let mut cell_comment_author_text = String::new();
    let mut cell_images: Vec<PendingImage> = Vec::new();
    let mut in_p = false;
    let mut in_annotation = false;
    let mut in_annotation_p = false;
    let mut in_annotation_creator = false;
    let mut cell_runs: Vec<crate::TextRun> = Vec::new();
    let mut cell_run_start = 0usize;
    let mut cell_saw_span = false;
    let mut span_depth = 0u8;
    let mut cell_base_font = Font::default();
    let mut cell_run_font = Font::default();
    let mut span_font_stack: Vec<Font> = Vec::new();
    let mut current_frame: Option<PendingFrame> = None;
    let mut frame_in_cell = false;
    let mut in_frame_description = false;

    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"named-range" => {
                    if let Some(name) = read_named_range(&e) {
                        defined_names.push(name);
                    }
                }
                b"database-range" => {
                    if let Some(db) = read_database_range(&e) {
                        if db.display_filter_buttons {
                            autofilters.insert(db.sheet.clone(), db.range);
                        }
                        database_ranges.push(db);
                    }
                }
                b"content-validation" => {
                    if let Some((name, validation)) = read_content_validation(&e) {
                        validation_rules.insert(name, validation);
                    }
                }
                b"table" => {
                    name = attr(&e, b"name").unwrap_or_default();
                    style_losses = styles.losses.clone();
                    let table_style_name = attr(&e, b"style-name");
                    let default_cell_name = attr(&e, b"default-cell-style-name");
                    record_missing_ods_style(
                        styles,
                        "table",
                        table_style_name.as_deref(),
                        &mut style_losses,
                    );
                    record_missing_ods_style(
                        styles,
                        "table-cell",
                        default_cell_name.as_deref(),
                        &mut style_losses,
                    );
                    let style = table_style_options(&e, styles);
                    tab_color = style.tab_color;
                    right_to_left = style.right_to_left.unwrap_or(false);
                    hidden = style.hidden();
                    protected = table_protected(&e);
                    print_gridlines = style.print_gridlines;
                    print_headings = style.print_headings;
                    page_setup = table_page_setup(&e, &name, style);
                    print_metadata = table_print_metadata(&e, &name, styles);
                    default_format =
                        ods_table_default_cell_style(styles, default_cell_name.as_deref());
                    default_number_format_state = match default_cell_name.as_deref() {
                        Some(name) => Some(
                            styles
                                .cell
                                .get(name)
                                .map(ods_number_format_state)
                                .unwrap_or(OdsNumberFormatState::Unresolved),
                        ),
                        None => styles.default_cell.as_ref().map(ods_number_format_state),
                    };
                    cells = Vec::new();
                    merges = Vec::new();
                    read_hyperlinks = Vec::new();
                    read_comments = Vec::new();
                    read_data_validations = Vec::new();
                    read_images = Vec::new();
                    drawing_metadata = Vec::new();
                    row_formats = BTreeMap::new();
                    row_number_format_states = BTreeMap::new();
                    col_formats = BTreeMap::new();
                    col_number_format_states = BTreeMap::new();
                    blank_styles = BTreeMap::new();
                    row_heights = BTreeMap::new();
                    col_widths = BTreeMap::new();
                    physical_col_widths = BTreeMap::new();
                    imported_row_axis_measures = BTreeMap::new();
                    imported_column_axis_measures = BTreeMap::new();
                    hidden_rows = std::collections::BTreeSet::new();
                    hidden_cols = std::collections::BTreeSet::new();
                    row_outline = BTreeMap::new();
                    col_outline = BTreeMap::new();
                    rich = BTreeMap::new();
                    row = 0;
                    table_column = 0;
                    in_table = true;
                    row_group_depth = 0;
                    col_group_depth = 0;
                    in_table_header_rows = false;
                    table_header_row_start = None;
                    in_table_header_columns = false;
                    table_header_column_count = 0;
                }
                b"table-column-group" if in_table => {
                    col_group_depth = col_group_depth.saturating_add(1);
                }
                b"table-row-group" if in_table => {
                    row_group_depth = row_group_depth.saturating_add(1);
                }
                b"table-header-rows" if in_table => {
                    in_table_header_rows = true;
                    table_header_row_start = Some(row);
                }
                b"table-header-columns" if in_table => {
                    in_table_header_columns = true;
                    table_header_column_count = 0;
                }
                b"table-column" if in_table => {
                    let repeat = read_column_repeat(&e);
                    record_ods_manual_breaks(
                        &e,
                        styles,
                        table_column,
                        repeat,
                        false,
                        &mut print_metadata,
                    );
                    record_col_outline(&mut col_outline, table_column, repeat, col_group_depth);
                    apply_ods_column_style(
                        &e,
                        styles,
                        table_column,
                        repeat,
                        &mut col_formats,
                        &mut col_number_format_states,
                        &mut col_widths,
                        &mut physical_col_widths,
                        &mut imported_column_axis_measures,
                        &mut hidden_cols,
                        &mut style_losses,
                    );
                    table_column = table_column.saturating_add(repeat).min(MAX_REPEAT);
                    if in_table_header_columns {
                        table_header_column_count = table_header_column_count
                            .saturating_add(repeat)
                            .min(MAX_REPEAT);
                    }
                }
                b"table-row" => {
                    col = 0;
                    row_start = cells.len();
                    row_hyperlink_start = read_hyperlinks.len();
                    row_comment_start = read_comments.len();
                    row_validation_start = read_data_validations.len();
                    row_image_start = read_images.len();
                    row_drawing_start = drawing_metadata.len();
                    row_rep = attr(&e, b"number-rows-repeated")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1)
                        .min(MAX_ROW_REPEAT);
                    record_ods_manual_breaks(&e, styles, row, row_rep, true, &mut print_metadata);
                    record_row_outline(&mut row_outline, row, row_rep, row_group_depth);
                    apply_ods_row_style(
                        &e,
                        styles,
                        row,
                        row_rep,
                        &mut row_formats,
                        &mut row_number_format_states,
                        &mut row_heights,
                        &mut automatic_row_height_candidates,
                        &mut imported_row_axis_measures,
                        &mut hidden_rows,
                        &mut style_losses,
                    );
                }
                b"table-cell" | b"covered-table-cell" if in_table => {
                    let cell = read_cell_attrs(&e);
                    let cell_style_reference =
                        (cell.style_name.as_deref(), cell.style_name_invalid);
                    record_ods_cell_style_reference(
                        styles,
                        cell_style_reference,
                        &mut style_losses,
                    );
                    cell_base_font = ods_cell_base_font(
                        styles,
                        default_format.as_ref(),
                        &row_formats,
                        &col_formats,
                        cell_style_reference,
                        row,
                        col,
                    );
                    cur = Some(cell);
                    cell_run_font = cell_base_font.clone();
                    span_font_stack.clear();
                    current_frame = None;
                    frame_in_cell = false;
                    in_frame_description = false;
                    text.clear();
                    cell_hyperlink = None;
                    cell_comment_text.clear();
                    cell_comment_author = None;
                    cell_comment_author_text.clear();
                    cell_images.clear();
                    cell_runs.clear();
                    cell_run_start = 0;
                    cell_saw_span = false;
                    span_depth = 0;
                    in_annotation = false;
                    in_annotation_p = false;
                    in_annotation_creator = false;
                }
                b"frame" if in_table => {
                    if drawing_metadata.len().saturating_add(cell_images.len()) < MAX_ODS_DRAWINGS {
                        frame_in_cell = cur.is_some();
                        let mut frame = ods_frame(
                            &e,
                            &name,
                            drawing_metadata.len() + cell_images.len(),
                            styles,
                            &mut style_losses,
                        );
                        if cur.is_some() {
                            frame.metadata.from_cell = Some((row, col));
                        }
                        current_frame = Some(frame);
                    } else {
                        add_ods_style_loss(&mut style_losses, StyleLossKind::LimitExceeded, 1);
                    }
                }
                b"image" if cur.is_some() || current_frame.is_some() => {
                    if let Some(mut image) = read_draw_image(&e, image_parts) {
                        if let Some(frame) = current_frame.as_mut() {
                            image.to = frame.to;
                            image.metadata = frame.metadata.clone();
                            frame.image = Some(image);
                        } else if cell_images.len() < MAX_ODS_DRAWINGS {
                            cell_images.push(image);
                        } else {
                            add_ods_style_loss(&mut style_losses, StyleLossKind::LimitExceeded, 1);
                        }
                    } else {
                        add_ods_style_loss(
                            &mut style_losses,
                            StyleLossKind::DrawingMetadataPartial,
                            1,
                        );
                    }
                }
                b"desc" | b"title" if current_frame.is_some() => {
                    in_frame_description = true;
                    if let Some(frame) = current_frame.as_mut() {
                        frame.description.clear();
                    }
                }
                b"annotation" if cur.is_some() => in_annotation = true,
                b"creator" if in_annotation => {
                    cell_comment_author_text.clear();
                    in_annotation_creator = true;
                }
                b"p" if cur.is_some() && in_annotation => {
                    if !cell_comment_text.is_empty() {
                        cell_comment_text.push('\n');
                    }
                    in_annotation_p = true;
                }
                b"p" if cur.is_some() => {
                    in_p = true;
                    let paragraph_name = attr(&e, b"style-name");
                    record_missing_ods_style(
                        styles,
                        "paragraph",
                        paragraph_name.as_deref(),
                        &mut style_losses,
                    );
                    let paragraph_style = paragraph_name
                        .as_deref()
                        .and_then(|name| styles.paragraph.get(name))
                        .or(styles.default_paragraph.as_ref());
                    let text_default =
                        ods_text_font(styles.default_text.as_ref(), cell_base_font.clone());
                    cell_run_font = ods_text_font(paragraph_style, text_default);
                }
                b"span" if cur.is_some() && in_p && !in_annotation => {
                    flush_ods_run(&text, &mut cell_run_start, &mut cell_runs, &cell_run_font);
                    span_font_stack.push(cell_run_font.clone());
                    let span_name = attr(&e, b"style-name");
                    record_missing_ods_style(
                        styles,
                        "text",
                        span_name.as_deref(),
                        &mut style_losses,
                    );
                    let span_style = span_name
                        .as_deref()
                        .and_then(|name| styles.text.get(name))
                        .or(styles.default_text.as_ref());
                    cell_run_font = ods_text_font(span_style, cell_run_font.clone());
                    span_depth = span_depth.saturating_add(1);
                    cell_saw_span = true;
                }
                b"a" if cur.is_some() && in_p && !in_annotation && cell_hyperlink.is_none() => {
                    cell_hyperlink = attr(&e, b"href");
                }
                b"s" | b"tab" | b"line-break" if in_annotation_p => {
                    append_odf_text_empty(&e, &mut cell_comment_text);
                }
                b"s" | b"tab" | b"line-break" if in_p => {
                    append_odf_text_empty(&e, &mut text);
                }
                _ => {}
            },
            // Self-closing elements (no End): an empty/spacer cell or empty row.
            Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"s" | b"tab" | b"line-break" if in_annotation_p => {
                    append_odf_text_empty(&e, &mut cell_comment_text);
                }
                b"s" | b"tab" | b"line-break" if in_p => {
                    append_odf_text_empty(&e, &mut text);
                }
                b"named-range" => {
                    if let Some(name) = read_named_range(&e) {
                        defined_names.push(name);
                    }
                }
                b"database-range" => {
                    if let Some(db) = read_database_range(&e) {
                        if db.display_filter_buttons {
                            autofilters.insert(db.sheet.clone(), db.range);
                        }
                        database_ranges.push(db);
                    }
                }
                b"content-validation" => {
                    if let Some((name, validation)) = read_content_validation(&e) {
                        validation_rules.insert(name, validation);
                    }
                }
                b"table" => {
                    let name = attr(&e, b"name").unwrap_or_default();
                    let mut style_losses = styles.losses.clone();
                    let table_style_name = attr(&e, b"style-name");
                    let default_cell_name = attr(&e, b"default-cell-style-name");
                    record_missing_ods_style(
                        styles,
                        "table",
                        table_style_name.as_deref(),
                        &mut style_losses,
                    );
                    record_missing_ods_style(
                        styles,
                        "table-cell",
                        default_cell_name.as_deref(),
                        &mut style_losses,
                    );
                    let style = table_style_options(&e, styles);
                    let style_fidelity = if !styles.has_source_styles {
                        StyleFidelity::Unavailable
                    } else if style_losses.is_empty() {
                        StyleFidelity::Retained
                    } else {
                        StyleFidelity::Partial
                    };
                    sheets.push(Sheet {
                        page_setup: table_page_setup(&e, &name, style),
                        print_metadata: table_print_metadata(&e, &name, styles),
                        name,
                        is_worksheet: true,
                        // Calc initializes every ODS worksheet column to its
                        // 64-point application width before applying explicit
                        // table-column styles.
                        imported_default_column_axis_measure: Some(ImportedAxisMeasure::Twips(
                            1_280,
                        )),
                        // Calc has no persisted document-wide row height for
                        // ODS the way BIFF's mandatory DEFAULTROWHEIGHT record
                        // or an authored OOXML sheetFormatPr does, so a row
                        // left undeclared by both its own style and any
                        // table-row default-style resolves to Calc's generic
                        // no-information application default of 0.5 cm
                        // (14.173228 pt), the same oracle-pinned value OOXML
                        // falls back to when its own implicit-height
                        // computation is unavailable.
                        imported_default_row_axis_measure: Some(
                            ImportedAxisMeasure::MillimeterHundredths(500),
                        ),
                        style_fidelity,
                        default_format: ods_table_default_cell_style(
                            styles,
                            default_cell_name.as_deref(),
                        ),
                        style_losses,
                        tab_color: style.tab_color,
                        right_to_left: style.right_to_left.unwrap_or(false),
                        hidden: style.hidden(),
                        protect: table_protected(&e),
                        print_gridlines: style.print_gridlines,
                        print_headings: style.print_headings,
                        ..Default::default()
                    });
                }
                b"table-cell" | b"covered-table-cell" if in_table => {
                    let a = read_cell_attrs(&e);
                    record_ods_cell_style_reference(
                        styles,
                        (a.style_name.as_deref(), a.style_name_invalid),
                        &mut style_losses,
                    );
                    let resolved_style = if a.style_name_invalid {
                        Some(CellStyle::default())
                    } else {
                        ods_named_cell_style(styles, a.style_name.as_deref())
                    };
                    let validation = a
                        .validation_name
                        .as_deref()
                        .and_then(|name| validation_rules.get(name));
                    let mut sink = CellSink {
                        cells: &mut cells,
                        merges: &mut merges,
                        read_hyperlinks: &mut read_hyperlinks,
                        read_comments: &mut read_comments,
                        read_data_validations: &mut read_data_validations,
                        read_images: &mut read_images,
                        drawing_metadata: &mut drawing_metadata,
                        blank_styles: &mut blank_styles,
                        style_losses: &mut style_losses,
                        budget: &mut budget,
                    };
                    finish_cell(
                        &mut sink,
                        row,
                        &mut col,
                        &a,
                        "",
                        CellMetadata {
                            hyperlink: None,
                            comment: None,
                            validation,
                            images: &[],
                            style: resolved_style.as_ref(),
                            number_format_state: if a.style_name_invalid {
                                Some(OdsNumberFormatState::Unresolved)
                            } else {
                                ods_named_cell_number_format_state(styles, a.style_name.as_deref())
                            },
                            row_formats: &row_formats,
                            row_number_format_states: &row_number_format_states,
                            col_formats: &col_formats,
                            col_number_format_states: &col_number_format_states,
                            default_format: default_format.as_ref(),
                            default_number_format_state,
                        },
                    );
                }
                b"image" if cur.is_some() || current_frame.is_some() => {
                    if let Some(mut image) = read_draw_image(&e, image_parts) {
                        if let Some(frame) = current_frame.as_mut() {
                            image.to = frame.to;
                            image.metadata = frame.metadata.clone();
                            frame.image = Some(image);
                        } else if cell_images.len() < MAX_ODS_DRAWINGS {
                            cell_images.push(image);
                        } else {
                            add_ods_style_loss(&mut style_losses, StyleLossKind::LimitExceeded, 1);
                        }
                    } else {
                        add_ods_style_loss(
                            &mut style_losses,
                            StyleLossKind::DrawingMetadataPartial,
                            1,
                        );
                    }
                }
                b"frame" if in_table => {
                    if drawing_metadata.len() < MAX_ODS_DRAWINGS {
                        let mut frame = ods_frame(
                            &e,
                            &name,
                            drawing_metadata.len() + cell_images.len(),
                            styles,
                            &mut style_losses,
                        );
                        if cur.is_some() {
                            frame.metadata.from_cell = Some((row, col));
                        }
                        frame.metadata.kind = DrawingObjectKind::Shape;
                        frame.metadata.object_index = 0;
                        drawing_metadata.push(frame.metadata);
                        add_ods_style_loss(
                            &mut style_losses,
                            StyleLossKind::UnsupportedProperty,
                            1,
                        );
                    } else {
                        add_ods_style_loss(&mut style_losses, StyleLossKind::LimitExceeded, 1);
                    }
                }
                b"table-column" if in_table => {
                    let repeat = read_column_repeat(&e);
                    record_ods_manual_breaks(
                        &e,
                        styles,
                        table_column,
                        repeat,
                        false,
                        &mut print_metadata,
                    );
                    record_col_outline(&mut col_outline, table_column, repeat, col_group_depth);
                    apply_ods_column_style(
                        &e,
                        styles,
                        table_column,
                        repeat,
                        &mut col_formats,
                        &mut col_number_format_states,
                        &mut col_widths,
                        &mut physical_col_widths,
                        &mut imported_column_axis_measures,
                        &mut hidden_cols,
                        &mut style_losses,
                    );
                    table_column = table_column.saturating_add(repeat).min(MAX_REPEAT);
                    if in_table_header_columns {
                        table_header_column_count = table_header_column_count
                            .saturating_add(repeat)
                            .min(MAX_REPEAT);
                    }
                }
                b"table-row" => {
                    let rep = attr(&e, b"number-rows-repeated")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1)
                        .min(MAX_ROW_REPEAT);
                    record_ods_manual_breaks(&e, styles, row, rep, true, &mut print_metadata);
                    record_row_outline(&mut row_outline, row, rep, row_group_depth);
                    apply_ods_row_style(
                        &e,
                        styles,
                        row,
                        rep,
                        &mut row_formats,
                        &mut row_number_format_states,
                        &mut row_heights,
                        &mut automatic_row_height_candidates,
                        &mut imported_row_axis_measures,
                        &mut hidden_rows,
                        &mut style_losses,
                    );
                    row = row.saturating_add(rep);
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_annotation_creator => {
                cell_comment_author_text.push_str(&text_of(&t));
            }
            Ok(Event::Text(t)) if in_annotation_p => {
                cell_comment_text.push_str(&text_of(&t));
            }
            Ok(Event::Text(t)) if in_frame_description => {
                if let Some(frame) = current_frame.as_mut() {
                    if frame.description.len() < MAX_ODS_DRAWING_TEXT {
                        frame.description.push_str(&text_of(&t));
                        while frame.description.len() > MAX_ODS_DRAWING_TEXT {
                            frame.description.pop();
                        }
                    }
                }
            }
            Ok(Event::Text(t)) if in_p => {
                text.push_str(&text_of(&t));
            }
            Ok(Event::GeneralRef(reference)) if in_annotation_creator => {
                append_general_ref(&mut cell_comment_author_text, &reference);
            }
            Ok(Event::GeneralRef(reference)) if in_annotation_p => {
                append_general_ref(&mut cell_comment_text, &reference);
            }
            Ok(Event::GeneralRef(reference)) if in_frame_description => {
                if let Some(frame) = current_frame.as_mut() {
                    append_general_ref(&mut frame.description, &reference);
                    while frame.description.len() > MAX_ODS_DRAWING_TEXT {
                        frame.description.pop();
                    }
                }
            }
            Ok(Event::GeneralRef(reference)) if in_p => {
                append_general_ref(&mut text, &reference);
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"creator" if in_annotation_creator => {
                    let author = cell_comment_author_text.trim();
                    if !author.is_empty() {
                        cell_comment_author = Some(author.to_string());
                    }
                    cell_comment_author_text.clear();
                    in_annotation_creator = false;
                }
                b"p" if in_annotation_p => in_annotation_p = false,
                b"p" => {
                    flush_ods_run(&text, &mut cell_run_start, &mut cell_runs, &cell_run_font);
                    in_p = false;
                    cell_run_font = cell_base_font.clone();
                }
                b"span" if span_depth > 0 => {
                    flush_ods_run(&text, &mut cell_run_start, &mut cell_runs, &cell_run_font);
                    cell_run_font = span_font_stack.pop().unwrap_or_default();
                    span_depth = span_depth.saturating_sub(1);
                }
                b"annotation" if in_annotation => {
                    in_annotation = false;
                    in_annotation_p = false;
                    in_annotation_creator = false;
                }
                b"desc" | b"title" if in_frame_description => {
                    if let Some(frame) = current_frame.as_mut() {
                        if !frame.description.trim().is_empty() {
                            frame.metadata.alt_text = Some(frame.description.trim().to_string());
                        }
                    }
                    in_frame_description = false;
                }
                b"frame" if current_frame.is_some() => {
                    if let Some(mut frame) = current_frame.take() {
                        if let Some(mut image) = frame.image.take() {
                            if let Some(clip_points) = frame.clip_points {
                                match normalize_ods_image_crop(
                                    clip_points,
                                    &image.data,
                                    image.format,
                                ) {
                                    Ok(crop) => frame.metadata.crop = crop,
                                    Err(()) => add_ods_style_loss(
                                        &mut style_losses,
                                        StyleLossKind::DrawingMetadataPartial,
                                        1,
                                    ),
                                }
                            }
                            image.to = frame.to;
                            image.metadata = frame.metadata;
                            if frame_in_cell {
                                cell_images.push(image);
                            } else if read_images.len() >= MAX_ODS_DRAWINGS {
                                add_ods_style_loss(
                                    &mut style_losses,
                                    StyleLossKind::LimitExceeded,
                                    1,
                                );
                            } else {
                                let cost = image.data.len().saturating_add(CELL_COST);
                                if cost > budget {
                                    budget = 0;
                                } else {
                                    budget -= cost;
                                    let object_index = read_images.len();
                                    read_images.push(Image {
                                        data: image.data,
                                        format: image.format,
                                        from: (0, 0),
                                        to: image.to,
                                    });
                                    let mut metadata = image.metadata;
                                    metadata.kind = DrawingObjectKind::Image;
                                    metadata.object_index = object_index;
                                    if metadata.behavior != DrawingAnchorBehavior::Absolute {
                                        add_ods_style_loss(
                                            &mut style_losses,
                                            StyleLossKind::DrawingMetadataPartial,
                                            1,
                                        );
                                    }
                                    drawing_metadata.push(metadata);
                                }
                            }
                        } else if drawing_metadata.len() < MAX_ODS_DRAWINGS {
                            frame.metadata.kind = DrawingObjectKind::Shape;
                            frame.metadata.object_index = 0;
                            drawing_metadata.push(frame.metadata);
                            add_ods_style_loss(
                                &mut style_losses,
                                StyleLossKind::UnsupportedProperty,
                                1,
                            );
                        } else {
                            add_ods_style_loss(&mut style_losses, StyleLossKind::LimitExceeded, 1);
                        }
                    }
                    frame_in_cell = false;
                    in_frame_description = false;
                }
                b"table-cell" | b"covered-table-cell" => {
                    if let Some(a) = cur.take() {
                        flush_ods_run(&text, &mut cell_run_start, &mut cell_runs, &cell_run_font);
                        let rich_start_col = col;
                        let pending_comment =
                            (!cell_comment_text.trim().is_empty()).then(|| PendingComment {
                                text: cell_comment_text.trim().to_string(),
                                author: cell_comment_author.clone(),
                            });
                        let validation = a
                            .validation_name
                            .as_deref()
                            .and_then(|name| validation_rules.get(name));
                        let resolved_style = if a.style_name_invalid {
                            Some(CellStyle::default())
                        } else {
                            ods_named_cell_style(styles, a.style_name.as_deref())
                        };
                        let mut sink = CellSink {
                            cells: &mut cells,
                            merges: &mut merges,
                            read_hyperlinks: &mut read_hyperlinks,
                            read_comments: &mut read_comments,
                            read_data_validations: &mut read_data_validations,
                            read_images: &mut read_images,
                            drawing_metadata: &mut drawing_metadata,
                            blank_styles: &mut blank_styles,
                            style_losses: &mut style_losses,
                            budget: &mut budget,
                        };
                        finish_cell(
                            &mut sink,
                            row,
                            &mut col,
                            &a,
                            &text,
                            CellMetadata {
                                hyperlink: cell_hyperlink.as_deref(),
                                comment: pending_comment.as_ref(),
                                validation,
                                images: &cell_images,
                                style: resolved_style.as_ref(),
                                number_format_state: if a.style_name_invalid {
                                    Some(OdsNumberFormatState::Unresolved)
                                } else {
                                    ods_named_cell_number_format_state(
                                        styles,
                                        a.style_name.as_deref(),
                                    )
                                },
                                row_formats: &row_formats,
                                row_number_format_states: &row_number_format_states,
                                col_formats: &col_formats,
                                col_number_format_states: &col_number_format_states,
                                default_format: default_format.as_ref(),
                                default_number_format_state,
                            },
                        );
                        if cell_saw_span && !cell_runs.is_empty() {
                            for rich_col in rich_start_col..col {
                                rich.insert((row, rich_col), cell_runs.clone());
                            }
                        }
                        cell_hyperlink = None;
                        cell_comment_text.clear();
                        cell_comment_author = None;
                        cell_comment_author_text.clear();
                        cell_images.clear();
                        cell_runs.clear();
                        cell_run_start = 0;
                        cell_saw_span = false;
                        span_depth = 0;
                        in_annotation = false;
                        in_annotation_p = false;
                        in_annotation_creator = false;
                    }
                }
                b"table-row" => {
                    // A `number-rows-repeated` row that carries values must be
                    // replicated, not just skipped (a common bug). Empty repeated
                    // rows have no cells, so this is a no-op spacer for them.
                    if row_rep > 1
                        && (cells.len() > row_start
                            || read_comments.len() > row_comment_start
                            || read_data_validations.len() > row_validation_start
                            || read_images.len() > row_image_start)
                    {
                        let template: Vec<CellEntry> = cells[row_start..].to_vec();
                        let hyperlink_template: Vec<(u16, String)> = read_hyperlinks
                            [row_hyperlink_start..]
                            .iter()
                            .map(|(_, col, url)| (*col, url.clone()))
                            .collect();
                        let comment_template: Vec<(u16, String, Option<String>)> = read_comments
                            [row_comment_start..]
                            .iter()
                            .map(|comment| {
                                (comment.col, comment.text.clone(), comment.author.clone())
                            })
                            .collect();
                        let validation_template: Vec<DataValidation> =
                            read_data_validations[row_validation_start..].to_vec();
                        let image_template: Vec<Image> = read_images[row_image_start..].to_vec();
                        let image_metadata_template: Vec<DrawingMetadata> = drawing_metadata
                            [row_drawing_start..]
                            .iter()
                            .filter(|metadata| {
                                metadata.kind == DrawingObjectKind::Image
                                    && metadata.object_index >= row_image_start
                            })
                            .cloned()
                            .collect();
                        let rich_template: Vec<(u16, Vec<crate::TextRun>)> = rich
                            .range((row, 0)..=(row, u16::MAX))
                            .map(|((_, col), runs)| (*col, runs.clone()))
                            .collect();
                        'rep: for r in 1..row_rep {
                            for c in &template {
                                // Per-clone budget charge (text + per-cell cost) so
                                // neither a large-text nor an empty-text repeated row
                                // can blow memory; the budget — not an arbitrary cap —
                                // is the bound.
                                let hyperlink = hyperlink_template
                                    .iter()
                                    .find(|(col, _)| *col == c.col)
                                    .map(|(_, url)| url.as_str());
                                let cost = c
                                    .text
                                    .len()
                                    .saturating_add(hyperlink.map(str::len).unwrap_or(0))
                                    .saturating_add(CELL_COST);
                                if cost > budget {
                                    budget = 0;
                                    break 'rep;
                                }
                                budget -= cost;
                                let out_row = row.saturating_add(r);
                                cells.push(CellEntry {
                                    row: out_row,
                                    ..c.clone()
                                });
                                if let Some(url) = hyperlink {
                                    read_hyperlinks.push((out_row, c.col, url.to_string()));
                                }
                            }
                            for (col, text, author) in &comment_template {
                                let cost = text
                                    .len()
                                    .saturating_add(author.as_deref().map(str::len).unwrap_or(0))
                                    .saturating_add(CELL_COST);
                                if cost > budget {
                                    budget = 0;
                                    break 'rep;
                                }
                                budget -= cost;
                                read_comments.push(Comment {
                                    row: row.saturating_add(r),
                                    col: *col,
                                    text: text.clone(),
                                    author: author.clone(),
                                });
                            }
                            for validation in &validation_template {
                                let cost = data_validation_cost(validation);
                                if cost > budget {
                                    budget = 0;
                                    break 'rep;
                                }
                                budget -= cost;
                                let mut cloned = validation.clone();
                                cloned.sqref.0 = cloned.sqref.0.saturating_add(r);
                                cloned.sqref.2 = cloned.sqref.2.saturating_add(r);
                                read_data_validations.push(cloned);
                            }
                            for (image, metadata) in
                                image_template.iter().zip(&image_metadata_template)
                            {
                                let cost = image.data.len().saturating_add(CELL_COST);
                                if cost > budget {
                                    budget = 0;
                                    break 'rep;
                                }
                                budget -= cost;
                                let mut cloned = image.clone();
                                cloned.from.0 = cloned.from.0.saturating_add(r);
                                if let Some((row, col)) = cloned.to {
                                    cloned.to = Some((row.saturating_add(r), col));
                                }
                                let object_index = read_images.len();
                                read_images.push(cloned);
                                let mut metadata = metadata.clone();
                                metadata.object_index = object_index;
                                drawing_metadata.push(metadata);
                            }
                            for (col, runs) in &rich_template {
                                rich.insert((row.saturating_add(r), *col), runs.clone());
                            }
                        }
                    }
                    row = row.saturating_add(row_rep.max(1));
                }
                b"table-header-rows" if in_table_header_rows => {
                    if let Some(start) = table_header_row_start.take() {
                        if row > start {
                            page_setup
                                .get_or_insert_with(PageSetup::default)
                                .repeat_rows = Some((start, row.saturating_sub(1)));
                        }
                    }
                    in_table_header_rows = false;
                }
                b"table-header-columns" if in_table_header_columns => {
                    if table_header_column_count > 0 {
                        let end_col = table_header_column_count.saturating_sub(1) as u16;
                        page_setup
                            .get_or_insert_with(PageSetup::default)
                            .repeat_cols = Some((0, end_col));
                    }
                    in_table_header_columns = false;
                    table_header_column_count = 0;
                }
                b"table-column-group" if col_group_depth > 0 => {
                    col_group_depth = col_group_depth.saturating_sub(1);
                }
                b"table-row-group" if row_group_depth > 0 => {
                    row_group_depth = row_group_depth.saturating_sub(1);
                }
                b"table" if in_table => {
                    let style_fidelity = if !styles.has_source_styles {
                        StyleFidelity::Unavailable
                    } else if style_losses.is_empty() {
                        StyleFidelity::Retained
                    } else {
                        StyleFidelity::Partial
                    };
                    sheets.push(Sheet {
                        name: std::mem::take(&mut name),
                        is_worksheet: true,
                        // Preserve Calc's application default in its native
                        // integer-twip domain for undeclared ODS columns.
                        imported_default_column_axis_measure: Some(ImportedAxisMeasure::Twips(
                            1_280,
                        )),
                        // See the sibling empty-table branch above: Calc has
                        // no persisted document-wide row height for ODS, so
                        // undeclared rows resolve to Calc's generic
                        // no-information application default of 0.5 cm
                        // (14.173228 pt).
                        imported_default_row_axis_measure: Some(
                            ImportedAxisMeasure::MillimeterHundredths(500),
                        ),
                        style_fidelity,
                        cells: std::mem::take(&mut cells),
                        default_format: default_format.take(),
                        row_formats: std::mem::take(&mut row_formats),
                        col_formats: std::mem::take(&mut col_formats),
                        blank_styles: std::mem::take(&mut blank_styles),
                        row_heights: std::mem::take(&mut row_heights),
                        automatic_row_height_candidates: std::mem::take(
                            &mut automatic_row_height_candidates,
                        ),
                        col_widths: std::mem::take(&mut col_widths),
                        physical_col_widths: std::mem::take(&mut physical_col_widths),
                        imported_row_axis_measures: std::mem::take(&mut imported_row_axis_measures),
                        imported_column_axis_measures: std::mem::take(
                            &mut imported_column_axis_measures,
                        ),
                        hidden_rows: std::mem::take(&mut hidden_rows),
                        hidden_cols: std::mem::take(&mut hidden_cols),
                        style_losses: std::mem::take(&mut style_losses),
                        read_merges: std::mem::take(&mut merges),
                        read_hyperlinks: std::mem::take(&mut read_hyperlinks),
                        comments: std::mem::take(&mut read_comments),
                        data_validations: std::mem::take(&mut read_data_validations),
                        images: std::mem::take(&mut read_images),
                        drawing_metadata: std::mem::take(&mut drawing_metadata),
                        row_outline: std::mem::take(&mut row_outline),
                        col_outline: std::mem::take(&mut col_outline),
                        rich: std::mem::take(&mut rich),
                        page_setup: page_setup.take(),
                        print_metadata: std::mem::take(&mut print_metadata),
                        tab_color,
                        right_to_left,
                        hidden,
                        protect: protected,
                        print_gridlines,
                        print_headings,
                        ..Default::default()
                    });
                    in_table = false;
                    row_group_depth = 0;
                    col_group_depth = 0;
                    in_table_header_rows = false;
                    table_header_row_start = None;
                    in_table_header_columns = false;
                    table_header_column_count = 0;
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    for sheet in &mut sheets {
        if let Some(range) = autofilters.get(&sheet.name) {
            sheet.autofilter = Some(*range);
        }
        for db in database_ranges
            .iter()
            .filter(|database_range| database_range.sheet == sheet.name)
        {
            if let Some(table) = table_from_database_range(sheet, db) {
                sheet.tables.push(table);
            }
        }
    }
    Workbook {
        sheets,
        defined_names,
        date1904: false,
        text_truncated: budget == 0,
        container_parse_mode: crate::ContainerParseMode::Primary,
        ..Default::default()
    }
}

struct CellSink<'a> {
    cells: &'a mut Vec<CellEntry>,
    merges: &'a mut Merges,
    read_hyperlinks: &'a mut Hyperlinks,
    read_comments: &'a mut Comments,
    read_data_validations: &'a mut DataValidations,
    read_images: &'a mut Images,
    drawing_metadata: &'a mut Vec<DrawingMetadata>,
    blank_styles: &'a mut BTreeMap<(u32, u16), CellStyle>,
    style_losses: &'a mut Vec<StyleLoss>,
    budget: &'a mut usize,
}

fn finish_cell(
    sink: &mut CellSink<'_>,
    row: u32,
    col: &mut u16,
    a: &CellAttrs,
    text: &str,
    metadata: CellMetadata<'_>,
) {
    let rep = a.col_rep.min(u32::from(u16::MAX));
    if let Some((value, fallback_display)) = build_cell(a, text) {
        // A merged cell spans col_span × row_span; record the range.
        if a.col_span > 1 || a.row_span > 1 {
            let r1 = row.saturating_add(a.row_span.saturating_sub(1));
            let c1 = col.saturating_add(a.col_span.saturating_sub(1));
            sink.merges.push((row, *col, r1, c1));
        }
        // Replicate a *valued* cell across the full repeat run, bounded by the
        // allocation budget (an empty cell has no value and just advances the
        // column cursor). Each clone costs its text length plus a per-cell charge,
        // so even empty-text valued cells consume budget and cannot amplify.
        for k in 0..rep {
            let out_col = col.saturating_add(k as u16);
            let number_format = ods_cell_number_format(
                metadata.style,
                metadata.number_format_state,
                metadata.row_formats,
                metadata.row_number_format_states,
                metadata.col_formats,
                metadata.col_number_format_states,
                metadata.default_format,
                metadata.default_number_format_state,
                row,
                out_col,
            );
            let display = match number_format {
                OdsCellNumberFormat::Resolved(format) => render_ods_number_format(&value, format)
                    .unwrap_or_else(|| {
                        if text.is_empty() {
                            fallback_display.clone()
                        } else {
                            text.to_string()
                        }
                    }),
                OdsCellNumberFormat::Unresolved if !text.is_empty() => text.to_string(),
                OdsCellNumberFormat::General | OdsCellNumberFormat::Unresolved => {
                    fallback_display.clone()
                }
            };
            let cost = display
                .len()
                .saturating_add(metadata.hyperlink.map(str::len).unwrap_or(0))
                .saturating_add(CELL_COST);
            if cost > *sink.budget {
                *sink.budget = 0;
                break;
            }
            *sink.budget -= cost;
            sink.cells.push(CellEntry {
                row,
                col: out_col,
                value: value.clone(),
                text: display,
                style: metadata.style.cloned(),
                xlsx_font_size_pt: None,
                hyperlink: None,
            });
            if let Some(url) = metadata.hyperlink {
                sink.read_hyperlinks.push((row, out_col, url.to_string()));
            }
        }
    } else if let Some(style) = metadata.style {
        for k in 0..rep {
            if sink.blank_styles.len() >= MAX_ODS_LAYOUT_ENTRIES {
                add_ods_style_loss(sink.style_losses, StyleLossKind::LimitExceeded, 1);
                break;
            }
            sink.blank_styles
                .insert((row, col.saturating_add(k as u16)), style.clone());
        }
    }
    if let Some(comment) = metadata.comment {
        for k in 0..rep {
            let cost = comment
                .text
                .len()
                .saturating_add(comment.author.as_deref().map(str::len).unwrap_or(0))
                .saturating_add(CELL_COST);
            if cost > *sink.budget {
                *sink.budget = 0;
                break;
            }
            *sink.budget -= cost;
            sink.read_comments.push(Comment {
                row,
                col: col.saturating_add(k as u16),
                text: comment.text.clone(),
                author: comment.author.clone(),
            });
        }
    }
    if let Some(validation) = metadata.validation {
        push_data_validation(sink, validation, row, *col, rep);
    }
    for image in metadata.images {
        push_image(sink, image, row, *col, rep);
    }
    *col = col.saturating_add(rep as u16);
}

fn push_image(sink: &mut CellSink<'_>, image: &PendingImage, row: u32, col: u16, rep: u32) {
    for k in 0..rep {
        if sink.read_images.len() >= MAX_ODS_DRAWINGS {
            add_ods_style_loss(sink.style_losses, StyleLossKind::LimitExceeded, 1);
            break;
        }
        let cost = image.data.len().saturating_add(CELL_COST);
        if cost > *sink.budget {
            *sink.budget = 0;
            break;
        }
        *sink.budget -= cost;
        let object_index = sink.read_images.len();
        sink.read_images.push(Image {
            data: image.data.clone(),
            format: image.format,
            from: (row, col.saturating_add(k as u16)),
            to: image
                .to
                .map(|(to_row, to_col)| (to_row, to_col.saturating_add(k as u16))),
        });
        let mut metadata = image.metadata.clone();
        metadata.kind = DrawingObjectKind::Image;
        metadata.object_index = object_index;
        metadata.from_cell = Some((row, col.saturating_add(k as u16)));
        metadata.to_cell = image
            .to
            .map(|(to_row, to_col)| (to_row, to_col.saturating_add(k as u16)));
        sink.drawing_metadata.push(metadata);
    }
}

fn push_data_validation(
    sink: &mut CellSink<'_>,
    validation: &DataValidation,
    row: u32,
    col: u16,
    rep: u32,
) {
    if rep == 0 {
        return;
    }
    let mut cloned = validation.clone();
    cloned.sqref = (
        row,
        col,
        row,
        col.saturating_add(rep.saturating_sub(1) as u16),
    );
    let cost = data_validation_cost(&cloned);
    if cost > *sink.budget {
        *sink.budget = 0;
        return;
    }
    *sink.budget -= cost;
    sink.read_data_validations.push(cloned);
}

fn data_validation_cost(validation: &DataValidation) -> usize {
    let prompt = validation
        .prompt
        .as_ref()
        .map(|(title, message)| title.len().saturating_add(message.len()))
        .unwrap_or(0);
    let error = validation
        .error
        .as_ref()
        .map(|(title, message)| title.len().saturating_add(message.len()))
        .unwrap_or(0);
    validation
        .formula1
        .len()
        .saturating_add(validation.formula2.as_deref().map(str::len).unwrap_or(0))
        .saturating_add(prompt)
        .saturating_add(error)
        .saturating_add(CELL_COST)
}

fn build_cell(a: &CellAttrs, text: &str) -> Option<(Cell, String)> {
    let formula = a.formula.as_ref().filter(|formula| !formula.is_empty());
    let cached = match a.vtype.as_str() {
        "float" => {
            let f: f64 = a.val.as_deref().and_then(|v| v.parse().ok())?;
            // A float without a data style has General semantics. Calc derives
            // its display from the typed office:value instead of treating the
            // serialized text:p cache as a fixed decimal scale.
            Some((Cell::Number(f), crate::format_number(f)))
        }
        "currency" => {
            let f: f64 = a.val.as_deref().and_then(|v| v.parse().ok())?;
            Some((
                Cell::Number(f),
                if text.is_empty() {
                    crate::format_number(f)
                } else {
                    text.to_string()
                },
            ))
        }
        "percentage" => {
            let f: f64 = a.val.as_deref().and_then(|v| v.parse().ok())?;
            Some((
                Cell::Number(f),
                if text.is_empty() {
                    crate::format::render_value(f, crate::format::Kind::Percent, false)
                } else {
                    text.to_string()
                },
            ))
        }
        "boolean" => {
            let b = a.val.as_deref()? == "true";
            Some((Cell::Bool(b), if b { "TRUE" } else { "FALSE" }.to_string()))
        }
        "date" => {
            let iso = a.val.as_deref()?;
            let serial = crate::format::iso_date_to_serial(iso)?;
            let disp = if text.is_empty() {
                iso.to_string()
            } else {
                text.to_string()
            };
            Some((Cell::Date(serial), disp))
        }
        "time" => {
            // office:time-value is an ISO-8601 duration (PTnHnMnS) → day fraction.
            let frac = parse_iso_duration(a.val.as_deref()?)?;
            let disp = if text.is_empty() {
                crate::format::render_value(frac, crate::format::Kind::Time, false)
            } else {
                text.to_string()
            };
            // `Cell::Date` represents date, time, and datetime serials. Keep
            // that semantic provenance even when an ODS time cell has no
            // explicit data style, and through formula cached values.
            Some((Cell::Date(frac), disp))
        }
        // "string" or untyped → the displayed text.
        _ => {
            if text.is_empty() {
                None
            } else {
                Some((Cell::Text(text.to_string()), text.to_string()))
            }
        }
    };

    let (value, display) = if let Some(formula) = formula {
        let (cached, display) = cached.unwrap_or_else(|| {
            let cached = Cell::Text(text.to_string());
            (cached, text.to_string())
        });
        (
            Cell::Formula {
                formula: formula.clone(),
                cached: Box::new(cached),
            },
            display,
        )
    } else {
        cached?
    };
    Some((value, display))
}

fn render_ods_number_format(value: &Cell, format: &str) -> Option<String> {
    match value {
        Cell::Number(number) | Cell::Date(number) => {
            Some(crate::format::render_format(*number, format, false))
        }
        Cell::Formula { cached, .. } => render_ods_number_format(cached, format),
        Cell::Text(_) | Cell::Bool(_) | Cell::Error(_) => None,
    }
}

fn normalize_formula(formula: String) -> String {
    formula
        .strip_prefix("of:=")
        .or_else(|| formula.strip_prefix("="))
        .unwrap_or(&formula)
        .to_string()
}

fn normalize_ods_validation_condition(condition: &str) -> String {
    let condition = condition.trim();
    condition
        .strip_prefix("of:=")
        .or_else(|| condition.strip_prefix("="))
        .unwrap_or(condition)
        .trim()
        .to_string()
}

fn attr_true(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true")
}

/// Parse an ISO-8601 duration `PTnHnMnS` to a fraction of a day (Excel time).
fn parse_iso_duration(s: &str) -> Option<f64> {
    let body = s.strip_prefix("PT")?;
    let (mut h, mut m, mut sec) = (0.0f64, 0.0f64, 0.0f64);
    let mut num = String::new();
    for c in body.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else {
            let v: f64 = num.parse().ok()?;
            num.clear();
            match c {
                'H' => h = v,
                'M' => m = v,
                'S' => sec = v,
                _ => {}
            }
        }
    }
    Some((h * 3600.0 + m * 60.0 + sec) / 86400.0)
}

#[cfg(test)]
mod tests;
