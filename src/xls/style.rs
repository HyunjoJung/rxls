use super::workbook::read_short_string;
use super::{
    u16le, u32le, Ctx, MAX_BIFF_FONT_RECORD_BYTES, MAX_BIFF_XF_RECORD_BYTES, MAX_XLS_STYLE_RECORDS,
};
use crate::format::Formats;
use crate::model::{
    Alignment, Border, BorderStyle, CellProtection, CellStyle, Color, Fill, Font, FormatPattern,
    FormatScript, HAlign, VAlign,
};

pub(super) const BIFF_DEFAULT_PALETTE: [Color; 56] = [
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
const BIFF_INVARIANT_COLORS: [Color; 8] = [
    Color::rgb(0x00, 0x00, 0x00),
    Color::rgb(0xFF, 0xFF, 0xFF),
    Color::rgb(0xFF, 0x00, 0x00),
    Color::rgb(0x00, 0xFF, 0x00),
    Color::rgb(0x00, 0x00, 0xFF),
    Color::rgb(0xFF, 0xFF, 0x00),
    Color::rgb(0xFF, 0x00, 0xFF),
    Color::rgb(0x00, 0xFF, 0xFF),
];

/// Raw BIFF style tables are compiled only after the Globals Substream ends, so
/// BIFF5 font names see the final CODEPAGE and FONT records see the final custom
/// PALETTE even though those records can appear later in the stream.
#[derive(Debug, Default)]
pub(super) struct XlsStyles {
    pub(super) font_records: Vec<Option<Vec<u8>>>,
    pub(super) xf_records: Vec<Vec<u8>>,
    normal_style_xf: Option<u16>,
    pub(super) fonts: Vec<Option<Font>>,
    pub(super) xfs: Vec<Option<CellStyle>>,
    compiled: bool,
}

impl XlsStyles {
    pub(super) fn push_font(&mut self, data: &[u8]) {
        if self.font_records.len() >= MAX_XLS_STYLE_RECORDS {
            return;
        }
        // BIFF's font index 4 is reserved and has no corresponding FONT record.
        if self.font_records.len() == 4 {
            self.font_records.push(None);
        }
        if self.font_records.len() < MAX_XLS_STYLE_RECORDS {
            self.font_records.push(Some(
                data[..data.len().min(MAX_BIFF_FONT_RECORD_BYTES)].to_vec(),
            ));
        }
    }

    pub(super) fn push_xf(&mut self, data: &[u8]) {
        if self.xf_records.len() < MAX_XLS_STYLE_RECORDS {
            self.xf_records
                .push(data[..data.len().min(MAX_BIFF_XF_RECORD_BYTES)].to_vec());
        }
    }

    pub(super) fn push_style(&mut self, data: &[u8]) {
        let Some(flags) = u16le(data, 0) else {
            return;
        };
        let built_in = flags & 0x8000 != 0;
        if built_in && data.get(2).copied() == Some(0) {
            self.normal_style_xf = Some(flags & 0x0FFF);
        }
    }

    pub(super) fn compile(&mut self, ctx: Ctx, formats: &Formats, palette: &[Color; 56]) {
        if self.compiled {
            return;
        }
        self.fonts = self
            .font_records
            .iter()
            .map(|record| {
                record
                    .as_deref()
                    .and_then(|data| parse_biff_font(data, ctx, palette))
            })
            .collect();
        // Cell XFs already store the complete formatting set. Their parent and
        // fAtr* fields describe how an authoring application propagates later
        // style edits; they are not read-time inheritance switches.
        self.xfs = self
            .xf_records
            .iter()
            .map(|data| {
                parse_biff_xf(data, ctx.biff8, &self.fonts, formats, palette)
                    .map(|raw| raw.components.into_cell_style())
            })
            .collect();
        self.compiled = true;
    }

    pub(super) fn clone_xf(&self, index: u16, budget: &mut usize) -> Option<CellStyle> {
        let style = self.xfs.get(usize::from(index))?.as_ref()?;
        let cost = retained_style_cost(style);
        if cost > *budget {
            *budget = 0;
            return None;
        }
        *budget -= cost;
        Some(style.clone())
    }

    pub(super) fn default_style(&self, budget: &mut usize) -> Option<CellStyle> {
        self.clone_xf(15, budget).or_else(|| {
            self.normal_style_xf
                .and_then(|index| self.clone_xf(index, budget))
        })
    }

    pub(super) fn font(&self, index: u16) -> Option<Font> {
        self.fonts.get(usize::from(index))?.clone()
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct XfComponents {
    font: Option<Font>,
    num_fmt: Option<String>,
    alignment: Alignment,
    border: Border,
    fill: Fill,
    protection: CellProtection,
}

impl XfComponents {
    pub(super) fn into_cell_style(self) -> CellStyle {
        let legacy_fill = (self.fill.pattern == FormatPattern::Solid)
            .then_some(self.fill.foreground)
            .flatten();
        CellStyle {
            font: self.font,
            fill: legacy_fill,
            pattern_fill: Some(self.fill),
            border: Some(self.border),
            num_fmt: self.num_fmt,
            align: Some(self.alignment),
            protection: Some(self.protection),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct RawXf {
    pub(super) components: XfComponents,
}

fn parse_biff_font(data: &[u8], ctx: Ctx, palette: &[Color; 56]) -> Option<Font> {
    if data.len() < 15 {
        return None;
    }
    let height_twips = u16le(data, 0)?;
    let flags = u16le(data, 2)?;
    let color_index = u16le(data, 4)?;
    let weight = u16le(data, 6)?;
    let script = match u16le(data, 8).unwrap_or(0) {
        1 => FormatScript::Superscript,
        2 => FormatScript::Subscript,
        _ => FormatScript::None,
    };
    let underline = matches!(
        data.get(10).copied(),
        Some(0x01) | Some(0x02) | Some(0x21) | Some(0x22)
    );
    let name = read_short_string(data, 14, ctx).filter(|name| !name.is_empty());
    Some(Font {
        name,
        size_pt: (20..=8191)
            .contains(&height_twips)
            .then(|| (((u32::from(height_twips) + 10) / 20) as u16).max(1)),
        color: biff_style_color(color_index, palette),
        bold: (700..=1000).contains(&weight),
        italic: flags & 0x0002 != 0,
        underline,
        strikethrough: flags & 0x0008 != 0,
        script,
    })
}

pub(super) fn parse_biff_xf(
    data: &[u8],
    biff8: bool,
    fonts: &[Option<Font>],
    formats: &Formats,
    palette: &[Color; 56],
) -> Option<RawXf> {
    let font_index = u16le(data, 0)?;
    let format_index = u16le(data, 2)?;
    let type_parent = u16le(data, 4)?;
    let protection = CellProtection {
        locked: Some(type_parent & 0x0001 != 0),
        hidden: type_parent & 0x0002 != 0,
    };
    let font = fonts.get(usize::from(font_index)).cloned().flatten();
    let num_fmt = formats.code_for_ifmt(format_index);

    let (alignment, border, fill) = if biff8 {
        if data.len() < 20 {
            return None;
        }
        let align1 = data[6];
        let rotation = data[7];
        let align2 = data[8];
        let border1 = u32le(data, 10)?;
        let border2 = u32le(data, 14)?;
        let fill_colors = u16le(data, 18)?;
        (
            parse_biff8_alignment(align1, rotation, align2),
            Border {
                left: biff_border_style(border1 & 0x0F),
                right: biff_border_style((border1 >> 4) & 0x0F),
                top: biff_border_style((border1 >> 8) & 0x0F),
                bottom: biff_border_style((border1 >> 12) & 0x0F),
                color: None,
                left_color: biff_border_color(
                    biff_border_style(border1 & 0x0F),
                    (border1 >> 16) & 0x7F,
                    palette,
                ),
                right_color: biff_border_color(
                    biff_border_style((border1 >> 4) & 0x0F),
                    (border1 >> 23) & 0x7F,
                    palette,
                ),
                top_color: biff_border_color(
                    biff_border_style((border1 >> 8) & 0x0F),
                    border2 & 0x7F,
                    palette,
                ),
                bottom_color: biff_border_color(
                    biff_border_style((border1 >> 12) & 0x0F),
                    (border2 >> 7) & 0x7F,
                    palette,
                ),
            },
            Fill {
                pattern: biff_fill_pattern((border2 >> 26) & 0x3F),
                foreground: biff_style_color(fill_colors & 0x7F, palette),
                background: biff_style_color((fill_colors >> 7) & 0x7F, palette),
            },
        )
    } else {
        if data.len() < 16 {
            return None;
        }
        let align1 = data[6];
        let orient_used = data[7];
        let border_fill1 = u32le(data, 8)?;
        let border2 = u32le(data, 12)?;
        (
            parse_biff5_alignment(align1, orient_used & 0x03),
            Border {
                left: biff_border_style((border2 >> 3) & 0x07),
                right: biff_border_style((border2 >> 6) & 0x07),
                top: biff_border_style(border2 & 0x07),
                bottom: biff_border_style((border_fill1 >> 22) & 0x07),
                color: None,
                left_color: biff_border_color(
                    biff_border_style((border2 >> 3) & 0x07),
                    (border2 >> 16) & 0x7F,
                    palette,
                ),
                right_color: biff_border_color(
                    biff_border_style((border2 >> 6) & 0x07),
                    (border2 >> 23) & 0x7F,
                    palette,
                ),
                top_color: biff_border_color(
                    biff_border_style(border2 & 0x07),
                    (border2 >> 9) & 0x7F,
                    palette,
                ),
                bottom_color: biff_border_color(
                    biff_border_style((border_fill1 >> 22) & 0x07),
                    (border_fill1 >> 25) & 0x7F,
                    palette,
                ),
            },
            Fill {
                pattern: biff_fill_pattern((border_fill1 >> 16) & 0x3F),
                foreground: biff_style_color((border_fill1 & 0x7F) as u16, palette),
                background: biff_style_color(((border_fill1 >> 7) & 0x7F) as u16, palette),
            },
        )
    };
    Some(RawXf {
        components: XfComponents {
            font,
            num_fmt,
            alignment,
            border,
            fill,
            protection,
        },
    })
}

fn parse_biff8_alignment(align1: u8, rotation: u8, align2: u8) -> Alignment {
    Alignment {
        horizontal: biff_horizontal_alignment(align1 & 0x07),
        vertical: biff_vertical_alignment((align1 >> 4) & 0x07),
        wrap: align1 & 0x08 != 0,
        rotation: biff_text_rotation(rotation),
        indent: align2 & 0x0F,
        shrink_to_fit: align2 & 0x10 != 0,
    }
}

fn parse_biff5_alignment(align1: u8, orientation: u8) -> Alignment {
    Alignment {
        horizontal: biff_horizontal_alignment(align1 & 0x07),
        vertical: biff_vertical_alignment((align1 >> 4) & 0x07),
        wrap: align1 & 0x08 != 0,
        rotation: match orientation {
            2 => 90,
            3 => -90,
            _ => 0,
        },
        indent: 0,
        shrink_to_fit: false,
    }
}

fn biff_horizontal_alignment(value: u8) -> Option<HAlign> {
    match value {
        1 => Some(HAlign::Left),
        2 => Some(HAlign::Center),
        3 => Some(HAlign::Right),
        _ => None,
    }
}

fn biff_vertical_alignment(value: u8) -> Option<VAlign> {
    match value {
        0 => Some(VAlign::Top),
        1 => Some(VAlign::Middle),
        2 => Some(VAlign::Bottom),
        _ => None,
    }
}

fn biff_text_rotation(value: u8) -> i16 {
    match value {
        0..=90 => i16::from(value),
        91..=180 => 90 - i16::from(value),
        _ => 0,
    }
}

fn biff_border_style(value: u32) -> BorderStyle {
    match value {
        1 => BorderStyle::Thin,
        2 => BorderStyle::Medium,
        5 => BorderStyle::Thick,
        6 => BorderStyle::Double,
        _ => BorderStyle::None,
    }
}

fn biff_fill_pattern(value: u32) -> FormatPattern {
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

fn biff_border_color(style: BorderStyle, value: u32, palette: &[Color; 56]) -> Option<Color> {
    (style != BorderStyle::None)
        .then(|| biff_style_color(value as u16, palette))
        .flatten()
}

fn biff_style_color(index: u16, palette: &[Color; 56]) -> Option<Color> {
    match index {
        0..=7 => BIFF_INVARIANT_COLORS.get(usize::from(index)).copied(),
        8..=63 => palette.get(usize::from(index - 8)).copied(),
        _ => None,
    }
}

fn retained_style_cost(style: &CellStyle) -> usize {
    std::mem::size_of::<CellStyle>()
        .saturating_add(
            style
                .font
                .as_ref()
                .and_then(|font| font.name.as_ref())
                .map_or(0, String::len),
        )
        .saturating_add(style.num_fmt.as_ref().map_or(0, String::len))
}

pub(super) fn apply_palette_record(data: &[u8], palette: &mut [Color; 56]) {
    let Some(count) = u16le(data, 0).map(|count| count as usize) else {
        return;
    };
    for idx in 0..count.min(palette.len()) {
        let offset = 2 + idx * 4;
        let Some(rgb) = data.get(offset..offset + 4) else {
            return;
        };
        palette[idx] = Color::rgb(rgb[0], rgb[1], rgb[2]);
    }
}

pub(super) fn biff_palette_color(icv: u8, palette: &[Color; 56]) -> Option<Color> {
    let idx = icv.checked_sub(0x08)? as usize;
    palette.get(idx).copied()
}
