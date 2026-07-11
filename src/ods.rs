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

use std::collections::{BTreeMap, HashMap};
use std::io::Read;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::{Error, Result};
use crate::{
    Cell, CellEntry, Color, Comment, DataValidation, DocProperties, DvKind, DvOp, Image, ImageFmt,
    PageSetup, Sheet, Table, Workbook,
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
            a.unescape_value().ok().map(|v| v.into_owned())
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
    let image_parts = read_image_parts(&mut zip);
    let mut style_definitions = OdsStyleDefinitions::default();
    read_ods_style_definitions(&styles_xml, &mut style_definitions);
    read_ods_style_definitions(&content, &mut style_definitions);
    let table_styles = style_definitions.into_table_styles();
    let mut workbook = parse_content(&content, &table_styles, &image_parts);
    workbook.properties = parse_meta_properties(&meta_xml);
    apply_ods_settings(&mut workbook, parse_settings(&settings_xml));
    Ok(workbook)
}

type Merges = Vec<(u32, u16, u32, u16)>;
type Hyperlinks = Vec<(u32, u16, String)>;
type Comments = Vec<Comment>;
type DataValidations = Vec<DataValidation>;
type Images = Vec<Image>;
type TableStyles = HashMap<String, TableStyleOptions>;
type AutoFilters = HashMap<String, (u32, u16, u32, u16)>;
type ValidationRules = HashMap<String, DataValidation>;
type ImageParts = HashMap<String, (ImageFmt, Vec<u8>)>;

#[derive(Clone, Copy, Default)]
struct TableStyleOptions {
    visible: Option<bool>,
    tab_color: Option<Color>,
    print_gridlines: bool,
    print_headings: bool,
    landscape: Option<bool>,
    scale: Option<u16>,
    first_page_number: Option<u16>,
    center_horizontally: bool,
    center_vertically: bool,
}

impl TableStyleOptions {
    fn hidden(self) -> bool {
        self.visible == Some(false)
    }
}

#[derive(Clone, Copy, Default)]
struct PageLayoutOptions {
    gridlines: bool,
    headings: bool,
    landscape: Option<bool>,
    scale: Option<u16>,
    first_page_number: Option<u16>,
    center_horizontally: bool,
    center_vertically: bool,
}

#[derive(Default)]
struct OdsStyleDefinitions {
    table_styles: TableStyles,
    table_master_pages: HashMap<String, String>,
    master_page_layouts: HashMap<String, String>,
    page_layout_options: HashMap<String, PageLayoutOptions>,
}

impl OdsStyleDefinitions {
    fn into_table_styles(mut self) -> TableStyles {
        for (style, master_page) in self.table_master_pages {
            let Some(page_layout) = self.master_page_layouts.get(&master_page) else {
                continue;
            };
            let Some(page_layout) = self.page_layout_options.get(page_layout) else {
                continue;
            };
            let entry = self.table_styles.entry(style).or_default();
            entry.print_gridlines = page_layout.gridlines;
            entry.print_headings = page_layout.headings;
            entry.landscape = page_layout.landscape;
            entry.scale = page_layout.scale;
            entry.first_page_number = page_layout.first_page_number;
            entry.center_horizontally = page_layout.center_horizontally;
            entry.center_vertically = page_layout.center_vertically;
        }
        self.table_styles
    }
}

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
}

struct CellMetadata<'a> {
    hyperlink: Option<&'a str>,
    comment: Option<&'a PendingComment>,
    validation: Option<&'a DataValidation>,
    images: &'a [PendingImage],
}

fn read_cell_attrs(e: &quick_xml::events::BytesStart<'_>) -> CellAttrs {
    CellAttrs {
        vtype: attr(e, b"value-type").unwrap_or_default(),
        val: attr(e, b"value")
            .or_else(|| attr(e, b"date-value"))
            .or_else(|| attr(e, b"boolean-value"))
            .or_else(|| attr(e, b"time-value")),
        formula: attr(e, b"formula").map(normalize_formula),
        validation_name: attr(e, b"content-validation-name").filter(|name| !name.trim().is_empty()),
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

fn apply_table_properties(
    e: &quick_xml::events::BytesStart<'_>,
    table_style: &Option<String>,
    styles: &mut TableStyles,
) {
    let Some(name) = table_style.as_ref() else {
        return;
    };
    if let Some(display) = attr(e, b"display") {
        styles.entry(name.clone()).or_default().visible = Some(display != "false");
    }
    if let Some(tab_color) = attr(e, b"tab-color").and_then(|value| parse_ods_color(&value)) {
        styles.entry(name.clone()).or_default().tab_color = Some(tab_color);
    }
}

fn parse_ods_color(value: &str) -> Option<Color> {
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

fn page_layout_options(e: &quick_xml::events::BytesStart<'_>) -> Option<PageLayoutOptions> {
    let mut options = PageLayoutOptions::default();
    let mut found = false;
    if let Some(print) = attr(e, b"print") {
        found = true;
        for value in print.split_ascii_whitespace() {
            match value {
                "grid" => options.gridlines = true,
                "headers" => options.headings = true,
                _ => {}
            }
        }
    }
    if let Some(orientation) = attr(e, b"print-orientation") {
        found = true;
        options.landscape = Some(orientation.eq_ignore_ascii_case("landscape"));
    }
    if let Some(scale) = attr(e, b"scale-to").and_then(|value| parse_ods_percentage(&value)) {
        found = true;
        options.scale = Some(scale);
    }
    if let Some(first_page) =
        attr(e, b"first-page-number").and_then(|value| parse_positive_u16(&value))
    {
        found = true;
        options.first_page_number = Some(first_page);
    }
    if let Some(table_centering) = attr(e, b"table-centering") {
        found = true;
        match table_centering.as_str() {
            "horizontal" => options.center_horizontally = true,
            "vertical" => options.center_vertically = true,
            "both" => {
                options.center_horizontally = true;
                options.center_vertically = true;
            }
            _ => {}
        }
    }
    found.then_some(options)
}

fn parse_ods_percentage(value: &str) -> Option<u16> {
    let percent = value.trim().strip_suffix('%')?.trim();
    parse_positive_u16(percent)
}

fn parse_positive_u16(value: &str) -> Option<u16> {
    value.trim().parse::<u16>().ok().filter(|value| *value > 0)
}

fn read_ods_style_definitions(xml: &str, definitions: &mut OdsStyleDefinitions) {
    let mut r = Reader::from_str(xml);
    let mut table_style: Option<String> = None;
    let mut page_layout: Option<String> = None;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"style" if attr(&e, b"family").as_deref() == Some("table") => {
                    table_style = attr(&e, b"name");
                    if let Some(name) = table_style.as_ref() {
                        definitions.table_styles.entry(name.clone()).or_default();
                        if let Some(master_page) = attr(&e, b"master-page-name") {
                            definitions
                                .table_master_pages
                                .insert(name.clone(), master_page);
                        }
                    }
                }
                b"table-properties" => {
                    apply_table_properties(&e, &table_style, &mut definitions.table_styles);
                }
                b"page-layout" => page_layout = attr(&e, b"name"),
                b"page-layout-properties" => {
                    if let (Some(name), Some(options)) =
                        (page_layout.as_ref(), page_layout_options(&e))
                    {
                        definitions
                            .page_layout_options
                            .insert(name.clone(), options);
                    }
                }
                b"master-page" => {
                    if let (Some(name), Some(page_layout)) =
                        (attr(&e, b"name"), attr(&e, b"page-layout-name"))
                    {
                        definitions.master_page_layouts.insert(name, page_layout);
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"table-properties" => {
                    apply_table_properties(&e, &table_style, &mut definitions.table_styles);
                }
                b"page-layout" => {
                    if let Some(name) = attr(&e, b"name") {
                        definitions.page_layout_options.entry(name).or_default();
                    }
                }
                b"page-layout-properties" => {
                    if let (Some(name), Some(options)) =
                        (page_layout.as_ref(), page_layout_options(&e))
                    {
                        definitions
                            .page_layout_options
                            .insert(name.clone(), options);
                    }
                }
                b"master-page" => {
                    if let (Some(name), Some(page_layout)) =
                        (attr(&e, b"name"), attr(&e, b"page-layout-name"))
                    {
                        definitions.master_page_layouts.insert(name, page_layout);
                    }
                }
                b"style" => {
                    if attr(&e, b"family").as_deref() == Some("table") {
                        if let Some(name) = attr(&e, b"name") {
                            definitions.table_styles.entry(name.clone()).or_default();
                            if let Some(master_page) = attr(&e, b"master-page-name") {
                                definitions.table_master_pages.insert(name, master_page);
                            }
                        }
                    }
                    table_style = None;
                }
                _ => {}
            },
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"style" => table_style = None,
            Ok(Event::End(e)) if local(e.name().as_ref()) == b"page-layout" => page_layout = None,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
}

fn table_style_options(
    e: &quick_xml::events::BytesStart<'_>,
    table_styles: &TableStyles,
) -> TableStyleOptions {
    attr(e, b"style-name")
        .and_then(|style| table_styles.get(&style).copied())
        .unwrap_or_default()
}

fn table_protected(e: &quick_xml::events::BytesStart<'_>) -> bool {
    attr(e, b"protected").as_deref().is_some_and(attr_true)
}

fn table_page_setup(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
    style: TableStyleOptions,
) -> Option<PageSetup> {
    let mut setup = style.landscape.map(|landscape| PageSetup {
        landscape,
        ..Default::default()
    });
    if let Some(print_area) = read_table_print_area(e, name) {
        setup.get_or_insert_with(PageSetup::default).print_area = Some(print_area);
    }
    if let Some(scale) = style.scale {
        setup.get_or_insert_with(PageSetup::default).scale = Some(scale);
    }
    if let Some(first_page_number) = style.first_page_number {
        setup
            .get_or_insert_with(PageSetup::default)
            .first_page_number = Some(first_page_number);
    }
    if style.center_horizontally {
        setup
            .get_or_insert_with(PageSetup::default)
            .center_horizontally = true;
    }
    if style.center_vertically {
        setup
            .get_or_insert_with(PageSetup::default)
            .center_vertically = true;
    }
    setup
}

fn text_of(e: &quick_xml::events::BytesText<'_>) -> String {
    e.unescape().map(|c| c.into_owned()).unwrap_or_default()
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
    })
}

fn parse_content(xml: &str, table_styles: &TableStyles, image_parts: &ImageParts) -> Workbook {
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
    let mut row_outline: BTreeMap<u32, u8> = BTreeMap::new();
    let mut col_outline: BTreeMap<u16, u8> = BTreeMap::new();
    let mut page_setup: Option<PageSetup> = None;
    let mut name = String::new();
    let mut tab_color: Option<Color> = None;
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
                    let style = table_style_options(&e, table_styles);
                    tab_color = style.tab_color;
                    hidden = style.hidden();
                    protected = table_protected(&e);
                    print_gridlines = style.print_gridlines;
                    print_headings = style.print_headings;
                    page_setup = table_page_setup(&e, &name, style);
                    cells = Vec::new();
                    merges = Vec::new();
                    read_hyperlinks = Vec::new();
                    read_comments = Vec::new();
                    read_data_validations = Vec::new();
                    read_images = Vec::new();
                    row_outline = BTreeMap::new();
                    col_outline = BTreeMap::new();
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
                    record_col_outline(&mut col_outline, table_column, repeat, col_group_depth);
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
                    row_rep = attr(&e, b"number-rows-repeated")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1)
                        .min(MAX_ROW_REPEAT);
                    record_row_outline(&mut row_outline, row, row_rep, row_group_depth);
                }
                b"table-cell" | b"covered-table-cell" if in_table => {
                    cur = Some(read_cell_attrs(&e));
                    text.clear();
                    cell_hyperlink = None;
                    cell_comment_text.clear();
                    cell_comment_author = None;
                    cell_comment_author_text.clear();
                    cell_images.clear();
                    in_annotation = false;
                    in_annotation_p = false;
                    in_annotation_creator = false;
                }
                b"image" if cur.is_some() => {
                    if let Some(image) = read_draw_image(&e, image_parts) {
                        cell_images.push(image);
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
                b"p" if cur.is_some() => in_p = true,
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
                    let style = table_style_options(&e, table_styles);
                    sheets.push(Sheet {
                        page_setup: table_page_setup(&e, &name, style),
                        name,
                        is_worksheet: true,
                        tab_color: style.tab_color,
                        hidden: style.hidden(),
                        protect: table_protected(&e),
                        print_gridlines: style.print_gridlines,
                        print_headings: style.print_headings,
                        ..Default::default()
                    });
                }
                b"table-cell" | b"covered-table-cell" if in_table => {
                    let a = read_cell_attrs(&e);
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
                        },
                    );
                }
                b"image" if cur.is_some() => {
                    if let Some(image) = read_draw_image(&e, image_parts) {
                        cell_images.push(image);
                    }
                }
                b"table-column" if in_table => {
                    let repeat = read_column_repeat(&e);
                    record_col_outline(&mut col_outline, table_column, repeat, col_group_depth);
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
                    record_row_outline(&mut row_outline, row, rep, row_group_depth);
                    row = row.saturating_add(rep);
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_annotation_creator => {
                cell_comment_author_text
                    .push_str(&t.unescape().map(|c| c.into_owned()).unwrap_or_default());
            }
            Ok(Event::Text(t)) if in_annotation_p => {
                cell_comment_text
                    .push_str(&t.unescape().map(|c| c.into_owned()).unwrap_or_default());
            }
            Ok(Event::Text(t)) if in_p => {
                text.push_str(&t.unescape().map(|c| c.into_owned()).unwrap_or_default());
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
                b"p" => in_p = false,
                b"annotation" if in_annotation => {
                    in_annotation = false;
                    in_annotation_p = false;
                    in_annotation_creator = false;
                }
                b"table-cell" | b"covered-table-cell" => {
                    if let Some(a) = cur.take() {
                        let pending_comment =
                            (!cell_comment_text.trim().is_empty()).then(|| PendingComment {
                                text: cell_comment_text.trim().to_string(),
                                author: cell_comment_author.clone(),
                            });
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
                            },
                        );
                        cell_hyperlink = None;
                        cell_comment_text.clear();
                        cell_comment_author = None;
                        cell_comment_author_text.clear();
                        cell_images.clear();
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
                            for image in &image_template {
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
                                read_images.push(cloned);
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
                    sheets.push(Sheet {
                        name: std::mem::take(&mut name),
                        is_worksheet: true,
                        cells: std::mem::take(&mut cells),
                        read_merges: std::mem::take(&mut merges),
                        read_hyperlinks: std::mem::take(&mut read_hyperlinks),
                        comments: std::mem::take(&mut read_comments),
                        data_validations: std::mem::take(&mut read_data_validations),
                        images: std::mem::take(&mut read_images),
                        row_outline: std::mem::take(&mut row_outline),
                        col_outline: std::mem::take(&mut col_outline),
                        page_setup: page_setup.take(),
                        tab_color,
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
    if let Some((value, disp)) = build_cell(a, text) {
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
            let cost = disp
                .len()
                .saturating_add(metadata.hyperlink.map(str::len).unwrap_or(0))
                .saturating_add(CELL_COST);
            if cost > *sink.budget {
                *sink.budget = 0;
                break;
            }
            *sink.budget -= cost;
            let out_col = col.saturating_add(k as u16);
            sink.cells.push(CellEntry {
                row,
                col: out_col,
                value: value.clone(),
                text: disp.clone(),
                style: None,
                hyperlink: None,
            });
            if let Some(url) = metadata.hyperlink {
                sink.read_hyperlinks.push((row, out_col, url.to_string()));
            }
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
        let cost = image.data.len().saturating_add(CELL_COST);
        if cost > *sink.budget {
            *sink.budget = 0;
            break;
        }
        *sink.budget -= cost;
        sink.read_images.push(Image {
            data: image.data.clone(),
            format: image.format,
            from: (row, col.saturating_add(k as u16)),
            to: None,
        });
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
        "float" | "currency" => {
            let f: f64 = a.val.as_deref().and_then(|v| v.parse().ok())?;
            Some((
                Cell::Number(f),
                if text.is_empty() {
                    num_text(f)
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
            Some((Cell::Number(frac), disp))
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

    let Some(formula) = formula else {
        return cached;
    };
    let (cached, display) = cached.unwrap_or_else(|| {
        let cached = Cell::Text(text.to_string());
        (cached, text.to_string())
    });
    Some((
        Cell::Formula {
            formula: formula.clone(),
            cached: Box::new(cached),
        },
        display,
    ))
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

fn num_text(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// Convert an ODF `office:date-value` (`YYYY-MM-DD` or `…THH:MM:SS`) to the Excel
/// 1900-system serial (days since 1899-12-30, plus the time-of-day fraction).
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn ods_bytes(content: &str) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        zw.start_file("mimetype", opt).unwrap();
        zw.write_all(ODS_MIME.as_bytes()).unwrap();
        zw.start_file("content.xml", opt).unwrap();
        zw.write_all(content.as_bytes()).unwrap();
        zw.finish().unwrap().into_inner()
    }

    fn ods_bytes_without_mimetype(content: &str) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        zw.start_file("content.xml", opt).unwrap();
        zw.write_all(content.as_bytes()).unwrap();
        zw.finish().unwrap().into_inner()
    }

    fn ods_bytes_with_styles(content: &str, styles: &str) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        zw.start_file("mimetype", opt).unwrap();
        zw.write_all(ODS_MIME.as_bytes()).unwrap();
        zw.start_file("content.xml", opt).unwrap();
        zw.write_all(content.as_bytes()).unwrap();
        zw.start_file("styles.xml", opt).unwrap();
        zw.write_all(styles.as_bytes()).unwrap();
        zw.finish().unwrap().into_inner()
    }

    fn ods_bytes_with_meta(content: &str, meta: &str) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        zw.start_file("mimetype", opt).unwrap();
        zw.write_all(ODS_MIME.as_bytes()).unwrap();
        zw.start_file("content.xml", opt).unwrap();
        zw.write_all(content.as_bytes()).unwrap();
        zw.start_file("meta.xml", opt).unwrap();
        zw.write_all(meta.as_bytes()).unwrap();
        zw.finish().unwrap().into_inner()
    }

    fn ods_bytes_with_part(content: &str, path: &str, data: &[u8]) -> Vec<u8> {
        ods_bytes_with_parts(content, &[(path, data)])
    }

    fn ods_bytes_with_parts(content: &str, parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        zw.start_file("mimetype", opt).unwrap();
        zw.write_all(ODS_MIME.as_bytes()).unwrap();
        zw.start_file("content.xml", opt).unwrap();
        zw.write_all(content.as_bytes()).unwrap();
        for (path, data) in parts {
            zw.start_file(*path, opt).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    fn encrypted_ods_bytes() -> Vec<u8> {
        let manifest = r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0">
  <manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet">
    <manifest:encryption-data manifest:checksum-type="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0#sha256-1k" manifest:checksum="abc">
      <manifest:algorithm manifest:algorithm-name="http://www.w3.org/2001/04/xmlenc#aes256-cbc" manifest:initialisation-vector="abc"/>
    </manifest:encryption-data>
  </manifest:file-entry>
  <manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml">
    <manifest:encryption-data manifest:checksum-type="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0#sha256-1k" manifest:checksum="abc"/>
  </manifest:file-entry>
</manifest:manifest>"#;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = SimpleFileOptions::default();
        zw.start_file("mimetype", opt).unwrap();
        zw.write_all(ODS_MIME.as_bytes()).unwrap();
        zw.start_file("content.xml", opt).unwrap();
        zw.write_all(&[0xff, 0xfe, 0xfd, 0xfc]).unwrap();
        zw.start_file("META-INF/manifest.xml", opt).unwrap();
        zw.write_all(manifest.as_bytes()).unwrap();
        zw.finish().unwrap().into_inner()
    }

    #[test]
    fn reads_a_synthetic_ods() {
        let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="시트"><table:table-row><table:table-cell office:value-type="string"><text:p>품목</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="42"><text:p>42</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="boolean" office:boolean-value="true"><text:p>TRUE</text:p></table:table-cell><table:table-cell office:value-type="date" office:date-value="2024-03-15"><text:p>2024-03-15</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        let s = &wb.sheets[0];
        assert_eq!(s.name, "시트");
        assert_eq!(s.cell(0, 0), Some(&Cell::Text("품목".to_string())));
        assert_eq!(s.cell(0, 1), Some(&Cell::Number(42.0)));
        assert_eq!(s.cell(1, 0), Some(&Cell::Bool(true)));
        assert_eq!(s.cell(1, 1), Some(&Cell::Date(45366.0))); // 2024-03-15
    }

    #[test]
    fn ods_table_protection_surfaces_public_metadata() {
        let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Locked" table:protected="true"></table:table><table:table table:name="LockedEmpty" table:protected="true"/><table:table table:name="Plain" table:protected="false"/></office:spreadsheet></office:body></office:document-content>"#;
        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let locked = wb.sheet_by_name("Locked").unwrap();
        let locked_empty = wb.sheet_by_name("LockedEmpty").unwrap();
        let plain = wb.sheet_by_name("Plain").unwrap();

        assert!(locked.is_protected());
        assert_eq!(locked.protection_options(), None);
        assert!(locked_empty.is_protected());
        assert_eq!(locked_empty.protection_options(), None);
        assert!(!plain.is_protected());

        let metadata = wb.worksheet_metadata("Locked").unwrap();
        assert!(metadata.protected);
        assert_eq!(metadata.protection_options, None);

        let generic_metadata =
            <Workbook as crate::Reader>::worksheet_metadata(&wb, "Locked").unwrap();
        assert!(generic_metadata.protected);
        assert_eq!(generic_metadata.protection_options, None);
    }

    #[test]
    fn ods_encrypted_package_is_reported_before_missing_workbook() {
        let err = Workbook::open(&encrypted_ods_bytes()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unsupported encrypted OpenDocument package"
        );
    }

    #[test]
    fn ods_percentage_and_time_fallback_display_text_is_formatted() {
        let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="formats"><table:table-row><table:table-cell office:value-type="percentage" office:value="0.5"/><table:table-cell office:value-type="time" office:time-value="PT12H"/><table:table-cell office:value-type="percentage" office:value="0.25"><text:p>quarter</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let sheet = &wb.sheets[0];
        let range = wb.worksheet_range("formats").expect("range");

        assert_eq!(sheet.cell(0, 0), Some(&Cell::Number(0.5)));
        assert_eq!(sheet.formatted(0, 0), Some("50%"));
        assert_eq!(range.formatted_abs(0, 0), Some("50%"));
        assert_eq!(sheet.cell(0, 1), Some(&Cell::Number(0.5)));
        assert_eq!(sheet.formatted(0, 1), Some("12:00:00"));
        assert_eq!(range.formatted_abs(0, 1), Some("12:00:00"));
        assert_eq!(sheet.formatted(0, 2), Some("quarter"));
    }

    #[test]
    fn ods_sheet_visibility_follows_table_style_display() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:automatic-styles><style:style style:name="ta_hidden" style:family="table"><style:table-properties table:display="false"/></style:style><style:style style:name="ta_visible" style:family="table"><style:table-properties table:display="true"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Visible" table:style-name="ta_visible"></table:table><table:table table:name="Hidden" table:style-name="ta_hidden"></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();

        assert_eq!(wb.sheets.len(), 2);
        assert!(!wb.sheets[0].is_hidden(), "table:display=true is visible");
        assert!(wb.sheets[1].is_hidden(), "table:display=false is hidden");
    }

    #[test]
    fn ods_without_mimetype_dispatches_from_content_xml() {
        let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Fallback"><table:table-row><table:table-cell office:value-type="string"><text:p>ok</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes_without_mimetype(content)).unwrap();

        assert_eq!(wb.sheets[0].name, "Fallback");
        assert_eq!(wb.sheets[0].cell(0, 0), Some(&Cell::Text("ok".into())));
    }

    #[test]
    fn ods_sheet_visibility_follows_styles_xml_table_style() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="FromStyles" table:style-name="ta_hidden"></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:styles><style:style style:name="ta_hidden" style:family="table"><style:table-properties table:display="false"/></style:style></office:styles></office:document-styles>"#;

        let wb = Workbook::open(&ods_bytes_with_styles(content, styles)).unwrap();

        assert_eq!(wb.sheets.len(), 1);
        assert!(wb.sheets[0].is_hidden());
    }

    #[test]
    fn ods_page_layout_print_options_surface_public_metadata() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Printable" table:style-name="ta_print"></table:table><table:table table:name="Plain" table:style-name="ta_plain"/></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:styles><style:style style:name="ta_print" style:family="table" style:master-page-name="mp_print"><style:table-properties table:display="true"/></style:style><style:style style:name="ta_plain" style:family="table"><style:table-properties table:display="true"/></style:style></office:styles><office:automatic-styles><style:page-layout style:name="pm_print"><style:page-layout-properties style:print="headers grid"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="mp_print" style:page-layout-name="pm_print"/></office:master-styles></office:document-styles>"#;

        let wb = Workbook::open(&ods_bytes_with_styles(content, styles)).unwrap();
        let printable = wb.sheet_by_name("Printable").unwrap();
        let plain = wb.sheet_by_name("Plain").unwrap();

        assert!(printable.print_gridlines());
        assert!(printable.print_headings());
        assert!(!plain.print_gridlines());
        assert!(!plain.print_headings());

        let metadata = wb.worksheet_metadata("Printable").unwrap();
        assert!(metadata.print_gridlines);
        assert!(metadata.print_headings);

        let generic_metadata =
            <Workbook as crate::Reader>::worksheet_metadata(&wb, "Printable").unwrap();
        assert!(generic_metadata.print_gridlines);
        assert!(generic_metadata.print_headings);
    }

    #[test]
    fn ods_table_groups_surface_outline_metadata() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Grouped"><table:table-column/><table:table-column-group table:display="false"><table:table-column table:number-columns-repeated="2"/><table:table-column-group><table:table-column/></table:table-column-group></table:table-column-group><table:table-row><table:table-cell office:value-type="string"><text:p>top</text:p></table:table-cell></table:table-row><table:table-row-group table:display="false"><table:table-row table:number-rows-repeated="2"><table:table-cell office:value-type="string"><text:p>detail</text:p></table:table-cell></table:table-row><table:table-row-group><table:table-row><table:table-cell office:value-type="string"><text:p>nested</text:p></table:table-cell></table:table-row></table:table-row-group></table:table-row-group></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let sheet = wb.sheet_by_name("Grouped").unwrap();

        assert_eq!(sheet.col_outline_levels().get(&1), Some(&1));
        assert_eq!(sheet.col_outline_levels().get(&2), Some(&1));
        assert_eq!(sheet.col_outline_levels().get(&3), Some(&2));
        assert_eq!(sheet.row_outline_levels().get(&1), Some(&1));
        assert_eq!(sheet.row_outline_levels().get(&2), Some(&1));
        assert_eq!(sheet.row_outline_levels().get(&3), Some(&2));

        let metadata = sheet.metadata();
        assert_eq!(metadata.col_outline_levels.get(&1), Some(&1));
        assert_eq!(metadata.row_outline_levels.get(&3), Some(&2));
    }

    #[test]
    fn ods_page_layout_orientation_surfaces_page_setup() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Landscape" table:style-name="ta_landscape"/><table:table table:name="Plain"/></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles><style:style style:name="ta_landscape" style:family="table" style:master-page-name="mp_landscape"/></office:styles><office:automatic-styles><style:page-layout style:name="pm_landscape"><style:page-layout-properties style:print-orientation="landscape"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="mp_landscape" style:page-layout-name="pm_landscape"/></office:master-styles></office:document-styles>"#;

        let wb = Workbook::open(&ods_bytes_with_styles(content, styles)).unwrap();
        let landscape = wb.sheet_by_name("Landscape").unwrap();
        let plain = wb.sheet_by_name("Plain").unwrap();

        assert!(landscape.page_setup().expect("page setup").landscape);
        assert_eq!(plain.page_setup(), None);

        let metadata = wb.worksheet_metadata("Landscape").unwrap();
        assert!(metadata.page_setup.expect("metadata page setup").landscape);

        let generic_metadata =
            <Workbook as crate::Reader>::worksheet_metadata(&wb, "Landscape").unwrap();
        assert!(
            generic_metadata
                .page_setup
                .expect("generic metadata page setup")
                .landscape
        );
    }

    #[test]
    fn ods_page_layout_numbering_surfaces_page_setup() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Layout" table:style-name="ta_layout"/><table:table table:name="Plain"/></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles><style:style style:name="ta_layout" style:family="table" style:master-page-name="mp_layout"/></office:styles><office:automatic-styles><style:page-layout style:name="pm_layout"><style:page-layout-properties style:scale-to="85%" style:first-page-number="3" style:table-centering="both"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="mp_layout" style:page-layout-name="pm_layout"/></office:master-styles></office:document-styles>"#;

        let wb = Workbook::open(&ods_bytes_with_styles(content, styles)).unwrap();
        let layout = wb.sheet_by_name("Layout").unwrap();
        let plain = wb.sheet_by_name("Plain").unwrap();

        let setup = layout.page_setup().expect("page setup");
        assert_eq!(setup.scale, Some(85));
        assert_eq!(setup.first_page_number, Some(3));
        assert!(setup.center_horizontally);
        assert!(setup.center_vertically);
        assert_eq!(plain.page_setup(), None);

        let metadata = wb.worksheet_metadata("Layout").unwrap();
        let metadata_setup = metadata.page_setup.expect("metadata page setup");
        assert_eq!(metadata_setup.scale, Some(85));
        assert_eq!(metadata_setup.first_page_number, Some(3));
        assert!(metadata_setup.center_horizontally);
        assert!(metadata_setup.center_vertically);
    }

    #[test]
    fn ods_doc_properties_surface_through_workbook_metadata() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Meta"/></office:spreadsheet></office:body></office:document-content>"#;
        let meta = r#"<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0"><office:meta><dc:title>ODS Report</dc:title><dc:subject>Procurement</dc:subject><meta:initial-creator>rxls ods</meta:initial-creator><dc:creator>reviewer</dc:creator><meta:keyword>bid</meta:keyword><meta:keyword>ods</meta:keyword><dc:description>ODS public metadata</dc:description><meta:creation-date>2026-06-24T02:03:04Z</meta:creation-date><meta:user-defined meta:name="Company">ACME ODS</meta:user-defined></office:meta></office:document-meta>"#;

        let wb = Workbook::open(&ods_bytes_with_meta(content, meta)).unwrap();
        let metadata = wb.metadata();

        assert_eq!(metadata.properties.title.as_deref(), Some("ODS Report"));
        assert_eq!(metadata.properties.subject.as_deref(), Some("Procurement"));
        assert_eq!(metadata.properties.creator.as_deref(), Some("rxls ods"));
        assert_eq!(
            metadata.properties.last_modified_by.as_deref(),
            Some("reviewer")
        );
        assert_eq!(metadata.properties.keywords.as_deref(), Some("bid,ods"));
        assert_eq!(
            metadata.properties.description.as_deref(),
            Some("ODS public metadata")
        );
        assert_eq!(
            metadata.properties.created.as_deref(),
            Some("2026-06-24T02:03:04Z")
        );
        assert_eq!(metadata.properties.company.as_deref(), Some("ACME ODS"));
    }

    #[test]
    fn ods_settings_active_table_surfaces_workbook_metadata() {
        let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data"/><table:table table:name="Summary"/></office:spreadsheet></office:body></office:document-content>"#;
        let settings = r#"<?xml version="1.0" encoding="UTF-8"?><office:document-settings xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:config="urn:oasis:names:tc:opendocument:xmlns:config:1.0"><office:settings><config:config-item-set config:name="ooo:view-settings"><config:config-item-map-indexed config:name="Views"><config:config-item-map-entry><config:config-item-map-named config:name="Tables"><config:config-item-map-entry config:name="Data"/><config:config-item-map-entry config:name="Summary"/></config:config-item-map-named><config:config-item config:name="ActiveTable" config:type="string">Summary</config:config-item></config:config-item-map-entry></config:config-item-map-indexed></config:config-item-set></office:settings></office:document-settings>"#;

        let wb = Workbook::open(&ods_bytes_with_parts(
            content,
            &[("settings.xml", settings.as_bytes())],
        ))
        .unwrap();
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
    fn ods_settings_table_view_state_surfaces_sheet_view_metadata() {
        let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data"/><table:table table:name="Summary"/></office:spreadsheet></office:body></office:document-content>"#;
        let settings = r#"<?xml version="1.0" encoding="UTF-8"?><office:document-settings xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:config="urn:oasis:names:tc:opendocument:xmlns:config:1.0"><office:settings><config:config-item-set config:name="ooo:view-settings"><config:config-item-map-indexed config:name="Views"><config:config-item-map-entry><config:config-item-map-named config:name="Tables"><config:config-item-map-entry config:name="Data"><config:config-item config:name="HorizontalSplitMode" config:type="short">2</config:config-item><config:config-item config:name="VerticalSplitMode" config:type="short">2</config:config-item><config:config-item config:name="HorizontalSplitPosition" config:type="int">2</config:config-item><config:config-item config:name="VerticalSplitPosition" config:type="int">1</config:config-item><config:config-item config:name="PositionRight" config:type="int">2</config:config-item><config:config-item config:name="PositionBottom" config:type="int">1</config:config-item><config:config-item config:name="ZoomValue" config:type="short">125</config:config-item><config:config-item config:name="ShowGrid" config:type="boolean">false</config:config-item></config:config-item-map-entry><config:config-item-map-entry config:name="Summary"/></config:config-item-map-named><config:config-item config:name="HasColumnRowHeaders" config:type="boolean">false</config:config-item></config:config-item-map-entry></config:config-item-map-indexed></config:config-item-set></office:settings></office:document-settings>"#;

        let wb = Workbook::open(&ods_bytes_with_parts(
            content,
            &[("settings.xml", settings.as_bytes())],
        ))
        .unwrap();
        let data = wb.sheet_by_name("Data").unwrap();
        let summary = wb.sheet_by_name("Summary").unwrap();

        assert_eq!(
            data.sheet_view(),
            crate::SheetView {
                freeze: Some((1, 2)),
                hide_gridlines: true,
                zoom: Some(125),
                show_headers: Some(false),
                right_to_left: false,
            }
        );
        let metadata = wb.worksheet_metadata("Data").unwrap();
        assert_eq!(metadata.sheet_view.freeze, Some((1, 2)));
        assert!(metadata.sheet_view.hide_gridlines);
        assert_eq!(metadata.sheet_view.zoom, Some(125));
        assert_eq!(metadata.sheet_view.show_headers, Some(false));

        assert_eq!(summary.sheet_view().show_headers, Some(false));
        assert_eq!(summary.sheet_view().freeze, None);
        assert_eq!(summary.sheet_view().zoom, None);
        assert!(!summary.sheet_view().hide_gridlines);
    }

    #[test]
    fn ods_draw_image_surfaces_public_metadata() {
        const PNG_1X1: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52,
            0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0A, 0x49,
            0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0D, 0x0A, 0x2D, 0xB4, 0, 0,
            0, 0, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="Images"><table:table-row><table:table-cell office:value-type="string"><text:p>Logo</text:p></table:table-cell><table:table-cell><draw:frame draw:name="LogoFrame"><draw:image xlink:href="Pictures/logo.png" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/></draw:frame></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes_with_part(content, "Pictures/logo.png", PNG_1X1))
            .expect("ods");
        let images = wb.sheets[0].images();

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].format, crate::ImageFmt::Png);
        assert_eq!(images[0].data, PNG_1X1);
        assert_eq!(images[0].from, (0, 1));
        assert_eq!(images[0].to, None);
        let pictures = wb.pictures().expect("pictures");
        assert_eq!(pictures, vec![("png".to_string(), PNG_1X1.to_vec())]);
    }

    #[test]
    fn ods_image_part_reader_obeys_aggregate_budget() {
        let first = vec![1_u8; 8];
        let second = vec![2_u8; 8];
        let bytes = ods_bytes_with_parts(
            "",
            &[
                ("Pictures/first.png", &first),
                ("Pictures/second.png", &second),
            ],
        );
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();

        let images = read_image_parts_with_limits(&mut zip, 16, 12);

        assert_eq!(images.len(), 1);
        assert_eq!(images["Pictures/first.png"].1, first);
        assert!(!images.contains_key("Pictures/second.png"));
    }

    #[test]
    fn ods_named_ranges_surface_through_workbook_defined_names() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data"/><table:named-expressions><table:named-range table:name="TaxRate" table:cell-range-address="$Data.$B$2"/><table:named-range table:name="DataBlock" table:cell-range-address="$Data.$A$1:$Data.$B$2"/></table:named-expressions></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();

        assert_eq!(
            wb.defined_names(),
            &[
                ("TaxRate".to_string(), "Data!$B$2".to_string()),
                ("DataBlock".to_string(), "Data!$A$1:Data!$B$2".to_string()),
            ]
        );
        assert_eq!(wb.metadata().defined_names, wb.defined_names());
    }

    #[test]
    fn ods_database_range_surfaces_as_autofilter_metadata() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data"/><table:database-ranges><table:database-range table:name="Filter" table:target-range-address="$Data.$A$1:$Data.$C$10" table:display-filter-buttons="true"/></table:database-ranges></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();

        assert_eq!(wb.sheets[0].autofilter_range(), Some((0, 0, 9, 2)));
    }

    #[test]
    fn ods_unnamed_database_range_still_surfaces_as_autofilter_metadata() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data"/><table:database-ranges><table:database-range table:target-range-address="$Data.$A$1:$Data.$C$10"/></table:database-ranges></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();

        assert_eq!(wb.sheets[0].autofilter_range(), Some((0, 0, 9, 2)));
        assert!(wb.sheets[0].tables().is_empty());
    }

    #[test]
    fn ods_table_print_ranges_surface_as_page_setup_print_area() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data" table:print-ranges="$Data.$B$2:$Data.$D$9"/></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();

        assert_eq!(
            wb.sheets[0].page_setup().and_then(|setup| setup.print_area),
            Some((1, 1, 8, 3))
        );
    }

    #[test]
    fn ods_table_print_ranges_allow_quoted_sheet_names_with_spaces() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Data Sheet" table:print-ranges="'Data Sheet'.$B$2:'Data Sheet'.$D$9"/></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();

        assert_eq!(
            wb.sheets[0].page_setup().and_then(|setup| setup.print_area),
            Some((1, 1, 8, 3))
        );
    }

    #[test]
    fn ods_table_header_rows_surface_as_page_setup_repeat_rows() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Data"><table:table-header-rows><table:table-row><table:table-cell office:value-type="string"><text:p>Region</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p>Amount</text:p></table:table-cell></table:table-row></table:table-header-rows><table:table-row><table:table-cell office:value-type="float" office:value="42"><text:p>42</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();

        assert_eq!(
            wb.sheets[0]
                .page_setup()
                .and_then(|setup| setup.repeat_rows),
            Some((0, 1))
        );
        assert_eq!(wb.sheets[0].cells[2].row, 2);
    }

    #[test]
    fn ods_table_header_columns_surface_as_page_setup_repeat_cols() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Data"><table:table-header-columns><table:table-column/><table:table-column table:number-columns-repeated="2"/></table:table-header-columns><table:table-row><table:table-cell office:value-type="string"><text:p>Region</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Amount</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Owner</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Notes</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();

        assert_eq!(
            wb.sheets[0]
                .page_setup()
                .and_then(|setup| setup.repeat_cols),
            Some((0, 2))
        );
    }

    #[test]
    fn ods_database_range_surfaces_as_table_metadata() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Data"><table:table-row><table:table-cell office:value-type="string"><text:p>Item</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Amount</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p>Paper</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="42"><text:p>42</text:p></table:table-cell></table:table-row></table:table><table:database-ranges><table:database-range table:name="DataBlock" table:target-range-address="$Data.$A$1:$Data.$B$2"/></table:database-ranges></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let tables = wb.sheets[0].tables();

        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "DataBlock");
        assert_eq!(tables[0].range, (0, 0, 1, 1));
        assert_eq!(tables[0].columns, ["Item", "Amount"]);
    }

    #[test]
    fn ods_database_range_without_filter_buttons_keeps_table_without_autofilter() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Data"><table:table-row><table:table-cell office:value-type="string"><text:p>Item</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Amount</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p>Paper</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="42"><text:p>42</text:p></table:table-cell></table:table-row></table:table><table:database-ranges><table:database-range table:name="DataBlock" table:target-range-address="$Data.$A$1:$Data.$B$2" table:display-filter-buttons="false"/></table:database-ranges></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let sheet = &wb.sheets[0];
        let tables = sheet.tables();

        assert_eq!(sheet.autofilter_range(), None);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "DataBlock");
        assert_eq!(tables[0].range, (0, 0, 1, 1));
    }

    #[test]
    fn ods_content_validation_surfaces_as_data_validation_metadata() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:content-validations><table:content-validation table:name="PositiveAmount" table:condition="cell-content() &gt;= 0" table:allow-empty-cell="false"/></table:content-validations><table:table table:name="Data"><table:table-row><table:table-cell table:content-validation-name="PositiveAmount" office:value-type="float" office:value="5"><text:p>5</text:p></table:table-cell><table:table-cell table:content-validation-name="PositiveAmount" table:number-columns-repeated="2" office:value-type="float" office:value="7"><text:p>7</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let validations = wb.sheets[0].data_validations();

        assert_eq!(validations.len(), 2);
        assert_eq!(validations[0].sqref, (0, 0, 0, 0));
        assert_eq!(validations[1].sqref, (0, 1, 0, 2));
        for validation in validations {
            assert_eq!(validation.kind, crate::DvKind::Custom);
            assert_eq!(validation.operator, crate::DvOp::Between);
            assert_eq!(validation.formula1, "cell-content() >= 0");
            assert!(validation.formula2.is_none());
            assert!(!validation.allow_blank);
            assert!(!validation.show_input_message);
            assert!(!validation.show_error_message);
        }
    }

    #[test]
    fn ods_cell_annotation_surfaces_as_comment_metadata() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Notes"><table:table-row><table:table-cell office:value-type="string"><text:p>Reviewed</text:p><office:annotation><dc:creator>auditor</dc:creator><text:p>Check source total</text:p><text:p>before award.</text:p></office:annotation></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let comments = wb.sheets[0].comments();

        assert_eq!(
            wb.sheets[0].cell(0, 0),
            Some(&Cell::Text("Reviewed".to_string()))
        );
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].row, 0);
        assert_eq!(comments[0].col, 0);
        assert_eq!(comments[0].text, "Check source total\nbefore award.");
        assert_eq!(comments[0].author.as_deref(), Some("auditor"));
    }

    #[test]
    fn ods_repeated_annotation_only_rows_replicate_comment_metadata() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Notes"><table:table-row table:number-rows-repeated="2"><table:table-cell><office:annotation><text:p>row note</text:p></office:annotation></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let comments = wb.sheets[0].comments();

        assert_eq!(comments.len(), 2);
        assert_eq!((comments[0].row, comments[0].col), (0, 0));
        assert_eq!((comments[1].row, comments[1].col), (1, 0));
        assert!(wb.sheets[0].cells().next().is_none());
    }

    #[test]
    fn ods_dc_creator_fills_creator_when_initial_creator_is_absent() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Meta"/></office:spreadsheet></office:body></office:document-content>"#;
        let meta = r#"<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/"><office:meta><dc:creator>standalone author</dc:creator></office:meta></office:document-meta>"#;

        let wb = Workbook::open(&ods_bytes_with_meta(content, meta)).unwrap();
        let metadata = wb.metadata();

        assert_eq!(
            metadata.properties.creator.as_deref(),
            Some("standalone author")
        );
        assert_eq!(
            metadata.properties.last_modified_by.as_deref(),
            Some("standalone author")
        );
    }

    #[test]
    fn ods_self_closing_empty_table_is_preserved() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Empty"/></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();

        assert_eq!(wb.sheets.len(), 1);
        assert_eq!(wb.sheets[0].name, "Empty");
        assert_eq!(wb.sheets[0].cells().count(), 0);
    }

    #[test]
    fn repeated_valued_rows_are_replicated() {
        // A `number-rows-repeated` row carrying a value must be replicated, not
        // collapsed to one row + a skip.
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="r"><table:table-row table:number-rows-repeated="3"><table:table-cell office:value-type="float" office:value="5"><text:p>5</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let s = &wb.sheets[0];
        assert_eq!(s.cell(0, 0), Some(&Cell::Number(5.0)));
        assert_eq!(s.cell(1, 0), Some(&Cell::Number(5.0)));
        assert_eq!(s.cell(2, 0), Some(&Cell::Number(5.0)));
    }

    #[test]
    fn large_row_repeat_is_not_truncated_at_64k() {
        // A legitimate `number-rows-repeated` above the old 64k cap must replicate
        // up to the row grid (bounded by the text budget), not silently truncate.
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="big"><table:table-row table:number-rows-repeated="70000"><table:table-cell office:value-type="float" office:value="5"><text:p>5</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let s = &wb.sheets[0];
        assert_eq!(
            s.cell(69_999, 0),
            Some(&Cell::Number(5.0)),
            "row 69999 survives"
        );
        assert_eq!(s.cell(70_000, 0), None, "exactly 70000 rows (0..=69999)");
    }

    #[test]
    fn hostile_row_and_column_repeats_exhaust_budget_without_hanging() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="dos"><table:table-row table:number-rows-repeated="999999999"><table:table-cell table:number-columns-repeated="999999999" office:value-type="string"><text:p>X</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();

        assert!(wb.text_truncated);
        assert_eq!(wb.sheets[0].cell(0, 0), Some(&Cell::Text("X".into())));
    }

    #[test]
    fn merged_range_from_span() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="m"><table:table-row><table:table-cell table:number-columns-spanned="3" table:number-rows-spanned="1" office:value-type="string"><text:p>title</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        assert_eq!(wb.sheets[0].merged_ranges(), &[(0, 0, 0, 2)]);
    }

    #[test]
    fn ods_text_space_elements_preserve_significant_whitespace() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="spaces"><table:table-row><table:table-cell office:value-type="string"><text:p>Value <text:s/>With spaces</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p><text:s text:c="2"/>Value <text:s text:c="2"/>With after <text:s/></text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p>A<text:tab/>B<text:line-break/>C</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let sheet = &wb.sheets[0];

        assert_eq!(
            sheet.cell(0, 0),
            Some(&Cell::Text("Value  With spaces".into()))
        );
        assert_eq!(
            sheet.cell(1, 0),
            Some(&Cell::Text("  Value   With after  ".into()))
        );
        assert_eq!(sheet.cell(2, 0), Some(&Cell::Text("A\tB\nC".into())));
    }

    #[test]
    fn ods_text_a_hyperlink_preserves_label_and_target() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="links"><table:table-row><table:table-cell office:value-type="string"><text:p><text:a xlink:href="https://example.com/path?q=1">Example</text:a></text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let sheet = &wb.sheets[0];

        assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("Example".to_string())));
        assert_eq!(
            sheet.hyperlinks(),
            &[(0u32, 0u16, "https://example.com/path?q=1".to_string())]
        );
    }

    #[test]
    fn ods_repeated_hyperlink_cells_replicate_targets() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="links"><table:table-row table:number-rows-repeated="2"><table:table-cell table:number-columns-repeated="2" office:value-type="string"><text:p><text:a xlink:href="https://example.com/repeat">Repeat</text:a></text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let sheet = &wb.sheets[0];

        for row in 0..=1 {
            for col in 0..=1 {
                assert_eq!(
                    sheet.cell(row, col),
                    Some(&Cell::Text("Repeat".to_string()))
                );
            }
        }
        assert_eq!(
            sheet.hyperlinks(),
            &[
                (0u32, 0u16, "https://example.com/repeat".to_string()),
                (0u32, 1u16, "https://example.com/repeat".to_string()),
                (1u32, 0u16, "https://example.com/repeat".to_string()),
                (1u32, 1u16, "https://example.com/repeat".to_string()),
            ]
        );
    }

    #[test]
    fn ods_formula_source_and_cached_value_are_preserved() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="formulas"><table:table-row><table:table-cell office:value-type="float" office:value="2"><text:p>2</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="5"><text:p>5</text:p></table:table-cell><table:table-cell table:formula="of:=SUM([.A1:.B1])" office:value-type="float" office:value="7"><text:p>7</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let sheet = &wb.sheets[0];

        match sheet.cell(0, 2).expect("formula cell") {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "SUM([.A1:.B1])");
                assert_eq!(cached.as_ref(), &Cell::Number(7.0));
            }
            other => panic!("expected formula cell, got {other:?}"),
        }
        assert_eq!(sheet.formatted(0, 2), Some("7"));
        let formulas = wb.worksheet_formula("formulas").expect("formula range");
        assert_eq!(formulas.get_abs(0, 2), Some("SUM([.A1:.B1])"));
    }

    #[test]
    fn ods_formula_without_cached_value_is_preserved() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="formulas"><table:table-row><table:table-cell table:formula="of:=SUM([.A1:.A2])"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let sheet = &wb.sheets[0];

        match sheet.cell(0, 0).expect("formula cell") {
            Cell::Formula { formula, cached } => {
                assert_eq!(formula, "SUM([.A1:.A2])");
                assert_eq!(cached.as_ref(), &Cell::Text(String::new()));
            }
            other => panic!("expected formula cell, got {other:?}"),
        }
        let formulas = wb.worksheet_formula("formulas").expect("formula range");
        assert_eq!(formulas.get_abs(0, 0), Some("SUM([.A1:.A2])"));
    }
}
