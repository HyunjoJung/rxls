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

use quick_xml::events::{BytesRef, Event};
use quick_xml::{Reader, XmlVersion};

use crate::error::{Error, Result};
use crate::{
    Alignment, Border, BorderStyle, Cell, CellEntry, CellProtection, CellStyle, Color, Comment,
    DataValidation, DocProperties, DrawingAnchorBehavior, DrawingCrop, DrawingMetadata,
    DrawingObjectKind, DvKind, DvOp, Fill, Font, FormatPattern, FormatScript, HAlign,
    HeaderFooterKind, Image, ImageFmt, PageSetup, PrintLossKind, PrintMetadata, PrintPageOrder,
    Sheet, StyleFidelity, StyleLoss, StyleLossKind, Table, VAlign, Workbook,
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
const MAX_ODS_STYLES: usize = 65_536;
const MAX_ODS_STYLE_NAME: usize = 1_024;
const MAX_ODS_STYLE_DEPTH: usize = 64;
const MAX_ODS_DRAWINGS: usize = 16_384;
const MAX_ODS_DRAWING_TEXT: usize = 4_096;
const MAX_ODS_LAYOUT_ENTRIES: usize = 1 << 18;
const MAX_ODS_CLIP_POINTS: f64 = 1_000_000_000.0;

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
type TableStyles = HashMap<String, TableStyleOptions>;
type AutoFilters = HashMap<String, (u32, u16, u32, u16)>;
type ValidationRules = HashMap<String, DataValidation>;
type ImageParts = HashMap<String, (ImageFmt, Vec<u8>)>;

#[derive(Clone, Copy, Default)]
struct TableStyleOptions {
    visible: Option<bool>,
    tab_color: Option<Color>,
    right_to_left: Option<bool>,
    print_gridlines: bool,
    print_headings: bool,
    landscape: Option<bool>,
    scale: Option<u16>,
    first_page_number: Option<u16>,
    center_horizontally: bool,
    center_vertically: bool,
    margins: Option<(f64, f64, f64, f64, f64, f64)>,
    paper_size: Option<u16>,
    page_order: Option<PrintPageOrder>,
    page_order_invalid: bool,
    print_options_seen: bool,
    centering_seen: bool,
    unsupported_print_property: bool,
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
    margins: Option<(f64, f64, f64, f64, f64, f64)>,
    paper_size: Option<u16>,
    page_order: Option<PrintPageOrder>,
    page_order_invalid: bool,
    print_options_seen: bool,
    centering_seen: bool,
    unsupported_print_property: bool,
}

#[derive(Clone, Default)]
struct OdsStyleProps {
    font_name: Option<String>,
    font_size_pt: Option<u16>,
    font_color: Option<Color>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    strikethrough: Option<bool>,
    script: Option<FormatScript>,
    fill_color: Option<Color>,
    fill_transparent: bool,
    border_left: Option<(BorderStyle, Option<Color>)>,
    border_right: Option<(BorderStyle, Option<Color>)>,
    border_top: Option<(BorderStyle, Option<Color>)>,
    border_bottom: Option<(BorderStyle, Option<Color>)>,
    num_fmt: Option<String>,
    horizontal: Option<HAlign>,
    vertical: Option<VAlign>,
    wrap: Option<bool>,
    rotation: Option<i16>,
    indent: Option<u8>,
    shrink_to_fit: Option<bool>,
    locked: Option<bool>,
    hidden_formula: Option<bool>,
    row_height_pt: Option<f32>,
    col_width_chars: Option<f32>,
    col_width_points: Option<f32>,
    hidden: Option<bool>,
    break_before_page: Option<bool>,
    break_after_page: Option<bool>,
    break_invalid: bool,
    clip: Option<OdsClip>,
}

#[derive(Clone, Copy)]
enum OdsClip {
    Auto,
    /// Top, right, bottom, and left crop distances in points.
    Rect([f64; 4]),
}

impl OdsStyleProps {
    fn overlay(&mut self, other: &Self) {
        macro_rules! overlay {
            ($($field:ident),+ $(,)?) => {$(
                if other.$field.is_some() {
                    self.$field = other.$field.clone();
                }
            )+};
        }
        overlay!(
            font_name,
            font_size_pt,
            font_color,
            bold,
            italic,
            underline,
            strikethrough,
            script,
            border_left,
            border_right,
            border_top,
            border_bottom,
            num_fmt,
            horizontal,
            vertical,
            wrap,
            rotation,
            indent,
            shrink_to_fit,
            locked,
            hidden_formula,
            row_height_pt,
            col_width_chars,
            col_width_points,
            hidden,
            break_before_page,
            break_after_page,
            clip,
        );
        if other.fill_transparent {
            self.fill_color = None;
            self.fill_transparent = true;
        } else if other.fill_color.is_some() {
            self.fill_color = other.fill_color;
            self.fill_transparent = false;
        }
        if other.break_invalid {
            self.break_invalid = true;
        }
    }

    fn to_cell_style(&self) -> CellStyle {
        let has_font = self.font_name.is_some()
            || self.font_size_pt.is_some()
            || self.font_color.is_some()
            || self.bold.is_some()
            || self.italic.is_some()
            || self.underline.is_some()
            || self.strikethrough.is_some()
            || self.script.is_some();
        let font = has_font.then(|| Font {
            name: self.font_name.clone(),
            size_pt: self.font_size_pt,
            color: self.font_color,
            bold: self.bold.unwrap_or(false),
            italic: self.italic.unwrap_or(false),
            underline: self.underline.unwrap_or(false),
            strikethrough: self.strikethrough.unwrap_or(false),
            script: self.script.unwrap_or(FormatScript::None),
        });
        let has_border = self.border_left.is_some()
            || self.border_right.is_some()
            || self.border_top.is_some()
            || self.border_bottom.is_some();
        let border = has_border.then(|| {
            let mut border = Border::default();
            if let Some((style, color)) = self.border_left {
                border.left = style;
                border.left_color = color;
            }
            if let Some((style, color)) = self.border_right {
                border.right = style;
                border.right_color = color;
            }
            if let Some((style, color)) = self.border_top {
                border.top = style;
                border.top_color = color;
            }
            if let Some((style, color)) = self.border_bottom {
                border.bottom = style;
                border.bottom_color = color;
            }
            border
        });
        let has_alignment = self.horizontal.is_some()
            || self.vertical.is_some()
            || self.wrap.is_some()
            || self.rotation.is_some()
            || self.indent.is_some()
            || self.shrink_to_fit.is_some();
        let align = has_alignment.then(|| Alignment {
            horizontal: self.horizontal,
            vertical: self.vertical,
            wrap: self.wrap.unwrap_or(false),
            rotation: self.rotation.unwrap_or(0),
            indent: self.indent.unwrap_or(0),
            shrink_to_fit: self.shrink_to_fit.unwrap_or(false),
        });
        let protection =
            (self.locked.is_some() || self.hidden_formula.is_some()).then(|| CellProtection {
                locked: self.locked,
                hidden: self.hidden_formula.unwrap_or(false),
            });
        let pattern_fill = self.fill_color.map(|color| Fill {
            pattern: FormatPattern::Solid,
            foreground: Some(color),
            background: Some(color),
        });
        CellStyle {
            font,
            fill: self.fill_color,
            pattern_fill,
            border,
            num_fmt: self.num_fmt.clone(),
            align,
            protection,
        }
    }
}

#[derive(Clone, Default)]
struct OdsRawStyle {
    parent: Option<String>,
    data_style: Option<String>,
    props: OdsStyleProps,
}

#[derive(Clone, Default)]
struct OdsResolvedStyle {
    props: OdsStyleProps,
    data_style: Option<String>,
}

#[derive(Default)]
struct OdsResolvedStyles {
    table_styles: TableStyles,
    cell: HashMap<String, OdsStyleProps>,
    row: HashMap<String, OdsStyleProps>,
    column: HashMap<String, OdsStyleProps>,
    text: HashMap<String, OdsStyleProps>,
    paragraph: HashMap<String, OdsStyleProps>,
    graphic: HashMap<String, OdsStyleProps>,
    default_cell: Option<OdsStyleProps>,
    default_row: Option<OdsStyleProps>,
    default_column: Option<OdsStyleProps>,
    default_text: Option<OdsStyleProps>,
    default_paragraph: Option<OdsStyleProps>,
    default_graphic: Option<OdsStyleProps>,
    losses: Vec<StyleLoss>,
    has_source_styles: bool,
    table_print_metadata: HashMap<String, PrintMetadata>,
}

#[derive(Default)]
struct OdsStyleDefinitions {
    table_styles: TableStyles,
    table_master_pages: HashMap<String, String>,
    master_page_layouts: HashMap<String, String>,
    master_page_print_metadata: HashMap<String, PrintMetadata>,
    page_layout_options: HashMap<String, PageLayoutOptions>,
    raw_styles: HashMap<(String, String), OdsRawStyle>,
    default_styles: HashMap<String, OdsStyleProps>,
    number_formats: HashMap<String, String>,
    losses: Vec<StyleLoss>,
    has_source_styles: bool,
}

fn add_ods_style_loss(losses: &mut Vec<StyleLoss>, kind: StyleLossKind, count: u32) {
    if count == 0 {
        return;
    }
    if let Some(loss) = losses.iter_mut().find(|loss| loss.kind == kind) {
        loss.occurrences = loss.occurrences.saturating_add(count);
    } else {
        losses.push(StyleLoss {
            kind,
            occurrences: count,
        });
    }
}

fn merge_table_style(base: TableStyleOptions, child: TableStyleOptions) -> TableStyleOptions {
    TableStyleOptions {
        visible: child.visible.or(base.visible),
        tab_color: child.tab_color.or(base.tab_color),
        right_to_left: child.right_to_left.or(base.right_to_left),
        print_gridlines: child.print_gridlines || base.print_gridlines,
        print_headings: child.print_headings || base.print_headings,
        landscape: child.landscape.or(base.landscape),
        scale: child.scale.or(base.scale),
        first_page_number: child.first_page_number.or(base.first_page_number),
        center_horizontally: child.center_horizontally || base.center_horizontally,
        center_vertically: child.center_vertically || base.center_vertically,
        margins: child.margins.or(base.margins),
        paper_size: child.paper_size.or(base.paper_size),
        page_order: child.page_order.or(base.page_order),
        page_order_invalid: child.page_order_invalid || base.page_order_invalid,
        print_options_seen: child.print_options_seen || base.print_options_seen,
        centering_seen: child.centering_seen || base.centering_seen,
        unsupported_print_property: child.unsupported_print_property
            || base.unsupported_print_property,
    }
}

fn resolve_ods_table_style(
    name: &str,
    definitions: &OdsStyleDefinitions,
    cache: &mut HashMap<String, TableStyleOptions>,
    visiting: &mut Vec<String>,
    losses: &mut Vec<StyleLoss>,
    depth: usize,
) -> TableStyleOptions {
    if let Some(style) = cache.get(name) {
        return *style;
    }
    if depth >= MAX_ODS_STYLE_DEPTH || visiting.iter().any(|item| item == name) {
        add_ods_style_loss(losses, StyleLossKind::InheritanceCycle, 1);
        return definitions
            .table_styles
            .get(name)
            .copied()
            .unwrap_or_default();
    }
    visiting.push(name.to_string());
    let mut style = TableStyleOptions::default();
    if let Some(raw) = definitions
        .raw_styles
        .get(&("table".to_string(), name.to_string()))
    {
        if let Some(parent) = raw.parent.as_deref() {
            if definitions
                .raw_styles
                .contains_key(&("table".to_string(), parent.to_string()))
            {
                style = resolve_ods_table_style(
                    parent,
                    definitions,
                    cache,
                    visiting,
                    losses,
                    depth + 1,
                );
            } else {
                add_ods_style_loss(losses, StyleLossKind::MissingReference, 1);
            }
        }
    }
    style = merge_table_style(
        style,
        definitions
            .table_styles
            .get(name)
            .copied()
            .unwrap_or_default(),
    );
    visiting.pop();
    cache.insert(name.to_string(), style);
    style
}

fn resolve_ods_style(
    family: &str,
    name: &str,
    definitions: &OdsStyleDefinitions,
    cache: &mut HashMap<(String, String), OdsResolvedStyle>,
    visiting: &mut Vec<(String, String)>,
    losses: &mut Vec<StyleLoss>,
    depth: usize,
) -> OdsResolvedStyle {
    let key = (family.to_string(), name.to_string());
    if let Some(style) = cache.get(&key) {
        return style.clone();
    }
    if depth >= MAX_ODS_STYLE_DEPTH || visiting.contains(&key) {
        add_ods_style_loss(losses, StyleLossKind::InheritanceCycle, 1);
        return OdsResolvedStyle {
            props: definitions
                .default_styles
                .get(family)
                .cloned()
                .unwrap_or_default(),
            data_style: None,
        };
    }
    let Some(raw) = definitions.raw_styles.get(&key) else {
        add_ods_style_loss(losses, StyleLossKind::MissingReference, 1);
        return OdsResolvedStyle::default();
    };
    visiting.push(key.clone());
    let mut resolved = OdsResolvedStyle {
        props: definitions
            .default_styles
            .get(family)
            .cloned()
            .unwrap_or_default(),
        data_style: None,
    };
    if let Some(parent) = raw.parent.as_deref() {
        resolved = resolve_ods_style(
            family,
            parent,
            definitions,
            cache,
            visiting,
            losses,
            depth + 1,
        );
    }
    resolved.props.overlay(&raw.props);
    if raw.data_style.is_some() {
        resolved.data_style.clone_from(&raw.data_style);
    }
    if let Some(format_name) = resolved.data_style.as_deref() {
        if let Some(format) = definitions.number_formats.get(format_name) {
            resolved.props.num_fmt = Some(format.clone());
        } else {
            add_ods_style_loss(losses, StyleLossKind::MissingReference, 1);
        }
    }
    visiting.pop();
    cache.insert(key, resolved.clone());
    resolved
}

impl OdsStyleDefinitions {
    fn into_resolved(mut self) -> OdsResolvedStyles {
        for (style, master_page) in &self.table_master_pages {
            let Some(page_layout) = self.master_page_layouts.get(master_page) else {
                add_ods_style_loss(&mut self.losses, StyleLossKind::MissingReference, 1);
                continue;
            };
            let Some(page_layout) = self.page_layout_options.get(page_layout) else {
                add_ods_style_loss(&mut self.losses, StyleLossKind::MissingReference, 1);
                continue;
            };
            let entry = self.table_styles.entry(style.clone()).or_default();
            entry.print_gridlines = page_layout.gridlines;
            entry.print_headings = page_layout.headings;
            entry.landscape = page_layout.landscape;
            entry.scale = page_layout.scale;
            entry.first_page_number = page_layout.first_page_number;
            entry.center_horizontally = page_layout.center_horizontally;
            entry.center_vertically = page_layout.center_vertically;
            entry.margins = page_layout.margins;
            entry.paper_size = page_layout.paper_size;
            entry.page_order = page_layout.page_order;
            entry.page_order_invalid = page_layout.page_order_invalid;
            entry.print_options_seen = page_layout.print_options_seen;
            entry.centering_seen = page_layout.centering_seen;
            entry.unsupported_print_property = page_layout.unsupported_print_property;
        }

        let keys: Vec<(String, String)> = self.raw_styles.keys().cloned().collect();
        let mut cache = HashMap::new();
        let mut visiting = Vec::new();
        let mut resolved = OdsResolvedStyles {
            default_cell: self.default_styles.get("table-cell").cloned(),
            default_row: self.default_styles.get("table-row").cloned(),
            default_column: self.default_styles.get("table-column").cloned(),
            default_text: self.default_styles.get("text").cloned(),
            default_paragraph: self.default_styles.get("paragraph").cloned(),
            default_graphic: self.default_styles.get("graphic").cloned(),
            losses: self.losses.clone(),
            has_source_styles: self.has_source_styles,
            ..Default::default()
        };
        let mut table_cache = HashMap::new();
        let mut table_visiting = Vec::new();
        let table_names: Vec<String> = self.table_styles.keys().cloned().collect();
        for name in table_names {
            let style = resolve_ods_table_style(
                &name,
                &self,
                &mut table_cache,
                &mut table_visiting,
                &mut resolved.losses,
                0,
            );
            let mut print_metadata = inherited_table_master_page(&name, &self, 0)
                .and_then(|master_page| self.master_page_print_metadata.get(&master_page).cloned())
                .unwrap_or_default();
            if style.print_options_seen {
                print_metadata.set_print_gridlines(style.print_gridlines);
                print_metadata.set_print_headings(style.print_headings);
            }
            if style.centering_seen {
                print_metadata.set_center_horizontally(style.center_horizontally);
                print_metadata.set_center_vertically(style.center_vertically);
            }
            if let Some(order) = style.page_order {
                print_metadata.set_page_order(order);
            }
            if style.page_order_invalid {
                print_metadata.add_loss(PrintLossKind::UnsupportedProperty);
            }
            if style.unsupported_print_property {
                print_metadata.add_loss(PrintLossKind::UnsupportedProperty);
            }
            if style.landscape.is_some()
                || style.scale.is_some()
                || style.first_page_number.is_some()
                || style.margins.is_some()
                || style.paper_size.is_some()
            {
                print_metadata.mark_source();
            }
            resolved
                .table_print_metadata
                .insert(name.clone(), print_metadata);
            resolved.table_styles.insert(name, style);
        }
        for (family, name) in keys {
            let style = resolve_ods_style(
                &family,
                &name,
                &self,
                &mut cache,
                &mut visiting,
                &mut resolved.losses,
                0,
            );
            match family.as_str() {
                "table-cell" => {
                    resolved.cell.insert(name, style.props);
                }
                "table-row" => {
                    resolved.row.insert(name, style.props);
                }
                "table-column" => {
                    resolved.column.insert(name, style.props);
                }
                "text" => {
                    resolved.text.insert(name, style.props);
                }
                "paragraph" => {
                    resolved.paragraph.insert(name, style.props);
                }
                "graphic" => {
                    resolved.graphic.insert(name, style.props);
                }
                "table" => {
                    // Table visibility/page metadata is represented separately;
                    // cell-like properties are not meaningful for this family.
                }
                _ => {
                    add_ods_style_loss(&mut resolved.losses, StyleLossKind::UnsupportedProperty, 1)
                }
            }
        }
        resolved
    }
}

fn inherited_table_master_page(
    name: &str,
    definitions: &OdsStyleDefinitions,
    depth: usize,
) -> Option<String> {
    if depth >= MAX_ODS_STYLE_DEPTH {
        return None;
    }
    if let Some(master_page) = definitions.table_master_pages.get(name) {
        return Some(master_page.clone());
    }
    definitions
        .raw_styles
        .get(&("table".to_string(), name.to_string()))
        .and_then(|style| style.parent.as_deref())
        .and_then(|parent| inherited_table_master_page(parent, definitions, depth + 1))
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
    style_name: Option<String>,
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
        style_name: attr(e, b"style-name").filter(|name| name.len() <= MAX_ODS_STYLE_NAME),
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
    if let Some(right_to_left) = attr(e, b"writing-mode").and_then(|value| match value.as_str() {
        "rl-tb" => Some(true),
        "lr-tb" => Some(false),
        _ => None,
    }) {
        styles.entry(name.clone()).or_default().right_to_left = Some(right_to_left);
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

fn parse_ods_signed_length_points(value: &str) -> Option<f64> {
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit() && character != '.' && character != '-')
        .unwrap_or(value.len());
    let number = value.get(..split)?.parse::<f64>().ok()?;
    if !number.is_finite() {
        return None;
    }
    let unit = value.get(split..)?.trim();
    Some(match unit {
        "pt" => number,
        "pc" => number * 12.0,
        "in" => number * 72.0,
        "cm" => number * 72.0 / 2.54,
        "mm" => number * 72.0 / 25.4,
        "px" => number * 0.75,
        _ => return None,
    })
}

fn parse_ods_length_points(value: &str) -> Option<f64> {
    parse_ods_signed_length_points(value).filter(|value| *value >= 0.0)
}

fn parse_ods_length_inches(value: &str) -> Option<f64> {
    parse_ods_length_points(value).map(|points| points / 72.0)
}

fn paper_size_from_inches(width: f64, height: f64) -> Option<u16> {
    let (short, long) = if width <= height {
        (width, height)
    } else {
        (height, width)
    };
    if (short - 8.27).abs() < 0.15 && (long - 11.69).abs() < 0.15 {
        Some(9) // A4
    } else if (short - 8.5).abs() < 0.15 && (long - 11.0).abs() < 0.15 {
        Some(1) // Letter
    } else if (short - 8.5).abs() < 0.15 && (long - 14.0).abs() < 0.15 {
        Some(5) // Legal
    } else {
        None
    }
}

fn page_layout_options(e: &quick_xml::events::BytesStart<'_>) -> Option<PageLayoutOptions> {
    let mut options = PageLayoutOptions::default();
    let mut found = false;
    if let Some(print) = attr(e, b"print") {
        found = true;
        options.print_options_seen = true;
        for value in print.split_ascii_whitespace() {
            match value {
                "grid" => options.gridlines = true,
                "headers" => options.headings = true,
                _ => options.unsupported_print_property = true,
            }
        }
    }
    if let Some(order) = attr(e, b"print-page-order") {
        found = true;
        match order.as_str() {
            "ttb" => options.page_order = Some(PrintPageOrder::DownThenOver),
            "ltr" => options.page_order = Some(PrintPageOrder::OverThenDown),
            _ => options.page_order_invalid = true,
        }
    }
    if let Some(orientation) = attr(e, b"print-orientation") {
        found = true;
        if !matches!(orientation.as_str(), "portrait" | "landscape") {
            options.unsupported_print_property = true;
        }
        options.landscape = Some(orientation.eq_ignore_ascii_case("landscape"));
    }
    if let Some(value) = attr(e, b"scale-to") {
        found = true;
        match parse_ods_percentage(&value) {
            Some(scale) => options.scale = Some(scale),
            None => options.unsupported_print_property = true,
        }
    }
    if let Some(value) = attr(e, b"first-page-number") {
        found = true;
        match parse_positive_u16(&value) {
            Some(first_page) => options.first_page_number = Some(first_page),
            None => options.unsupported_print_property = true,
        }
    }
    if let Some(table_centering) = attr(e, b"table-centering") {
        found = true;
        options.centering_seen = true;
        match table_centering.as_str() {
            "horizontal" => options.center_horizontally = true,
            "vertical" => options.center_vertically = true,
            "both" => {
                options.center_horizontally = true;
                options.center_vertically = true;
            }
            _ => options.unsupported_print_property = true,
        }
    }
    let all_margin = attr(e, b"margin").and_then(|value| parse_ods_length_inches(&value));
    let left = attr(e, b"margin-left")
        .and_then(|value| parse_ods_length_inches(&value))
        .or(all_margin);
    let right = attr(e, b"margin-right")
        .and_then(|value| parse_ods_length_inches(&value))
        .or(all_margin);
    let top = attr(e, b"margin-top")
        .and_then(|value| parse_ods_length_inches(&value))
        .or(all_margin);
    let bottom = attr(e, b"margin-bottom")
        .and_then(|value| parse_ods_length_inches(&value))
        .or(all_margin);
    if [left, right, top, bottom].iter().any(Option::is_some) {
        found = true;
        options.margins = Some((
            left.unwrap_or(0.0),
            right.unwrap_or(0.0),
            top.unwrap_or(0.0),
            bottom.unwrap_or(0.0),
            0.0,
            0.0,
        ));
    }
    if let (Some(width), Some(height)) = (
        attr(e, b"page-width").and_then(|value| parse_ods_length_inches(&value)),
        attr(e, b"page-height").and_then(|value| parse_ods_length_inches(&value)),
    ) {
        found = true;
        options.paper_size = paper_size_from_inches(width, height);
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

fn ods_border(value: &str, losses: &mut Vec<StyleLoss>) -> Option<(BorderStyle, Option<Color>)> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("none") || value.eq_ignore_ascii_case("hidden") {
        return Some((BorderStyle::None, None));
    }
    let mut width = 0.75;
    let mut saw_width = false;
    let mut style = None;
    let mut color = None;
    for part in value.split_ascii_whitespace() {
        if let Some(points) = parse_ods_length_points(part) {
            width = points;
            saw_width = true;
        } else if part.eq_ignore_ascii_case("solid") {
            style = Some(BorderStyle::Thin);
        } else if part.eq_ignore_ascii_case("double") {
            style = Some(BorderStyle::Double);
        } else if matches!(
            part.to_ascii_lowercase().as_str(),
            "dotted" | "dashed" | "groove" | "ridge" | "inset" | "outset"
        ) {
            style = Some(BorderStyle::Thin);
            add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
        } else if let Some(parsed) = parse_ods_color(part) {
            color = Some(parsed);
        } else {
            add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
        }
    }
    let Some(mut style) = style else {
        // A border shorthand without a line style is malformed; do not invent
        // a visible edge from an arbitrary token.
        add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
        return None;
    };
    if style != BorderStyle::Double && (saw_width || width != 0.75) {
        style = if width >= 2.0 {
            BorderStyle::Thick
        } else if width >= 1.25 {
            BorderStyle::Medium
        } else {
            BorderStyle::Thin
        };
    }
    Some((style, color))
}

fn ods_bool(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true")
}

fn ods_bool_value(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

fn parse_ods_clip_length_points(value: &str) -> Option<f64> {
    if value.eq_ignore_ascii_case("auto") {
        return Some(0.0);
    }
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(value.len());
    let number = value.get(..split)?.parse::<f64>().ok()?;
    if !number.is_finite() || !(0.0..=MAX_ODS_CLIP_POINTS).contains(&number) {
        return None;
    }
    let points = match value.get(split..)?.trim().to_ascii_lowercase().as_str() {
        "pt" => number,
        "pc" => number * 12.0,
        "in" => number * 72.0,
        "cm" => number * 72.0 / 2.54,
        "mm" => number * 72.0 / 25.4,
        _ => return None,
    };
    (points.is_finite() && points <= MAX_ODS_CLIP_POINTS).then_some(points)
}

fn parse_ods_clip(value: &str) -> Option<OdsClip> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("auto") {
        return Some(OdsClip::Auto);
    }
    let body = value.strip_prefix("rect(")?.strip_suffix(')')?;
    let values = body
        .split(|character: char| character == ',' || character.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .map(parse_ods_clip_length_points)
        .collect::<Option<Vec<_>>>()?;
    let values: [f64; 4] = values.try_into().ok()?;
    Some(OdsClip::Rect(values))
}

fn apply_ods_style_properties(
    element: &[u8],
    e: &quick_xml::events::BytesStart<'_>,
    props: &mut OdsStyleProps,
    losses: &mut Vec<StyleLoss>,
) {
    match element {
        b"text-properties" => {
            let font_name = attr(e, b"font-name")
                .or_else(|| attr(e, b"font-family"))
                .map(|name| name.trim_matches(['\'', '"']).to_string());
            if font_name
                .as_ref()
                .is_some_and(|name| name.len() > MAX_ODS_STYLE_NAME)
            {
                add_ods_style_loss(losses, StyleLossKind::LimitExceeded, 1);
            } else if font_name.is_some() {
                props.font_name = font_name;
            }
            if let Some(size) = attr(e, b"font-size") {
                if let Some(points) = parse_ods_length_points(&size) {
                    if points.fract().abs() > f64::EPSILON {
                        add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
                    }
                    props.font_size_pt =
                        Some(points.round().clamp(1.0, f64::from(u16::MAX)) as u16);
                } else {
                    add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
                }
            }
            if let Some(color) = attr(e, b"color") {
                if color != "transparent" {
                    match parse_ods_color(&color) {
                        Some(color) => props.font_color = Some(color),
                        None => add_ods_style_loss(losses, StyleLossKind::UnresolvedColor, 1),
                    }
                }
            }
            if let Some(weight) = attr(e, b"font-weight") {
                props.bold = Some(
                    weight.eq_ignore_ascii_case("bold")
                        || weight.parse::<u16>().is_ok_and(|weight| weight >= 600),
                );
            }
            if let Some(style) = attr(e, b"font-style") {
                props.italic = Some(
                    style.eq_ignore_ascii_case("italic") || style.eq_ignore_ascii_case("oblique"),
                );
            }
            if let Some(underline) = attr(e, b"text-underline-style") {
                props.underline = Some(!underline.eq_ignore_ascii_case("none"));
            }
            if let Some(strike) = attr(e, b"text-line-through-style") {
                props.strikethrough = Some(!strike.eq_ignore_ascii_case("none"));
            }
            if let Some(position) = attr(e, b"text-position") {
                props.script = Some(
                    if position.starts_with("sub") || position.starts_with('-') {
                        FormatScript::Subscript
                    } else if position.starts_with("super")
                        || position
                            .split_ascii_whitespace()
                            .next()
                            .and_then(|value| value.trim_end_matches('%').parse::<i32>().ok())
                            .is_some_and(|value| value > 0)
                    {
                        FormatScript::Superscript
                    } else {
                        FormatScript::None
                    },
                );
            }
        }
        b"paragraph-properties" => {
            if let Some(alignment) = attr(e, b"text-align") {
                props.horizontal = match alignment.as_str() {
                    "left" | "start" => Some(HAlign::Left),
                    "center" => Some(HAlign::Center),
                    "right" | "end" => Some(HAlign::Right),
                    "justify" => {
                        add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
                        Some(HAlign::Left)
                    }
                    _ => props.horizontal,
                };
            }
            if let Some(indent) =
                attr(e, b"margin-left").and_then(|value| parse_ods_length_points(&value))
            {
                props.indent = Some((indent / 5.25).round().clamp(0.0, 250.0) as u8);
            }
        }
        b"table-cell-properties" => {
            if let Some(background) = attr(e, b"background-color") {
                if background == "transparent" {
                    props.fill_color = None;
                    props.fill_transparent = true;
                } else {
                    match parse_ods_color(&background) {
                        Some(color) => {
                            props.fill_color = Some(color);
                            props.fill_transparent = false;
                        }
                        None => add_ods_style_loss(losses, StyleLossKind::UnresolvedColor, 1),
                    }
                }
            }
            if let Some(value) = attr(e, b"vertical-align") {
                match value.as_str() {
                    "top" => props.vertical = Some(VAlign::Top),
                    "middle" | "center" => props.vertical = Some(VAlign::Middle),
                    "bottom" => props.vertical = Some(VAlign::Bottom),
                    _ => add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1),
                }
            }
            if let Some(wrap) = attr(e, b"wrap-option") {
                match wrap.as_str() {
                    "wrap" => props.wrap = Some(true),
                    "no-wrap" => props.wrap = Some(false),
                    _ => add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1),
                }
            }
            if let Some(rotation) =
                attr(e, b"rotation-angle").and_then(|value| value.parse::<f64>().ok())
            {
                let normalized = rotation.rem_euclid(360.0);
                let representable = if normalized <= 90.0 {
                    Some(normalized)
                } else if normalized >= 270.0 {
                    Some(normalized - 360.0)
                } else {
                    add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
                    None
                };
                if let Some(representable) = representable {
                    if representable.fract().abs() > f64::EPSILON {
                        add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
                    }
                    props.rotation = Some(representable.round().clamp(-90.0, 90.0) as i16);
                }
            } else if attr(e, b"rotation-angle").is_some() {
                add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
            }
            if let Some(shrink) = attr(e, b"shrink-to-fit") {
                props.shrink_to_fit = Some(ods_bool(&shrink));
            }
            if let Some(protect) = attr(e, b"cell-protect") {
                props.locked = Some(protect != "none");
                props.hidden_formula = Some(protect.contains("formula-hidden"));
            }
            if let Some(border) = attr(e, b"border").and_then(|value| ods_border(&value, losses)) {
                props.border_left = Some(border);
                props.border_right = Some(border);
                props.border_top = Some(border);
                props.border_bottom = Some(border);
            }
            if let Some(border) =
                attr(e, b"border-left").and_then(|value| ods_border(&value, losses))
            {
                props.border_left = Some(border);
            }
            if let Some(border) =
                attr(e, b"border-right").and_then(|value| ods_border(&value, losses))
            {
                props.border_right = Some(border);
            }
            if let Some(border) =
                attr(e, b"border-top").and_then(|value| ods_border(&value, losses))
            {
                props.border_top = Some(border);
            }
            if let Some(border) =
                attr(e, b"border-bottom").and_then(|value| ods_border(&value, losses))
            {
                props.border_bottom = Some(border);
            }
            let unsupported_visible = |name: &[u8]| {
                attr(e, name).is_some_and(|value| {
                    !value.is_empty()
                        && !value.eq_ignore_ascii_case("none")
                        && !value.eq_ignore_ascii_case("hidden")
                })
            };
            if unsupported_visible(b"shadow")
                || unsupported_visible(b"diagonal-bl-tr")
                || unsupported_visible(b"diagonal-tl-br")
            {
                add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
            }
        }
        b"table-row-properties" => {
            if let Some(value) = attr(e, b"row-height") {
                if let Some(height) = parse_ods_length_points(&value) {
                    props.row_height_pt = Some(height.clamp(0.0, f64::from(f32::MAX)) as f32);
                } else {
                    add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
                }
            }
            if let Some(display) = attr(e, b"display") {
                props.hidden = Some(!ods_bool(&display));
            }
            apply_ods_page_break_properties(e, props, losses);
        }
        b"table-column-properties" => {
            if let Some(value) = attr(e, b"column-width") {
                if let Some(width) = parse_ods_length_points(&value) {
                    props.col_width_points = Some(width.clamp(0.0, f64::from(f32::MAX)) as f32);
                    // The public model stores Excel-compatible character units.
                    props.col_width_chars = Some((width / 5.25).clamp(0.0, 255.0) as f32);
                } else {
                    add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
                }
            }
            if let Some(display) = attr(e, b"display") {
                props.hidden = Some(!ods_bool(&display));
            }
            apply_ods_page_break_properties(e, props, losses);
        }
        b"graphic-properties" => {
            if let Some(clip) = attr(e, b"clip") {
                match parse_ods_clip(&clip) {
                    Some(clip) => props.clip = Some(clip),
                    None => add_ods_style_loss(losses, StyleLossKind::DrawingMetadataPartial, 1),
                }
            }
        }
        _ => {}
    }
}

fn apply_ods_page_break_properties(
    e: &quick_xml::events::BytesStart<'_>,
    props: &mut OdsStyleProps,
    losses: &mut Vec<StyleLoss>,
) {
    for (key, target) in [
        (b"break-before".as_slice(), &mut props.break_before_page),
        (b"break-after".as_slice(), &mut props.break_after_page),
    ] {
        if let Some(value) = attr(e, key) {
            match value.as_str() {
                "page" => *target = Some(true),
                "auto" => *target = Some(false),
                _ => {
                    props.break_invalid = true;
                    add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OdsNumberStyleKind {
    Number,
    Currency,
    Percentage,
    Date,
    Time,
    Boolean,
    Text,
}

fn number_pattern(e: &quick_xml::events::BytesStart<'_>, losses: &mut Vec<StyleLoss>) -> String {
    let decimals = attr(e, b"decimal-places")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
        .min(30);
    let min_decimals = attr(e, b"min-decimal-places")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(decimals)
        .min(decimals);
    let min_integer = attr(e, b"min-integer-digits")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 30);
    let grouped = attr(e, b"grouping").as_deref().is_some_and(ods_bool);
    let mut pattern = if grouped {
        "#,##".to_string()
    } else {
        String::new()
    };
    pattern.push_str(&"0".repeat(min_integer));
    if decimals > 0 {
        pattern.push('.');
        pattern.push_str(&"0".repeat(min_decimals));
        pattern.push_str(&"#".repeat(decimals - min_decimals));
    }
    if attr(e, b"decimal-replacement").is_some() || attr(e, b"display-factor").is_some() {
        add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
    }
    pattern
}

fn scientific_pattern(
    e: &quick_xml::events::BytesStart<'_>,
    losses: &mut Vec<StyleLoss>,
) -> String {
    let mut pattern = number_pattern(e, losses);
    pattern.push('E');
    match attr(e, b"forced-exponent-sign")
        .as_deref()
        .map(ods_bool_value)
    {
        Some(Some(true)) => pattern.push('+'),
        Some(Some(false)) | None => {}
        Some(None) => add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1),
    }
    let exponent_digits = attr(e, b"min-exponent-digits")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 30);
    pattern.push_str(&"0".repeat(exponent_digits));
    if attr(e, b"exponent-interval").is_some() {
        add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
    }
    pattern
}

fn fraction_pattern(e: &quick_xml::events::BytesStart<'_>, losses: &mut Vec<StyleLoss>) -> String {
    let min_integer = attr(e, b"min-integer-digits")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
        .min(30);
    let mut pattern = if min_integer == 0 {
        "#".to_string()
    } else {
        "0".repeat(min_integer)
    };
    pattern.push(' ');
    let numerator_digits = attr(e, b"min-numerator-digits")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 30);
    pattern.push_str(&"?".repeat(numerator_digits));
    pattern.push('/');
    if let Some(denominator) = attr(e, b"denominator-value") {
        if denominator.parse::<u32>().is_ok_and(|value| value > 0) {
            pattern.push_str(&denominator);
        } else {
            pattern.push('?');
            add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
        }
    } else {
        let denominator_digits = attr(e, b"min-denominator-digits")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 30);
        pattern.push_str(&"?".repeat(denominator_digits));
        if attr(e, b"max-denominator-value").is_some() {
            add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
        }
    }
    pattern
}

fn number_component(element: &[u8], e: &quick_xml::events::BytesStart<'_>) -> Option<String> {
    let long = attr(e, b"style").as_deref() == Some("long");
    Some(match element {
        b"day" => if long { "dd" } else { "d" }.to_string(),
        b"month" => {
            if attr(e, b"textual").as_deref().is_some_and(ods_bool) {
                if long { "mmmm" } else { "mmm" }.to_string()
            } else if long {
                "mm".to_string()
            } else {
                "m".to_string()
            }
        }
        b"year" => if long { "yyyy" } else { "yy" }.to_string(),
        b"day-of-week" => if long { "dddd" } else { "ddd" }.to_string(),
        b"hours" => if long { "hh" } else { "h" }.to_string(),
        b"minutes" => if long { "mm" } else { "m" }.to_string(),
        b"seconds" => {
            let decimals = attr(e, b"decimal-places")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0)
                .min(9);
            let mut out = if long { "ss" } else { "s" }.to_string();
            if decimals > 0 {
                out.push('.');
                out.push_str(&"0".repeat(decimals));
            }
            out
        }
        b"am-pm" => "AM/PM".to_string(),
        b"text-content" => "@".to_string(),
        _ => return None,
    })
}

fn read_ods_number_formats(xml: &str, definitions: &mut OdsStyleDefinitions) {
    let mut reader = Reader::from_str(xml);
    let mut current: Option<(String, OdsNumberStyleKind, String)> = None;
    let mut text_depth = 0usize;
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qname = e.name();
                let element = local(qname.as_ref());
                let kind = match element {
                    b"number-style" => Some(OdsNumberStyleKind::Number),
                    b"currency-style" => Some(OdsNumberStyleKind::Currency),
                    b"percentage-style" => Some(OdsNumberStyleKind::Percentage),
                    b"date-style" => Some(OdsNumberStyleKind::Date),
                    b"time-style" => Some(OdsNumberStyleKind::Time),
                    b"boolean-style" => Some(OdsNumberStyleKind::Boolean),
                    b"text-style" => Some(OdsNumberStyleKind::Text),
                    _ => None,
                };
                if let (Some(kind), Some(name)) = (kind, attr(&e, b"name")) {
                    if name.len() <= MAX_ODS_STYLE_NAME {
                        current = Some((name, kind, String::new()));
                    }
                } else if let Some((_, kind, code)) = current.as_mut() {
                    match element {
                        b"number" => code.push_str(&number_pattern(&e, &mut definitions.losses)),
                        b"scientific-number" => {
                            code.push_str(&scientific_pattern(&e, &mut definitions.losses));
                        }
                        b"fraction" => {
                            code.push_str(&fraction_pattern(&e, &mut definitions.losses));
                        }
                        b"map" => add_ods_style_loss(
                            &mut definitions.losses,
                            StyleLossKind::UnsupportedProperty,
                            1,
                        ),
                        b"currency-symbol" | b"text" => {
                            if !e.is_empty() {
                                text_depth = 1;
                                text.clear();
                            }
                        }
                        _ => {
                            if let Some(component) = number_component(element, &e) {
                                code.push_str(&component);
                            }
                        }
                    }
                    if e.is_empty()
                        && element == b"currency-symbol"
                        && *kind == OdsNumberStyleKind::Currency
                    {
                        code.push('¤');
                    }
                }
            }
            Ok(Event::Text(value)) if text_depth > 0 => text.push_str(&text_of(&value)),
            Ok(Event::GeneralRef(reference)) if text_depth > 0 => {
                append_general_ref(&mut text, &reference)
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let element = local(qname.as_ref());
                if matches!(element, b"currency-symbol" | b"text") && text_depth > 0 {
                    if let Some((_, _, code)) = current.as_mut() {
                        code.push_str(&text);
                    }
                    text.clear();
                    text_depth = 0;
                } else {
                    text_depth = text_depth.saturating_sub(1);
                }
                let closes = match element {
                    b"number-style" => Some(OdsNumberStyleKind::Number),
                    b"currency-style" => Some(OdsNumberStyleKind::Currency),
                    b"percentage-style" => Some(OdsNumberStyleKind::Percentage),
                    b"date-style" => Some(OdsNumberStyleKind::Date),
                    b"time-style" => Some(OdsNumberStyleKind::Time),
                    b"boolean-style" => Some(OdsNumberStyleKind::Boolean),
                    b"text-style" => Some(OdsNumberStyleKind::Text),
                    _ => None,
                };
                if closes.is_some() {
                    if let Some((name, kind, mut code)) = current.take() {
                        if kind == OdsNumberStyleKind::Percentage && !code.contains('%') {
                            code.push('%');
                        }
                        if kind == OdsNumberStyleKind::Boolean && code.is_empty() {
                            code.push_str("BOOLEAN");
                        }
                        if kind == OdsNumberStyleKind::Text && code.is_empty() {
                            code.push('@');
                        }
                        if code.len() <= 4_096 {
                            definitions.number_formats.insert(name, code);
                        } else {
                            add_ods_style_loss(
                                &mut definitions.losses,
                                StyleLossKind::LimitExceeded,
                                1,
                            );
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
}

fn start_ods_style(
    e: &quick_xml::events::BytesStart<'_>,
    definitions: &mut OdsStyleDefinitions,
    default: bool,
) -> Option<(String, Option<String>)> {
    let family = attr(e, b"family")?;
    if !matches!(
        family.as_str(),
        "table" | "table-cell" | "table-row" | "table-column" | "text" | "paragraph" | "graphic"
    ) {
        add_ods_style_loss(
            &mut definitions.losses,
            StyleLossKind::UnsupportedProperty,
            1,
        );
        return None;
    }
    definitions.has_source_styles = true;
    if default {
        definitions
            .default_styles
            .entry(family.clone())
            .or_default();
        return Some((family, None));
    }
    let name = attr(e, b"name")?;
    if name.len() > MAX_ODS_STYLE_NAME || definitions.raw_styles.len() >= MAX_ODS_STYLES {
        add_ods_style_loss(&mut definitions.losses, StyleLossKind::LimitExceeded, 1);
        return None;
    }
    let raw = OdsRawStyle {
        parent: attr(e, b"parent-style-name").filter(|parent| parent.len() <= MAX_ODS_STYLE_NAME),
        data_style: attr(e, b"data-style-name").filter(|style| style.len() <= MAX_ODS_STYLE_NAME),
        props: OdsStyleProps::default(),
    };
    definitions
        .raw_styles
        .insert((family.clone(), name.clone()), raw);
    if family == "table" {
        definitions.table_styles.entry(name.clone()).or_default();
        if let Some(master_page) = attr(e, b"master-page-name") {
            definitions
                .table_master_pages
                .insert(name.clone(), master_page);
        }
    }
    Some((family, Some(name)))
}

fn ods_header_footer_kind(element: &[u8]) -> Option<(HeaderFooterKind, bool, bool)> {
    match element {
        b"header" => Some((HeaderFooterKind::OddHeader, false, false)),
        b"footer" => Some((HeaderFooterKind::OddFooter, false, false)),
        b"header-left" => Some((HeaderFooterKind::EvenHeader, true, false)),
        b"footer-left" => Some((HeaderFooterKind::EvenFooter, true, false)),
        b"header-first" => Some((HeaderFooterKind::FirstHeader, false, true)),
        b"footer-first" => Some((HeaderFooterKind::FirstFooter, false, true)),
        _ => None,
    }
}

fn begin_ods_header_footer(
    e: &quick_xml::events::BytesStart<'_>,
    element: &[u8],
    master_page: Option<&str>,
    definitions: &mut OdsStyleDefinitions,
) -> Option<HeaderFooterKind> {
    let (kind, even, first) = ods_header_footer_kind(element)?;
    let master_page = master_page?;
    let display = match attr(e, b"display").as_deref() {
        Some(value) if value.eq_ignore_ascii_case("true") || value == "1" => true,
        Some(value) if value.eq_ignore_ascii_case("false") || value == "0" => false,
        Some(_) => {
            definitions
                .master_page_print_metadata
                .entry(master_page.to_string())
                .or_default()
                .add_loss(PrintLossKind::MalformedHeaderFooter);
            false
        }
        None => true,
    };
    let metadata = definitions
        .master_page_print_metadata
        .entry(master_page.to_string())
        .or_default();
    let mut different_odd_even = metadata.header_footer().different_odd_even();
    let mut different_first = metadata.header_footer().different_first();
    let scale = metadata.header_footer().scale_with_document();
    let align = metadata.header_footer().align_with_margins();
    if even {
        different_odd_even = Some(display);
    }
    if first {
        different_first = Some(display);
    }
    metadata.set_header_footer_flag(different_odd_even, different_first, scale, align);
    if display {
        metadata.set_header_footer(kind, String::new());
        Some(kind)
    } else {
        None
    }
}

fn append_ods_header_control(metadata: &mut PrintMetadata, kind: HeaderFooterKind, element: &[u8]) {
    match element {
        b"region-left" => metadata.append_header_footer(kind, "&L"),
        b"region-center" => metadata.append_header_footer(kind, "&C"),
        b"region-right" => metadata.append_header_footer(kind, "&R"),
        b"page-number" | b"page-count" | b"date" | b"time" | b"sheet-name" | b"title" => {
            metadata.add_loss(PrintLossKind::UnsupportedProperty);
        }
        b"span" => metadata.add_loss(PrintLossKind::UnsupportedProperty),
        _ => {}
    }
}

fn read_ods_style_definitions(xml: &str, definitions: &mut OdsStyleDefinitions) {
    read_ods_number_formats(xml, definitions);
    let mut reader = Reader::from_str(xml);
    let mut current_style: Option<(String, Option<String>)> = None;
    let mut page_layout = None;
    let mut master_page: Option<String> = None;
    let mut header_footer_capture: Option<HeaderFooterKind> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let element = local(qname.as_ref());
                match element {
                    b"style" => current_style = start_ods_style(&e, definitions, false),
                    b"default-style" => current_style = start_ods_style(&e, definitions, true),
                    b"table-properties" => {
                        let table = current_style
                            .as_ref()
                            .filter(|(family, _)| family == "table")
                            .and_then(|(_, name)| name.clone());
                        apply_table_properties(&e, &table, &mut definitions.table_styles);
                    }
                    b"text-properties"
                    | b"paragraph-properties"
                    | b"table-cell-properties"
                    | b"table-row-properties"
                    | b"table-column-properties"
                    | b"graphic-properties" => {
                        if let Some((family, name)) = current_style.as_ref() {
                            let props = if let Some(name) = name {
                                definitions
                                    .raw_styles
                                    .get_mut(&(family.clone(), name.clone()))
                                    .map(|style| &mut style.props)
                            } else {
                                definitions.default_styles.get_mut(family)
                            };
                            if let Some(props) = props {
                                apply_ods_style_properties(
                                    element,
                                    &e,
                                    props,
                                    &mut definitions.losses,
                                );
                            }
                        }
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
                        master_page = attr(&e, b"name");
                        if let Some(name) = master_page.as_ref() {
                            let metadata = definitions
                                .master_page_print_metadata
                                .entry(name.clone())
                                .or_default();
                            metadata.set_header_footer_flag(Some(false), Some(false), None, None);
                            if let Some(layout) = attr(&e, b"page-layout-name") {
                                definitions.master_page_layouts.insert(name.clone(), layout);
                            }
                        }
                    }
                    b"header" | b"footer" | b"header-left" | b"footer-left" | b"header-first"
                    | b"footer-first" => {
                        header_footer_capture = begin_ods_header_footer(
                            &e,
                            element,
                            master_page.as_deref(),
                            definitions,
                        );
                    }
                    b"p" if header_footer_capture.is_some() => {
                        if let (Some(master), Some(kind)) =
                            (master_page.as_ref(), header_footer_capture)
                        {
                            let metadata = definitions
                                .master_page_print_metadata
                                .entry(master.clone())
                                .or_default();
                            if metadata
                                .header_footer()
                                .get(kind)
                                .is_some_and(|text| !text.is_empty())
                            {
                                metadata.append_header_footer(kind, "\n");
                            }
                        }
                    }
                    _ if header_footer_capture.is_some() => {
                        if let (Some(master), Some(kind)) =
                            (master_page.as_ref(), header_footer_capture)
                        {
                            append_ods_header_control(
                                definitions
                                    .master_page_print_metadata
                                    .entry(master.clone())
                                    .or_default(),
                                kind,
                                element,
                            );
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                let qname = e.name();
                let element = local(qname.as_ref());
                match element {
                    b"style" => {
                        let _ = start_ods_style(&e, definitions, false);
                    }
                    b"default-style" => {
                        let _ = start_ods_style(&e, definitions, true);
                    }
                    b"table-properties" => {
                        let table = current_style
                            .as_ref()
                            .filter(|(family, _)| family == "table")
                            .and_then(|(_, name)| name.clone());
                        apply_table_properties(&e, &table, &mut definitions.table_styles);
                    }
                    b"text-properties"
                    | b"paragraph-properties"
                    | b"table-cell-properties"
                    | b"table-row-properties"
                    | b"table-column-properties"
                    | b"graphic-properties" => {
                        if let Some((family, name)) = current_style.as_ref() {
                            let props = if let Some(name) = name {
                                definitions
                                    .raw_styles
                                    .get_mut(&(family.clone(), name.clone()))
                                    .map(|style| &mut style.props)
                            } else {
                                definitions.default_styles.get_mut(family)
                            };
                            if let Some(props) = props {
                                apply_ods_style_properties(
                                    element,
                                    &e,
                                    props,
                                    &mut definitions.losses,
                                );
                            }
                        }
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
                        if let Some(name) = attr(&e, b"name") {
                            let metadata = definitions
                                .master_page_print_metadata
                                .entry(name.clone())
                                .or_default();
                            metadata.set_header_footer_flag(Some(false), Some(false), None, None);
                            if let Some(layout) = attr(&e, b"page-layout-name") {
                                definitions.master_page_layouts.insert(name, layout);
                            }
                        }
                    }
                    b"header" | b"footer" | b"header-left" | b"footer-left" | b"header-first"
                    | b"footer-first" => {
                        let _ = begin_ods_header_footer(
                            &e,
                            element,
                            master_page.as_deref(),
                            definitions,
                        );
                    }
                    b"s" | b"tab" | b"line-break" if header_footer_capture.is_some() => {
                        if let (Some(master), Some(kind)) =
                            (master_page.as_ref(), header_footer_capture)
                        {
                            let text = match element {
                                b"s" => " ",
                                b"tab" => "\t",
                                _ => "\n",
                            };
                            definitions
                                .master_page_print_metadata
                                .entry(master.clone())
                                .or_default()
                                .append_header_footer(kind, text);
                        }
                    }
                    _ if header_footer_capture.is_some() => {
                        if let (Some(master), Some(kind)) =
                            (master_page.as_ref(), header_footer_capture)
                        {
                            append_ods_header_control(
                                definitions
                                    .master_page_print_metadata
                                    .entry(master.clone())
                                    .or_default(),
                                kind,
                                element,
                            );
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(value)) if header_footer_capture.is_some() => {
                if let (Some(master), Some(kind)) = (master_page.as_ref(), header_footer_capture) {
                    definitions
                        .master_page_print_metadata
                        .entry(master.clone())
                        .or_default()
                        .append_header_footer(kind, &text_of(&value));
                }
            }
            Ok(Event::GeneralRef(reference)) if header_footer_capture.is_some() => {
                if let (Some(master), Some(kind)) = (master_page.as_ref(), header_footer_capture) {
                    let mut text = String::new();
                    append_general_ref(&mut text, &reference);
                    definitions
                        .master_page_print_metadata
                        .entry(master.clone())
                        .or_default()
                        .append_header_footer(kind, &text);
                }
            }
            Ok(Event::CData(value)) if header_footer_capture.is_some() => {
                if let (Some(master), Some(kind)) = (master_page.as_ref(), header_footer_capture) {
                    definitions
                        .master_page_print_metadata
                        .entry(master.clone())
                        .or_default()
                        .append_header_footer(
                            kind,
                            String::from_utf8_lossy(value.as_ref()).as_ref(),
                        );
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let element = local(qname.as_ref());
                match element {
                    b"style" | b"default-style" => current_style = None,
                    b"page-layout" => page_layout = None,
                    b"header" | b"footer" | b"header-left" | b"footer-left" | b"header-first"
                    | b"footer-first" => header_footer_capture = None,
                    b"master-page" => {
                        master_page = None;
                        header_footer_capture = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
}

fn table_style_options(
    e: &quick_xml::events::BytesStart<'_>,
    styles: &OdsResolvedStyles,
) -> TableStyleOptions {
    attr(e, b"style-name")
        .and_then(|style| styles.table_styles.get(&style).copied())
        .unwrap_or_default()
}

fn table_print_metadata(
    e: &quick_xml::events::BytesStart<'_>,
    default_sheet: &str,
    styles: &OdsResolvedStyles,
) -> PrintMetadata {
    let mut metadata = attr(e, b"style-name")
        .and_then(|style| styles.table_print_metadata.get(&style).cloned())
        .unwrap_or_default();
    if let Some(ranges) = attr(e, b"print-ranges") {
        metadata.mark_source();
        for reference in split_ods_reference_list(&ranges) {
            match parse_ods_cell_range_with_default(reference, Some(default_sheet)) {
                Some((sheet, range)) if sheet == default_sheet => {
                    metadata.push_print_area(range);
                }
                Some(_) => metadata.add_loss(PrintLossKind::InvalidPrintArea),
                None if reference.contains("#REF!") => {
                    metadata.add_loss(PrintLossKind::MissingReference);
                }
                None => metadata.add_loss(PrintLossKind::InvalidPrintArea),
            }
        }
    }
    metadata
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
    if let Some(margins) = style.margins {
        setup.get_or_insert_with(PageSetup::default).margins = Some(margins);
    }
    if let Some(paper_size) = style.paper_size {
        setup.get_or_insert_with(PageSetup::default).paper_size = Some(paper_size);
    }
    setup
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

fn ods_frame(
    e: &quick_xml::events::BytesStart<'_>,
    sheet_name: &str,
    z_fallback: usize,
    styles: &OdsResolvedStyles,
    losses: &mut Vec<StyleLoss>,
) -> PendingFrame {
    let width = attr(e, b"width")
        .and_then(|value| parse_ods_length_points(&value))
        .and_then(ods_points_to_emu);
    let height = attr(e, b"height")
        .and_then(|value| parse_ods_length_points(&value))
        .and_then(ods_points_to_emu);
    let x = attr(e, b"x")
        .and_then(|value| parse_ods_signed_length_points(&value))
        .and_then(ods_signed_points_to_emu);
    let y = attr(e, b"y")
        .and_then(|value| parse_ods_signed_length_points(&value))
        .and_then(ods_signed_points_to_emu);
    let end_x = attr(e, b"end-x")
        .and_then(|value| parse_ods_signed_length_points(&value))
        .and_then(ods_signed_points_to_emu);
    let end_y = attr(e, b"end-y")
        .and_then(|value| parse_ods_signed_length_points(&value))
        .and_then(ods_signed_points_to_emu);
    let to = attr(e, b"end-cell-address")
        .and_then(|address| parse_ods_cell_range_with_default(&address, Some(sheet_name)))
        .map(|(_, range)| (range.2, range.3));
    let behavior = match attr(e, b"anchor-type").as_deref() {
        Some("page") => DrawingAnchorBehavior::Absolute,
        Some("cell") if to.is_some() => DrawingAnchorBehavior::MoveAndSize,
        Some("cell" | "paragraph" | "char" | "as-char") => DrawingAnchorBehavior::MoveOnly,
        _ if to.is_some() => DrawingAnchorBehavior::MoveAndSize,
        _ => DrawingAnchorBehavior::MoveOnly,
    };
    let rotation_mdeg = attr(e, b"transform").and_then(|transform| {
        let start = transform.find("rotate")?;
        let body = transform.get(start + "rotate".len()..)?.trim();
        let body = body.trim_start_matches('(').split(')').next()?.trim();
        let radians = body.parse::<f64>().ok()?;
        let degrees = radians.to_degrees() * 1_000.0;
        (degrees.is_finite() && degrees >= f64::from(i32::MIN) && degrees <= f64::from(i32::MAX))
            .then_some(degrees.round() as i32)
    });
    let style_name = attr(e, b"style-name");
    record_missing_ods_style(styles, "graphic", style_name.as_deref(), losses);
    let graphic_style = style_name
        .as_deref()
        .and_then(|name| styles.graphic.get(name))
        .or(styles.default_graphic.as_ref());
    let clip_points = match graphic_style.and_then(|style| style.clip) {
        Some(OdsClip::Rect(points)) => Some(points),
        Some(OdsClip::Auto) | None => None,
    };
    PendingFrame {
        image: None,
        to,
        metadata: DrawingMetadata {
            kind: DrawingObjectKind::Image,
            to_cell: to,
            from_offset_emu: x.zip(y),
            to_offset_emu: end_x.zip(end_y),
            absolute_size_emu: width.zip(height),
            rotation_mdeg,
            z_order: attr(e, b"z-index")
                .and_then(|value| value.parse::<i32>().ok())
                .or_else(|| Some(z_fallback.min(i32::MAX as usize) as i32)),
            name: attr(e, b"name").filter(|value| value.len() <= MAX_ODS_DRAWING_TEXT),
            behavior,
            ..Default::default()
        },
        description: String::new(),
        clip_points,
    }
}

fn ods_named_cell_style(styles: &OdsResolvedStyles, name: Option<&str>) -> Option<CellStyle> {
    name.and_then(|name| styles.cell.get(name))
        .map(OdsStyleProps::to_cell_style)
}

fn record_missing_ods_style(
    styles: &OdsResolvedStyles,
    family: &str,
    name: Option<&str>,
    losses: &mut Vec<StyleLoss>,
) {
    let Some(name) = name.filter(|name| !name.is_empty()) else {
        return;
    };
    let found = match family {
        "table" => styles.table_styles.contains_key(name),
        "table-cell" => styles.cell.contains_key(name),
        "table-row" => styles.row.contains_key(name),
        "table-column" => styles.column.contains_key(name),
        "text" => styles.text.contains_key(name),
        "paragraph" => styles.paragraph.contains_key(name),
        "graphic" => styles.graphic.contains_key(name),
        _ => false,
    };
    if !found {
        add_ods_style_loss(losses, StyleLossKind::MissingReference, 1);
    }
}

fn ods_default_cell_style(styles: &OdsResolvedStyles) -> Option<CellStyle> {
    styles
        .default_cell
        .as_ref()
        .map(OdsStyleProps::to_cell_style)
        .filter(|style| style != &CellStyle::default())
}

fn ods_table_default_cell_style(
    styles: &OdsResolvedStyles,
    name: Option<&str>,
) -> Option<CellStyle> {
    ods_named_cell_style(styles, name).or_else(|| ods_default_cell_style(styles))
}

fn merge_layout_cell_style(
    base: Option<CellStyle>,
    layout: Option<&OdsStyleProps>,
) -> Option<CellStyle> {
    let overlay = layout.map(OdsStyleProps::to_cell_style);
    match (base, overlay) {
        (None, None) => None,
        (Some(style), None) | (None, Some(style)) => Some(style),
        (Some(base), Some(overlay)) => Some(base.merge(&overlay)),
    }
    .filter(|style| style != &CellStyle::default())
}

#[allow(clippy::too_many_arguments)]
fn apply_ods_column_style(
    e: &quick_xml::events::BytesStart<'_>,
    styles: &OdsResolvedStyles,
    first: u32,
    repeat: u32,
    col_formats: &mut BTreeMap<u16, CellStyle>,
    col_widths: &mut BTreeMap<u16, f32>,
    physical_col_widths: &mut BTreeMap<u16, f32>,
    hidden_cols: &mut std::collections::BTreeSet<u16>,
    losses: &mut Vec<StyleLoss>,
) {
    let style_name = attr(e, b"style-name");
    let default_cell_name = attr(e, b"default-cell-style-name");
    record_missing_ods_style(styles, "table-column", style_name.as_deref(), losses);
    record_missing_ods_style(styles, "table-cell", default_cell_name.as_deref(), losses);
    let layout = style_name
        .as_deref()
        .and_then(|name| styles.column.get(name))
        .or(styles.default_column.as_ref());
    let default_cell = ods_table_default_cell_style(styles, default_cell_name.as_deref());
    let cell_style = merge_layout_cell_style(default_cell, layout);
    let directly_hidden = matches!(
        attr(e, b"visibility").as_deref(),
        Some("collapse" | "filter")
    );
    let end = first.saturating_add(repeat).min(MAX_REPEAT);
    for raw_col in first..end {
        if col_formats
            .len()
            .max(col_widths.len())
            .max(hidden_cols.len())
            >= MAX_ODS_LAYOUT_ENTRIES
        {
            add_ods_style_loss(losses, StyleLossKind::LimitExceeded, 1);
            break;
        }
        let col = raw_col.min(u32::from(u16::MAX)) as u16;
        if let Some(style) = cell_style.as_ref() {
            col_formats.insert(col, style.clone());
        }
        if let Some(width) = layout.and_then(|style| style.col_width_chars) {
            col_widths.insert(col, width);
        }
        if let Some(width) = layout.and_then(|style| style.col_width_points) {
            physical_col_widths.insert(col, width);
        }
        if directly_hidden || layout.and_then(|style| style.hidden) == Some(true) {
            hidden_cols.insert(col);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_ods_row_style(
    e: &quick_xml::events::BytesStart<'_>,
    styles: &OdsResolvedStyles,
    first: u32,
    repeat: u32,
    row_formats: &mut BTreeMap<u32, CellStyle>,
    row_heights: &mut BTreeMap<u32, f32>,
    hidden_rows: &mut std::collections::BTreeSet<u32>,
    losses: &mut Vec<StyleLoss>,
) {
    let style_name = attr(e, b"style-name");
    let default_cell_name = attr(e, b"default-cell-style-name");
    record_missing_ods_style(styles, "table-row", style_name.as_deref(), losses);
    record_missing_ods_style(styles, "table-cell", default_cell_name.as_deref(), losses);
    let layout = style_name
        .as_deref()
        .and_then(|name| styles.row.get(name))
        .or(styles.default_row.as_ref());
    let default_cell = ods_table_default_cell_style(styles, default_cell_name.as_deref());
    let cell_style = merge_layout_cell_style(default_cell, layout);
    let directly_hidden = matches!(
        attr(e, b"visibility").as_deref(),
        Some("collapse" | "filter")
    );
    let end = first.saturating_add(repeat).min(MAX_ROW_REPEAT);
    for row in first..end {
        if row_formats
            .len()
            .max(row_heights.len())
            .max(hidden_rows.len())
            >= MAX_ODS_LAYOUT_ENTRIES
        {
            add_ods_style_loss(losses, StyleLossKind::LimitExceeded, 1);
            break;
        }
        if let Some(style) = cell_style.as_ref() {
            row_formats.insert(row, style.clone());
        }
        if let Some(height) = layout.and_then(|style| style.row_height_pt) {
            row_heights.insert(row, height);
        }
        if directly_hidden || layout.and_then(|style| style.hidden) == Some(true) {
            hidden_rows.insert(row);
        }
    }
}

fn record_ods_manual_breaks(
    e: &quick_xml::events::BytesStart<'_>,
    styles: &OdsResolvedStyles,
    first: u32,
    repeat: u32,
    rows: bool,
    metadata: &mut PrintMetadata,
) {
    let layout = attr(e, b"style-name")
        .and_then(|name| {
            if rows {
                styles.row.get(&name)
            } else {
                styles.column.get(&name)
            }
        })
        .or({
            if rows {
                styles.default_row.as_ref()
            } else {
                styles.default_column.as_ref()
            }
        });
    let Some(layout) = layout else { return };
    if layout.break_invalid {
        metadata.add_loss(PrintLossKind::UnsupportedProperty);
    }
    let before = layout.break_before_page == Some(true);
    let after = layout.break_after_page == Some(true);
    if !before && !after {
        return;
    }
    metadata.mark_source();
    let retained_repeat = repeat.min(1_027);
    for offset in 0..retained_repeat {
        let index = first.saturating_add(offset);
        if before {
            record_ods_manual_break(index, rows, metadata);
        }
        if after {
            record_ods_manual_break(index.saturating_add(1), rows, metadata);
        }
    }
    if repeat > retained_repeat {
        metadata.add_loss(PrintLossKind::LimitExceeded);
    }
}

fn record_ods_manual_break(index: u32, rows: bool, metadata: &mut PrintMetadata) {
    if rows {
        metadata.push_manual_row_break(index);
    } else {
        match u16::try_from(index) {
            Ok(col) => metadata.push_manual_col_break(col),
            Err(_) => metadata.add_loss(PrintLossKind::InvalidPageBreak),
        }
    }
}

fn ods_cell_base_font(
    styles: &OdsResolvedStyles,
    default_format: Option<&CellStyle>,
    row_formats: &BTreeMap<u32, CellStyle>,
    col_formats: &BTreeMap<u16, CellStyle>,
    style_name: Option<&str>,
    row: u32,
    col: u16,
) -> Font {
    style_name
        .and_then(|name| styles.cell.get(name))
        .and_then(|style| style.to_cell_style().font)
        .or_else(|| row_formats.get(&row).and_then(|style| style.font.clone()))
        .or_else(|| col_formats.get(&col).and_then(|style| style.font.clone()))
        .or_else(|| default_format.and_then(|style| style.font.clone()))
        .unwrap_or_default()
}

fn ods_text_font(props: Option<&OdsStyleProps>, mut base: Font) -> Font {
    let Some(props) = props else {
        return base;
    };
    if props.font_name.is_some() {
        base.name.clone_from(&props.font_name);
    }
    if props.font_size_pt.is_some() {
        base.size_pt = props.font_size_pt;
    }
    if props.font_color.is_some() {
        base.color = props.font_color;
    }
    if let Some(value) = props.bold {
        base.bold = value;
    }
    if let Some(value) = props.italic {
        base.italic = value;
    }
    if let Some(value) = props.underline {
        base.underline = value;
    }
    if let Some(value) = props.strikethrough {
        base.strikethrough = value;
    }
    if let Some(value) = props.script {
        base.script = value;
    }
    base
}

fn flush_ods_run(text: &str, start: &mut usize, runs: &mut Vec<crate::TextRun>, font: &Font) {
    let end = text.len();
    if *start < end {
        if let Some(fragment) = text.get(*start..end) {
            if !fragment.is_empty() {
                runs.push(crate::TextRun::new(fragment, font.clone()));
            }
        }
    }
    *start = end;
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
    let mut row_formats: BTreeMap<u32, CellStyle> = BTreeMap::new();
    let mut col_formats: BTreeMap<u16, CellStyle> = BTreeMap::new();
    let mut blank_styles: BTreeMap<(u32, u16), CellStyle> = BTreeMap::new();
    let mut row_heights: BTreeMap<u32, f32> = BTreeMap::new();
    let mut col_widths: BTreeMap<u16, f32> = BTreeMap::new();
    let mut physical_col_widths: BTreeMap<u16, f32> = BTreeMap::new();
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
                    cells = Vec::new();
                    merges = Vec::new();
                    read_hyperlinks = Vec::new();
                    read_comments = Vec::new();
                    read_data_validations = Vec::new();
                    read_images = Vec::new();
                    drawing_metadata = Vec::new();
                    row_formats = BTreeMap::new();
                    col_formats = BTreeMap::new();
                    blank_styles = BTreeMap::new();
                    row_heights = BTreeMap::new();
                    col_widths = BTreeMap::new();
                    physical_col_widths = BTreeMap::new();
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
                        &mut col_widths,
                        &mut physical_col_widths,
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
                        &mut row_heights,
                        &mut hidden_rows,
                        &mut style_losses,
                    );
                }
                b"table-cell" | b"covered-table-cell" if in_table => {
                    cur = Some(read_cell_attrs(&e));
                    record_missing_ods_style(
                        styles,
                        "table-cell",
                        cur.as_ref().and_then(|cell| cell.style_name.as_deref()),
                        &mut style_losses,
                    );
                    cell_base_font = ods_cell_base_font(
                        styles,
                        default_format.as_ref(),
                        &row_formats,
                        &col_formats,
                        cur.as_ref().and_then(|cell| cell.style_name.as_deref()),
                        row,
                        col,
                    );
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
                    record_missing_ods_style(
                        styles,
                        "table-cell",
                        a.style_name.as_deref(),
                        &mut style_losses,
                    );
                    let resolved_style = ods_named_cell_style(styles, a.style_name.as_deref());
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
                        &mut col_widths,
                        &mut physical_col_widths,
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
                        &mut row_heights,
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
                        let resolved_style = ods_named_cell_style(styles, a.style_name.as_deref());
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
                        style_fidelity,
                        cells: std::mem::take(&mut cells),
                        default_format: default_format.take(),
                        row_formats: std::mem::take(&mut row_formats),
                        col_formats: std::mem::take(&mut col_formats),
                        blank_styles: std::mem::take(&mut blank_styles),
                        row_heights: std::mem::take(&mut row_heights),
                        col_widths: std::mem::take(&mut col_widths),
                        physical_col_widths: std::mem::take(&mut physical_col_widths),
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
                style: metadata.style.cloned(),
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

    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0A, 0x49, 0x44,
        0x41, 0x54, 0x78, 0x9C, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0D, 0x0A, 0x2D, 0xB4, 0, 0, 0, 0,
        0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

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

    fn png_1x1_with_physical_density() -> Vec<u8> {
        // 1,000 px/m horizontally and 2,000 px/m vertically. The resulting
        // intrinsic dimensions are exactly 0.1 cm by 0.05 cm.
        const PHYS_CHUNK: &[u8] = &[
            0x00, 0x00, 0x00, 0x09, b'p', b'H', b'Y', b's', 0x00, 0x00, 0x03, 0xe8, 0x00, 0x00,
            0x07, 0xd0, 0x01, 0xa5, 0xed, 0x46, 0x4c,
        ];
        let mut png = Vec::with_capacity(PNG_1X1.len() + PHYS_CHUNK.len());
        png.extend_from_slice(&PNG_1X1[..33]);
        png.extend_from_slice(PHYS_CHUNK);
        png.extend_from_slice(&PNG_1X1[33..]);
        png
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
    fn ods_rich_spans_preserve_text_boundaries_and_unicode() {
        let content = r#"<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="RTL"><table:table-row><table:table-cell office:value-type="string"><text:p>한글 <text:span text:style-name="em">مرحباً 👩‍💻</text:span> e&#x301;</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let sheet = &wb.sheets[0];
        let expected = "한글 مرحباً 👩‍💻 e\u{301}";
        assert_eq!(sheet.cell(0, 0), Some(&Cell::Text(expected.to_string())));
        let runs = sheet.rich_text_runs(0, 0).expect("ODF span boundaries");
        assert_eq!(
            runs.iter().map(|run| run.text.as_str()).collect::<Vec<_>>(),
            ["한글 ", "مرحباً 👩‍💻", " e\u{301}"]
        );
        assert_eq!(
            runs.iter().map(|run| run.text.as_str()).collect::<String>(),
            expected
        );
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
    fn ods_table_writing_mode_surfaces_inherited_right_to_left_view() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:automatic-styles><style:style style:name="rtl-parent" style:family="table"><style:table-properties style:writing-mode="rl-tb"/></style:style><style:style style:name="rtl-child" style:family="table" style:parent-style-name="rtl-parent"/><style:style style:name="ltr-child" style:family="table" style:parent-style-name="rtl-parent"><style:table-properties style:writing-mode="lr-tb"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="RTL" table:style-name="rtl-child"><table:table-row/></table:table><table:table table:name="LTR" table:style-name="ltr-child"/></office:spreadsheet></office:body></office:document-content>"#;

        let workbook = Workbook::open(&ods_bytes(content)).expect("ods");

        assert!(
            workbook
                .sheet_by_name("RTL")
                .expect("RTL sheet")
                .sheet_view()
                .right_to_left
        );
        assert!(
            !workbook
                .sheet_by_name("LTR")
                .expect("LTR sheet")
                .sheet_view()
                .right_to_left
        );
    }

    #[test]
    fn ods_direct_row_and_column_visibility_is_retained() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Hidden axes"><table:table-column/><table:table-column table:visibility="collapse" table:number-columns-repeated="2"/><table:table-column table:visibility="filter"/><table:table-row><table:table-cell office:value-type="string"><text:p>visible</text:p></table:table-cell></table:table-row><table:table-row table:visibility="collapse"><table:table-cell office:value-type="string"><text:p>collapsed</text:p></table:table-cell></table:table-row><table:table-row table:visibility="filter"><table:table-cell office:value-type="string"><text:p>filtered</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let wb = Workbook::open(&ods_bytes(content)).unwrap();
        let sheet = &wb.sheets[0];

        assert_eq!(
            sheet.hidden_columns().iter().copied().collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(
            sheet.hidden_rows().iter().copied().collect::<Vec<_>>(),
            [1, 2]
        );
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
    fn ods_print_sidecar_retains_ranges_breaks_order_and_header_variants() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Print" table:style-name="ta" table:print-ranges="$Print.$A$1:$Print.$B$2 $Print.$D$4:$Print.$F$8"><table:table-column table:style-name="cb"/><table:table-row/><table:table-row table:style-name="rb"/></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:style style:name="ta" style:family="table" style:master-page-name="mp"/><style:style style:name="rb" style:family="table-row"><style:table-row-properties fo:break-before="page"/></style:style><style:style style:name="cb" style:family="table-column"><style:table-column-properties fo:break-after="page"/></style:style></office:styles><office:automatic-styles><style:page-layout style:name="pm"><style:page-layout-properties style:print="headers" style:table-centering="horizontal" style:print-page-order="ltr"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="mp" style:page-layout-name="pm"><style:header><text:p>odd-h</text:p></style:header><style:footer><text:p>odd-f</text:p></style:footer><style:header-left><text:p>even-h</text:p></style:header-left><style:footer-left><text:p>even-f</text:p></style:footer-left><style:header-first><text:p>first-h</text:p></style:header-first><style:footer-first><text:p>first-f</text:p></style:footer-first></style:master-page></office:master-styles></office:document-styles>"#;

        let wb = Workbook::open(&ods_bytes_with_styles(content, styles)).unwrap();
        let metadata = wb.sheets[0].print_metadata();

        assert_eq!(metadata.fidelity(), crate::PrintFidelity::Retained);
        assert_eq!(metadata.print_areas(), &[(0, 0, 1, 1), (3, 3, 7, 5)]);
        assert_eq!(metadata.manual_row_breaks(), &[1]);
        assert_eq!(metadata.manual_col_breaks(), &[1]);
        assert_eq!(metadata.page_order(), Some(PrintPageOrder::OverThenDown));
        assert_eq!(metadata.print_gridlines(), Some(false));
        assert_eq!(metadata.print_headings(), Some(true));
        assert_eq!(metadata.center_horizontally(), Some(true));
        assert_eq!(metadata.center_vertically(), Some(false));
        assert_eq!(metadata.header_footer().odd_header(), Some("odd-h"));
        assert_eq!(metadata.header_footer().odd_footer(), Some("odd-f"));
        assert_eq!(metadata.header_footer().even_header(), Some("even-h"));
        assert_eq!(metadata.header_footer().even_footer(), Some("even-f"));
        assert_eq!(metadata.header_footer().first_header(), Some("first-h"));
        assert_eq!(metadata.header_footer().first_footer(), Some("first-f"));
        assert_eq!(metadata.header_footer().different_odd_even(), Some(true));
        assert_eq!(metadata.header_footer().different_first(), Some(true));
        assert_eq!(
            wb.sheets[0].page_setup().and_then(|setup| setup.print_area),
            Some((0, 0, 1, 1))
        );
    }

    #[test]
    fn malformed_ods_print_state_reports_typed_losses() {
        let content = r##"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Bad" table:style-name="ta" table:print-ranges="#REF! bad"/></office:spreadsheet></office:body></office:document-content>"##;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles><style:style style:name="ta" style:family="table" style:master-page-name="mp"/></office:styles><office:automatic-styles><style:page-layout style:name="pm"><style:page-layout-properties style:print-page-order="diagonal"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="mp" style:page-layout-name="pm"/></office:master-styles></office:document-styles>"#;

        let wb = Workbook::open(&ods_bytes_with_styles(content, styles)).unwrap();
        let metadata = wb.sheets[0].print_metadata();
        assert_eq!(metadata.fidelity(), crate::PrintFidelity::Partial);
        assert!(metadata
            .losses()
            .iter()
            .any(|loss| loss.kind == PrintLossKind::MissingReference));
        assert!(metadata
            .losses()
            .iter()
            .any(|loss| loss.kind == PrintLossKind::InvalidPrintArea));
        assert!(metadata
            .losses()
            .iter()
            .any(|loss| loss.kind == PrintLossKind::UnsupportedProperty));
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
    fn general_refs_are_reassembled_across_ods_text_surfaces() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="A&amp;B"><table:table-row><table:table-cell office:value-type="string"><text:p>A&amp;B&#33;</text:p><office:annotation><dc:creator>R&amp;D</dc:creator><text:p>Check &lt;now&gt;</text:p></office:annotation></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let meta = r#"<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/"><office:meta><dc:title>Budget &amp; Plan&#33;</dc:title></office:meta></office:document-meta>"#;

        let wb = Workbook::open(&ods_bytes_with_meta(content, meta)).unwrap();
        assert_eq!(wb.sheets[0].name, "A&B");
        assert_eq!(
            wb.sheets[0].cell(0, 0),
            Some(&Cell::Text("A&B!".to_string()))
        );
        assert_eq!(wb.sheets[0].comments()[0].author.as_deref(), Some("R&D"));
        assert_eq!(wb.sheets[0].comments()[0].text, "Check <now>");
        assert_eq!(
            wb.metadata().properties.title.as_deref(),
            Some("Budget & Plan!")
        );

        let settings = parse_settings(
            r#"<config><config-item config:name="ActiveTable">A&amp;B</config-item></config>"#,
        );
        assert_eq!(settings.active_table.as_deref(), Some("A&B"));
    }

    #[test]
    fn unknown_and_illegal_general_refs_are_preserved_lexically_on_read() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Data"><table:table-row><table:table-cell office:value-type="string"><text:p>A&bogus;&#x1;</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let workbook = Workbook::open(&ods_bytes(content)).unwrap();

        assert_eq!(
            workbook.sheets[0].cell(0, 0),
            Some(&Cell::Text("A&bogus;&#x1;".to_string()))
        );
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

    #[test]
    fn ods_style_cascade_retains_cell_row_column_text_and_number_formats() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Styled"><table:table-column table:style-name="Column" table:default-cell-style-name="Base" table:number-columns-repeated="2"/><table:table-row table:style-name="Row" table:default-cell-style-name="Base"><table:table-cell office:value-type="string"><text:p>row default</text:p></table:table-cell><table:table-cell table:style-name="Child" office:value-type="float" office:value="1234.5"><text:p><text:span text:style-name="Em">₩1,234.50</text:span></text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r##"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"><office:styles><style:default-style style:family="table-cell"><style:text-properties fo:font-family="Noto Sans" fo:font-size="10pt"/></style:default-style><number:currency-style style:name="Money"><number:currency-symbol>₩</number:currency-symbol><number:number number:decimal-places="2" number:min-integer-digits="1" number:grouping="true"/></number:currency-style><style:style style:name="Base" style:family="table-cell" style:data-style-name="Money"><style:table-cell-properties fo:background-color="#ffeecc" fo:border="0.75pt solid #112233"/><style:text-properties fo:font-weight="bold"/></style:style><style:style style:name="Child" style:family="table-cell" style:parent-style-name="Base"><style:text-properties fo:color="#2244aa" fo:font-style="italic"/></style:style><style:style style:name="Row" style:family="table-row"><style:table-row-properties style:row-height="0.5in"/></style:style><style:style style:name="Column" style:family="table-column"><style:table-column-properties style:column-width="2cm"/></style:style><style:style style:name="Em" style:family="text"><style:text-properties fo:font-style="italic" fo:color="#008800"/></style:style></office:styles></office:document-styles>"##;

        let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.style_fidelity(), StyleFidelity::Retained);
        assert!(sheet.style_losses().is_empty());
        assert_eq!(sheet.row_heights().get(&0), Some(&36.0));
        assert!((sheet.column_widths()[&0] - (2.0 * 72.0 / 2.54 / 5.25) as f32).abs() < 0.01);
        assert!((sheet.physical_column_widths()[&0] - (2.0 * 72.0 / 2.54) as f32).abs() < 0.01);

        let inherited = sheet.resolved_cell_style(0, 0).expect("row style");
        assert!(inherited.font.as_ref().is_some_and(|font| font.bold));
        assert_eq!(inherited.num_fmt.as_deref(), Some("₩#,##0.00"));
        assert_eq!(inherited.fill, Some(Color::rgb(0xff, 0xee, 0xcc)));

        let explicit = sheet.cell_style(0, 1).expect("child style");
        let font = explicit.font.as_ref().expect("font");
        assert_eq!(font.name.as_deref(), Some("Noto Sans"));
        assert!(font.bold);
        assert!(font.italic);
        assert_eq!(font.color, Some(Color::rgb(0x22, 0x44, 0xaa)));
        assert_eq!(explicit.num_fmt.as_deref(), Some("₩#,##0.00"));
        assert_eq!(
            explicit.border.as_ref().map(|border| border.left),
            Some(BorderStyle::Thin)
        );

        let runs = sheet.rich_text_runs(0, 1).expect("text style run");
        assert_eq!(runs.len(), 1);
        assert!(runs[0].font.italic);
        assert_eq!(runs[0].font.color, Some(Color::rgb(0x00, 0x88, 0x00)));
    }

    #[test]
    fn ods_default_family_styles_apply_without_named_references() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Defaults"><table:table-column/><table:table-row><table:table-cell office:value-type="string"><text:p>defaulted</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r##"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:default-style style:family="table-cell"><style:text-properties fo:font-family="Noto Sans KR" fo:font-weight="bold"/><style:table-cell-properties fo:background-color="#ddeeff"/></style:default-style><style:default-style style:family="table-row"><style:table-row-properties style:row-height="18pt"/></style:default-style><style:default-style style:family="table-column"><style:table-column-properties style:column-width="1in"/></style:default-style></office:styles></office:document-styles>"##;

        let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
        let sheet = &workbook.sheets[0];
        let style = sheet.resolved_cell_style(0, 0).expect("default style");
        assert!(style.font.as_ref().is_some_and(|font| font.bold));
        assert_eq!(style.fill, Some(Color::rgb(0xDD, 0xEE, 0xFF)));
        assert_eq!(sheet.row_heights().get(&0), Some(&18.0));
        assert!((sheet.column_widths()[&0] - (72.0 / 5.25) as f32).abs() < 0.01);
        assert_eq!(sheet.physical_column_widths().get(&0), Some(&72.0));
    }

    #[test]
    fn ods_text_and_paragraph_style_names_do_not_collide() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Style namespaces"><table:table-row><table:table-cell office:value-type="string"><text:p text:style-name="Same">paragraph<text:span text:style-name="Same">span</text:span></text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r##"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:style style:name="Same" style:family="paragraph"><style:text-properties fo:font-weight="bold" fo:color="#aa0000"/></style:style><style:style style:name="Same" style:family="text"><style:text-properties fo:font-style="italic" fo:color="#008800"/></style:style></office:styles></office:document-styles>"##;

        let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.style_fidelity(), StyleFidelity::Retained);
        let runs = sheet.rich_text_runs(0, 0).expect("paragraph and span runs");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "paragraph");
        assert!(runs[0].font.bold);
        assert!(!runs[0].font.italic);
        assert_eq!(runs[0].font.color, Some(Color::rgb(0xaa, 0x00, 0x00)));
        assert_eq!(runs[1].text, "span");
        assert!(runs[1].font.bold);
        assert!(runs[1].font.italic);
        assert_eq!(runs[1].font.color, Some(Color::rgb(0x00, 0x88, 0x00)));
    }

    #[test]
    fn ods_missing_content_style_references_are_aggregated() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Missing" table:style-name="MissingTable" table:default-cell-style-name="MissingTableCell"><table:table-column table:style-name="MissingColumn" table:default-cell-style-name="MissingColumnCell"/><table:table-row table:style-name="MissingRow" table:default-cell-style-name="MissingRowCell"><table:table-cell table:style-name="MissingCell" office:value-type="string"><text:p text:style-name="MissingParagraph">plain<text:span text:style-name="MissingText">span</text:span></text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:styles><style:default-style style:family="table-cell"/></office:styles></office:document-styles>"#;

        let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.style_fidelity(), StyleFidelity::Partial);
        assert_eq!(
            sheet
                .style_losses()
                .iter()
                .find(|loss| loss.kind == StyleLossKind::MissingReference)
                .map(|loss| loss.occurrences),
            Some(9)
        );
    }

    #[test]
    fn ods_transparent_child_fill_clears_parent_fill() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Transparent"><table:table-row><table:table-cell table:style-name="Child" office:value-type="string" office:string-value="clear"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r##"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:style style:name="Base" style:family="table-cell"><style:table-cell-properties fo:background-color="#cc3300"/></style:style><style:style style:name="Child" style:family="table-cell" style:parent-style-name="Base"><style:table-cell-properties fo:background-color="transparent"/></style:style></office:styles></office:document-styles>"##;

        let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.style_fidelity(), StyleFidelity::Retained);
        let style = sheet.cell_style(0, 0).expect("explicit child style");
        assert_eq!(style.fill, None);
        assert_eq!(style.pattern_fill, None);
    }

    #[test]
    fn ods_scientific_and_fixed_fraction_number_styles_are_retained() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Numbers"><table:table-row><table:table-cell table:style-name="ScientificCell" office:value-type="float" office:value="12345"/><table:table-cell table:style-name="FractionCell" office:value-type="float" office:value="0.375"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0"><office:styles><number:number-style style:name="Scientific"><number:scientific-number number:decimal-places="2" number:min-decimal-places="2" number:min-exponent-digits="3" number:forced-exponent-sign="true"/></number:number-style><number:number-style style:name="Fraction"><number:fraction number:min-numerator-digits="2" number:denominator-value="8"/></number:number-style><style:style style:name="ScientificCell" style:family="table-cell" style:data-style-name="Scientific"/><style:style style:name="FractionCell" style:family="table-cell" style:data-style-name="Fraction"/></office:styles></office:document-styles>"#;

        let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.style_fidelity(), StyleFidelity::Retained);
        assert_eq!(
            sheet
                .cell_style(0, 0)
                .and_then(|style| style.num_fmt.as_deref()),
            Some("0.00E+000")
        );
        assert_eq!(
            sheet
                .cell_style(0, 1)
                .and_then(|style| style.num_fmt.as_deref()),
            Some("# ??/8")
        );
    }

    #[test]
    fn ods_sheet_level_page_image_retains_absolute_geometry() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="Page image"><table:shapes><draw:frame draw:name="Page logo" draw:z-index="5" text:anchor-type="page" svg:x="1cm" svg:y="2cm" svg:width="3cm" svg:height="4cm"><draw:image xlink:href="Pictures/page.png"/><svg:desc>Page-level description</svg:desc></draw:frame></table:shapes></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let workbook = Workbook::open(&ods_bytes_with_part(content, "Pictures/page.png", PNG_1X1))
            .expect("ods");
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.images().len(), 1);
        assert_eq!(sheet.images()[0].from, (0, 0));
        assert_eq!(sheet.images()[0].to, None);
        let metadata = &sheet.drawing_metadata()[0];
        assert_eq!(metadata.kind, DrawingObjectKind::Image);
        assert_eq!(metadata.object_index, 0);
        assert_eq!(metadata.from_cell, None);
        assert_eq!(metadata.to_cell, None);
        assert_eq!(metadata.from_offset_emu, Some((360_000, 720_000)));
        assert_eq!(metadata.absolute_size_emu, Some((1_080_000, 1_440_000)));
        assert_eq!(metadata.z_order, Some(5));
        assert_eq!(metadata.name.as_deref(), Some("Page logo"));
        assert_eq!(metadata.alt_text.as_deref(), Some("Page-level description"));
        assert_eq!(metadata.behavior, DrawingAnchorBehavior::Absolute);
    }

    #[test]
    fn ods_missing_image_target_is_a_typed_fidelity_loss() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:automatic-styles><style:default-style style:family="table-cell"/></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Missing image"><table:shapes><draw:frame text:anchor-type="page"><draw:image xlink:href="Pictures/missing.png"/></draw:frame></table:shapes></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
        let sheet = &workbook.sheets[0];
        assert!(sheet.images().is_empty());
        assert_eq!(sheet.style_fidelity(), StyleFidelity::Partial);
        assert!(sheet
            .style_losses()
            .iter()
            .any(|loss| loss.kind == StyleLossKind::DrawingMetadataPartial));
    }

    #[test]
    fn ods_parent_style_cycle_is_bounded_and_typed() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Cycle"><table:table-row><table:table-cell table:style-name="A" office:value-type="string"><text:p>safe</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let styles = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"><office:styles><style:style style:name="A" style:family="table-cell" style:parent-style-name="B"><style:text-properties fo:font-weight="bold"/></style:style><style:style style:name="B" style:family="table-cell" style:parent-style-name="A"><style:text-properties fo:font-style="italic"/></style:style></office:styles></office:document-styles>"#;

        let workbook = Workbook::open(&ods_bytes_with_styles(content, styles)).expect("ods");
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.cell(0, 0), Some(&Cell::Text("safe".to_string())));
        assert_eq!(sheet.style_fidelity(), StyleFidelity::Partial);
        assert!(sheet
            .style_losses()
            .iter()
            .any(|loss| loss.kind == StyleLossKind::InheritanceCycle));
    }

    #[test]
    fn ods_drawing_frame_retains_physical_geometry_and_accessibility() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="Images"><table:table-row><table:table-cell/><table:table-cell><draw:frame draw:name="Korean logo" draw:z-index="7" text:anchor-type="cell" svg:x="-1cm" svg:y="2cm" svg:width="3cm" svg:height="4cm" table:end-cell-address=".D5" table:end-x="0.5cm" table:end-y="-0.25cm" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><draw:image xlink:href="Pictures/logo.png"/><svg:desc>접근 가능한 설명</svg:desc></draw:frame></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let workbook = Workbook::open(&ods_bytes_with_part(content, "Pictures/logo.png", PNG_1X1))
            .expect("ods");
        let sheet = &workbook.sheets[0];
        assert_eq!(sheet.images()[0].from, (0, 1));
        assert_eq!(sheet.images()[0].to, Some((4, 3)));
        let metadata = &sheet.drawing_metadata()[0];
        assert_eq!(metadata.kind, DrawingObjectKind::Image);
        assert_eq!(metadata.object_index, 0);
        assert_eq!(metadata.from_cell, Some((0, 1)));
        assert_eq!(metadata.to_cell, Some((4, 3)));
        assert_eq!(metadata.from_offset_emu, Some((-360_000, 720_000)));
        assert_eq!(metadata.to_offset_emu, Some((180_000, -90_000)));
        assert_eq!(metadata.absolute_size_emu, Some((1_080_000, 1_440_000)));
        assert_eq!(metadata.z_order, Some(7));
        assert_eq!(metadata.name.as_deref(), Some("Korean logo"));
        assert_eq!(metadata.alt_text.as_deref(), Some("접근 가능한 설명"));
        assert_eq!(metadata.behavior, DrawingAnchorBehavior::MoveAndSize);
    }

    #[test]
    fn ods_graphic_style_inheritance_normalizes_clip_to_crop_ppm() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:automatic-styles><style:style style:name="CropBase" style:family="graphic"><style:graphic-properties fo:clip="rect(0.01cm, 0.02cm, 0.005cm, 0.01cm)"/></style:style><style:style style:name="CropChild" style:family="graphic" style:parent-style-name="CropBase"/></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Crop"><table:table-row><table:table-cell><draw:frame draw:style-name="CropChild"><draw:image xlink:href="Pictures/crop.png"/></draw:frame></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let png = png_1x1_with_physical_density();

        let workbook =
            Workbook::open(&ods_bytes_with_part(content, "Pictures/crop.png", &png)).expect("ods");
        let sheet = &workbook.sheets[0];

        assert_eq!(sheet.style_fidelity(), StyleFidelity::Retained);
        assert_eq!(sheet.images().len(), 1);
        assert_eq!(
            sheet.drawing_metadata()[0].crop,
            Some(DrawingCrop {
                left_ppm: 100_000,
                top_ppm: 200_000,
                right_ppm: 200_000,
                bottom_ppm: 100_000,
            })
        );
    }

    #[test]
    fn ods_clip_without_physical_density_is_a_typed_partial_loss() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:automatic-styles><style:style style:name="Crop" style:family="graphic"><style:graphic-properties fo:clip="rect(0.01cm, 0cm, 0cm, 0cm)"/></style:style></office:automatic-styles><office:body><office:spreadsheet><table:table table:name="Crop"><table:table-row><table:table-cell><draw:frame draw:style-name="Crop"><draw:image xlink:href="Pictures/crop.png"/></draw:frame></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let workbook = Workbook::open(&ods_bytes_with_part(content, "Pictures/crop.png", PNG_1X1))
            .expect("ods");
        let sheet = &workbook.sheets[0];

        assert_eq!(sheet.drawing_metadata()[0].crop, None);
        assert_eq!(sheet.style_fidelity(), StyleFidelity::Partial);
        assert!(sheet
            .style_losses()
            .iter()
            .any(|loss| loss.kind == StyleLossKind::DrawingMetadataPartial));
    }

    #[test]
    fn ods_jpeg_jfif_density_produces_bounded_physical_dimensions() {
        let jpeg = [
            0xff, 0xd8, // SOI
            0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x01, 0x00, 0x48,
            0x00, 0x90, 0x00, 0x00, // JFIF: 72 x 144 dpi
            0xff, 0xc0, 0x00, 0x07, 0x08, 0x00, 0xc8, 0x00, 0x64, // 100 x 200 px
            0xff, 0xd9, // EOI
        ];

        assert_eq!(jpeg_physical_size_points(&jpeg), Some((100.0, 100.0)));
    }

    #[test]
    fn ods_unsupported_shape_retains_cell_anchor_sidecar() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Shape"><table:table-row><table:table-cell/><table:table-cell><draw:frame text:anchor-type="cell" table:end-cell-address=".D3" svg:width="2cm" svg:height="1cm"></draw:frame></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let workbook = Workbook::open(&ods_bytes(content)).expect("ods");
        let metadata = &workbook.sheets[0].drawing_metadata()[0];

        assert_eq!(metadata.kind, DrawingObjectKind::Shape);
        assert_eq!(metadata.from_cell, Some((0, 1)));
        assert_eq!(metadata.to_cell, Some((2, 3)));
        assert_eq!(metadata.absolute_size_emu, Some((720_000, 360_000)));
    }
}
