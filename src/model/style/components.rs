//! Primitive cell-style components.

/// An RGB color, emitted as an 8-hex ARGB string (`FF` + `RRGGBB`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Color(pub [u8; 3]);

impl Color {
    /// Build an RGB color from red, green, and blue bytes.
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self([red, green, blue])
    }

    /// Return this color as `[red, green, blue]`.
    pub const fn as_rgb(self) -> [u8; 3] {
        self.0
    }
}

impl From<[u8; 3]> for Color {
    fn from(rgb: [u8; 3]) -> Self {
        Self(rgb)
    }
}

/// Font superscript/subscript setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum FormatScript {
    /// No superscript or subscript.
    #[default]
    None,
    /// Superscript.
    Superscript,
    /// Subscript.
    Subscript,
}

/// Excel cell fill pattern.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum FormatPattern {
    /// Automatic or empty pattern.
    #[default]
    None,
    /// Solid fill.
    Solid,
    /// Medium gray pattern.
    MediumGray,
    /// Dark gray pattern.
    DarkGray,
    /// Light gray pattern.
    LightGray,
    /// Dark horizontal lines.
    DarkHorizontal,
    /// Dark vertical lines.
    DarkVertical,
    /// Dark diagonal stripes.
    DarkDown,
    /// Reverse dark diagonal stripes.
    DarkUp,
    /// Dark grid pattern.
    DarkGrid,
    /// Dark trellis pattern.
    DarkTrellis,
    /// Light horizontal lines.
    LightHorizontal,
    /// Light vertical lines.
    LightVertical,
    /// Light diagonal stripes.
    LightDown,
    /// Reverse light diagonal stripes.
    LightUp,
    /// Light grid pattern.
    LightGrid,
    /// Light trellis pattern.
    LightTrellis,
    /// 12.5% gray pattern.
    Gray125,
    /// 6.25% gray pattern.
    Gray0625,
}

/// Cell fill formatting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Fill {
    /// Pattern type.
    pub pattern: FormatPattern,
    /// Background color.
    pub background: Option<Color>,
    /// Foreground or pattern color.
    pub foreground: Option<Color>,
}

impl Fill {
    /// Construct an empty fill.
    pub fn new() -> Self {
        Self::default()
    }

    /// A solid fill with the given RGB color.
    pub fn solid(color: impl Into<Color>) -> Self {
        Self {
            pattern: FormatPattern::Solid,
            background: Some(color.into()),
            foreground: None,
        }
    }

    /// Set the fill pattern.
    pub fn with_pattern(mut self, pattern: FormatPattern) -> Self {
        self.pattern = pattern;
        self
    }

    /// Set the fill background color.
    pub fn with_background(mut self, color: impl Into<Color>) -> Self {
        self.background = Some(color.into());
        if self.pattern == FormatPattern::None {
            self.pattern = FormatPattern::Solid;
        }
        self
    }

    /// Set the fill foreground or pattern color.
    pub fn with_foreground(mut self, color: impl Into<Color>) -> Self {
        self.foreground = Some(color.into());
        self
    }
}

/// Horizontal cell alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HAlign {
    /// Left-aligned.
    Left,
    /// Centered.
    Center,
    /// Right-aligned.
    Right,
}

/// Vertical cell alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VAlign {
    /// Top.
    Top,
    /// Middle.
    Middle,
    /// Bottom.
    Bottom,
}

/// Cell text alignment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Alignment {
    /// Horizontal alignment.
    pub horizontal: Option<HAlign>,
    /// Vertical alignment.
    pub vertical: Option<VAlign>,
    /// Wrap long text within the cell (essential for long Korean `공고명`).
    pub wrap: bool,
    /// Text rotation in degrees (`-90..=90`).
    pub rotation: i16,
    /// Left indent in character units (`0` = none).
    pub indent: u8,
    /// Shrink text to fit within the cell width.
    pub shrink_to_fit: bool,
}

impl Alignment {
    /// Construct an empty alignment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set horizontal alignment.
    pub fn with_horizontal(mut self, horizontal: HAlign) -> Self {
        self.horizontal = Some(horizontal);
        self
    }

    /// Set vertical alignment.
    pub fn with_vertical(mut self, vertical: VAlign) -> Self {
        self.vertical = Some(vertical);
        self
    }

    /// Wrap long text within the cell.
    pub fn wrapped(mut self) -> Self {
        self.wrap = true;
        self
    }

    /// Set whether long text wraps within the cell.
    pub fn with_wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    /// Set the left indent in character units.
    pub fn with_indent(mut self, level: u8) -> Self {
        self.indent = level;
        self
    }

    /// Set text rotation in degrees (`-90..=90`).
    pub fn with_rotation(mut self, degrees: i16) -> Self {
        self.rotation = degrees.clamp(-90, 90);
        self
    }

    /// Shrink text to fit within the cell width.
    pub fn with_shrink_to_fit(mut self) -> Self {
        self.shrink_to_fit = true;
        self
    }
}

/// A cell font.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Font {
    /// Font family name (e.g. `맑은 고딕`).
    pub name: Option<String>,
    /// Size in points.
    pub size_pt: Option<u16>,
    /// Text color.
    pub color: Option<Color>,
    /// Bold.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
    /// Single underline.
    pub underline: bool,
    /// Strikethrough.
    pub strikethrough: bool,
    /// Superscript/subscript setting.
    pub script: FormatScript,
}

impl Font {
    /// Construct a font with inherited/default run properties.
    pub fn new() -> Self {
        Font::default()
    }

    /// Set the font family name.
    pub fn with_name(mut self, name: impl AsRef<str>) -> Self {
        self.name = Some(name.as_ref().to_string());
        self
    }

    /// Set the font size in points.
    pub fn with_size(mut self, points: u16) -> Self {
        self.size_pt = Some(points);
        self
    }

    /// Set the font color.
    pub fn with_color(mut self, color: impl Into<Color>) -> Self {
        self.color = Some(color.into());
        self
    }

    /// Make the font bold.
    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    /// Make the font italic.
    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    /// Apply single underline.
    pub fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    /// Apply strikethrough.
    pub fn strikethrough(mut self) -> Self {
        self.strikethrough = true;
        self
    }

    /// Set superscript/subscript.
    pub fn with_script(mut self, script: FormatScript) -> Self {
        self.script = script;
        self
    }
}

/// One run of a rich (mixed-format) string: a text fragment plus the font applied
/// to it. Author a multi-format cell with [`crate::Sheet::write_rich`]; readers
/// retain supported run metadata through [`crate::Sheet::rich_text_runs`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct TextRun {
    /// The run's text.
    pub text: String,
    /// The run's font (`Font::default()` inherits the cell font).
    pub font: Font,
}

impl TextRun {
    /// A run with the given text and font.
    pub fn new(text: impl Into<String>, font: Font) -> Self {
        Self {
            text: text.into(),
            font,
        }
    }
}

/// A single border edge style.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum BorderStyle {
    /// No edge.
    #[default]
    None,
    /// Thin edge.
    Thin,
    /// Medium edge.
    Medium,
    /// Thick edge.
    Thick,
    /// Double edge.
    Double,
}

/// Cell borders (per edge).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Border {
    /// Left edge.
    pub left: BorderStyle,
    /// Right edge.
    pub right: BorderStyle,
    /// Top edge.
    pub top: BorderStyle,
    /// Bottom edge.
    pub bottom: BorderStyle,
    /// Border color (all edges).
    pub color: Option<Color>,
    /// Left edge color, overriding [`Self::color`] for the left edge.
    pub left_color: Option<Color>,
    /// Right edge color, overriding [`Self::color`] for the right edge.
    pub right_color: Option<Color>,
    /// Top edge color, overriding [`Self::color`] for the top edge.
    pub top_color: Option<Color>,
    /// Bottom edge color, overriding [`Self::color`] for the bottom edge.
    pub bottom_color: Option<Color>,
}

impl Border {
    /// Construct an empty border.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the same style on every edge.
    pub fn with_all(mut self, style: BorderStyle) -> Self {
        self.left = style;
        self.right = style;
        self.top = style;
        self.bottom = style;
        self
    }

    /// Set the left edge style.
    pub fn with_left(mut self, style: BorderStyle) -> Self {
        self.left = style;
        self
    }

    /// Set the right edge style.
    pub fn with_right(mut self, style: BorderStyle) -> Self {
        self.right = style;
        self
    }

    /// Set the top edge style.
    pub fn with_top(mut self, style: BorderStyle) -> Self {
        self.top = style;
        self
    }

    /// Set the bottom edge style.
    pub fn with_bottom(mut self, style: BorderStyle) -> Self {
        self.bottom = style;
        self
    }

    /// Set the color used by all configured edges.
    pub fn with_color(mut self, color: impl Into<Color>) -> Self {
        self.color = Some(color.into());
        self
    }

    /// Set the left edge color.
    pub fn with_left_color(mut self, color: impl Into<Color>) -> Self {
        self.left_color = Some(color.into());
        self
    }

    /// Set the right edge color.
    pub fn with_right_color(mut self, color: impl Into<Color>) -> Self {
        self.right_color = Some(color.into());
        self
    }

    /// Set the top edge color.
    pub fn with_top_color(mut self, color: impl Into<Color>) -> Self {
        self.top_color = Some(color.into());
        self
    }

    /// Set the bottom edge color.
    pub fn with_bottom_color(mut self, color: impl Into<Color>) -> Self {
        self.bottom_color = Some(color.into());
        self
    }
}
