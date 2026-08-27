//! Cell style composition, overlays, and builder methods.

use super::{
    Alignment, Border, BorderStyle, Color, Fill, Font, FormatAlign, FormatBorder, FormatPattern,
    FormatScript, HAlign, VAlign,
};

/// Inline cell style for authoring. All `None`/default ⇒ Excel "General"; the
/// writer interns these into the workbook's deduped style tables.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CellStyle {
    /// Font.
    pub font: Option<Font>,
    /// Legacy solid background fill color. Prefer [`CellStyle::pattern_fill`]
    /// for non-solid fills.
    pub fill: Option<Color>,
    /// Pattern fill.
    pub pattern_fill: Option<Fill>,
    /// Cell borders.
    pub border: Option<Border>,
    /// Number format code (e.g. `₩#,##0`, `0.0%`, `yyyy"년" mm"월"`).
    pub num_fmt: Option<String>,
    /// Text alignment.
    pub align: Option<Alignment>,
    /// Cell protection flags used when worksheet protection is enabled.
    pub protection: Option<CellProtection>,
}

/// Sparse OOXML cell-XF overlay with explicit component replacement flags.
///
/// Table differential formats merge individual properties, while an applied
/// cell-XF component replaces that entire component. Keeping the flags
/// separate from [`CellStyle`] also represents explicit resets such as
/// `numFmtId="0"`, whose resolved style value is `None` (General).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CellStyleOverlay {
    pub(crate) style: CellStyle,
    pub(crate) replace_font: bool,
    pub(crate) replace_fill: bool,
    pub(crate) replace_border: bool,
    pub(crate) replace_num_fmt: bool,
    pub(crate) replace_alignment: bool,
    pub(crate) replace_protection: bool,
}

impl CellStyleOverlay {
    pub(in crate::model) fn apply_to(&self, base: Option<CellStyle>) -> CellStyle {
        let mut resolved = base.unwrap_or_default();
        if self.replace_font {
            resolved.font = self.style.font.clone();
        }
        if self.replace_fill {
            resolved.fill = self.style.fill;
            resolved.pattern_fill = self.style.pattern_fill;
        }
        if self.replace_border {
            resolved.border = self.style.border.clone();
        }
        if self.replace_num_fmt {
            resolved.num_fmt = self.style.num_fmt.clone();
        }
        if self.replace_alignment {
            resolved.align = self.style.align.clone();
        }
        if self.replace_protection {
            resolved.protection = self.style.protection.clone();
        }
        resolved
    }
}

/// Cell-level protection flags in an authored cell style.
///
/// Excel treats cells as locked by default, so `locked = None` means "inherit the
/// default locked state"; `Some(false)` explicitly unlocks a cell on a protected
/// worksheet. `hidden` hides formula text while sheet protection is enabled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CellProtection {
    /// Explicit locked state. `None` leaves Excel's default locked state.
    pub locked: Option<bool>,
    /// Hide formula text when the worksheet is protected.
    pub hidden: bool,
}

fn merge_font(base: Option<&Font>, overlay: Option<&Font>) -> Option<Font> {
    match (base, overlay) {
        (None, None) => None,
        (Some(base), None) => Some(base.clone()),
        (None, Some(overlay)) => Some(overlay.clone()),
        (Some(base), Some(overlay)) => {
            let mut merged = base.clone();
            if overlay.name.is_some() {
                merged.name = overlay.name.clone();
            }
            if overlay.size_pt.is_some() {
                merged.size_pt = overlay.size_pt;
            }
            if overlay.color.is_some() {
                merged.color = overlay.color;
            }
            if overlay.bold {
                merged.bold = true;
            }
            if overlay.italic {
                merged.italic = true;
            }
            if overlay.underline {
                merged.underline = true;
            }
            if overlay.strikethrough {
                merged.strikethrough = true;
            }
            if overlay.script != FormatScript::None {
                merged.script = overlay.script;
            }
            Some(merged)
        }
    }
}

fn merge_alignment(base: Option<&Alignment>, overlay: Option<&Alignment>) -> Option<Alignment> {
    match (base, overlay) {
        (None, None) => None,
        (Some(base), None) => Some(base.clone()),
        (None, Some(overlay)) => Some(overlay.clone()),
        (Some(base), Some(overlay)) => {
            let mut merged = base.clone();
            if overlay.horizontal.is_some() {
                merged.horizontal = overlay.horizontal;
            }
            if overlay.vertical.is_some() {
                merged.vertical = overlay.vertical;
            }
            if overlay.wrap {
                merged.wrap = true;
            }
            if overlay.rotation != 0 {
                merged.rotation = overlay.rotation;
            }
            if overlay.indent != 0 {
                merged.indent = overlay.indent;
            }
            if overlay.shrink_to_fit {
                merged.shrink_to_fit = true;
            }
            Some(merged)
        }
    }
}

fn merge_border(base: Option<&Border>, overlay: Option<&Border>) -> Option<Border> {
    match (base, overlay) {
        (None, None) => None,
        (Some(base), None) => Some(base.clone()),
        (None, Some(overlay)) => Some(overlay.clone()),
        (Some(base), Some(overlay)) => {
            let mut merged = base.clone();
            if overlay.left != BorderStyle::None {
                merged.left = overlay.left;
            }
            if overlay.right != BorderStyle::None {
                merged.right = overlay.right;
            }
            if overlay.top != BorderStyle::None {
                merged.top = overlay.top;
            }
            if overlay.bottom != BorderStyle::None {
                merged.bottom = overlay.bottom;
            }
            if overlay.color.is_some() {
                merged.color = overlay.color;
            }
            if overlay.left_color.is_some() {
                merged.left_color = overlay.left_color;
            }
            if overlay.right_color.is_some() {
                merged.right_color = overlay.right_color;
            }
            if overlay.top_color.is_some() {
                merged.top_color = overlay.top_color;
            }
            if overlay.bottom_color.is_some() {
                merged.bottom_color = overlay.bottom_color;
            }
            Some(merged)
        }
    }
}

fn merge_protection(
    base: Option<&CellProtection>,
    overlay: Option<&CellProtection>,
) -> Option<CellProtection> {
    match (base, overlay) {
        (None, None) => None,
        (Some(base), None) => Some(base.clone()),
        (None, Some(overlay)) => Some(overlay.clone()),
        (Some(base), Some(overlay)) => {
            let mut merged = base.clone();
            if overlay.locked.is_some() {
                merged.locked = overlay.locked;
            }
            if overlay.hidden {
                merged.hidden = true;
            }
            Some(merged)
        }
    }
}

impl CellStyle {
    /// A new empty style.
    pub fn new() -> Self {
        Self::default()
    }
    /// Merge this style with `overlay`, where explicitly set overlay fields
    /// override this style and unset overlay fields preserve `self`.
    pub fn merge(&self, overlay: &CellStyle) -> Self {
        let mut merged = self.clone();
        merged.font = merge_font(self.font.as_ref(), overlay.font.as_ref());
        if overlay.fill.is_some() {
            merged.fill = overlay.fill;
        }
        if overlay.pattern_fill.is_some() {
            merged.pattern_fill = overlay.pattern_fill;
        }
        merged.border = merge_border(self.border.as_ref(), overlay.border.as_ref());
        if overlay.num_fmt.is_some() {
            merged.num_fmt = overlay.num_fmt.clone();
        }
        merged.align = merge_alignment(self.align.as_ref(), overlay.align.as_ref());
        merged.protection = merge_protection(self.protection.as_ref(), overlay.protection.as_ref());
        merged
    }
    /// Set the font family name.
    pub fn font_name(mut self, name: impl AsRef<str>) -> Self {
        self.font.get_or_insert_with(Font::default).name = Some(name.as_ref().to_string());
        self
    }
    /// Set the font size in points.
    pub fn size(mut self, points: u16) -> Self {
        self.font.get_or_insert_with(Font::default).size_pt = Some(points);
        self
    }
    /// Set the text color.
    pub fn color(mut self, color: impl Into<Color>) -> Self {
        self.font.get_or_insert_with(Font::default).color = Some(color.into());
        self
    }
    /// Make the font bold.
    pub fn bold(mut self) -> Self {
        self.font.get_or_insert_with(Font::default).bold = true;
        self
    }
    /// Make the font italic.
    pub fn italic(mut self) -> Self {
        self.font.get_or_insert_with(Font::default).italic = true;
        self
    }

    /// Apply single underline to the font.
    pub fn underline(mut self) -> Self {
        self.font.get_or_insert_with(Font::default).underline = true;
        self
    }

    /// Apply strikethrough to the font.
    pub fn strikethrough(mut self) -> Self {
        self.font.get_or_insert_with(Font::default).strikethrough = true;
        self
    }

    /// Set the font superscript/subscript property.
    pub fn font_script(mut self, script: FormatScript) -> Self {
        self.font.get_or_insert_with(Font::default).script = script;
        self
    }

    /// Set a solid background fill color.
    pub fn fill(mut self, color: impl Into<Color>) -> Self {
        let color = color.into();
        self.fill = Some(color);
        self.pattern_fill = Some(Fill::solid(color));
        self
    }
    /// Set the fill pattern.
    pub fn pattern(mut self, pattern: FormatPattern) -> Self {
        self.pattern_fill.get_or_insert_with(Fill::default).pattern = pattern;
        self
    }
    /// Set the fill background color.
    pub fn background_color(mut self, color: impl Into<Color>) -> Self {
        let color = color.into();
        self.fill = Some(color);
        let fill = self.pattern_fill.get_or_insert_with(Fill::default);
        fill.background = Some(color);
        if fill.pattern == FormatPattern::None {
            fill.pattern = FormatPattern::Solid;
        }
        self
    }
    /// Set the fill foreground or pattern color.
    pub fn foreground_color(mut self, color: impl Into<Color>) -> Self {
        self.pattern_fill
            .get_or_insert_with(Fill::default)
            .foreground = Some(color.into());
        self
    }
    /// Set the fill object.
    pub fn pattern_fill(mut self, fill: Fill) -> Self {
        self.fill = fill.background;
        self.pattern_fill = Some(fill);
        self
    }
    #[cfg_attr(not(feature = "xlsx"), allow(dead_code))]
    pub(crate) fn effective_fill(&self) -> Option<Fill> {
        self.pattern_fill
            .or_else(|| self.fill.map(|c| Fill::solid(c.0)))
    }
    /// Set the number format code (e.g. `₩#,##0`, `0.0%`).
    pub fn num_fmt(mut self, code: impl AsRef<str>) -> Self {
        self.num_fmt = Some(code.as_ref().to_string());
        self
    }
    /// Wrap long text within the cell.
    pub fn wrap(mut self) -> Self {
        self.align.get_or_insert_with(Alignment::default).wrap = true;
        self
    }
    /// Set horizontal alignment.
    pub fn align(mut self, h: HAlign) -> Self {
        self.align.get_or_insert_with(Alignment::default).horizontal = Some(h);
        self
    }
    /// Set vertical alignment.
    pub fn valign(mut self, v: VAlign) -> Self {
        self.align.get_or_insert_with(Alignment::default).vertical = Some(v);
        self
    }
    /// Set the alignment object.
    pub fn alignment(mut self, alignment: Alignment) -> Self {
        self.align = Some(alignment);
        self
    }
    /// Set the left indent in character units.
    pub fn indent(mut self, level: u8) -> Self {
        self.align.get_or_insert_with(Alignment::default).indent = level;
        self
    }
    /// Shrink text to fit within the cell width.
    pub fn shrink_to_fit(mut self) -> Self {
        self.align
            .get_or_insert_with(Alignment::default)
            .shrink_to_fit = true;
        self
    }
    /// Set text rotation in degrees (`-90..=90`).
    pub fn text_rotation(mut self, degrees: i16) -> Self {
        self.align.get_or_insert_with(Alignment::default).rotation = degrees.clamp(-90, 90);
        self
    }
    /// Explicitly lock the cell when worksheet protection is enabled.
    pub fn locked(mut self) -> Self {
        self.protection
            .get_or_insert_with(CellProtection::default)
            .locked = Some(true);
        self
    }
    /// Unlock the cell when worksheet protection is enabled.
    pub fn unlocked(mut self) -> Self {
        self.protection
            .get_or_insert_with(CellProtection::default)
            .locked = Some(false);
        self
    }
    /// Hide formula text when worksheet protection is enabled.
    pub fn hidden(mut self) -> Self {
        self.protection
            .get_or_insert_with(CellProtection::default)
            .hidden = true;
        self
    }
    /// Set the cell borders.
    pub fn border(mut self, b: Border) -> Self {
        self.border = Some(b);
        self
    }
    /// Set the top border edge style.
    pub fn border_top(mut self, style: FormatBorder) -> Self {
        self.border.get_or_insert_with(Border::default).top = style;
        self
    }
    /// Set the bottom border edge style.
    pub fn border_bottom(mut self, style: FormatBorder) -> Self {
        self.border.get_or_insert_with(Border::default).bottom = style;
        self
    }
    /// Set the left border edge style.
    pub fn border_left(mut self, style: FormatBorder) -> Self {
        self.border.get_or_insert_with(Border::default).left = style;
        self
    }
    /// Set the right border edge style.
    pub fn border_right(mut self, style: FormatBorder) -> Self {
        self.border.get_or_insert_with(Border::default).right = style;
        self
    }
    /// Set the top border edge color.
    pub fn border_top_color(mut self, color: impl Into<Color>) -> Self {
        self.border.get_or_insert_with(Border::default).top_color = Some(color.into());
        self
    }
    /// Set the bottom border edge color.
    pub fn border_bottom_color(mut self, color: impl Into<Color>) -> Self {
        self.border.get_or_insert_with(Border::default).bottom_color = Some(color.into());
        self
    }
    /// Set the left border edge color.
    pub fn border_left_color(mut self, color: impl Into<Color>) -> Self {
        self.border.get_or_insert_with(Border::default).left_color = Some(color.into());
        self
    }
    /// Set the right border edge color.
    pub fn border_right_color(mut self, color: impl Into<Color>) -> Self {
        self.border.get_or_insert_with(Border::default).right_color = Some(color.into());
        self
    }
    /// Set the font family name.
    pub fn set_font_name(self, name: impl AsRef<str>) -> Self {
        self.font_name(name)
    }
    /// Set the font size in points.
    pub fn set_font_size(self, points: u16) -> Self {
        self.size(points)
    }
    /// Set the text color.
    pub fn set_font_color(self, color: impl Into<Color>) -> Self {
        self.color(color)
    }
    /// Make the font bold.
    pub fn set_bold(self) -> Self {
        self.bold()
    }
    /// Make the font italic.
    pub fn set_italic(self) -> Self {
        self.italic()
    }

    /// Apply single underline to the font.
    pub fn set_underline(self) -> Self {
        self.underline()
    }

    /// Apply strikethrough to the font.
    pub fn set_font_strikethrough(self) -> Self {
        self.strikethrough()
    }

    /// Apply strikethrough to the font.
    pub fn set_strikethrough(self) -> Self {
        self.strikethrough()
    }

    /// Set the font superscript/subscript property.
    pub fn set_font_script(self, script: FormatScript) -> Self {
        self.font_script(script)
    }

    /// Set a solid background fill color.
    pub fn set_bg_color(self, color: impl Into<Color>) -> Self {
        self.fill(color)
    }
    /// Set the fill background color.
    pub fn set_background_color(self, color: impl Into<Color>) -> Self {
        self.background_color(color)
    }
    /// Set the fill foreground or pattern color.
    pub fn set_foreground_color(self, color: impl Into<Color>) -> Self {
        self.foreground_color(color)
    }
    /// Set the fill object.
    pub fn set_pattern_fill(self, fill: Fill) -> Self {
        self.pattern_fill(fill)
    }
    /// Set the fill pattern.
    pub fn set_pattern(self, pattern: FormatPattern) -> Self {
        self.pattern(pattern)
    }
    /// Set the number format code.
    pub fn set_num_format(self, code: impl AsRef<str>) -> Self {
        self.num_fmt(code)
    }
    /// Set horizontal alignment.
    pub fn set_align(self, h: FormatAlign) -> Self {
        self.align(h)
    }
    /// Set vertical alignment.
    pub fn set_valign(self, v: VAlign) -> Self {
        self.valign(v)
    }
    /// Set the alignment object.
    pub fn set_alignment(self, alignment: Alignment) -> Self {
        self.alignment(alignment)
    }
    /// Wrap long text within the cell.
    pub fn set_text_wrap(self) -> Self {
        self.wrap()
    }
    /// Set the left indent in character units.
    pub fn set_indent(self, level: u8) -> Self {
        self.indent(level)
    }
    /// Shrink text to fit within the cell width.
    pub fn set_shrink_to_fit(self) -> Self {
        self.shrink_to_fit()
    }
    /// Set text rotation in degrees (`-90..=90`).
    pub fn set_text_rotation(self, degrees: i16) -> Self {
        self.text_rotation(degrees)
    }
    /// Explicitly lock the cell when worksheet protection is enabled.
    pub fn set_locked(self) -> Self {
        self.locked()
    }
    /// Unlock the cell when worksheet protection is enabled.
    pub fn set_unlocked(self) -> Self {
        self.unlocked()
    }
    /// Hide formula text when worksheet protection is enabled.
    pub fn set_hidden(self) -> Self {
        self.hidden()
    }
    /// Set the same border style on every cell edge.
    pub fn set_border(mut self, style: FormatBorder) -> Self {
        let border = self.border.get_or_insert_with(Border::default);
        border.left = style;
        border.right = style;
        border.top = style;
        border.bottom = style;
        self
    }
    /// Set the top border edge style.
    pub fn set_border_top(self, style: FormatBorder) -> Self {
        self.border_top(style)
    }
    /// Set the bottom border edge style.
    pub fn set_border_bottom(self, style: FormatBorder) -> Self {
        self.border_bottom(style)
    }
    /// Set the left border edge style.
    pub fn set_border_left(self, style: FormatBorder) -> Self {
        self.border_left(style)
    }
    /// Set the right border edge style.
    pub fn set_border_right(self, style: FormatBorder) -> Self {
        self.border_right(style)
    }
    /// Set the top border edge color.
    pub fn set_border_top_color(self, color: impl Into<Color>) -> Self {
        self.border_top_color(color)
    }
    /// Set the bottom border edge color.
    pub fn set_border_bottom_color(self, color: impl Into<Color>) -> Self {
        self.border_bottom_color(color)
    }
    /// Set the left border edge color.
    pub fn set_border_left_color(self, color: impl Into<Color>) -> Self {
        self.border_left_color(color)
    }
    /// Set the right border edge color.
    pub fn set_border_right_color(self, color: impl Into<Color>) -> Self {
        self.border_right_color(color)
    }
    /// Set the color used by all configured border edges.
    pub fn set_border_color(mut self, color: impl Into<Color>) -> Self {
        self.border.get_or_insert_with(Border::default).color = Some(color.into());
        self
    }
}
