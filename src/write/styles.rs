//! Interned OOXML style resource tables (`<styleSheet>`): fonts, fills, borders,
//! number formats, cell formats (`cellXfs`), and differential formats (`<dxfs>`).

use std::collections::HashMap;

use crate::write::xml::{esc_attr, hex, NS_MAIN, XML_DECL};
use crate::{
    Alignment, Border, BorderStyle, CellProtection, CellStyle, Color, Fill, Font, FormatPattern,
    FormatScript, HAlign, VAlign,
};

pub(super) fn halign_str(h: HAlign) -> &'static str {
    match h {
        HAlign::Left => "left",
        HAlign::Center => "center",
        HAlign::Right => "right",
    }
}
pub(super) fn valign_str(v: VAlign) -> &'static str {
    match v {
        VAlign::Top => "top",
        VAlign::Middle => "center",
        VAlign::Bottom => "bottom",
    }
}
fn text_rotation_value(degrees: i16) -> i16 {
    let degrees = degrees.clamp(-90, 90);
    if degrees < 0 {
        90 - degrees
    } else {
        degrees
    }
}
pub(super) fn border_style_str(s: BorderStyle) -> &'static str {
    match s {
        BorderStyle::None => "none",
        BorderStyle::Thin => "thin",
        BorderStyle::Medium => "medium",
        BorderStyle::Thick => "thick",
        BorderStyle::Double => "double",
    }
}

fn pattern_str(pattern: FormatPattern) -> &'static str {
    match pattern {
        FormatPattern::None => "none",
        FormatPattern::Solid => "solid",
        FormatPattern::MediumGray => "mediumGray",
        FormatPattern::DarkGray => "darkGray",
        FormatPattern::LightGray => "lightGray",
        FormatPattern::DarkHorizontal => "darkHorizontal",
        FormatPattern::DarkVertical => "darkVertical",
        FormatPattern::DarkDown => "darkDown",
        FormatPattern::DarkUp => "darkUp",
        FormatPattern::DarkGrid => "darkGrid",
        FormatPattern::DarkTrellis => "darkTrellis",
        FormatPattern::LightHorizontal => "lightHorizontal",
        FormatPattern::LightVertical => "lightVertical",
        FormatPattern::LightDown => "lightDown",
        FormatPattern::LightUp => "lightUp",
        FormatPattern::LightGrid => "lightGrid",
        FormatPattern::LightTrellis => "lightTrellis",
        FormatPattern::Gray125 => "gray125",
        FormatPattern::Gray0625 => "gray0625",
    }
}

pub(super) fn script_str(script: FormatScript) -> Option<&'static str> {
    match script {
        FormatScript::None => None,
        FormatScript::Superscript => Some("superscript"),
        FormatScript::Subscript => Some("subscript"),
    }
}

fn fill_xml(fill: Fill) -> String {
    let mut xml = format!(
        r#"<fill><patternFill patternType="{}""#,
        pattern_str(fill.pattern)
    );
    match fill.pattern {
        FormatPattern::Solid => {
            let fg = fill.foreground.or(fill.background);
            if let Some(c) = fg {
                xml.push_str(&format!(
                    r#"><fgColor rgb="{}"/></patternFill></fill>"#,
                    hex(c)
                ));
            } else {
                xml.push_str("/></fill>");
            }
        }
        _ => {
            let has_colors = fill.foreground.is_some() || fill.background.is_some();
            if has_colors {
                xml.push('>');
                if let Some(c) = fill.foreground {
                    xml.push_str(&format!(r#"<fgColor rgb="{}"/>"#, hex(c)));
                }
                if let Some(c) = fill.background {
                    xml.push_str(&format!(r#"<bgColor rgb="{}"/>"#, hex(c)));
                }
                xml.push_str("</patternFill></fill>");
            } else {
                xml.push_str("/></fill>");
            }
        }
    }
    xml
}

fn protection_xml(protection: &CellProtection) -> String {
    let mut attrs = String::new();
    if let Some(locked) = protection.locked {
        attrs.push_str(if locked {
            r#" locked="1""#
        } else {
            r#" locked="0""#
        });
    }
    if protection.hidden {
        attrs.push_str(r#" hidden="1""#);
    }
    format!("<protection{attrs}/>")
}

#[cfg(test)]
mod tests {
    use super::{protection_xml, text_rotation_value};
    use crate::CellProtection;

    #[test]
    fn negative_text_rotation_uses_ooxml_encoding() {
        assert_eq!(text_rotation_value(45), 45);
        assert_eq!(text_rotation_value(-45), 135);
        assert_eq!(text_rotation_value(-90), 180);
        assert_eq!(text_rotation_value(-120), 180);
    }

    #[test]
    fn protection_xml_emits_only_explicit_flags() {
        assert_eq!(
            protection_xml(&CellProtection {
                locked: Some(false),
                hidden: true,
            }),
            r#"<protection locked="0" hidden="1"/>"#
        );
        assert_eq!(
            protection_xml(&CellProtection {
                locked: None,
                hidden: true,
            }),
            r#"<protection hidden="1"/>"#
        );
    }
}

/// The OOXML `cellXfs` key: a deduped tuple of resource indices + alignment.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub(super) struct XfKey {
    num_fmt: u16,
    font: u32,
    fill: u32,
    border: u32,
    align: Option<Alignment>,
    protection: Option<CellProtection>,
}

/// Interned style resource tables — the OOXML factoring of inline cell styles.
/// `fonts[0]`/`fills[0,1]`/`borders[0]`/`xfs[0]` are reserved defaults.
#[derive(Clone, Default)]
pub(super) struct StyleTable {
    fonts: Vec<Font>,
    fills: Vec<Fill>,
    borders: Vec<Border>,
    numfmts: Vec<(u16, String)>,
    xfs: Vec<XfKey>,
    font_idx: HashMap<Font, u32>,
    fill_idx: HashMap<Fill, u32>,
    border_idx: HashMap<Border, u32>,
    numfmt_idx: HashMap<String, u16>,
    xf_idx: HashMap<XfKey, u32>,
    /// Differential formats (`<dxfs>`) — solid fills referenced by conditional
    /// formatting `cellIs`/`top` rules via `dxfId`.
    dxfs: Vec<Color>,
}

impl StyleTable {
    pub(super) fn new() -> Self {
        let mut t = StyleTable::default();
        t.xfs.push(XfKey::default()); // xf 0 = General
        t.xf_idx.insert(XfKey::default(), 0);
        t
    }

    pub(super) fn intern_with_budget(
        &mut self,
        style: Option<&CellStyle>,
        is_date: bool,
        budget: &mut usize,
    ) -> Option<u32> {
        if style.is_none() && !is_date {
            return Some(0);
        }
        let before = self.to_xml().len();
        let mut next = self.clone();
        let id = next.intern(style, is_date);
        let cost = next.to_xml().len().saturating_sub(before);
        if !consume_budget(budget, cost) {
            return None;
        }
        *self = next;
        Some(id)
    }

    /// Intern `(style, is_date)` → its `cellXfs` index.
    pub(super) fn intern(&mut self, style: Option<&CellStyle>, is_date: bool) -> u32 {
        let num_fmt = match style.and_then(|s| s.num_fmt.as_deref()) {
            Some(code) => self.intern_numfmt(code),
            None if is_date => 14,
            None => 0,
        };
        let font = match style.and_then(|s| s.font.as_ref()) {
            Some(f) if *f != Font::default() => self.intern_font(f),
            _ => 0,
        };
        let fill = match style.and_then(CellStyle::effective_fill) {
            Some(c) => self.intern_fill(c),
            None => 0,
        };
        let border = match style.and_then(|s| s.border.as_ref()) {
            Some(b) if *b != Border::default() => self.intern_border(b),
            _ => 0,
        };
        let align = style
            .and_then(|s| s.align.clone())
            .filter(|a| *a != Alignment::default());
        let protection = style
            .and_then(|s| s.protection.clone())
            .filter(|p| *p != CellProtection::default());
        let key = XfKey {
            num_fmt,
            font,
            fill,
            border,
            align,
            protection,
        };
        if let Some(&id) = self.xf_idx.get(&key) {
            return id;
        }
        let id = self.xfs.len() as u32;
        self.xfs.push(key.clone());
        self.xf_idx.insert(key, id);
        id
    }
    fn intern_font(&mut self, f: &Font) -> u32 {
        if let Some(&id) = self.font_idx.get(f) {
            return id;
        }
        let id = self.fonts.len() as u32 + 1; // 0 = reserved default
        self.fonts.push(f.clone());
        self.font_idx.insert(f.clone(), id);
        id
    }
    fn intern_fill(&mut self, fill: Fill) -> u32 {
        if let Some(&id) = self.fill_idx.get(&fill) {
            return id;
        }
        let id = self.fills.len() as u32 + 2; // 0,1 reserved (none, gray125)
        self.fills.push(fill);
        self.fill_idx.insert(fill, id);
        id
    }
    fn intern_border(&mut self, b: &Border) -> u32 {
        if let Some(&id) = self.border_idx.get(b) {
            return id;
        }
        let id = self.borders.len() as u32 + 1; // 0 = reserved empty
        self.borders.push(b.clone());
        self.border_idx.insert(b.clone(), id);
        id
    }
    fn intern_numfmt(&mut self, code: &str) -> u16 {
        if let Some(&id) = self.numfmt_idx.get(code) {
            return id;
        }
        // Custom numFmt ids live in 164..=65535. Past that ceiling (absurd: ~65k
        // distinct formats) fall back to General (0) rather than emitting a
        // duplicate or out-of-range id — saturating would collide every overflowing
        // format on 65535. The usize add itself can't overflow on the count.
        let Ok(id) = u16::try_from(164 + self.numfmts.len()) else {
            return 0;
        };
        self.numfmts.push((id, code.to_string()));
        self.numfmt_idx.insert(code.to_string(), id);
        id
    }
    /// Intern a differential-format solid fill → its `dxfId`.
    pub(super) fn intern_dxf(&mut self, fill: Color) -> u32 {
        if let Some(i) = self.dxfs.iter().position(|&c| c == fill) {
            return i as u32;
        }
        let id = self.dxfs.len() as u32;
        self.dxfs.push(fill);
        id
    }

    pub(super) fn intern_dxf_with_budget(
        &mut self,
        fill: Color,
        budget: &mut usize,
    ) -> Option<u32> {
        let before = self.to_xml().len();
        let mut next = self.clone();
        let id = next.intern_dxf(fill);
        let cost = next.to_xml().len().saturating_sub(before);
        if !consume_budget(budget, cost) {
            return None;
        }
        *self = next;
        Some(id)
    }

    pub(super) fn to_xml(&self) -> String {
        let mut s = String::new();
        s.push_str(XML_DECL);
        s.push_str(&format!(r#"<styleSheet xmlns="{NS_MAIN}">"#));
        if !self.numfmts.is_empty() {
            s.push_str(&format!(r#"<numFmts count="{}">"#, self.numfmts.len()));
            for (id, code) in &self.numfmts {
                s.push_str(&format!(
                    r#"<numFmt numFmtId="{id}" formatCode="{}"/>"#,
                    esc_attr(code)
                ));
            }
            s.push_str("</numFmts>");
        }
        s.push_str(&format!(r#"<fonts count="{}">"#, self.fonts.len() + 1));
        s.push_str(r#"<font><sz val="11"/><name val="Calibri"/></font>"#);
        for f in &self.fonts {
            s.push_str("<font>");
            s.push_str(&format!(r#"<sz val="{}"/>"#, f.size_pt.unwrap_or(11)));
            if let Some(c) = f.color {
                s.push_str(&format!(r#"<color rgb="{}"/>"#, hex(c)));
            }
            s.push_str(&format!(
                r#"<name val="{}"/>"#,
                esc_attr(f.name.as_deref().unwrap_or("Calibri"))
            ));
            if f.bold {
                s.push_str("<b/>");
            }
            if f.italic {
                s.push_str("<i/>");
            }
            if f.strikethrough {
                s.push_str("<strike/>");
            }
            if f.underline {
                s.push_str("<u/>");
            }
            if let Some(script) = script_str(f.script) {
                s.push_str(&format!(r#"<vertAlign val="{script}"/>"#));
            }
            s.push_str("</font>");
        }
        s.push_str("</fonts>");
        s.push_str(&format!(r#"<fills count="{}">"#, self.fills.len() + 2));
        s.push_str(
            r#"<fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill>"#,
        );
        for &fill in &self.fills {
            s.push_str(&fill_xml(fill));
        }
        s.push_str("</fills>");
        s.push_str(&format!(r#"<borders count="{}">"#, self.borders.len() + 1));
        s.push_str(r#"<border><left/><right/><top/><bottom/><diagonal/></border>"#);
        for b in &self.borders {
            s.push_str("<border>");
            for (tag, side, color) in [
                ("left", b.left, b.left_color.or(b.color)),
                ("right", b.right, b.right_color.or(b.color)),
                ("top", b.top, b.top_color.or(b.color)),
                ("bottom", b.bottom, b.bottom_color.or(b.color)),
            ] {
                if side == BorderStyle::None {
                    s.push_str(&format!("<{tag}/>"));
                } else {
                    let st = border_style_str(side);
                    match color {
                        Some(c) => s.push_str(&format!(
                            r#"<{tag} style="{st}"><color rgb="{}"/></{tag}>"#,
                            hex(c)
                        )),
                        None => s.push_str(&format!(r#"<{tag} style="{st}"/>"#)),
                    }
                }
            }
            s.push_str("<diagonal/></border>");
        }
        s.push_str("</borders>");
        s.push_str(
            r#"<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>"#,
        );
        s.push_str(&format!(r#"<cellXfs count="{}">"#, self.xfs.len()));
        for xf in &self.xfs {
            s.push_str(&format!(
                r#"<xf numFmtId="{}" fontId="{}" fillId="{}" borderId="{}" xfId="0""#,
                xf.num_fmt, xf.font, xf.fill, xf.border
            ));
            if xf.num_fmt != 0 {
                s.push_str(r#" applyNumberFormat="1""#);
            }
            if xf.font != 0 {
                s.push_str(r#" applyFont="1""#);
            }
            if xf.fill != 0 {
                s.push_str(r#" applyFill="1""#);
            }
            if xf.border != 0 {
                s.push_str(r#" applyBorder="1""#);
            }
            if xf.align.is_some() {
                s.push_str(r#" applyAlignment="1""#);
            }
            if xf.protection.is_some() {
                s.push_str(r#" applyProtection="1""#);
            }
            let mut body = String::new();
            if let Some(a) = &xf.align {
                body.push_str("<alignment");
                if let Some(h) = a.horizontal {
                    body.push_str(&format!(r#" horizontal="{}""#, halign_str(h)));
                }
                if let Some(v) = a.vertical {
                    body.push_str(&format!(r#" vertical="{}""#, valign_str(v)));
                }
                if a.wrap {
                    body.push_str(r#" wrapText="1""#);
                }
                if a.rotation != 0 {
                    body.push_str(&format!(
                        r#" textRotation="{}""#,
                        text_rotation_value(a.rotation)
                    ));
                }
                if a.indent != 0 {
                    body.push_str(&format!(r#" indent="{}""#, a.indent));
                }
                if a.shrink_to_fit {
                    body.push_str(r#" shrinkToFit="1""#);
                }
                body.push_str("/>");
            }
            if let Some(protection) = &xf.protection {
                body.push_str(&protection_xml(protection));
            }
            if body.is_empty() {
                s.push_str("/>");
            } else {
                s.push('>');
                s.push_str(&body);
                s.push_str("</xf>");
            }
        }
        s.push_str("</cellXfs>");
        s.push_str(
            r#"<cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles>"#,
        );
        if !self.dxfs.is_empty() {
            s.push_str(&format!(r#"<dxfs count="{}">"#, self.dxfs.len()));
            for &c in &self.dxfs {
                s.push_str(&format!(
                    r#"<dxf><fill><patternFill patternType="solid"><fgColor rgb="{}"/></patternFill></fill></dxf>"#,
                    hex(c)
                ));
            }
            s.push_str("</dxfs>");
        }
        s.push_str("</styleSheet>");
        s
    }
}

fn consume_budget(budget: &mut usize, cost: usize) -> bool {
    if cost > *budget {
        *budget = 0;
        return false;
    }
    *budget -= cost;
    true
}
