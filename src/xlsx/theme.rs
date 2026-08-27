//! OOXML theme colors, chart font defaults, and color resolution.

use quick_xml::events::Event;
use quick_xml::Reader;

use super::{attr, local, theme_markup_is_supported, unique_attr};
use crate::Color;

fn parse_chart_rgb(value: &str) -> Option<Color> {
    if value.len() != 6 || !value.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    Some(Color::rgb(
        u8::from_str_radix(&value[0..2], 16).ok()?,
        u8::from_str_radix(&value[2..4], 16).ok()?,
        u8::from_str_radix(&value[4..6], 16).ok()?,
    ))
}

fn chart_text_attributes_are_subset(
    element: &quick_xml::events::BytesStart<'_>,
    allowed: &[&[u8]],
) -> bool {
    element.attributes().all(|attribute| {
        let Ok(attribute) = attribute else {
            return false;
        };
        let qualified_name = attribute.key.as_ref();
        qualified_name == b"xmlns"
            || qualified_name.starts_with(b"xmlns:")
            || allowed.contains(&qualified_name)
    })
}
pub(super) fn parse_color(value: &str) -> Option<Color> {
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

#[derive(Clone)]
pub(crate) struct ThemeColors {
    // Canonical DrawingML order: lt1, dk1, lt2, dk2, accent1..6,
    // hyperlink, followed-hyperlink.
    pub(super) colors: [Option<Color>; 12],
    pub(super) major_latin_font_family: Option<String>,
    pub(super) minor_latin_font_family: Option<String>,
    /// False only when a present theme part contained ambiguous or malformed
    /// data. A missing theme uses the deterministic application fallback and
    /// therefore keeps this true.
    pub(super) source_valid: bool,
}

impl Default for ThemeColors {
    fn default() -> Self {
        Self {
            colors: [None; 12],
            major_latin_font_family: None,
            minor_latin_font_family: None,
            source_valid: true,
        }
    }
}

pub(super) const MAX_IMPORTED_CHART_LATIN_FONT_FAMILY_BYTES: usize = 255;
pub(crate) const CALC_IMPORTED_CHART_LATIN_FONT_FAMILY: &str = "Liberation Sans";

pub(crate) fn bounded_imported_chart_latin_font_family(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= MAX_IMPORTED_CHART_LATIN_FONT_FAMILY_BYTES)
        .then(|| value.to_string())
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
    pub(super) fn color(&self, idx: usize, tint: Option<f64>) -> Option<Color> {
        let color = self.colors.get(idx).copied().flatten()?;
        Some(apply_optional_tint(color, tint))
    }

    pub(super) fn chart_palette(&self) -> Vec<Color> {
        (0..OFFICE_CHART_ACCENTS.len())
            .map(|index| self.colors[index + 4].unwrap_or(OFFICE_CHART_ACCENTS[index]))
            .collect()
    }

    pub(super) fn chart_default_latin_font_family(&self) -> &str {
        self.minor_latin_font_family
            .as_deref()
            .unwrap_or(CALC_IMPORTED_CHART_LATIN_FONT_FAMILY)
    }

    pub(super) fn chart_major_latin_font_family(&self) -> &str {
        self.major_latin_font_family
            .as_deref()
            .unwrap_or(CALC_IMPORTED_CHART_LATIN_FONT_FAMILY)
    }

    pub(crate) fn source_valid(&self) -> bool {
        self.source_valid
    }

    #[cfg(feature = "xlsb")]
    pub(crate) fn xlsb_ordered_colors(&self) -> [Option<Color>; 12] {
        [
            self.colors[1],
            self.colors[0],
            self.colors[3],
            self.colors[2],
            self.colors[4],
            self.colors[5],
            self.colors[6],
            self.colors[7],
            self.colors[8],
            self.colors[9],
            self.colors[10],
            self.colors[11],
        ]
    }

    #[cfg(feature = "xlsb")]
    pub(crate) fn source_major_latin_font_family(&self) -> Option<&str> {
        self.major_latin_font_family.as_deref()
    }

    #[cfg(feature = "xlsb")]
    pub(crate) fn source_minor_latin_font_family(&self) -> Option<&str> {
        self.minor_latin_font_family.as_deref()
    }
}

#[cfg(feature = "xlsb")]
pub(crate) fn chart_theme(
    colors: [Option<Color>; 12],
    major_latin_font_family: Option<&str>,
    minor_latin_font_family: Option<&str>,
    source_valid: bool,
) -> ThemeColors {
    ThemeColors {
        colors,
        major_latin_font_family: major_latin_font_family
            .and_then(bounded_imported_chart_latin_font_family),
        minor_latin_font_family: minor_latin_font_family
            .and_then(bounded_imported_chart_latin_font_family),
        source_valid,
    }
}

pub(super) fn theme_color_slot(name: &[u8]) -> Option<usize> {
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

pub(crate) fn parse_theme(xml: &str) -> ThemeColors {
    fn retain_theme_color(
        theme: &mut ThemeColors,
        active_slot: &mut Option<(usize, bool)>,
        name: &[u8],
        element: &quick_xml::events::BytesStart<'_>,
    ) {
        let Some((slot, painted)) = active_slot.as_mut() else {
            return;
        };
        if *painted {
            theme.source_valid = false;
            return;
        }
        *painted = true;
        let color = match name {
            b"srgbClr" => {
                if !chart_text_attributes_are_subset(element, &[b"val"]) {
                    None
                } else {
                    unique_attr(element, b"val")
                        .ok()
                        .flatten()
                        .as_deref()
                        .and_then(parse_chart_rgb)
                }
            }
            b"sysClr" => {
                if !chart_text_attributes_are_subset(element, &[b"val", b"lastClr"])
                    || !matches!(unique_attr(element, b"val"), Ok(Some(value)) if !value.is_empty())
                {
                    None
                } else {
                    unique_attr(element, b"lastClr")
                        .ok()
                        .flatten()
                        .as_deref()
                        .and_then(parse_chart_rgb)
                }
            }
            _ => None,
        };
        if let Some(color) = color {
            theme.colors[*slot] = Some(color);
        } else {
            theme.source_valid = false;
        }
    }

    fn retain_theme_latin(
        theme: &mut ThemeColors,
        in_major_font: bool,
        in_minor_font: bool,
        element: &quick_xml::events::BytesStart<'_>,
    ) {
        if !in_major_font && !in_minor_font {
            return;
        }
        let family = if chart_text_attributes_are_subset(element, &[b"typeface"]) {
            unique_attr(element, b"typeface")
                .ok()
                .flatten()
                .as_deref()
                .and_then(bounded_imported_chart_latin_font_family)
        } else {
            None
        };
        let target = if in_major_font {
            &mut theme.major_latin_font_family
        } else {
            &mut theme.minor_latin_font_family
        };
        if family.is_none() || target.is_some() {
            theme.source_valid = false;
        } else {
            *target = family;
        }
    }

    let mut r = Reader::from_str(xml);
    let mut theme = ThemeColors {
        source_valid: theme_markup_is_supported(xml),
        ..ThemeColors::default()
    };
    let mut active_slot: Option<(usize, bool)> = None;
    let mut seen_slots = [false; 12];
    let mut element_stack = Vec::<Vec<u8>>::new();
    let mut theme_seen = false;
    let mut theme_open = false;
    let mut theme_elements_seen = false;
    let mut theme_elements_open = false;
    let mut in_color_scheme = false;
    let mut color_scheme_seen = false;
    let mut in_font_scheme = false;
    let mut font_scheme_seen = false;
    let mut in_major_font = false;
    let mut in_minor_font = false;
    let mut major_font_seen = false;
    let mut minor_font_seen = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = local(qname.as_ref());
                let depth = element_stack.len();
                if name == b"theme" {
                    if theme_seen || theme_open || depth != 0 {
                        theme.source_valid = false;
                    }
                    theme_seen = true;
                    theme_open = true;
                } else if name == b"themeElements" {
                    if theme_elements_seen || theme_elements_open || !theme_open || depth != 1 {
                        theme.source_valid = false;
                    }
                    theme_elements_seen = true;
                    theme_elements_open = true;
                } else if name == b"clrScheme" {
                    if color_scheme_seen || in_color_scheme || !theme_elements_open || depth != 2 {
                        theme.source_valid = false;
                    }
                    color_scheme_seen = true;
                    in_color_scheme = true;
                } else if name == b"fontScheme" {
                    if font_scheme_seen || in_font_scheme || !theme_elements_open || depth != 2 {
                        theme.source_valid = false;
                    }
                    font_scheme_seen = true;
                    in_font_scheme = true;
                } else if name == b"majorFont" && in_font_scheme {
                    if major_font_seen || in_major_font || in_minor_font || depth != 3 {
                        theme.source_valid = false;
                    }
                    major_font_seen = true;
                    in_major_font = true;
                } else if name == b"minorFont" && in_font_scheme {
                    if minor_font_seen || in_major_font || in_minor_font || depth != 3 {
                        theme.source_valid = false;
                    }
                    minor_font_seen = true;
                    in_minor_font = true;
                } else if let Some(next_slot) =
                    in_color_scheme.then(|| theme_color_slot(name)).flatten()
                {
                    if active_slot.is_some() || seen_slots[next_slot] || depth != 3 {
                        theme.source_valid = false;
                    }
                    seen_slots[next_slot] = true;
                    active_slot = Some((next_slot, false));
                } else if matches!(name, b"srgbClr" | b"sysClr") {
                    if active_slot.is_some() && depth != 4 {
                        theme.source_valid = false;
                    }
                    retain_theme_color(&mut theme, &mut active_slot, name, &e);
                } else if name == b"latin" {
                    if (in_major_font || in_minor_font) && depth != 4 {
                        theme.source_valid = false;
                    }
                    retain_theme_latin(&mut theme, in_major_font, in_minor_font, &e);
                } else if active_slot.is_some() {
                    // Theme slot color choices and transforms outside the exact
                    // sRGB/system-color subset cannot be resolved portably.
                    theme.source_valid = false;
                }
                if element_stack.len() >= 64 {
                    theme.source_valid = false;
                    break;
                }
                element_stack.push(name.to_vec());
            }
            Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local(qname.as_ref());
                let depth = element_stack.len();
                if matches!(
                    name,
                    b"theme"
                        | b"themeElements"
                        | b"clrScheme"
                        | b"fontScheme"
                        | b"majorFont"
                        | b"minorFont"
                ) {
                    theme.source_valid = false;
                } else if let Some(next_slot) =
                    in_color_scheme.then(|| theme_color_slot(name)).flatten()
                {
                    if seen_slots[next_slot] || depth != 3 {
                        theme.source_valid = false;
                    }
                    seen_slots[next_slot] = true;
                    // An empty color slot has no deterministic color and must
                    // not leak into the following sibling.
                    theme.source_valid = false;
                } else if matches!(name, b"srgbClr" | b"sysClr") {
                    if active_slot.is_some() && depth != 4 {
                        theme.source_valid = false;
                    }
                    retain_theme_color(&mut theme, &mut active_slot, name, &e);
                } else if name == b"latin" {
                    if (in_major_font || in_minor_font) && depth != 4 {
                        theme.source_valid = false;
                    }
                    retain_theme_latin(&mut theme, in_major_font, in_minor_font, &e);
                } else if active_slot.is_some() {
                    theme.source_valid = false;
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let name = local(qname.as_ref());
                let Some(open_name) = element_stack.pop() else {
                    theme.source_valid = false;
                    break;
                };
                if open_name.as_slice() != name {
                    theme.source_valid = false;
                }
                let depth = element_stack.len();
                if name == b"clrScheme" {
                    if active_slot.is_some() || depth != 2 {
                        theme.source_valid = false;
                        active_slot = None;
                    }
                    in_color_scheme = false;
                } else if name == b"fontScheme" {
                    if depth != 2 {
                        theme.source_valid = false;
                    }
                    in_font_scheme = false;
                    in_major_font = false;
                    in_minor_font = false;
                } else if name == b"majorFont" {
                    if depth != 3 {
                        theme.source_valid = false;
                    }
                    in_major_font = false;
                } else if name == b"minorFont" {
                    if depth != 3 {
                        theme.source_valid = false;
                    }
                    in_minor_font = false;
                } else if name == b"themeElements" {
                    if depth != 1 {
                        theme.source_valid = false;
                    }
                    theme_elements_open = false;
                } else if name == b"theme" {
                    if depth != 0 {
                        theme.source_valid = false;
                    }
                    theme_open = false;
                }
                if let Some(slot) = theme_color_slot(name) {
                    if depth == 3
                        && active_slot.is_some_and(|(active, painted)| active == slot && painted)
                    {
                        active_slot = None;
                    } else if in_color_scheme {
                        theme.source_valid = false;
                        active_slot = None;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                theme.source_valid = false;
                break;
            }
            _ => {}
        }
    }
    if active_slot.is_some()
        || !element_stack.is_empty()
        || theme_open
        || theme_elements_open
        || in_color_scheme
        || in_font_scheme
        || in_major_font
        || in_minor_font
    {
        theme.source_valid = false;
    }
    if !theme_seen
        || !theme_elements_seen
        || !color_scheme_seen
        || seen_slots.iter().any(|seen| !seen)
        || !font_scheme_seen
        || !major_font_seen
        || !minor_font_seen
        || theme.major_latin_font_family.is_none()
        || theme.minor_latin_font_family.is_none()
    {
        theme.source_valid = false;
    }
    theme
}

pub(super) fn apply_tint(color: Color, tint: f64) -> Color {
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

pub(super) fn color_attr(
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
