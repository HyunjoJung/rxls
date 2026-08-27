use std::collections::{HashMap, HashSet};

use crate::{
    format, Alignment, Border, BorderStyle, CellProtection, CellStyle, Color, Fill, Font,
    FormatPattern, FormatScript, HAlign, StyleLoss, StyleLossKind, VAlign,
};

use super::{
    u16le, u32le, wide_string, RecReader, BRT_AC_BEGIN, BRT_AC_END, BRT_BEGIN_CELL_STYLE_XFS,
    BRT_BEGIN_CELL_XFS, BRT_BEGIN_FONTS, BRT_BEGIN_STYLES, BRT_BEGIN_STYLE_SHEET, BRT_BORDER,
    BRT_END_CELL_STYLE_XFS, BRT_END_CELL_XFS, BRT_END_FONTS, BRT_END_STYLES, BRT_END_STYLE_SHEET,
    BRT_FILL, BRT_FMT, BRT_FONT, BRT_STYLE, BRT_XF, MAX_VERIFIED_XLSB_FONT_SIZE_POINTS,
    MAX_VERIFIED_XLSB_STYLE_RECORDS, MAX_XLSB_FONT_RECORDS, MAX_XLSB_STYLE_RECORDS,
};

#[derive(Clone)]
pub(super) struct XlsbTheme {
    pub(super) colors: [Option<Color>; 12],
    pub(super) major_latin_font_family: Option<String>,
    pub(super) minor_latin_font_family: Option<String>,
    pub(super) source_valid: bool,
}

impl Default for XlsbTheme {
    fn default() -> Self {
        Self {
            colors: [None; 12],
            major_latin_font_family: None,
            minor_latin_font_family: None,
            source_valid: true,
        }
    }
}

impl XlsbTheme {
    pub(super) fn invalid() -> Self {
        Self {
            source_valid: false,
            ..Self::default()
        }
    }

    pub(super) fn chart_palette(&self) -> Vec<Color> {
        const OFFICE_ACCENTS: [Color; 6] = [
            Color::rgb(68, 114, 196),
            Color::rgb(237, 125, 49),
            Color::rgb(165, 165, 165),
            Color::rgb(255, 192, 0),
            Color::rgb(91, 155, 213),
            Color::rgb(112, 173, 71),
        ];
        (0..OFFICE_ACCENTS.len())
            .map(|index| self.colors[index + 4].unwrap_or(OFFICE_ACCENTS[index]))
            .collect()
    }

    pub(super) fn chart_default_latin_font_family(&self) -> &str {
        self.minor_latin_font_family
            .as_deref()
            .unwrap_or(crate::xlsx::CALC_IMPORTED_CHART_LATIN_FONT_FAMILY)
    }
}

pub(super) fn parse_xlsb_theme(xml: &str) -> XlsbTheme {
    let theme = crate::xlsx::parse_theme(xml);
    XlsbTheme {
        colors: theme.xlsb_ordered_colors(),
        major_latin_font_family: theme.source_major_latin_font_family().map(str::to_string),
        minor_latin_font_family: theme.source_minor_latin_font_family().map(str::to_string),
        source_valid: theme.source_valid(),
    }
}

pub(super) fn add_style_loss(losses: &mut Vec<StyleLoss>, kind: StyleLossKind, occurrences: u32) {
    if occurrences == 0 {
        return;
    }
    if let Some(loss) = losses.iter_mut().find(|loss| loss.kind == kind) {
        loss.occurrences = loss.occurrences.saturating_add(occurrences);
    } else {
        losses.push(StyleLoss { kind, occurrences });
    }
}

#[derive(Clone, Default)]
pub(super) struct Styles {
    pub(super) xf_numfmt: Vec<u16>,
    pub(super) custom: HashMap<u16, String>,
    pub(super) fonts: Vec<Font>,
    pub(super) fills: Vec<Fill>,
    pub(super) borders: Vec<Border>,
    pub(super) cell_styles: Vec<CellStyle>,
    /// Exact integral Normal font size proven from complete XLSB style tables.
    pub(super) xlsb_normal_font_size_pt: Option<u16>,
    /// Exact integral effective font size per retained CellXF.
    pub(super) xlsb_cell_xf_font_sizes_pt: Vec<Option<u16>>,
    pub(super) losses: Vec<StyleLoss>,
    pub(super) has_source_styles: bool,
}

impl Styles {
    pub(super) fn format_id(&self, style_idx: usize) -> u16 {
        self.xf_numfmt.get(style_idx).copied().unwrap_or(0)
    }

    pub(super) fn kind(&self, style_idx: usize) -> format::Kind {
        let id = self.format_id(style_idx);
        format::classify(id, self.custom.get(&id).map(String::as_str))
    }

    pub(super) fn custom_format(&self, style_idx: usize) -> Option<&str> {
        let id = self.xf_numfmt.get(style_idx).copied()?;
        self.custom.get(&id).map(String::as_str)
    }

    pub(super) fn render_text(&self, style_idx: usize, value: &str) -> String {
        self.custom_format(style_idx).map_or_else(
            || value.to_string(),
            |code| format::render_text_format(value, code),
        )
    }

    pub(super) fn cell_style(&self, style_idx: usize) -> Option<&CellStyle> {
        self.cell_styles.get(style_idx)
    }

    pub(super) fn xlsb_cell_font_size_pt(&self, style_idx: usize) -> Option<u16> {
        self.xlsb_cell_xf_font_sizes_pt
            .get(style_idx)
            .copied()
            .flatten()
    }
}

#[derive(Clone, Default)]
struct RawXf {
    parent: Option<usize>,
    num_fmt: u16,
    font: usize,
    fill: usize,
    border: usize,
    alignment: Alignment,
    protection: CellProtection,
    changed_groups: u8,
}

fn bounded_wide_string(b: &[u8], offset: usize, max_chars: usize) -> Option<(String, usize)> {
    let chars = usize::try_from(u32le(b, offset)?).ok()?;
    (chars <= max_chars)
        .then(|| wide_string(b, offset))
        .flatten()
}

fn bounded_nullable_wide_string(
    b: &[u8],
    offset: usize,
    max_chars: usize,
) -> Option<(Option<String>, usize)> {
    if u32le(b, offset)? == u32::MAX {
        return Some((None, 4));
    }
    bounded_wide_string(b, offset, max_chars).map(|(value, used)| (Some(value), used))
}

fn tint_color(color: Color, tint: i16) -> Color {
    let factor = f64::from(tint) / if tint < 0 { 32_768.0 } else { 32_767.0 };
    let tint_channel = |channel: u8| {
        let channel = f64::from(channel);
        let result = if factor < 0.0 {
            channel * (1.0 + factor)
        } else {
            channel + (255.0 - channel) * factor
        };
        result.round().clamp(0.0, 255.0) as u8
    };
    let [red, green, blue] = color.as_rgb();
    Color::rgb(tint_channel(red), tint_channel(green), tint_channel(blue))
}

fn indexed_xlsb_color(index: u8) -> Option<Color> {
    Some(match index {
        0 | 8 => Color::rgb(0, 0, 0),
        1 | 9 => Color::rgb(255, 255, 255),
        2 | 10 => Color::rgb(255, 0, 0),
        3 | 11 => Color::rgb(0, 255, 0),
        4 | 12 => Color::rgb(0, 0, 255),
        5 | 13 => Color::rgb(255, 255, 0),
        6 | 14 => Color::rgb(255, 0, 255),
        7 | 15 => Color::rgb(0, 255, 255),
        16 => Color::rgb(128, 0, 0),
        17 => Color::rgb(0, 128, 0),
        18 => Color::rgb(0, 0, 128),
        19 => Color::rgb(128, 128, 0),
        20 => Color::rgb(128, 0, 128),
        21 => Color::rgb(0, 128, 128),
        22 => Color::rgb(192, 192, 192),
        23 => Color::rgb(128, 128, 128),
        _ => return None,
    })
}

fn parse_style_color(p: &[u8], theme: &XlsbTheme, losses: &mut Vec<StyleLoss>) -> Option<Color> {
    let flags = *p.first()?;
    let valid_rgb = flags & 1 != 0;
    let color_type = flags >> 1;
    let index = *p.get(1)?;
    let tint = i16::from_le_bytes([*p.get(2)?, *p.get(3)?]);
    let base = match color_type {
        0 => return None,
        1 => indexed_xlsb_color(index),
        2 if valid_rgb => Some(Color::rgb(*p.get(4)?, *p.get(5)?, *p.get(6)?)),
        3 => theme.colors.get(usize::from(index)).copied().flatten(),
        _ => None,
    };
    if base.is_none() {
        add_style_loss(losses, StyleLossKind::UnresolvedColor, 1);
    }
    base.map(|color| tint_color(color, tint))
}

fn parse_xlsb_font(p: &[u8], theme: &XlsbTheme, losses: &mut Vec<StyleLoss>) -> Font {
    let height_twips = u16le(p, 0).unwrap_or(220);
    if height_twips % 20 != 0 {
        add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
    }
    let flags = u16le(p, 2).unwrap_or_default();
    let weight = u16le(p, 4).unwrap_or(400);
    if !matches!(weight, 400 | 700) {
        add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
    }
    let script = match u16le(p, 6).unwrap_or_default() {
        1 => FormatScript::Superscript,
        2 => FormatScript::Subscript,
        _ => FormatScript::None,
    };
    let name = bounded_wide_string(p, 21, 1_024).map(|(name, _)| name);
    if name.is_none() && u32le(p, 21).is_some_and(|length| length > 0) {
        add_style_loss(losses, StyleLossKind::LimitExceeded, 1);
    }
    let underline = p.get(8).copied().unwrap_or_default();
    if !matches!(underline, 0 | 1) {
        add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
    }
    // Outline, shadow, condensed, and extended font flags have no public
    // model equivalent. Italic and strikethrough are retained below.
    if flags & !0x000A != 0 {
        add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
    }
    Font {
        name,
        size_pt: Some(((u32::from(height_twips) + 10) / 20).clamp(1, u32::from(u16::MAX)) as u16),
        color: p
            .get(12..20)
            .and_then(|color| parse_style_color(color, theme, losses)),
        bold: weight >= 700,
        italic: flags & 0x0002 != 0,
        underline: underline != 0,
        strikethrough: flags & 0x0008 != 0,
        script,
    }
}

fn xlsb_fill_pattern(value: u32) -> FormatPattern {
    match value {
        1 => FormatPattern::Solid,
        2 => FormatPattern::MediumGray,
        3 => FormatPattern::DarkGray,
        4 => FormatPattern::LightGray,
        5 => FormatPattern::DarkHorizontal,
        6 => FormatPattern::DarkVertical,
        7 => FormatPattern::DarkDown,
        8 => FormatPattern::DarkUp,
        9 => FormatPattern::DarkGrid,
        10 => FormatPattern::DarkTrellis,
        11 => FormatPattern::LightHorizontal,
        12 => FormatPattern::LightVertical,
        13 => FormatPattern::LightDown,
        14 => FormatPattern::LightUp,
        15 => FormatPattern::LightGrid,
        16 => FormatPattern::LightTrellis,
        17 => FormatPattern::Gray125,
        18 => FormatPattern::Gray0625,
        _ => FormatPattern::None,
    }
}

fn parse_xlsb_fill(p: &[u8], theme: &XlsbTheme, losses: &mut Vec<StyleLoss>) -> Fill {
    let raw_pattern = u32le(p, 0).unwrap_or_default();
    if raw_pattern == 0x28 {
        add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
        return Fill::default();
    }
    if raw_pattern > 18 {
        add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
    }
    Fill {
        pattern: xlsb_fill_pattern(raw_pattern),
        foreground: p
            .get(4..12)
            .and_then(|color| parse_style_color(color, theme, losses)),
        background: p
            .get(12..20)
            .and_then(|color| parse_style_color(color, theme, losses)),
    }
}

fn xlsb_border_style(value: u8, losses: &mut Vec<StyleLoss>) -> BorderStyle {
    let style = match value {
        0 => BorderStyle::None,
        1 => BorderStyle::Thin,
        2 => BorderStyle::Medium,
        3 | 4 | 7 | 9 | 11 => BorderStyle::Thin,
        8 | 10 | 12 => BorderStyle::Medium,
        5 | 13 => BorderStyle::Thick,
        6 => BorderStyle::Double,
        _ => BorderStyle::None,
    };
    if !matches!(value, 0 | 1 | 2 | 5 | 6) {
        add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
    }
    style
}

fn parse_xlsb_border(p: &[u8], theme: &XlsbTheme, losses: &mut Vec<StyleLoss>) -> Border {
    let mut border = Border::default();
    for (offset, edge) in [(1usize, 0u8), (11, 1), (21, 2), (31, 3)] {
        let style = xlsb_border_style(p.get(offset).copied().unwrap_or_default(), losses);
        let color = p
            .get(offset + 2..offset + 10)
            .and_then(|value| parse_style_color(value, theme, losses));
        match edge {
            0 => {
                border.top = style;
                border.top_color = color;
            }
            1 => {
                border.bottom = style;
                border.bottom_color = color;
            }
            2 => {
                border.left = style;
                border.left_color = color;
            }
            _ => {
                border.right = style;
                border.right_color = color;
            }
        }
    }
    if p.first().is_some_and(|flags| flags & 0x03 != 0) {
        add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
    }
    border
}

fn parse_xlsb_xf(p: &[u8], losses: &mut Vec<StyleLoss>) -> Option<RawXf> {
    let parent = u16le(p, 0)?;
    let alignment_bits = u16le(p, 12).unwrap_or_default();
    let horizontal_code = alignment_bits & 0x07;
    let horizontal = match horizontal_code {
        1 => Some(HAlign::Left),
        2 | 6 | 7 => Some(HAlign::Center),
        3 => Some(HAlign::Right),
        _ => None,
    };
    if matches!(horizontal_code, 4..=7) {
        add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
    }
    let vertical_code = (alignment_bits >> 3) & 0x07;
    let vertical = match vertical_code {
        0 => Some(VAlign::Top),
        1 | 3 | 4 => Some(VAlign::Middle),
        2 => Some(VAlign::Bottom),
        _ => None,
    };
    if vertical_code >= 3 {
        add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
    }
    // The first four bytes carry the parent and number-format identifiers.
    // Preserve that valid prefix when an XF's trailing style-component fields
    // are absent; losing the whole record here also loses date typing.
    let raw_rotation = i16::from(p.get(10).copied().unwrap_or_default());
    let rotation = match raw_rotation {
        0..=90 => raw_rotation,
        91..=180 => 90 - raw_rotation,
        _ => 0,
    };
    if p.get(10).is_some() && raw_rotation > 180 {
        add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
    }
    let raw_indent = p.get(11).copied().unwrap_or_default();
    if raw_indent > 250 {
        add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
    }
    if p.get(12..14).is_some() {
        // Justify-last-line, merge-cell, reading order, PivotTable button, and
        // quote-prefix bits are not represented by `Alignment`.
        for mask in [1 << 7, 1 << 9, 0b11 << 10, 1 << 14, 1 << 15] {
            if alignment_bits & mask != 0 {
                add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
            }
        }
    }
    Some(RawXf {
        parent: (parent != u16::MAX).then_some(usize::from(parent)),
        num_fmt: u16le(p, 2)?,
        font: usize::from(u16le(p, 4).unwrap_or_default()),
        fill: usize::from(u16le(p, 6).unwrap_or_default()),
        border: usize::from(u16le(p, 8).unwrap_or_default()),
        alignment: Alignment {
            horizontal,
            vertical,
            wrap: alignment_bits & (1 << 6) != 0,
            rotation,
            indent: raw_indent.min(250),
            shrink_to_fit: alignment_bits & (1 << 8) != 0,
        },
        protection: CellProtection {
            locked: Some(alignment_bits & (1 << 12) != 0),
            hidden: alignment_bits & (1 << 13) != 0,
        },
        changed_groups: (u16le(p, 14).unwrap_or_default() & 0x3f) as u8,
    })
}

pub(super) fn built_in_xlsb_num_fmt(id: u16) -> Option<&'static str> {
    match id {
        1 => Some("0"),
        2 => Some("0.00"),
        3 => Some("#,##0"),
        4 => Some("#,##0.00"),
        9 => Some("0%"),
        10 => Some("0.00%"),
        11 => Some("0.00E+00"),
        12 => Some("# ?/?"),
        13 => Some("# ??/??"),
        14 => Some("mm-dd-yy"),
        15 => Some("d-mmm-yy"),
        16 => Some("d-mmm"),
        17 => Some("mmm-yy"),
        18 => Some("h:mm AM/PM"),
        19 => Some("h:mm:ss AM/PM"),
        20 => Some("h:mm"),
        21 => Some("h:mm:ss"),
        22 => Some("m/d/yy h:mm"),
        45 => Some("mm:ss"),
        46 => Some("[h]:mm:ss"),
        47 => Some("mm:ss.0"),
        48 => Some("##0.0E+0"),
        49 => Some("@"),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn raw_xf_style(
    xf: &RawXf,
    custom: &HashMap<u16, String>,
    fonts: &[Font],
    fills: &[Fill],
    borders: &[Border],
    parent: Option<&CellStyle>,
    style_xf: bool,
    losses: &mut Vec<StyleLoss>,
) -> CellStyle {
    let mut result = parent.cloned().unwrap_or_default();
    let uses = |group: u8| {
        if style_xf {
            xf.changed_groups & (1 << group) == 0
        } else {
            parent.is_none() || xf.changed_groups & (1 << group) != 0
        }
    };
    if uses(0) {
        result.num_fmt = custom
            .get(&xf.num_fmt)
            .cloned()
            .or_else(|| built_in_xlsb_num_fmt(xf.num_fmt).map(str::to_string));
        if result.num_fmt.is_none() && xf.num_fmt >= 164 {
            add_style_loss(losses, StyleLossKind::MissingReference, 1);
        } else if result.num_fmt.is_none() && xf.num_fmt != 0 {
            // Locale-dependent built-ins cannot be converted to a truthful,
            // locale-neutral format code without workbook locale metadata.
            add_style_loss(losses, StyleLossKind::UnsupportedProperty, 1);
        }
    }
    if uses(1) {
        result.font = fonts.get(xf.font).cloned();
        if result.font.is_none() && xf.font != 0 {
            add_style_loss(losses, StyleLossKind::MissingReference, 1);
        }
    }
    if uses(2) {
        result.align = Some(xf.alignment.clone());
    }
    if uses(3) {
        result.border = borders.get(xf.border).cloned();
        if result.border.is_none() && xf.border != 0 {
            add_style_loss(losses, StyleLossKind::MissingReference, 1);
        }
    }
    if uses(4) {
        result.pattern_fill = fills.get(xf.fill).copied();
        result.fill = result.pattern_fill.and_then(|fill| {
            (fill.pattern == FormatPattern::Solid)
                .then_some(fill.foreground.or(fill.background))
                .flatten()
        });
        if result.pattern_fill.is_none() && xf.fill != 0 {
            add_style_loss(losses, StyleLossKind::MissingReference, 1);
        }
    }
    if uses(5) {
        result.protection = Some(xf.protection.clone());
    }
    result
}

/// Bounds-checked BIFF12 reader used only by the renderer provenance oracle.
///
/// The general reader deliberately recovers from a truncated final record.
/// Font-size provenance instead fails closed, including when a variable-width
/// record header uses more than the schema's two/four byte limits.
struct StrictRecReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> StrictRecReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn var(&mut self, max_bytes: usize) -> std::result::Result<u32, ()> {
        let mut value = 0_u32;
        for index in 0..max_bytes {
            let byte = *self.bytes.get(self.pos).ok_or(())?;
            self.pos += 1;
            value |= u32::from(byte & 0x7F) << (7 * index);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(())
    }

    fn next(&mut self) -> std::result::Result<Option<(u32, &'a [u8])>, ()> {
        if self.pos == self.bytes.len() {
            return Ok(None);
        }
        let record_type = self.var(2)?;
        let size = usize::try_from(self.var(4)?).map_err(|_| ())?;
        let end = self.pos.checked_add(size).ok_or(())?;
        let payload = self.bytes.get(self.pos..end).ok_or(())?;
        self.pos = end;
        Ok(Some((record_type, payload)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProvenanceTable {
    Fonts,
    StyleXfs,
    CellXfs,
    Styles,
}

#[derive(Clone)]
struct ParsedXlsbStyle {
    xf_index: usize,
    built_in: bool,
    builtin_id: u8,
}

#[derive(Default)]
struct VerifiedXlsbStyleProvenance {
    normal_font_size_pt: Option<u16>,
    cell_xf_font_sizes_pt: Vec<Option<u16>>,
}

fn exact_integral_xlsb_font_size(payload: &[u8]) -> Option<u16> {
    let height_twips = u16le(payload, 0)?;
    let (name, used) = bounded_wide_string(payload, 21, 1_024)?;
    if name.is_empty() || 21_usize.checked_add(used)? != payload.len() || height_twips % 20 != 0 {
        return None;
    }
    let points = height_twips / 20;
    (1..=MAX_VERIFIED_XLSB_FONT_SIZE_POINTS)
        .contains(&points)
        .then_some(points)
}

fn exact_xlsb_xf(payload: &[u8]) -> Option<RawXf> {
    // [MS-XLSB] §2.4.876 BrtXF has a fixed 16-byte payload. Bits above the six
    // formatting-group flags are reserved and cannot establish provenance.
    if payload.len() != 16 || u16le(payload, 14)? & !0x003F != 0 {
        return None;
    }
    parse_xlsb_xf(payload, &mut Vec::new())
}

fn parsed_xlsb_style(payload: &[u8]) -> Option<ParsedXlsbStyle> {
    // [MS-XLSB] §2.4.809: ixf, StyleFlags, built-in id/level, CellStyleName.
    let xf_index = usize::try_from(u32le(payload, 0)?).ok()?;
    let flags = u16le(payload, 4)?;
    let builtin_id = *payload.get(6)?;
    let level = *payload.get(7)?;
    let (_, used) = bounded_nullable_wide_string(payload, 8, 255)?;
    if 8_usize.checked_add(used)? != payload.len()
        || flags & !0x0007 != 0
        || (flags & 0x0004 != 0 && flags & 0x0001 == 0)
        || (builtin_id != 0 && flags & 0x0001 == 0)
        || (matches!(builtin_id, 1 | 2) && level > 6)
    {
        return None;
    }
    Some(ParsedXlsbStyle {
        xf_index,
        built_in: flags & 0x0001 != 0,
        builtin_id,
    })
}

pub(super) fn verified_xlsb_collection_count(payload: &[u8], max: usize) -> Option<usize> {
    if payload.len() != 4 {
        return None;
    }
    let count = usize::try_from(u32le(payload, 0)?).ok()?;
    (1..=max).contains(&count).then_some(count)
}

fn verified_xlsb_ac_begin(payload: &[u8]) -> bool {
    // [MS-XLSB] §2.4.2: cver:u16 followed by exactly cver four-byte
    // ACProductVersion structures. The u16 count is the schema bound; checked
    // arithmetic keeps the framing proof allocation-free.
    let Some(count) = u16le(payload, 0).map(usize::from) else {
        return false;
    };
    count != 0
        && count
            .checked_mul(4)
            .and_then(|versions| versions.checked_add(2))
            == Some(payload.len())
}

fn verified_xlsb_style_provenance(
    bytes: &[u8],
    parsed_fonts: &[Font],
) -> VerifiedXlsbStyleProvenance {
    let mut reader = StrictRecReader::new(bytes);
    let mut structurally_valid = true;
    let mut root_state = 0_u8;
    let mut active_table = None;
    let mut in_alternate_content = false;

    let mut saw_fonts = false;
    let mut expected_fonts = None;
    let mut exact_font_sizes = Vec::new();

    let mut saw_style_xfs = false;
    let mut expected_style_xfs = None;
    let mut style_xfs = Vec::new();

    let mut saw_cell_xfs = false;
    let mut expected_cell_xfs = None;
    let mut cell_xfs = Vec::new();

    let mut saw_styles = false;
    let mut expected_styles = None;
    let mut parsed_styles = Vec::new();

    loop {
        let record = match reader.next() {
            Ok(Some(record)) => record,
            Ok(None) => break,
            Err(()) => {
                structurally_valid = false;
                break;
            }
        };
        let (record_type, payload) = record;
        if in_alternate_content {
            match record_type {
                // Alternate-content blocks are not recursive. Keep consuming
                // after a nested begin so the final result fails closed without
                // allowing any enclosed record to mutate the core table state.
                BRT_AC_BEGIN => structurally_valid = false,
                BRT_AC_END => {
                    if !payload.is_empty() {
                        structurally_valid = false;
                    }
                    in_alternate_content = false;
                }
                // The permissive public style reader is not alternate-content
                // aware. Reject blocks that carry provenance table records or
                // delimiters so its indexes cannot diverge from this oracle's
                // ignored alternate content.
                BRT_BEGIN_STYLE_SHEET
                | BRT_END_STYLE_SHEET
                | BRT_BEGIN_FONTS
                | BRT_END_FONTS
                | BRT_BEGIN_CELL_STYLE_XFS
                | BRT_END_CELL_STYLE_XFS
                | BRT_BEGIN_CELL_XFS
                | BRT_END_CELL_XFS
                | BRT_BEGIN_STYLES
                | BRT_END_STYLES
                | BRT_FONT
                | BRT_XF
                | BRT_STYLE => structurally_valid = false,
                _ => {}
            }
            continue;
        }
        match record_type {
            BRT_AC_BEGIN => {
                if verified_xlsb_ac_begin(payload) {
                    in_alternate_content = true;
                } else {
                    structurally_valid = false;
                }
            }
            BRT_AC_END => structurally_valid = false,
            BRT_BEGIN_STYLE_SHEET => {
                if root_state != 0 || !payload.is_empty() || active_table.is_some() {
                    structurally_valid = false;
                } else {
                    root_state = 1;
                }
            }
            BRT_END_STYLE_SHEET => {
                if root_state != 1 || !payload.is_empty() || active_table.is_some() {
                    structurally_valid = false;
                } else {
                    root_state = 2;
                }
            }
            _ if root_state != 1 => structurally_valid = false,
            BRT_BEGIN_FONTS => {
                let count = verified_xlsb_collection_count(payload, MAX_XLSB_FONT_RECORDS);
                if saw_fonts || active_table.is_some() || count.is_none() {
                    structurally_valid = false;
                } else {
                    saw_fonts = true;
                    expected_fonts = count;
                    active_table = Some(ProvenanceTable::Fonts);
                }
            }
            BRT_END_FONTS => {
                if active_table != Some(ProvenanceTable::Fonts) || !payload.is_empty() {
                    structurally_valid = false;
                } else {
                    active_table = None;
                }
            }
            BRT_BEGIN_CELL_STYLE_XFS => {
                let count =
                    verified_xlsb_collection_count(payload, MAX_VERIFIED_XLSB_STYLE_RECORDS);
                if saw_style_xfs || active_table.is_some() || count.is_none() {
                    structurally_valid = false;
                } else {
                    saw_style_xfs = true;
                    expected_style_xfs = count;
                    active_table = Some(ProvenanceTable::StyleXfs);
                }
            }
            BRT_END_CELL_STYLE_XFS => {
                if active_table != Some(ProvenanceTable::StyleXfs) || !payload.is_empty() {
                    structurally_valid = false;
                } else {
                    active_table = None;
                }
            }
            BRT_BEGIN_CELL_XFS => {
                let count =
                    verified_xlsb_collection_count(payload, MAX_VERIFIED_XLSB_STYLE_RECORDS);
                if saw_cell_xfs || active_table.is_some() || count.is_none() {
                    structurally_valid = false;
                } else {
                    saw_cell_xfs = true;
                    expected_cell_xfs = count;
                    active_table = Some(ProvenanceTable::CellXfs);
                }
            }
            BRT_END_CELL_XFS => {
                if active_table != Some(ProvenanceTable::CellXfs) || !payload.is_empty() {
                    structurally_valid = false;
                } else {
                    active_table = None;
                }
            }
            BRT_BEGIN_STYLES => {
                let count =
                    verified_xlsb_collection_count(payload, MAX_VERIFIED_XLSB_STYLE_RECORDS);
                if saw_styles || active_table.is_some() || count.is_none() {
                    structurally_valid = false;
                } else {
                    saw_styles = true;
                    expected_styles = count;
                    active_table = Some(ProvenanceTable::Styles);
                }
            }
            BRT_END_STYLES => {
                if active_table != Some(ProvenanceTable::Styles) || !payload.is_empty() {
                    structurally_valid = false;
                } else {
                    active_table = None;
                }
            }
            BRT_FONT => {
                if active_table != Some(ProvenanceTable::Fonts)
                    || exact_font_sizes.len() >= MAX_XLSB_FONT_RECORDS
                {
                    structurally_valid = false;
                } else {
                    exact_font_sizes.push(exact_integral_xlsb_font_size(payload));
                }
            }
            BRT_XF => match active_table {
                Some(ProvenanceTable::StyleXfs)
                    if style_xfs.len() < MAX_VERIFIED_XLSB_STYLE_RECORDS =>
                {
                    style_xfs.push(exact_xlsb_xf(payload));
                }
                Some(ProvenanceTable::CellXfs)
                    if cell_xfs.len() < MAX_VERIFIED_XLSB_STYLE_RECORDS =>
                {
                    cell_xfs.push(exact_xlsb_xf(payload));
                }
                _ => structurally_valid = false,
            },
            BRT_STYLE => {
                if active_table != Some(ProvenanceTable::Styles)
                    || parsed_styles.len() >= MAX_VERIFIED_XLSB_STYLE_RECORDS
                {
                    structurally_valid = false;
                } else {
                    parsed_styles.push(parsed_xlsb_style(payload));
                }
            }
            _ if active_table.is_some() => structurally_valid = false,
            _ => {}
        }
    }

    structurally_valid &= root_state == 2
        && active_table.is_none()
        && !in_alternate_content
        && saw_fonts
        && saw_style_xfs
        && saw_cell_xfs
        && saw_styles
        && expected_fonts == Some(exact_font_sizes.len())
        && expected_style_xfs == Some(style_xfs.len())
        && expected_cell_xfs == Some(cell_xfs.len())
        && expected_styles == Some(parsed_styles.len())
        && parsed_styles.iter().all(Option::is_some)
        && parsed_fonts.len() == exact_font_sizes.len();
    if !structurally_valid {
        return VerifiedXlsbStyleProvenance::default();
    }

    let style_xf_font_ids = style_xfs
        .iter()
        .map(|candidate| {
            let xf = candidate.as_ref()?;
            // A CellStyleXF is a root formatting record. Font group bit one is
            // inverted for style XFs: zero means the source font is present.
            (xf.parent.is_none() && xf.changed_groups & (1 << 1) == 0).then_some(xf.font)
        })
        .collect::<Vec<_>>();
    let cell_xf_font_ids = cell_xfs
        .iter()
        .map(|candidate| {
            let xf = candidate.as_ref()?;
            let parent_index = xf.parent?;
            let parent_xf = style_xfs.get(parent_index)?.as_ref()?;
            if parent_xf.parent.is_some() {
                return None;
            }
            if xf.changed_groups & (1 << 1) != 0 {
                Some(xf.font)
            } else {
                style_xf_font_ids.get(parent_index).copied().flatten()
            }
        })
        .collect::<Vec<_>>();
    let cell_xf_font_sizes_pt = cell_xf_font_ids
        .iter()
        .map(|font_id| {
            let font_id = (*font_id)?;
            exact_font_sizes.get(font_id).copied().flatten()
        })
        .collect::<Vec<_>>();

    let normal_font_size_pt = (saw_styles
        && expected_styles == Some(parsed_styles.len())
        && parsed_styles.iter().all(Option::is_some))
    .then(|| {
        let styles = parsed_styles
            .iter()
            .map(|style| style.as_ref().expect("validated style"))
            .collect::<Vec<_>>();
        let mut unique_xfs = HashSet::with_capacity(styles.len());
        if !styles.iter().all(|style| unique_xfs.insert(style.xf_index)) {
            return None;
        }
        let mut normals = styles
            .iter()
            .filter(|style| style.built_in && style.builtin_id == 0);
        let normal = *normals.next()?;
        if normals.next().is_some() || normal.xf_index != 0 {
            return None;
        }

        // [MS-XLSB] §2.2.6.1.2.2 requires Normal to reference the first
        // CellStyleXF. The first CellXF must in turn resolve to that same
        // source font before its point size is usable as a row-height oracle.
        let first_cell_xf = cell_xfs.first()?.as_ref()?;
        if first_cell_xf.parent != Some(normal.xf_index) {
            return None;
        }
        let normal_font_id = style_xf_font_ids.get(normal.xf_index).copied().flatten()?;
        let first_font_id = cell_xf_font_ids.first().copied().flatten()?;
        if normal_font_id != first_font_id {
            return None;
        }
        let normal_points = exact_font_sizes.get(normal_font_id).copied().flatten()?;
        let first_points = exact_font_sizes.get(first_font_id).copied().flatten()?;
        (normal_points == first_points).then_some(normal_points)
    })
    .flatten();

    VerifiedXlsbStyleProvenance {
        normal_font_size_pt,
        cell_xf_font_sizes_pt,
    }
}

pub(super) fn parse_styles(b: &[u8], theme: &XlsbTheme) -> Styles {
    let mut s = Styles::default();
    if !theme.source_valid {
        add_style_loss(&mut s.losses, StyleLossKind::UnsupportedProperty, 1);
    }
    let mut r = RecReader::new(b);
    let mut in_cell_xfs = false;
    let mut in_style_xfs = false;
    let mut raw_cell_xfs = Vec::new();
    let mut raw_style_xfs = Vec::new();
    while let Some((rt, p)) = r.next() {
        match rt {
            BRT_FONT if s.fonts.len() < MAX_XLSB_STYLE_RECORDS => {
                s.has_source_styles = true;
                s.fonts.push(parse_xlsb_font(p, theme, &mut s.losses));
            }
            BRT_FILL if s.fills.len() < MAX_XLSB_STYLE_RECORDS => {
                s.has_source_styles = true;
                s.fills.push(parse_xlsb_fill(p, theme, &mut s.losses));
            }
            BRT_BORDER if s.borders.len() < MAX_XLSB_STYLE_RECORDS => {
                s.has_source_styles = true;
                s.borders.push(parse_xlsb_border(p, theme, &mut s.losses));
            }
            BRT_FMT => {
                // ifmt:u16, stFmtCode: XLWideString.
                s.has_source_styles = true;
                if let (Some(ifmt), Some((code, _))) =
                    (u16le(p, 0), bounded_wide_string(p, 2, 4_096))
                {
                    s.custom.insert(ifmt, code);
                } else {
                    add_style_loss(&mut s.losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            BRT_BEGIN_CELL_STYLE_XFS => in_style_xfs = true,
            BRT_END_CELL_STYLE_XFS => in_style_xfs = false,
            BRT_BEGIN_CELL_XFS => {
                in_cell_xfs = true;
                s.has_source_styles = true;
            }
            BRT_END_CELL_XFS => {
                in_cell_xfs = false;
            }
            BRT_XF if in_cell_xfs => {
                if raw_cell_xfs.len() < MAX_XLSB_STYLE_RECORDS {
                    if let Some(xf) = parse_xlsb_xf(p, &mut s.losses) {
                        s.xf_numfmt.push(xf.num_fmt);
                        raw_cell_xfs.push(xf);
                    }
                } else {
                    add_style_loss(&mut s.losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            BRT_XF if in_style_xfs => {
                if raw_style_xfs.len() < MAX_XLSB_STYLE_RECORDS {
                    if let Some(xf) = parse_xlsb_xf(p, &mut s.losses) {
                        raw_style_xfs.push(xf);
                    }
                } else {
                    add_style_loss(&mut s.losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            BRT_FONT | BRT_FILL | BRT_BORDER => {
                add_style_loss(&mut s.losses, StyleLossKind::LimitExceeded, 1);
            }
            _ => {}
        }
    }
    let mut resolved_style_xfs = Vec::with_capacity(raw_style_xfs.len());
    for xf in &raw_style_xfs {
        let style = raw_xf_style(
            xf,
            &s.custom,
            &s.fonts,
            &s.fills,
            &s.borders,
            None,
            true,
            &mut s.losses,
        );
        resolved_style_xfs.push(style);
    }
    for xf in &raw_cell_xfs {
        let parent = xf.parent.and_then(|index| resolved_style_xfs.get(index));
        if xf.parent.is_some() && parent.is_none() {
            add_style_loss(&mut s.losses, StyleLossKind::MissingReference, 1);
        }
        let style = raw_xf_style(
            xf,
            &s.custom,
            &s.fonts,
            &s.fills,
            &s.borders,
            parent,
            false,
            &mut s.losses,
        );
        s.cell_styles.push(style);
    }
    let provenance = verified_xlsb_style_provenance(b, &s.fonts);
    s.xlsb_normal_font_size_pt = provenance.normal_font_size_pt;
    s.xlsb_cell_xf_font_sizes_pt = provenance.cell_xf_font_sizes_pt;
    s
}
