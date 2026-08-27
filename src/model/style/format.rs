//! Writer-facing format builder facade.

use super::{
    Alignment, Border, CellStyle, Color, Fill, FormatAlign, FormatBorder, FormatPattern,
    FormatScript, HAlign, VAlign,
};

/// Writer format object, compatible with the existing [`CellStyle`] model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Format {
    style: CellStyle,
}

impl Format {
    /// A new empty writer format.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap an existing [`CellStyle`] as a writer format.
    pub fn from_cell_style(style: CellStyle) -> Self {
        Self { style }
    }

    /// Borrow the underlying [`CellStyle`].
    pub fn as_cell_style(&self) -> &CellStyle {
        &self.style
    }

    /// Convert this format into its underlying [`CellStyle`].
    pub fn into_cell_style(self) -> CellStyle {
        self.style
    }

    /// Merge this format with `overlay`, where fields explicitly set on
    /// `overlay` override this format and unset overlay fields preserve `self`.
    pub fn merge(&self, overlay: &Format) -> Self {
        Self {
            style: self.style.merge(overlay.as_cell_style()),
        }
    }

    /// Set the font family name.
    pub fn font_name(mut self, name: impl AsRef<str>) -> Self {
        self.style = self.style.font_name(name);
        self
    }

    /// Set the font size in points.
    pub fn size(mut self, points: u16) -> Self {
        self.style = self.style.size(points);
        self
    }

    /// Set the text color.
    pub fn color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.color(color);
        self
    }

    /// Make the font bold.
    pub fn bold(mut self) -> Self {
        self.style = self.style.bold();
        self
    }

    /// Make the font italic.
    pub fn italic(mut self) -> Self {
        self.style = self.style.italic();
        self
    }

    /// Apply single underline to the font.
    pub fn underline(mut self) -> Self {
        self.style = self.style.underline();
        self
    }

    /// Apply strikethrough to the font.
    pub fn strikethrough(mut self) -> Self {
        self.style = self.style.strikethrough();
        self
    }

    /// Set the font superscript/subscript property.
    pub fn font_script(mut self, script: FormatScript) -> Self {
        self.style = self.style.font_script(script);
        self
    }

    /// Set a solid background fill color.
    pub fn fill(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.fill(color);
        self
    }

    /// Set the fill pattern.
    pub fn pattern(mut self, pattern: FormatPattern) -> Self {
        self.style = self.style.pattern(pattern);
        self
    }

    /// Set the fill background color.
    pub fn background_color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.background_color(color);
        self
    }

    /// Set the fill foreground or pattern color.
    pub fn foreground_color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.foreground_color(color);
        self
    }

    /// Set the fill object.
    pub fn pattern_fill(mut self, fill: Fill) -> Self {
        self.style = self.style.pattern_fill(fill);
        self
    }

    /// Set the number format code (e.g. `0.0%`).
    pub fn num_fmt(mut self, code: impl AsRef<str>) -> Self {
        self.style = self.style.num_fmt(code);
        self
    }

    /// Wrap long text within the cell.
    pub fn wrap(mut self) -> Self {
        self.style = self.style.wrap();
        self
    }

    /// Set horizontal alignment.
    pub fn align(mut self, h: HAlign) -> Self {
        self.style = self.style.align(h);
        self
    }

    /// Set vertical alignment.
    pub fn valign(mut self, v: VAlign) -> Self {
        self.style = self.style.valign(v);
        self
    }

    /// Set the alignment object.
    pub fn alignment(mut self, alignment: Alignment) -> Self {
        self.style = self.style.alignment(alignment);
        self
    }

    /// Set the left indent in character units.
    pub fn indent(mut self, level: u8) -> Self {
        self.style = self.style.indent(level);
        self
    }

    /// Shrink text to fit within the cell width.
    pub fn shrink_to_fit(mut self) -> Self {
        self.style = self.style.shrink_to_fit();
        self
    }

    /// Set text rotation in degrees (`-90..=90`).
    pub fn text_rotation(mut self, degrees: i16) -> Self {
        self.style = self.style.text_rotation(degrees);
        self
    }

    /// Explicitly lock the cell when worksheet protection is enabled.
    pub fn locked(mut self) -> Self {
        self.style = self.style.locked();
        self
    }

    /// Unlock the cell when worksheet protection is enabled.
    pub fn unlocked(mut self) -> Self {
        self.style = self.style.unlocked();
        self
    }

    /// Hide formula text when worksheet protection is enabled.
    pub fn hidden(mut self) -> Self {
        self.style = self.style.hidden();
        self
    }

    /// Set the cell borders.
    pub fn border(mut self, border: Border) -> Self {
        self.style = self.style.border(border);
        self
    }

    /// Set the top border edge style.
    pub fn border_top(mut self, style: FormatBorder) -> Self {
        self.style = self.style.border_top(style);
        self
    }

    /// Set the bottom border edge style.
    pub fn border_bottom(mut self, style: FormatBorder) -> Self {
        self.style = self.style.border_bottom(style);
        self
    }

    /// Set the left border edge style.
    pub fn border_left(mut self, style: FormatBorder) -> Self {
        self.style = self.style.border_left(style);
        self
    }

    /// Set the right border edge style.
    pub fn border_right(mut self, style: FormatBorder) -> Self {
        self.style = self.style.border_right(style);
        self
    }

    /// Set the top border edge color.
    pub fn border_top_color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.border_top_color(color);
        self
    }

    /// Set the bottom border edge color.
    pub fn border_bottom_color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.border_bottom_color(color);
        self
    }

    /// Set the left border edge color.
    pub fn border_left_color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.border_left_color(color);
        self
    }

    /// Set the right border edge color.
    pub fn border_right_color(mut self, color: impl Into<Color>) -> Self {
        self.style = self.style.border_right_color(color);
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
        self.style = self.style.set_border(style);
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
        self.style = self.style.set_border_color(color);
        self
    }
}

impl From<CellStyle> for Format {
    fn from(style: CellStyle) -> Self {
        Self::from_cell_style(style)
    }
}

impl From<Format> for CellStyle {
    fn from(format: Format) -> Self {
        format.into_cell_style()
    }
}

impl std::ops::Deref for Format {
    type Target = CellStyle;

    fn deref(&self) -> &Self::Target {
        self.as_cell_style()
    }
}

impl std::ops::DerefMut for Format {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.style
    }
}
