//! XLSX style-table parsing and style model resolution.

use std::collections::HashMap;

use quick_xml::events::Event;
use quick_xml::Reader;

use super::theme::{apply_tint, color_attr, parse_color, ThemeColors};
use super::{attr, attr_true, local, parse_bool_attr, unique_attr, unique_parsed_attr};
use crate::model::{CellStyleOverlay, TableStyleDefinition, TableStyleRegion};
use crate::{
    format, Alignment, Border, BorderStyle, CellProtection, CellStyle, Color, Fill, Font,
    FormatPattern, FormatScript, HAlign, StyleLoss, StyleLossKind, VAlign,
};
pub(super) const MAX_XLSX_STYLE_RECORDS: usize = 65_536;
const MAX_XLSX_CUSTOM_NUMBER_FORMATS: usize = 65_536;
pub(super) const MAX_XLSX_FORMAT_CODE_BYTES: usize = 4_096;
pub(super) const MAX_XLSX_INDEXED_COLORS: usize = 256;
// [MS-OI29500] Part 1 §18.4.11: Office accepts SpreadsheetML font sizes from
// 1 through 409.55 points. The verified renderer sidecar is deliberately
// narrower because the public Font model uses whole points: only exact
// integral source values through 409 are eligible.
const MAX_VERIFIED_XLSX_FONT_SIZE_POINTS: u16 = 409;

/// Per-style number format, derived from `styles.xml`.
#[derive(Default)]
pub(super) struct Styles {
    /// `numFmtId` per `cellXfs` style index.
    pub(super) xf_numfmt: Vec<u16>,
    /// Custom `formatCode` strings keyed by `numFmtId`.
    pub(super) custom: HashMap<u16, String>,
    /// Custom OOXML indexed color table from `<colors><indexedColors>`.
    pub(super) indexed_colors: Vec<Color>,
    /// Full differential styles and typed parse losses per `dxfs` index.
    pub(super) differential_styles: Vec<DifferentialStyle>,
    /// Common public style subset per `cellXfs` index.
    pub(super) cell_styles: Vec<CellStyle>,
    /// Exact integral Normal-style font size retained only when the first cell
    /// XF and named/built-in Normal style resolve to the same source font.
    pub(super) xlsx_normal_font_size_pt: Option<u16>,
    /// Exact integral source font size per retained `cellXfs` record.
    ///
    /// Entries are `None` when the XF/font provenance is inherited,
    /// fractional, malformed, duplicated, out of range, or otherwise
    /// ambiguous.
    pub(super) xlsx_cell_xf_font_sizes_pt: Vec<Option<u16>>,
    /// Sparse direct-format overlays per `cellXfs` style index.
    pub(super) cell_style_overlays: Vec<CellStyleOverlay>,
    /// Imported custom table region styles keyed by `<tableStyle name>`.
    pub(super) table_styles: HashMap<String, ParsedTableStyle>,
    /// Workbook-global style-table truncation and parse losses.
    pub(super) losses: Vec<StyleLoss>,
}

impl Styles {
    pub(super) fn format_id(&self, style_idx: usize) -> u16 {
        self.xf_numfmt.get(style_idx).copied().unwrap_or(0)
    }

    pub(super) fn kind(&self, style_idx: usize) -> format::Kind {
        let numfmt_id = self.format_id(style_idx);
        format::classify(numfmt_id, self.custom.get(&numfmt_id).map(String::as_str))
    }

    pub(super) fn custom_format(&self, style_idx: usize) -> Option<&str> {
        let numfmt_id = self.xf_numfmt.get(style_idx).copied()?;
        self.custom.get(&numfmt_id).map(String::as_str)
    }

    pub(super) fn render_text(&self, style_idx: usize, value: &str) -> String {
        self.custom_format(style_idx).map_or_else(
            || value.to_string(),
            |code| format::render_text_format(value, code),
        )
    }

    pub(super) fn differential_style(&self, dxf_id: usize) -> Option<&DifferentialStyle> {
        self.differential_styles.get(dxf_id)
    }

    pub(super) fn cell_style(&self, style_idx: usize) -> Option<&CellStyle> {
        self.cell_styles.get(style_idx)
    }

    pub(super) fn xlsx_cell_font_size_pt(&self, style_idx: usize) -> Option<u16> {
        self.xlsx_cell_xf_font_sizes_pt
            .get(style_idx)
            .copied()
            .flatten()
    }

    pub(super) fn cell_style_overlay(&self, style_idx: usize) -> Option<&CellStyleOverlay> {
        self.cell_style_overlays.get(style_idx)
    }

    pub(super) fn table_style(&self, name: &str, theme: &ThemeColors) -> Option<ParsedTableStyle> {
        self.table_styles
            .get(name)
            .cloned()
            .or_else(|| built_in_table_style(name, theme))
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct DifferentialStyle {
    pub(super) style: CellStyle,
    pub(super) losses: Vec<StyleLoss>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ParsedTableStyle {
    pub(super) definition: TableStyleDefinition,
    pub(super) losses: Vec<StyleLoss>,
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

pub(super) fn parse_styles(xml: &str, theme: &ThemeColors) -> Styles {
    let mut r = Reader::from_str(xml);
    let mut styles = Styles::default();
    if !theme.source_valid() {
        add_differential_loss(&mut styles.losses, StyleLossKind::UnsupportedProperty, 1);
    }
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
    let (fonts, exact_font_sizes, font_table_complete) =
        parse_font_table(xml, theme, &styles.indexed_colors, &mut styles.losses);
    styles.xlsx_normal_font_size_pt =
        verified_xlsx_normal_font_size(xml, &fonts, &exact_font_sizes, font_table_complete);
    styles.xlsx_cell_xf_font_sizes_pt =
        verified_xlsx_cell_xf_font_sizes(xml, &exact_font_sizes, font_table_complete);
    let (cell_styles, cell_style_overlays) = parse_cell_styles(
        xml,
        theme,
        &styles.indexed_colors,
        &styles.custom,
        &fonts,
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

pub(super) fn add_differential_loss(
    losses: &mut Vec<StyleLoss>,
    kind: StyleLossKind,
    occurrences: u32,
) {
    if occurrences == 0 {
        return;
    }
    if let Some(loss) = losses.iter_mut().find(|loss| loss.kind == kind) {
        loss.occurrences = loss.occurrences.saturating_add(occurrences);
    } else {
        losses.push(StyleLoss { kind, occurrences });
    }
}

pub(super) fn retain_xlsx_style_record<T>(
    records: &mut Vec<T>,
    value: T,
    losses: &mut Vec<StyleLoss>,
) {
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

pub(super) fn parse_table_styles(
    xml: &str,
    dxfs: &[DifferentialStyle],
) -> HashMap<String, ParsedTableStyle> {
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

pub(super) fn built_in_table_style(name: &str, theme: &ThemeColors) -> Option<ParsedTableStyle> {
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
pub(super) fn built_in_table_header_style(name: &str, theme: &ThemeColors) -> Option<CellStyle> {
    built_in_table_style(name, theme).and_then(|style| {
        style
            .definition
            .get(TableStyleRegion::HeaderRow)
            .map(|element| element.style.clone())
    })
}

pub(super) fn parse_font_table(
    xml: &str,
    theme: &ThemeColors,
    indexed: &[Color],
    losses: &mut Vec<StyleLoss>,
) -> (Vec<Font>, Vec<Option<u16>>, bool) {
    #[derive(Clone, Copy)]
    enum ExactSize {
        Absent,
        Valid(u16),
        Invalid,
    }

    impl ExactSize {
        fn observe(self, value: Option<u16>) -> Self {
            match self {
                Self::Absent => value.map_or(Self::Invalid, Self::Valid),
                Self::Valid(_) | Self::Invalid => Self::Invalid,
            }
        }

        fn invalidate(self) -> Self {
            Self::Invalid
        }

        fn value(self) -> Option<u16> {
            match self {
                Self::Valid(value) => Some(value),
                Self::Absent | Self::Invalid => None,
            }
        }
    }

    let mut reader = Reader::from_str(xml);
    let mut depth = 0_usize;
    let mut saw_root = false;
    let mut root_open = false;
    let mut in_fonts = false;
    let mut fonts_depth = None;
    let mut saw_fonts = false;
    let mut font_records_seen = 0_usize;
    let mut provenance_complete = true;
    let mut current: Option<Font> = None;
    let mut current_font_depth = None;
    let mut current_exact_size = ExactSize::Absent;
    let mut current_saw_name = false;
    let mut current_saw_bold = false;
    let mut current_saw_italic = false;
    let mut current_saw_vert_align = false;
    let mut fonts = Vec::new();
    let mut exact_sizes = Vec::new();
    let retain_font = |fonts: &mut Vec<Font>,
                       exact_sizes: &mut Vec<Option<u16>>,
                       font: Font,
                       exact_size: Option<u16>,
                       losses: &mut Vec<StyleLoss>| {
        let previous_len = fonts.len();
        retain_xlsx_style_record(fonts, font, losses);
        if fonts.len() > previous_len {
            exact_sizes.push(exact_size);
        }
    };
    loop {
        match reader.read_event() {
            Ok(event @ (Event::Start(_) | Event::Empty(_))) => {
                let (e, is_empty) = match event {
                    Event::Start(e) => (e, false),
                    Event::Empty(e) => (e, true),
                    _ => unreachable!(),
                };
                let qualified_name = e.name();
                let name = local(qualified_name.as_ref());
                let element_depth = depth;
                if element_depth == 0 {
                    if saw_root || name != b"styleSheet" {
                        provenance_complete = false;
                    }
                    saw_root = true;
                    root_open = !is_empty;
                }
                match name {
                    b"fonts" => {
                        let is_direct_table = element_depth == 1 && root_open;
                        if !is_direct_table || saw_fonts || in_fonts || current.is_some() {
                            provenance_complete = false;
                        }
                        saw_fonts = true;
                        in_fonts = is_direct_table && !is_empty;
                        fonts_depth = in_fonts.then_some(element_depth);
                    }
                    b"font" if in_fonts => {
                        let is_direct_record =
                            fonts_depth.is_some_and(|table| element_depth == table + 1);
                        if !is_direct_record {
                            provenance_complete = false;
                        } else {
                            if current.is_some() {
                                provenance_complete = false;
                                retain_font(
                                    &mut fonts,
                                    &mut exact_sizes,
                                    current.take().unwrap_or_default(),
                                    current_exact_size.value().filter(|_| current_saw_name),
                                    losses,
                                );
                            }
                            font_records_seen = font_records_seen.saturating_add(1);
                            if font_records_seen > MAX_XLSX_STYLE_RECORDS {
                                provenance_complete = false;
                                add_differential_loss(losses, StyleLossKind::LimitExceeded, 1);
                                current = None;
                                current_font_depth = None;
                            } else {
                                current = Some(Font::default());
                                current_font_depth = Some(element_depth);
                                current_exact_size = ExactSize::Absent;
                                current_saw_name = false;
                                current_saw_bold = false;
                                current_saw_italic = false;
                                current_saw_vert_align = false;
                                if is_empty {
                                    retain_font(
                                        &mut fonts,
                                        &mut exact_sizes,
                                        current.take().unwrap_or_default(),
                                        current_exact_size.value().filter(|_| current_saw_name),
                                        losses,
                                    );
                                    current_font_depth = None;
                                }
                            }
                        }
                    }
                    b"name" if current.is_some() => {
                        if current_font_depth.is_none_or(|font| element_depth != font + 1) {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        if current_saw_name {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        current_saw_name = true;
                        let source = match unique_attr(&e, b"val") {
                            Ok(Some(value)) if !value.is_empty() => Some(value),
                            Ok(None) => {
                                current_exact_size = current_exact_size.invalidate();
                                None
                            }
                            Ok(Some(value)) => {
                                current_exact_size = current_exact_size.invalidate();
                                Some(value)
                            }
                            Err(()) => {
                                current_exact_size = current_exact_size.invalidate();
                                attr(&e, b"val")
                            }
                        };
                        current.as_mut().expect("font").name = source;
                    }
                    b"sz" if current.is_some() => {
                        if current_font_depth.is_none_or(|font| element_depth != font + 1) {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        let source = match unique_attr(&e, b"val") {
                            Ok(value) => value,
                            Err(()) => {
                                current_exact_size = current_exact_size.invalidate();
                                attr(&e, b"val")
                            }
                        };
                        if !matches!(current_exact_size, ExactSize::Invalid) {
                            current_exact_size = current_exact_size
                                .observe(source.as_deref().and_then(exact_integral_xlsx_font_size));
                        }
                        current.as_mut().expect("font").size_pt = source
                            .and_then(|value| value.parse::<f32>().ok())
                            .map(|value| value.round().clamp(1.0, f32::from(u16::MAX)) as u16);
                    }
                    b"color" if current.is_some() => {
                        current.as_mut().expect("font").color = color_attr(&e, theme, indexed);
                    }
                    b"b" if current.is_some() => {
                        if current_font_depth.is_none_or(|font| element_depth != font + 1)
                            || current_saw_bold
                        {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        current_saw_bold = true;
                        let enabled = match unique_attr(&e, b"val") {
                            Ok(None) => true,
                            Ok(Some(value)) => matches!(value.as_str(), "1" | "true" | "on"),
                            Err(()) => false,
                        };
                        if !enabled {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        current.as_mut().expect("font").bold = true;
                    }
                    b"i" if current.is_some() => {
                        if current_font_depth.is_none_or(|font| element_depth != font + 1)
                            || current_saw_italic
                        {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        current_saw_italic = true;
                        let enabled = match unique_attr(&e, b"val") {
                            Ok(None) => true,
                            Ok(Some(value)) => matches!(value.as_str(), "1" | "true" | "on"),
                            Err(()) => false,
                        };
                        if !enabled {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        current.as_mut().expect("font").italic = true;
                    }
                    b"u" if current.is_some() => current.as_mut().expect("font").underline = true,
                    b"strike" if current.is_some() => {
                        current.as_mut().expect("font").strikethrough = true;
                    }
                    b"vertAlign" if current.is_some() => {
                        if current_font_depth.is_none_or(|font| element_depth != font + 1)
                            || current_saw_vert_align
                        {
                            current_exact_size = current_exact_size.invalidate();
                        }
                        current_saw_vert_align = true;
                        let source = match unique_attr(&e, b"val") {
                            Ok(value) => value,
                            Err(()) => {
                                current_exact_size = current_exact_size.invalidate();
                                attr(&e, b"val")
                            }
                        };
                        current.as_mut().expect("font").script = match source.as_deref() {
                            Some("superscript") => FormatScript::Superscript,
                            Some("subscript") => FormatScript::Subscript,
                            Some("baseline") => FormatScript::None,
                            _ => {
                                current_exact_size = current_exact_size.invalidate();
                                FormatScript::None
                            }
                        };
                    }
                    _ => {}
                }
                if !is_empty {
                    depth = depth.saturating_add(1);
                }
            }
            Ok(Event::End(e)) => {
                if depth == 0 {
                    provenance_complete = false;
                    continue;
                }
                depth -= 1;
                let qualified_name = e.name();
                let name = local(qualified_name.as_ref());
                let element_depth = depth;
                if element_depth == 0 {
                    if name != b"styleSheet" || !root_open {
                        provenance_complete = false;
                    }
                    root_open = false;
                }
                match name {
                    b"font" if current.is_some() && current_font_depth == Some(element_depth) => {
                        retain_font(
                            &mut fonts,
                            &mut exact_sizes,
                            current.take().unwrap_or_default(),
                            current_exact_size.value().filter(|_| current_saw_name),
                            losses,
                        );
                        current_font_depth = None;
                    }
                    b"font" if in_fonts => {
                        provenance_complete = false;
                    }
                    b"fonts" => {
                        if !in_fonts || fonts_depth != Some(element_depth) || current.is_some() {
                            provenance_complete = false;
                        }
                        in_fonts = false;
                        fonts_depth = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => {
                if depth != 0
                    || !saw_root
                    || root_open
                    || in_fonts
                    || fonts_depth.is_some()
                    || current.is_some()
                    || current_font_depth.is_some()
                {
                    provenance_complete = false;
                }
                break;
            }
            Err(_) => {
                provenance_complete = false;
                exact_sizes.fill(None);
                break;
            }
            _ => {}
        }
    }
    (fonts, exact_sizes, provenance_complete)
}

fn exact_integral_xlsx_font_size(value: &str) -> Option<u16> {
    if value.is_empty() || value.len() > 64 {
        return None;
    }
    let (mantissa, exponent) = match value.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => {
            if exponent.contains(['e', 'E']) {
                return None;
            }
            (mantissa, exponent.parse::<i32>().ok()?)
        }
        None => (value, 0),
    };
    let mantissa = mantissa.strip_prefix('+').unwrap_or(mantissa);
    if mantissa.is_empty() || mantissa.starts_with('-') {
        return None;
    }

    let mut digits = 0_u128;
    let mut saw_digit = false;
    let mut saw_decimal = false;
    let mut fractional_digits = 0_i32;
    for byte in mantissa.bytes() {
        match byte {
            b'0'..=b'9' => {
                saw_digit = true;
                digits = digits
                    .checked_mul(10)?
                    .checked_add(u128::from(byte - b'0'))?;
                if saw_decimal {
                    fractional_digits = fractional_digits.checked_add(1)?;
                }
            }
            b'.' if !saw_decimal => saw_decimal = true,
            _ => return None,
        }
    }
    if !saw_digit {
        return None;
    }

    let decimal_scale = fractional_digits.checked_sub(exponent)?;
    let integral = if decimal_scale >= 0 {
        let divisor = 10_u128.checked_pow(decimal_scale as u32)?;
        if digits % divisor != 0 {
            return None;
        }
        digits / divisor
    } else {
        digits.checked_mul(10_u128.checked_pow(decimal_scale.unsigned_abs())?)?
    };
    let points = u16::try_from(integral).ok()?;
    (1..=MAX_VERIFIED_XLSX_FONT_SIZE_POINTS)
        .contains(&points)
        .then_some(points)
}

pub(super) fn verified_xlsx_normal_font_size(
    xml: &str,
    fonts: &[Font],
    exact_sizes: &[Option<u16>],
    font_table_complete: bool,
) -> Option<u16> {
    if !font_table_complete {
        return None;
    }

    let mut reader = Reader::from_str(xml);
    let mut depth = 0_usize;
    let mut in_cell_style_xfs = false;
    let mut in_cell_xfs = false;
    let mut in_cell_styles = false;
    let mut cell_style_xfs_depth = None;
    let mut cell_xfs_depth = None;
    let mut cell_styles_depth = None;
    let mut saw_cell_style_xfs = false;
    let mut saw_cell_xfs = false;
    let mut saw_cell_styles = false;
    let mut cell_style_xf_font_ids = Vec::<Option<usize>>::new();
    let mut cell_xf_count = 0_usize;
    let mut cell_style_count = 0_usize;
    let mut first_cell_xf_font_id = None;
    let mut first_cell_xf_style_id = None;
    let mut saw_first_cell_xf = false;
    let mut normal_xf_id = None;
    let mut saw_normal_style = false;
    let mut normal_is_ambiguous = false;

    loop {
        match reader.read_event() {
            Ok(event @ (Event::Start(_) | Event::Empty(_))) => {
                let (e, is_empty) = match event {
                    Event::Start(e) => (e, false),
                    Event::Empty(e) => (e, true),
                    _ => unreachable!(),
                };
                let element_depth = depth;
                match local(e.name().as_ref()) {
                    b"cellStyleXfs" => {
                        if element_depth != 1
                            || saw_cell_style_xfs
                            || in_cell_style_xfs
                            || in_cell_xfs
                            || in_cell_styles
                        {
                            return None;
                        }
                        saw_cell_style_xfs = true;
                        in_cell_style_xfs = !is_empty;
                        cell_style_xfs_depth = in_cell_style_xfs.then_some(element_depth);
                    }
                    b"cellXfs" => {
                        if element_depth != 1
                            || saw_cell_xfs
                            || in_cell_style_xfs
                            || in_cell_xfs
                            || in_cell_styles
                        {
                            return None;
                        }
                        saw_cell_xfs = true;
                        in_cell_xfs = !is_empty;
                        cell_xfs_depth = in_cell_xfs.then_some(element_depth);
                    }
                    b"cellStyles" => {
                        if element_depth != 1
                            || saw_cell_styles
                            || in_cell_style_xfs
                            || in_cell_xfs
                            || in_cell_styles
                        {
                            return None;
                        }
                        saw_cell_styles = true;
                        in_cell_styles = !is_empty;
                        cell_styles_depth = in_cell_styles.then_some(element_depth);
                    }
                    b"xf" if in_cell_style_xfs => {
                        if cell_style_xfs_depth.is_none_or(|table| element_depth != table + 1) {
                            return None;
                        }
                        if cell_style_xf_font_ids.len() >= MAX_XLSX_STYLE_RECORDS {
                            return None;
                        }
                        cell_style_xf_font_ids
                            .push(unique_parsed_attr::<usize>(&e, b"fontId").ok()?);
                    }
                    b"xf" if in_cell_xfs => {
                        if cell_xfs_depth.is_none_or(|table| element_depth != table + 1) {
                            return None;
                        }
                        if cell_xf_count >= MAX_XLSX_STYLE_RECORDS {
                            return None;
                        }
                        cell_xf_count += 1;
                        if !saw_first_cell_xf {
                            saw_first_cell_xf = true;
                            first_cell_xf_font_id =
                                unique_parsed_attr::<usize>(&e, b"fontId").ok()?;
                            first_cell_xf_style_id =
                                unique_parsed_attr::<usize>(&e, b"xfId").ok()?;
                        }
                    }
                    b"cellStyle" if in_cell_styles => {
                        if cell_styles_depth.is_none_or(|table| element_depth != table + 1) {
                            return None;
                        }
                        if cell_style_count >= MAX_XLSX_STYLE_RECORDS {
                            return None;
                        }
                        cell_style_count += 1;
                        let builtin_id = unique_parsed_attr::<u32>(&e, b"builtinId").ok()?;
                        let name = unique_attr(&e, b"name").ok()?;
                        let named_normal = name
                            .as_deref()
                            .is_some_and(|name| name.eq_ignore_ascii_case("Normal"));
                        if named_normal && builtin_id.is_some_and(|id| id != 0) {
                            normal_is_ambiguous = true;
                        }
                        let is_normal =
                            builtin_id == Some(0) || (builtin_id.is_none() && named_normal);
                        if is_normal {
                            let candidate = unique_parsed_attr::<usize>(&e, b"xfId").ok()?;
                            if candidate.is_none() || saw_normal_style {
                                normal_is_ambiguous = true;
                            } else {
                                normal_xf_id = candidate;
                            }
                            saw_normal_style = true;
                        }
                    }
                    _ => {}
                }
                if !is_empty {
                    depth = depth.checked_add(1)?;
                }
            }
            Ok(Event::End(e)) => {
                depth = depth.checked_sub(1)?;
                match local(e.name().as_ref()) {
                    b"cellStyleXfs" => {
                        if !in_cell_style_xfs || cell_style_xfs_depth != Some(depth) {
                            return None;
                        }
                        in_cell_style_xfs = false;
                        cell_style_xfs_depth = None;
                    }
                    b"cellXfs" => {
                        if !in_cell_xfs || cell_xfs_depth != Some(depth) {
                            return None;
                        }
                        in_cell_xfs = false;
                        cell_xfs_depth = None;
                    }
                    b"cellStyles" => {
                        if !in_cell_styles || cell_styles_depth != Some(depth) {
                            return None;
                        }
                        in_cell_styles = false;
                        cell_styles_depth = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => {
                if depth != 0
                    || in_cell_style_xfs
                    || in_cell_xfs
                    || in_cell_styles
                    || cell_style_xfs_depth.is_some()
                    || cell_xfs_depth.is_some()
                    || cell_styles_depth.is_some()
                {
                    return None;
                }
                break;
            }
            Err(_) => return None,
            _ => {}
        }
    }

    if normal_is_ambiguous {
        return None;
    }
    let normal_style_id = normal_xf_id?;
    if first_cell_xf_style_id != Some(normal_style_id) {
        return None;
    }
    let first_font_id = first_cell_xf_font_id?;
    let normal_font_id = cell_style_xf_font_ids.get(normal_style_id)?.as_ref()?;
    let first_font = fonts.get(first_font_id)?;
    let normal_font = fonts.get(*normal_font_id)?;
    let first_points = exact_sizes.get(first_font_id).copied().flatten()?;
    let normal_points = exact_sizes.get(*normal_font_id).copied().flatten()?;
    (first_points == normal_points && first_font == normal_font).then_some(first_points)
}

#[derive(Clone, Copy)]
enum XlsxCellXfFontUse {
    Explicit(bool),
    Implicit { style_xf_id: Option<usize> },
}

#[derive(Clone, Copy)]
struct XlsxCellXfFontCandidate {
    font_id: usize,
    font_use: XlsxCellXfFontUse,
}

fn xlsx_cell_xf_font_candidate(
    e: &quick_xml::events::BytesStart<'_>,
) -> Option<XlsxCellXfFontCandidate> {
    let font_id = unique_parsed_attr::<usize>(e, b"fontId").ok().flatten()?;
    let font_use = match unique_attr(e, b"applyFont") {
        Ok(None) => XlsxCellXfFontUse::Implicit {
            style_xf_id: unique_parsed_attr::<usize>(e, b"xfId").ok()?,
        },
        Ok(Some(value)) => XlsxCellXfFontUse::Explicit(parse_bool_attr(&value)?),
        Err(()) => return None,
    };
    Some(XlsxCellXfFontCandidate { font_id, font_use })
}

fn exact_xlsx_cell_xf_font_size(
    candidate: XlsxCellXfFontCandidate,
    exact_sizes: &[Option<u16>],
    cell_style_xf_count: usize,
) -> Option<u16> {
    // LibreOffice's Xf::importXf/createPattern contract applies an omitted
    // `applyFont` directly when `xfId` is absent. With a parent style XF, the
    // cell font is still effective when it differs and otherwise names the
    // same font as the parent. A missing or invalid parent cannot establish
    // that provenance, so fail closed before using Calc's declared-height path.
    let applies = match candidate.font_use {
        XlsxCellXfFontUse::Explicit(applies) => applies,
        XlsxCellXfFontUse::Implicit { style_xf_id: None } => true,
        XlsxCellXfFontUse::Implicit {
            style_xf_id: Some(style_xf_id),
        } => style_xf_id < cell_style_xf_count,
    };
    if !applies {
        return None;
    }
    exact_sizes.get(candidate.font_id).copied().flatten()
}

fn verified_xlsx_cell_xf_font_sizes(
    xml: &str,
    exact_sizes: &[Option<u16>],
    font_table_complete: bool,
) -> Vec<Option<u16>> {
    if !font_table_complete {
        return Vec::new();
    }

    let mut reader = Reader::from_str(xml);
    let mut depth = 0_usize;
    let mut saw_cell_style_xfs = false;
    let mut saw_cell_xfs = false;
    let mut in_cell_style_xfs = false;
    let mut in_cell_xfs = false;
    let mut cell_style_xfs_depth = None;
    let mut cell_xfs_depth = None;
    let mut current_style_xf_depth = None;
    let mut current_xf_depth = None;
    let mut cell_style_xf_count = 0_usize;
    let mut candidates = Vec::new();

    loop {
        match reader.read_event() {
            Ok(event @ (Event::Start(_) | Event::Empty(_))) => {
                let (e, is_empty) = match event {
                    Event::Start(e) => (e, false),
                    Event::Empty(e) => (e, true),
                    _ => unreachable!(),
                };
                let element_depth = depth;
                match local(e.name().as_ref()) {
                    b"cellStyleXfs" => {
                        if element_depth != 1
                            || saw_cell_style_xfs
                            || in_cell_style_xfs
                            || in_cell_xfs
                        {
                            return Vec::new();
                        }
                        saw_cell_style_xfs = true;
                        in_cell_style_xfs = !is_empty;
                        cell_style_xfs_depth = in_cell_style_xfs.then_some(element_depth);
                    }
                    b"cellXfs" => {
                        if element_depth != 1 || saw_cell_xfs || in_cell_style_xfs || in_cell_xfs {
                            return Vec::new();
                        }
                        saw_cell_xfs = true;
                        in_cell_xfs = !is_empty;
                        cell_xfs_depth = in_cell_xfs.then_some(element_depth);
                    }
                    b"xf" if in_cell_style_xfs => {
                        if cell_style_xfs_depth.is_none_or(|table| element_depth != table + 1)
                            || current_style_xf_depth.is_some()
                            || cell_style_xf_count >= MAX_XLSX_STYLE_RECORDS
                        {
                            return Vec::new();
                        }
                        cell_style_xf_count += 1;
                        if !is_empty {
                            current_style_xf_depth = Some(element_depth);
                        }
                    }
                    b"xf" if in_cell_xfs => {
                        if cell_xfs_depth.is_none_or(|table| element_depth != table + 1)
                            || current_xf_depth.is_some()
                            || candidates.len() >= MAX_XLSX_STYLE_RECORDS
                        {
                            return Vec::new();
                        }
                        candidates.push(xlsx_cell_xf_font_candidate(&e));
                        if !is_empty {
                            current_xf_depth = Some(element_depth);
                        }
                    }
                    _ => {}
                }
                if !is_empty {
                    let Some(next_depth) = depth.checked_add(1) else {
                        return Vec::new();
                    };
                    depth = next_depth;
                }
            }
            Ok(Event::End(e)) => {
                let Some(next_depth) = depth.checked_sub(1) else {
                    return Vec::new();
                };
                depth = next_depth;
                match local(e.name().as_ref()) {
                    b"xf" if in_cell_style_xfs => {
                        if current_style_xf_depth != Some(depth) {
                            return Vec::new();
                        }
                        current_style_xf_depth = None;
                    }
                    b"xf" if in_cell_xfs => {
                        if current_xf_depth != Some(depth) {
                            return Vec::new();
                        }
                        current_xf_depth = None;
                    }
                    b"cellStyleXfs" => {
                        if !in_cell_style_xfs
                            || cell_style_xfs_depth != Some(depth)
                            || current_style_xf_depth.is_some()
                        {
                            return Vec::new();
                        }
                        in_cell_style_xfs = false;
                        cell_style_xfs_depth = None;
                    }
                    b"cellXfs" => {
                        if !in_cell_xfs
                            || cell_xfs_depth != Some(depth)
                            || current_xf_depth.is_some()
                        {
                            return Vec::new();
                        }
                        in_cell_xfs = false;
                        cell_xfs_depth = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => {
                if depth != 0
                    || in_cell_style_xfs
                    || in_cell_xfs
                    || cell_style_xfs_depth.is_some()
                    || cell_xfs_depth.is_some()
                    || current_style_xf_depth.is_some()
                    || current_xf_depth.is_some()
                {
                    return Vec::new();
                }
                break;
            }
            Err(_) => return Vec::new(),
            _ => {}
        }
    }
    candidates
        .into_iter()
        .map(|candidate| {
            candidate.and_then(|candidate| {
                exact_xlsx_cell_xf_font_size(candidate, exact_sizes, cell_style_xf_count)
            })
        })
        .collect()
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
    fonts: &[Font],
    losses: &mut Vec<StyleLoss>,
) -> (Vec<CellStyle>, Vec<CellStyleOverlay>) {
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
                        cell_style_from_xf(&e, fonts, &fills, &borders, custom),
                        cell_style_overlay_from_xf(&e, fonts, &fills, &borders, custom),
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
                        cell_style_from_xf(&e, fonts, &fills, &borders, custom),
                        cell_style_overlay_from_xf(&e, fonts, &fills, &borders, custom),
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
