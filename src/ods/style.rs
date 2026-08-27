//! ODS style definitions, inheritance, layout, and resolved style application.

use std::collections::{BTreeMap, HashMap, HashSet};

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::model::{parse_decimal_ratio_u64, parse_decimal_scaled_u32, ImportedAxisMeasure};
use crate::{
    Alignment, Border, BorderStyle, CellProtection, CellStyle, Color, DrawingAnchorBehavior,
    DrawingMetadata, DrawingObjectKind, Fill, Font, FormatPattern, FormatScript, HAlign,
    HeaderFooterKind, PageSetup, PrintLossKind, PrintMetadata, PrintPageOrder, StyleLoss,
    StyleLossKind, VAlign,
};

use super::{
    append_general_ref, attr, attr_true, local, ods_points_to_emu, ods_signed_points_to_emu,
    parse_ods_cell_range_with_default, read_table_print_area, split_ods_reference_list, text_of,
    PendingFrame, MAX_ODS_DRAWING_TEXT, MAX_REPEAT, MAX_ROW_REPEAT,
};

type TableStyles = HashMap<String, TableStyleOptions>;

const MAX_ODS_STYLES: usize = 65_536;
pub(super) const MAX_ODS_STYLE_NAME: usize = 1_024;
const MAX_ODS_STYLE_DEPTH: usize = 64;
pub(super) const MAX_ODS_LAYOUT_ENTRIES: usize = 1 << 18;
const MAX_ODS_CLIP_POINTS: f64 = 1_000_000_000.0;

#[derive(Clone, Copy, Default)]
pub(super) struct TableStyleOptions {
    visible: Option<bool>,
    pub(super) tab_color: Option<Color>,
    pub(super) right_to_left: Option<bool>,
    pub(super) print_gridlines: bool,
    pub(super) print_headings: bool,
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
    pub(super) fn hidden(self) -> bool {
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
pub(super) struct OdsStyleProps {
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
    unresolved_number_format: bool,
    decimal_places: Option<usize>,
    decimal_places_invalid: bool,
    horizontal: Option<HAlign>,
    vertical: Option<VAlign>,
    wrap: Option<bool>,
    rotation: Option<i16>,
    indent: Option<u8>,
    shrink_to_fit: Option<bool>,
    locked: Option<bool>,
    hidden_formula: Option<bool>,
    row_height_pt: Option<f32>,
    row_axis_measure: Option<ImportedAxisMeasure>,
    use_optimal_row_height: Option<bool>,
    col_width_chars: Option<f32>,
    col_width_points: Option<f32>,
    col_axis_measure: Option<ImportedAxisMeasure>,
    hidden: Option<bool>,
    break_before_page: Option<bool>,
    break_after_page: Option<bool>,
    break_invalid: bool,
    clip: Option<OdsClip>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum OdsNumberFormatState {
    #[default]
    General,
    Resolved,
    Unresolved,
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
            use_optimal_row_height,
            col_width_chars,
            col_width_points,
            hidden,
            break_before_page,
            break_after_page,
            clip,
        );
        // A valid child length that is not exactly representable in the retained
        // integer domain must clear the parent's exact measure while preserving
        // the compatibility floating-point projection.
        if other.row_height_pt.is_some() {
            self.row_axis_measure = other.row_axis_measure;
        }
        if other.col_width_points.is_some() {
            self.col_axis_measure = other.col_axis_measure;
        }
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
        if other.decimal_places.is_some() || other.decimal_places_invalid {
            self.decimal_places = other.decimal_places;
            self.decimal_places_invalid = other.decimal_places_invalid;
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
    unresolved_parent: bool,
    data_style: Option<String>,
    unresolved_data_style: bool,
    props: OdsStyleProps,
}

#[derive(Clone, Default)]
struct OdsResolvedStyle {
    props: OdsStyleProps,
    data_style: Option<String>,
    unresolved_data_style: bool,
}

#[derive(Default)]
pub(super) struct OdsResolvedStyles {
    table_styles: TableStyles,
    pub(super) cell: HashMap<String, OdsStyleProps>,
    row: HashMap<String, OdsStyleProps>,
    column: HashMap<String, OdsStyleProps>,
    pub(super) text: HashMap<String, OdsStyleProps>,
    pub(super) paragraph: HashMap<String, OdsStyleProps>,
    graphic: HashMap<String, OdsStyleProps>,
    pub(super) default_cell: Option<OdsStyleProps>,
    default_row: Option<OdsStyleProps>,
    default_column: Option<OdsStyleProps>,
    pub(super) default_text: Option<OdsStyleProps>,
    pub(super) default_paragraph: Option<OdsStyleProps>,
    default_graphic: Option<OdsStyleProps>,
    pub(super) losses: Vec<StyleLoss>,
    pub(super) has_source_styles: bool,
    table_print_metadata: HashMap<String, PrintMetadata>,
}

#[derive(Default)]
pub(super) struct OdsStyleDefinitions {
    table_styles: TableStyles,
    table_master_pages: HashMap<String, String>,
    master_page_layouts: HashMap<String, String>,
    master_page_print_metadata: HashMap<String, PrintMetadata>,
    page_layout_options: HashMap<String, PageLayoutOptions>,
    raw_styles: HashMap<(String, String), OdsRawStyle>,
    default_styles: HashMap<String, OdsStyleProps>,
    number_formats: HashMap<String, String>,
    unresolved_number_formats: HashSet<String>,
    losses: Vec<StyleLoss>,
    has_source_styles: bool,
}

pub(super) fn add_ods_style_loss(losses: &mut Vec<StyleLoss>, kind: StyleLossKind, count: u32) {
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
            props: OdsStyleProps {
                unresolved_number_format: true,
                ..definitions
                    .default_styles
                    .get(family)
                    .cloned()
                    .unwrap_or_default()
            },
            data_style: None,
            unresolved_data_style: true,
        };
    }
    let Some(raw) = definitions.raw_styles.get(&key) else {
        add_ods_style_loss(losses, StyleLossKind::MissingReference, 1);
        return OdsResolvedStyle {
            props: OdsStyleProps {
                unresolved_number_format: true,
                ..OdsStyleProps::default()
            },
            data_style: None,
            unresolved_data_style: true,
        };
    };
    visiting.push(key.clone());
    let mut resolved = OdsResolvedStyle {
        props: definitions
            .default_styles
            .get(family)
            .cloned()
            .unwrap_or_default(),
        data_style: None,
        unresolved_data_style: false,
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
    } else if raw.unresolved_parent {
        resolved.props.num_fmt = None;
        resolved.props.unresolved_number_format = true;
        resolved.data_style = None;
        resolved.unresolved_data_style = true;
    }
    resolved.props.overlay(&raw.props);
    if raw.data_style.is_some() {
        resolved.data_style.clone_from(&raw.data_style);
        resolved.unresolved_data_style = false;
    } else if raw.unresolved_data_style {
        resolved.data_style = None;
        resolved.unresolved_data_style = true;
    }
    if resolved.unresolved_data_style {
        resolved.props.num_fmt = None;
        resolved.props.unresolved_number_format = true;
    } else if let Some(format_name) = resolved.data_style.as_deref() {
        resolved.props.num_fmt = None;
        resolved.props.unresolved_number_format = false;
        if let Some(format) = definitions.number_formats.get(format_name) {
            resolved.props.num_fmt = Some(format.clone());
        } else if definitions.unresolved_number_formats.contains(format_name) {
            resolved.props.unresolved_number_format = true;
        } else {
            resolved.data_style = None;
            resolved.unresolved_data_style = true;
            resolved.props.unresolved_number_format = true;
            add_ods_style_loss(losses, StyleLossKind::MissingReference, 1);
        }
    }
    visiting.pop();
    cache.insert(key, resolved.clone());
    resolved
}

impl OdsStyleDefinitions {
    pub(super) fn into_resolved(mut self) -> OdsResolvedStyles {
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

pub(super) fn parse_ods_axis_measure(value: &str) -> Option<ImportedAxisMeasure> {
    let value = value.trim();
    let (number, unit) = ["pt", "pc", "in", "px", "cm", "mm"]
        .into_iter()
        .find_map(|unit| value.strip_suffix(unit).map(|number| (number.trim(), unit)))?;
    let integral = match unit {
        "pt" => parse_decimal_scaled_u32(number, 20).map(ImportedAxisMeasure::Twips),
        "pc" => parse_decimal_scaled_u32(number, 240).map(ImportedAxisMeasure::Twips),
        "in" => parse_decimal_scaled_u32(number, 1_440).map(ImportedAxisMeasure::Twips),
        "px" => parse_decimal_scaled_u32(number, 15).map(ImportedAxisMeasure::Twips),
        "cm" => {
            parse_decimal_scaled_u32(number, 1_000).map(ImportedAxisMeasure::MillimeterHundredths)
        }
        "mm" => {
            parse_decimal_scaled_u32(number, 100).map(ImportedAxisMeasure::MillimeterHundredths)
        }
        _ => None,
    };
    if let Some(measure) = integral.filter(|measure| {
        !matches!(
            measure,
            ImportedAxisMeasure::Twips(0) | ImportedAxisMeasure::MillimeterHundredths(0)
        )
    }) {
        return Some(measure);
    }

    let (numerator, denominator) = parse_decimal_ratio_u64(number)?;
    if numerator == 0 {
        return None;
    }
    let (point_numerator, point_denominator) = match unit {
        "pt" => exact_ratio_product(numerator, denominator, 1, 1),
        "pc" => exact_ratio_product(numerator, denominator, 12, 1),
        "in" => exact_ratio_product(numerator, denominator, 72, 1),
        "px" => exact_ratio_product(numerator, denominator, 3, 4),
        "cm" => exact_ratio_product(numerator, denominator, 3_600, 127),
        "mm" => exact_ratio_product(numerator, denominator, 360, 127),
        _ => None,
    }?;
    Some(ImportedAxisMeasure::PointRatio(
        point_numerator,
        point_denominator,
    ))
}

fn exact_ratio_product(
    numerator: u64,
    denominator: u64,
    multiplier_numerator: u64,
    multiplier_denominator: u64,
) -> Option<(u64, u64)> {
    let numerator = u128::from(numerator).checked_mul(u128::from(multiplier_numerator))?;
    let denominator = u128::from(denominator).checked_mul(u128::from(multiplier_denominator))?;
    let divisor = gcd_u128(numerator, denominator);
    Some((
        u64::try_from(numerator / divisor).ok()?,
        u64::try_from(denominator / divisor).ok()?,
    ))
}

const fn gcd_u128(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    if left == 0 {
        1
    } else {
        left
    }
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

fn parse_ods_bounded_usize(value: &str, max: usize) -> std::result::Result<usize, StyleLossKind> {
    let value = value.trim().parse::<usize>().map_err(|error| {
        if matches!(error.kind(), std::num::IntErrorKind::PosOverflow) {
            StyleLossKind::LimitExceeded
        } else {
            StyleLossKind::UnsupportedProperty
        }
    })?;
    if value > max {
        Err(StyleLossKind::LimitExceeded)
    } else {
        Ok(value)
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
            if let Some(value) = attr(e, b"decimal-places") {
                match parse_ods_bounded_usize(&value, 30) {
                    Ok(value) => {
                        props.decimal_places = Some(value);
                        props.decimal_places_invalid = false;
                    }
                    Err(kind) => {
                        props.decimal_places = None;
                        props.decimal_places_invalid = true;
                        add_ods_style_loss(losses, kind, 1);
                    }
                }
            }
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
                    props.row_axis_measure = parse_ods_axis_measure(&value);
                } else {
                    add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
                }
            }
            if let Some(display) = attr(e, b"display") {
                props.hidden = Some(!ods_bool(&display));
            }
            if let Some(value) = attr(e, b"use-optimal-row-height") {
                if let Some(value) = ods_bool_value(&value) {
                    props.use_optimal_row_height = Some(value);
                } else {
                    add_ods_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
                }
            }
            apply_ods_page_break_properties(e, props, losses);
        }
        b"table-column-properties" => {
            if let Some(value) = attr(e, b"column-width") {
                if let Some(width) = parse_ods_length_points(&value) {
                    props.col_width_points = Some(width.clamp(0.0, f64::from(f32::MAX)) as f32);
                    // The public model stores Excel-compatible character units.
                    props.col_width_chars = Some((width / 5.25).clamp(0.0, 255.0) as f32);
                    props.col_axis_measure = parse_ods_axis_measure(&value);
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

#[derive(Clone, Copy)]
enum OdsInheritedDecimalPlaces {
    Absent,
    Value(usize),
    Invalid,
}

#[derive(Clone, Copy)]
enum OdsUnresolvedNumberFormat {
    AlreadyReported,
    Report(StyleLossKind),
}

fn merge_unresolved_number_format(
    current: &mut Option<OdsUnresolvedNumberFormat>,
    next: OdsUnresolvedNumberFormat,
) {
    match (*current, next) {
        (Some(OdsUnresolvedNumberFormat::Report(StyleLossKind::LimitExceeded)), _) => {}
        (_, OdsUnresolvedNumberFormat::Report(StyleLossKind::LimitExceeded)) => {
            *current = Some(next);
        }
        (None | Some(OdsUnresolvedNumberFormat::AlreadyReported), _) => {
            *current = Some(next);
        }
        (
            Some(OdsUnresolvedNumberFormat::Report(_)),
            OdsUnresolvedNumberFormat::AlreadyReported,
        ) => {}
        (Some(OdsUnresolvedNumberFormat::Report(_)), OdsUnresolvedNumberFormat::Report(_)) => {}
    }
}

fn number_pattern(
    e: &quick_xml::events::BytesStart<'_>,
    inherited_decimals: OdsInheritedDecimalPlaces,
) -> std::result::Result<String, OdsUnresolvedNumberFormat> {
    // ODF 1.2 §19.343.2 inherits an omitted number:decimal-places
    // value from style:decimal-places on the default table-cell style.
    let explicit_decimals = attr(e, b"decimal-places");
    let decimals = match explicit_decimals.as_deref() {
        Some(value) => {
            parse_ods_bounded_usize(value, 30).map_err(OdsUnresolvedNumberFormat::Report)?
        }
        None => match inherited_decimals {
            OdsInheritedDecimalPlaces::Value(value) => value,
            OdsInheritedDecimalPlaces::Absent => {
                return Err(OdsUnresolvedNumberFormat::Report(
                    StyleLossKind::UnsupportedProperty,
                ));
            }
            OdsInheritedDecimalPlaces::Invalid => {
                return Err(OdsUnresolvedNumberFormat::AlreadyReported);
            }
        },
    };
    let min_decimals = match attr(e, b"min-decimal-places") {
        Some(value) => {
            let value =
                parse_ods_bounded_usize(&value, 30).map_err(OdsUnresolvedNumberFormat::Report)?;
            if value > decimals {
                return Err(OdsUnresolvedNumberFormat::Report(
                    StyleLossKind::UnsupportedProperty,
                ));
            }
            value
        }
        None if explicit_decimals.is_some() => decimals,
        None => 0,
    };
    let min_integer = match attr(e, b"min-integer-digits") {
        Some(value) => {
            parse_ods_bounded_usize(&value, 30).map_err(OdsUnresolvedNumberFormat::Report)?
        }
        None => 1,
    };
    let grouped = match attr(e, b"grouping") {
        Some(value) => ods_bool_value(value.trim()).ok_or(OdsUnresolvedNumberFormat::Report(
            StyleLossKind::UnsupportedProperty,
        ))?,
        None => false,
    };
    let mut pattern = if grouped && min_integer == 0 {
        "#,###".to_string()
    } else if grouped {
        "#,##".to_string()
    } else if min_integer == 0 {
        "#".to_string()
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
        return Err(OdsUnresolvedNumberFormat::Report(
            StyleLossKind::UnsupportedProperty,
        ));
    }
    Ok(pattern)
}

fn scientific_pattern(
    e: &quick_xml::events::BytesStart<'_>,
    inherited_decimals: OdsInheritedDecimalPlaces,
) -> std::result::Result<String, OdsUnresolvedNumberFormat> {
    let mut pattern = number_pattern(e, inherited_decimals)?;
    pattern.push('E');
    match attr(e, b"forced-exponent-sign")
        .as_deref()
        .map(ods_bool_value)
    {
        Some(Some(true)) => pattern.push('+'),
        Some(Some(false)) | None => {}
        Some(None) => {
            return Err(OdsUnresolvedNumberFormat::Report(
                StyleLossKind::UnsupportedProperty,
            ));
        }
    }
    let exponent_digits = match attr(e, b"min-exponent-digits") {
        Some(value) => parse_ods_bounded_usize(&value, 30)
            .map_err(OdsUnresolvedNumberFormat::Report)?
            .max(1),
        None => 1,
    };
    pattern.push_str(&"0".repeat(exponent_digits));
    if attr(e, b"exponent-interval").is_some() {
        return Err(OdsUnresolvedNumberFormat::Report(
            StyleLossKind::UnsupportedProperty,
        ));
    }
    Ok(pattern)
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

fn number_component(
    element: &[u8],
    e: &quick_xml::events::BytesStart<'_>,
) -> std::result::Result<Option<String>, OdsUnresolvedNumberFormat> {
    let long = attr(e, b"style").as_deref() == Some("long");
    Ok(Some(match element {
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
            let decimals = match attr(e, b"decimal-places") {
                Some(value) => {
                    parse_ods_bounded_usize(&value, 9).map_err(OdsUnresolvedNumberFormat::Report)?
                }
                None => 0,
            };
            let mut out = if long { "ss" } else { "s" }.to_string();
            if decimals > 0 {
                out.push('.');
                out.push_str(&"0".repeat(decimals));
            }
            out
        }
        b"am-pm" => "AM/PM".to_string(),
        b"text-content" => "@".to_string(),
        _ => return Ok(None),
    }))
}

fn append_ods_number_literal(
    code: &mut String,
    literal: &str,
    activate_percent: bool,
    active_percent: &mut bool,
) {
    // The shared formatter consumes Excel-style format codes. Escape every ODF
    // literal character so letters such as `d`, `m`, and `s` cannot be
    // reinterpreted as date/time fields and punctuation cannot become a
    // format directive. A percentage style is the exception: its first
    // explicit percent glyph must remain an active scaling token.
    for character in literal.chars() {
        if activate_percent && character == '%' && !*active_percent {
            code.push('%');
            *active_percent = true;
        } else {
            code.push('\\');
            code.push(character);
        }
    }
}

fn read_ods_number_formats(xml: &str, definitions: &mut OdsStyleDefinitions) {
    let mut reader = Reader::from_str(xml);
    let inherited_decimals = match definitions.default_styles.get("table-cell") {
        Some(style) if style.decimal_places_invalid => OdsInheritedDecimalPlaces::Invalid,
        Some(style) if style.decimal_places.is_some() => {
            OdsInheritedDecimalPlaces::Value(style.decimal_places.unwrap_or_default())
        }
        _ => OdsInheritedDecimalPlaces::Absent,
    };
    let mut current: Option<(
        String,
        OdsNumberStyleKind,
        String,
        bool,
        Option<OdsUnresolvedNumberFormat>,
    )> = None;
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
                        current = Some((name, kind, String::new(), false, None));
                    }
                } else if let Some((_, kind, code, _, unresolved_reason)) = current.as_mut() {
                    match element {
                        b"number" => match number_pattern(&e, inherited_decimals) {
                            Ok(pattern) => code.push_str(&pattern),
                            Err(reason) => {
                                merge_unresolved_number_format(unresolved_reason, reason)
                            }
                        },
                        b"scientific-number" => match scientific_pattern(&e, inherited_decimals) {
                            Ok(pattern) => code.push_str(&pattern),
                            Err(reason) => {
                                merge_unresolved_number_format(unresolved_reason, reason)
                            }
                        },
                        b"fraction" => {
                            code.push_str(&fraction_pattern(&e, &mut definitions.losses));
                        }
                        b"map" => merge_unresolved_number_format(
                            unresolved_reason,
                            OdsUnresolvedNumberFormat::Report(StyleLossKind::UnsupportedProperty),
                        ),
                        b"currency-symbol" | b"text" => {
                            if !e.is_empty() {
                                text_depth = 1;
                                text.clear();
                            }
                        }
                        _ => match number_component(element, &e) {
                            Ok(Some(component)) => code.push_str(&component),
                            Ok(None) => {}
                            Err(reason) => {
                                merge_unresolved_number_format(unresolved_reason, reason)
                            }
                        },
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
                    if let Some((_, kind, code, active_percent, _)) = current.as_mut() {
                        append_ods_number_literal(
                            code,
                            &text,
                            *kind == OdsNumberStyleKind::Percentage,
                            active_percent,
                        );
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
                    if let Some((name, kind, mut code, active_percent, unresolved_reason)) =
                        current.take()
                    {
                        if kind == OdsNumberStyleKind::Percentage && !active_percent {
                            code.push('%');
                        }
                        if kind == OdsNumberStyleKind::Boolean && code.is_empty() {
                            code.push_str("BOOLEAN");
                        }
                        if kind == OdsNumberStyleKind::Text && code.is_empty() {
                            code.push('@');
                        }
                        let unresolved_reason = if code.len() > 4_096 {
                            Some(OdsUnresolvedNumberFormat::Report(
                                StyleLossKind::LimitExceeded,
                            ))
                        } else {
                            unresolved_reason
                        };
                        if let Some(reason) = unresolved_reason {
                            definitions.number_formats.remove(&name);
                            definitions.unresolved_number_formats.insert(name);
                            if let OdsUnresolvedNumberFormat::Report(reason) = reason {
                                add_ods_style_loss(&mut definitions.losses, reason, 1);
                            }
                        } else {
                            definitions.unresolved_number_formats.remove(&name);
                            definitions.number_formats.insert(name, code);
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
        if attr(e, b"data-style-name").is_some() {
            add_ods_style_loss(
                &mut definitions.losses,
                StyleLossKind::UnsupportedProperty,
                1,
            );
        }
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
    let raw_parent = attr(e, b"parent-style-name");
    let parent = raw_parent
        .as_ref()
        .filter(|parent| parent.len() <= MAX_ODS_STYLE_NAME)
        .cloned();
    let unresolved_parent = raw_parent.is_some() && parent.is_none();
    if unresolved_parent {
        add_ods_style_loss(&mut definitions.losses, StyleLossKind::LimitExceeded, 1);
    }
    let raw_data_style = attr(e, b"data-style-name");
    let (data_style, unresolved_data_style) = if family == "table-cell" {
        let data_style = raw_data_style
            .as_ref()
            .filter(|style| style.len() <= MAX_ODS_STYLE_NAME)
            .cloned();
        let unresolved = raw_data_style.is_some() && data_style.is_none();
        if unresolved {
            add_ods_style_loss(&mut definitions.losses, StyleLossKind::LimitExceeded, 1);
        }
        (data_style, unresolved)
    } else {
        if raw_data_style.is_some() {
            add_ods_style_loss(
                &mut definitions.losses,
                StyleLossKind::UnsupportedProperty,
                1,
            );
        }
        (None, false)
    };
    let raw = OdsRawStyle {
        parent,
        unresolved_parent,
        data_style,
        unresolved_data_style,
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

pub(super) fn read_ods_style_definitions(xml: &str, definitions: &mut OdsStyleDefinitions) {
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
    // Number components with omitted precision inherit style:decimal-places
    // from the default table-cell style parsed above.
    read_ods_number_formats(xml, definitions);
}

pub(super) fn table_style_options(
    e: &quick_xml::events::BytesStart<'_>,
    styles: &OdsResolvedStyles,
) -> TableStyleOptions {
    attr(e, b"style-name")
        .and_then(|style| styles.table_styles.get(&style).copied())
        .unwrap_or_default()
}

pub(super) fn table_print_metadata(
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

pub(super) fn table_protected(e: &quick_xml::events::BytesStart<'_>) -> bool {
    attr(e, b"protected").as_deref().is_some_and(attr_true)
}

pub(super) fn table_page_setup(
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

pub(super) fn ods_frame(
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

pub(super) fn ods_named_cell_style(
    styles: &OdsResolvedStyles,
    name: Option<&str>,
) -> Option<CellStyle> {
    name.map(|name| {
        styles
            .cell
            .get(name)
            .map(OdsStyleProps::to_cell_style)
            .unwrap_or_default()
    })
}

pub(super) fn ods_number_format_state(props: &OdsStyleProps) -> OdsNumberFormatState {
    if props.unresolved_number_format {
        OdsNumberFormatState::Unresolved
    } else if props.num_fmt.is_some() {
        OdsNumberFormatState::Resolved
    } else {
        OdsNumberFormatState::General
    }
}

pub(super) fn ods_named_cell_number_format_state(
    styles: &OdsResolvedStyles,
    name: Option<&str>,
) -> Option<OdsNumberFormatState> {
    name.map(|name| {
        styles
            .cell
            .get(name)
            .map(ods_number_format_state)
            .unwrap_or(OdsNumberFormatState::Unresolved)
    })
}

pub(super) fn record_missing_ods_style(
    styles: &OdsResolvedStyles,
    family: &str,
    name: Option<&str>,
    losses: &mut Vec<StyleLoss>,
) {
    let Some(name) = name.filter(|name| !name.is_empty()) else {
        return;
    };
    if name.len() > MAX_ODS_STYLE_NAME {
        add_ods_style_loss(losses, StyleLossKind::LimitExceeded, 1);
        return;
    }
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

pub(super) fn record_ods_cell_style_reference(
    styles: &OdsResolvedStyles,
    style_reference: (Option<&str>, bool),
    losses: &mut Vec<StyleLoss>,
) {
    let (style_name, style_name_invalid) = style_reference;
    if style_name_invalid {
        add_ods_style_loss(losses, StyleLossKind::LimitExceeded, 1);
    } else {
        record_missing_ods_style(styles, "table-cell", style_name, losses);
    }
}

pub(super) fn ods_default_cell_style(styles: &OdsResolvedStyles) -> Option<CellStyle> {
    styles
        .default_cell
        .as_ref()
        .map(OdsStyleProps::to_cell_style)
        .filter(|style| style != &CellStyle::default())
}

pub(super) fn ods_table_default_cell_style(
    styles: &OdsResolvedStyles,
    name: Option<&str>,
) -> Option<CellStyle> {
    match name {
        Some(name) => Some(
            styles
                .cell
                .get(name)
                .map(OdsStyleProps::to_cell_style)
                .unwrap_or_default(),
        ),
        None => ods_default_cell_style(styles),
    }
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
pub(super) fn apply_ods_column_style(
    e: &quick_xml::events::BytesStart<'_>,
    styles: &OdsResolvedStyles,
    first: u32,
    repeat: u32,
    col_formats: &mut BTreeMap<u16, CellStyle>,
    col_number_format_states: &mut BTreeMap<u16, OdsNumberFormatState>,
    col_widths: &mut BTreeMap<u16, f32>,
    physical_col_widths: &mut BTreeMap<u16, f32>,
    imported_column_axis_measures: &mut BTreeMap<u16, ImportedAxisMeasure>,
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
    let default_cell_props = default_cell_name
        .as_deref()
        .and_then(|name| styles.cell.get(name));
    let default_cell_state = default_cell_name.as_deref().map(|name| {
        styles
            .cell
            .get(name)
            .map(ods_number_format_state)
            .unwrap_or(OdsNumberFormatState::Unresolved)
    });
    let default_cell = default_cell_props.map(OdsStyleProps::to_cell_style);
    let cell_style = merge_layout_cell_style(default_cell, layout);
    let number_format_state = default_cell_state;
    let directly_hidden = matches!(
        attr(e, b"visibility").as_deref(),
        Some("collapse" | "filter")
    );
    let end = first.saturating_add(repeat).min(MAX_REPEAT);
    for raw_col in first..end {
        if col_formats
            .len()
            .max(col_number_format_states.len())
            .max(col_widths.len())
            .max(imported_column_axis_measures.len())
            .max(hidden_cols.len())
            >= MAX_ODS_LAYOUT_ENTRIES
        {
            add_ods_style_loss(losses, StyleLossKind::LimitExceeded, 1);
            break;
        }
        let col = raw_col.min(u32::from(u16::MAX)) as u16;
        if let Some(style) = cell_style.as_ref() {
            col_formats.insert(col, style.clone());
        } else if number_format_state.is_some() {
            // Preserve an explicitly selected General/Unresolved whole style
            // so lower-precedence defaults cannot leak through the public
            // resolved-style cascade.
            col_formats.insert(col, CellStyle::default());
        }
        if let Some(state) = number_format_state {
            col_number_format_states.insert(col, state);
        } else {
            col_number_format_states.remove(&col);
        }
        if let Some(width) = layout.and_then(|style| style.col_width_chars) {
            col_widths.insert(col, width);
        }
        if let Some(width) = layout.and_then(|style| style.col_width_points) {
            physical_col_widths.insert(col, width);
        }
        if let Some(measure) = layout.and_then(|style| style.col_axis_measure) {
            imported_column_axis_measures.insert(col, measure);
        }
        if directly_hidden || layout.and_then(|style| style.hidden) == Some(true) {
            hidden_cols.insert(col);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_ods_row_style(
    e: &quick_xml::events::BytesStart<'_>,
    styles: &OdsResolvedStyles,
    first: u32,
    repeat: u32,
    row_formats: &mut BTreeMap<u32, CellStyle>,
    row_number_format_states: &mut BTreeMap<u32, OdsNumberFormatState>,
    row_heights: &mut BTreeMap<u32, f32>,
    automatic_row_height_candidates: &mut std::collections::BTreeSet<u32>,
    imported_row_axis_measures: &mut BTreeMap<u32, ImportedAxisMeasure>,
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
    let default_cell_props = default_cell_name
        .as_deref()
        .and_then(|name| styles.cell.get(name));
    let default_cell_state = default_cell_name.as_deref().map(|name| {
        styles
            .cell
            .get(name)
            .map(ods_number_format_state)
            .unwrap_or(OdsNumberFormatState::Unresolved)
    });
    let default_cell = default_cell_props.map(OdsStyleProps::to_cell_style);
    let cell_style = merge_layout_cell_style(default_cell, layout);
    let number_format_state = default_cell_state;
    let directly_hidden = matches!(
        attr(e, b"visibility").as_deref(),
        Some("collapse" | "filter")
    );
    let end = first.saturating_add(repeat).min(MAX_ROW_REPEAT);
    for row in first..end {
        if row_formats
            .len()
            .max(row_number_format_states.len())
            .max(row_heights.len())
            .max(automatic_row_height_candidates.len())
            .max(imported_row_axis_measures.len())
            .max(hidden_rows.len())
            >= MAX_ODS_LAYOUT_ENTRIES
        {
            add_ods_style_loss(losses, StyleLossKind::LimitExceeded, 1);
            break;
        }
        if let Some(style) = cell_style.as_ref() {
            row_formats.insert(row, style.clone());
        } else if number_format_state.is_some() {
            row_formats.insert(row, CellStyle::default());
        }
        if let Some(state) = number_format_state {
            row_number_format_states.insert(row, state);
        } else {
            row_number_format_states.remove(&row);
        }
        if let Some(height) = layout.and_then(|style| style.row_height_pt) {
            row_heights.insert(row, height);
        }
        if row_heights.contains_key(&row)
            && layout.and_then(|style| style.use_optimal_row_height) == Some(true)
        {
            automatic_row_height_candidates.insert(row);
        } else {
            automatic_row_height_candidates.remove(&row);
        }
        if let Some(measure) = layout.and_then(|style| style.row_axis_measure) {
            imported_row_axis_measures.insert(row, measure);
        }
        if directly_hidden || layout.and_then(|style| style.hidden) == Some(true) {
            hidden_rows.insert(row);
        }
    }
}

pub(super) fn record_ods_manual_breaks(
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

pub(super) fn ods_cell_base_font(
    styles: &OdsResolvedStyles,
    default_format: Option<&CellStyle>,
    row_formats: &BTreeMap<u32, CellStyle>,
    col_formats: &BTreeMap<u16, CellStyle>,
    style_reference: (Option<&str>, bool),
    row: u32,
    col: u16,
) -> Font {
    // ODF 1.2 §19.615 applies a row/column default only when a cell has no
    // explicit style, and the row default takes precedence over the column
    // default. Select the complete style before reading one component.
    let (style_name, style_name_invalid) = style_reference;
    if style_name_invalid {
        return Font::default();
    }
    if let Some(name) = style_name {
        return styles
            .cell
            .get(name)
            .and_then(|style| style.to_cell_style().font)
            .unwrap_or_default();
    }
    row_formats
        .get(&row)
        .or_else(|| col_formats.get(&col))
        .or(default_format)
        .and_then(|style| style.font.clone())
        .unwrap_or_default()
}

pub(super) enum OdsCellNumberFormat<'a> {
    General,
    Resolved(&'a str),
    Unresolved,
}

fn ods_render_number_format_state(
    state: OdsNumberFormatState,
    style: Option<&CellStyle>,
) -> OdsCellNumberFormat<'_> {
    match state {
        OdsNumberFormatState::General => OdsCellNumberFormat::General,
        OdsNumberFormatState::Resolved => style
            .and_then(|style| style.num_fmt.as_deref())
            .map(OdsCellNumberFormat::Resolved)
            .unwrap_or(OdsCellNumberFormat::Unresolved),
        OdsNumberFormatState::Unresolved => OdsCellNumberFormat::Unresolved,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn ods_cell_number_format<'a>(
    explicit: Option<&'a CellStyle>,
    explicit_state: Option<OdsNumberFormatState>,
    row_formats: &'a BTreeMap<u32, CellStyle>,
    row_states: &BTreeMap<u32, OdsNumberFormatState>,
    col_formats: &'a BTreeMap<u16, CellStyle>,
    col_states: &BTreeMap<u16, OdsNumberFormatState>,
    default_format: Option<&'a CellStyle>,
    default_state: Option<OdsNumberFormatState>,
    row: u32,
    col: u16,
) -> OdsCellNumberFormat<'a> {
    // Keep the same whole-style precedence required by ODF 1.2 §19.615; a
    // component missing from the selected style has General semantics. A
    // present-but-unresolved ODF data style is distinct: retain its producer
    // display cache instead of inventing explicit decimal precision.
    if let Some(state) = explicit_state {
        return ods_render_number_format_state(state, explicit);
    }
    if let Some(state) = row_states.get(&row) {
        return ods_render_number_format_state(*state, row_formats.get(&row));
    }
    if let Some(state) = col_states.get(&col) {
        return ods_render_number_format_state(*state, col_formats.get(&col));
    }
    if let Some(state) = default_state {
        return ods_render_number_format_state(state, default_format);
    }
    OdsCellNumberFormat::General
}

pub(super) fn ods_text_font(props: Option<&OdsStyleProps>, mut base: Font) -> Font {
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

pub(super) fn flush_ods_run(
    text: &str,
    start: &mut usize,
    runs: &mut Vec<crate::TextRun>,
    font: &Font,
) {
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
