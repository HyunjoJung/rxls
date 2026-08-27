//! XLSX chart parsing and import budgets.

use std::collections::HashMap;

use quick_xml::events::Event;
use quick_xml::Reader;

use super::{
    attr, bounded_imported_chart_latin_font_family, chart_markup_is_supported, local, text_of,
    theme_color_slot, unique_attr, unique_parsed_attr, with_general_ref_text, ThemeColors,
};
use crate::{
    Chart, ChartBarDirection, ChartCachedPoint, ChartFrameFill, ChartFrameStyleLossKind, ChartKind,
    ChartMarkerSymbol, ChartSeriesCache, ChartSeriesStyle, ChartSeriesStyleLossKind,
    ChartTextStyle, ChartTextStyles, ChartUnsupportedReason, Color, Series,
};

const MAX_XLSX_CHARTS_PER_WORKBOOK: usize = 16_384;

pub(crate) struct ChartImportBudget {
    pub(crate) charts_remaining: usize,
    pub(crate) cache_points_remaining: usize,
    pub(crate) series_remaining: usize,
    pub(crate) xml_work_remaining: usize,
    pub(crate) xml_work_limit: usize,
}

impl Default for ChartImportBudget {
    fn default() -> Self {
        Self {
            charts_remaining: MAX_XLSX_CHARTS_PER_WORKBOOK,
            cache_points_remaining: MAX_XLSX_CHART_CACHE_POINTS_PER_WORKBOOK,
            series_remaining: MAX_XLSX_CHART_SERIES_PER_WORKBOOK,
            xml_work_remaining: MAX_XLSX_CHART_XML_WORK_BYTES_PER_WORKBOOK,
            xml_work_limit: MAX_XLSX_CHART_XML_WORK_BYTES_PER_WORKBOOK,
        }
    }
}

impl ChartImportBudget {
    pub(crate) fn reserve_chart(&mut self) -> bool {
        if self.charts_remaining == 0 {
            false
        } else {
            self.charts_remaining -= 1;
            true
        }
    }

    pub(crate) fn reserve_xml_work(&mut self, work: usize) -> bool {
        if work > self.xml_work_remaining {
            false
        } else {
            self.xml_work_remaining -= work;
            true
        }
    }

    pub(crate) fn reconcile_xml_work(&mut self, declared: usize, actual: usize) -> bool {
        if actual > declared {
            self.reserve_xml_work(actual - declared)
        } else {
            self.xml_work_remaining = self
                .xml_work_remaining
                .saturating_add(declared - actual)
                .min(self.xml_work_limit);
            true
        }
    }
}

#[derive(Default)]
struct ParsedChartSeries {
    name: Option<String>,
    categories: Option<String>,
    values: Option<String>,
    bubble_sizes: Option<String>,
    invalid_text_fields: u8,
    source_position: usize,
    source_index_seen: bool,
    source_order_seen: bool,
    cache: ChartSeriesCache,
    style: ChartSeriesStyle,
}

const MAX_XLSX_CHART_SERIES_PER_WORKBOOK: usize = 4_096;
const MAX_XLSX_CHART_CACHE_POINTS_PER_WORKBOOK: usize = 1_000_000;
const MAX_XLSX_CHART_CACHE_VALUE_BYTES: usize = 4_096;
pub(super) const MAX_XLSX_CHART_TEXT_FIELD_BYTES: usize = 32_768;
const MAX_XLSX_CHART_AXIS_ITEMS: usize = 32;
pub(crate) const MAX_XLSX_CHART_XML_BYTES: u64 = 8 << 20;
pub(crate) const XLSX_CHART_XML_SCAN_PASSES: usize = 6;
pub(crate) const MAX_XLSX_CHART_XML_WORK_BYTES_PER_WORKBOOK: usize = 128 << 20;
// ECMA-376 Part 1 §20.1.10.35 (`ST_LineWidth`), in English Metric Units.
const MAX_OOXML_CHART_LINE_WIDTH_EMU: u32 = 20_116_800;

pub(crate) struct ParsedChart {
    pub(crate) chart: Chart,
    pub(crate) series_caches: Vec<ChartSeriesCache>,
    pub(crate) series_styles: Vec<ChartSeriesStyle>,
    pub(crate) text_styles: ChartTextStyles,
    pub(crate) frame_fill: ChartFrameFill,
    pub(crate) frame_style_losses: Vec<ChartFrameStyleLossKind>,
    pub(crate) category_major_gridlines: bool,
    pub(crate) value_major_gridlines: bool,
    pub(crate) category_axis_visible: Option<bool>,
    pub(crate) category_axis_shifted: Option<bool>,
    pub(crate) value_axis_visible: Option<bool>,
    pub(crate) limit_exceeded: bool,
    pub(crate) unsupported_reasons: Vec<ChartUnsupportedReason>,
    pub(crate) bar_direction: ChartBarDirection,
}

#[derive(Clone, Copy)]
enum ChartSeriesField {
    Name,
    Categories,
    Values,
    BubbleSizes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChartAxisContext {
    Category,
    Value,
}

#[derive(Clone, Copy)]
enum ChartTitleTarget {
    Main,
    CategoryAxis,
    ValueAxis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChartTextSemanticRole {
    ChartDefault,
    ChartTitle,
    CategoryAxisTitle,
    ValueAxisTitle,
    Legend,
    CategoryAxisLabels,
    ValueAxisLabels,
    DataLabels,
}

impl ChartTextSemanticRole {
    const COUNT: usize = 8;

    fn index(self) -> usize {
        match self {
            Self::ChartDefault => 0,
            Self::ChartTitle => 1,
            Self::CategoryAxisTitle => 2,
            Self::ValueAxisTitle => 3,
            Self::Legend => 4,
            Self::CategoryAxisLabels => 5,
            Self::ValueAxisLabels => 6,
            Self::DataLabels => 7,
        }
    }

    fn default_style(self, theme: &ThemeColors, color_map: &ChartTextColorMap) -> ChartTextStyle {
        let (size_hundredths_of_point, bold) = match self {
            Self::ChartDefault => (1_000, false),
            Self::ChartTitle => (1_800, true),
            Self::CategoryAxisTitle | Self::ValueAxisTitle => (1_000, true),
            Self::Legend | Self::CategoryAxisLabels | Self::ValueAxisLabels | Self::DataLabels => {
                (1_000, false)
            }
        };
        ChartTextStyle {
            latin_font_family: theme.chart_default_latin_font_family().to_string(),
            size_hundredths_of_point,
            color: chart_scheme_color(theme, color_map, "tx1")
                .unwrap_or_else(|| Color::rgb(0, 0, 0)),
            bold,
            italic: false,
            underline: false,
            strikethrough: false,
            kerning_minimum_hundredths_of_point: None,
            rotation_degrees: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PartialChartTextStyle {
    latin_font_family: Option<String>,
    size_hundredths_of_point: Option<u32>,
    color: Option<Color>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    strikethrough: Option<bool>,
    kerning_minimum_hundredths_of_point: Option<u32>,
}

impl PartialChartTextStyle {
    fn merge_from(&mut self, overlay: &Self) {
        if overlay.latin_font_family.is_some() {
            self.latin_font_family
                .clone_from(&overlay.latin_font_family);
        }
        if overlay.size_hundredths_of_point.is_some() {
            self.size_hundredths_of_point = overlay.size_hundredths_of_point;
        }
        if overlay.color.is_some() {
            self.color = overlay.color;
        }
        if overlay.bold.is_some() {
            self.bold = overlay.bold;
        }
        if overlay.italic.is_some() {
            self.italic = overlay.italic;
        }
        if overlay.underline.is_some() {
            self.underline = overlay.underline;
        }
        if overlay.strikethrough.is_some() {
            self.strikethrough = overlay.strikethrough;
        }
        if overlay.kerning_minimum_hundredths_of_point.is_some() {
            self.kerning_minimum_hundredths_of_point = overlay.kerning_minimum_hundredths_of_point;
        }
    }

    fn apply_to(&self, style: &mut ChartTextStyle) {
        if let Some(family) = self.latin_font_family.as_ref() {
            style.latin_font_family.clone_from(family);
        }
        if let Some(size) = self.size_hundredths_of_point {
            style.size_hundredths_of_point = size;
        }
        if let Some(color) = self.color {
            style.color = color;
        }
        if let Some(bold) = self.bold {
            style.bold = bold;
        }
        if let Some(italic) = self.italic {
            style.italic = italic;
        }
        if let Some(underline) = self.underline {
            style.underline = underline;
        }
        if let Some(strikethrough) = self.strikethrough {
            style.strikethrough = strikethrough;
        }
        if let Some(kerning) = self.kerning_minimum_hundredths_of_point {
            style.kerning_minimum_hundredths_of_point = Some(kerning);
        }
    }
}

#[derive(Debug, Clone, Default)]
enum ChartTextStyleObservation {
    #[default]
    Unseen,
    Uniform(ChartTextStyle),
    Mixed,
    Unsupported,
}

impl ChartTextStyleObservation {
    fn observe(&mut self, style: ChartTextStyle) {
        match self {
            Self::Unseen => *self = Self::Uniform(style),
            Self::Uniform(previous) if *previous == style => {}
            Self::Uniform(_) => *self = Self::Mixed,
            Self::Mixed | Self::Unsupported => {}
        }
    }

    fn mark_unsupported(&mut self) {
        *self = Self::Unsupported;
    }

    fn mark_mixed(&mut self) {
        if !matches!(self, Self::Unsupported) {
            *self = Self::Mixed;
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ChartTextStyleObservations {
    chart_default: ChartTextStyleObservation,
    chart_title: ChartTextStyleObservation,
    category_axis_title: ChartTextStyleObservation,
    value_axis_title: ChartTextStyleObservation,
    legend: ChartTextStyleObservation,
    category_axis_labels: ChartTextStyleObservation,
    value_axis_labels: ChartTextStyleObservation,
    data_labels: ChartTextStyleObservation,
}

impl ChartTextStyleObservations {
    fn get_mut(&mut self, role: ChartTextSemanticRole) -> &mut ChartTextStyleObservation {
        match role {
            ChartTextSemanticRole::ChartDefault => &mut self.chart_default,
            ChartTextSemanticRole::ChartTitle => &mut self.chart_title,
            ChartTextSemanticRole::CategoryAxisTitle => &mut self.category_axis_title,
            ChartTextSemanticRole::ValueAxisTitle => &mut self.value_axis_title,
            ChartTextSemanticRole::Legend => &mut self.legend,
            ChartTextSemanticRole::CategoryAxisLabels => &mut self.category_axis_labels,
            ChartTextSemanticRole::ValueAxisLabels => &mut self.value_axis_labels,
            ChartTextSemanticRole::DataLabels => &mut self.data_labels,
        }
    }

    fn finish(self, unsupported_reasons: &mut Vec<ChartUnsupportedReason>) -> ChartTextStyles {
        fn finish_one(
            observation: ChartTextStyleObservation,
            unsupported_reasons: &mut Vec<ChartUnsupportedReason>,
        ) -> Option<ChartTextStyle> {
            match observation {
                ChartTextStyleObservation::Unseen => None,
                ChartTextStyleObservation::Uniform(style) => Some(style),
                ChartTextStyleObservation::Mixed => {
                    add_chart_unsupported(
                        unsupported_reasons,
                        ChartUnsupportedReason::MixedTextStyle,
                    );
                    None
                }
                ChartTextStyleObservation::Unsupported => {
                    add_chart_unsupported(
                        unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedTextStyle,
                    );
                    None
                }
            }
        }

        let _ = finish_one(self.chart_default, unsupported_reasons);
        ChartTextStyles {
            chart_title: finish_one(self.chart_title, unsupported_reasons),
            category_axis_title: finish_one(self.category_axis_title, unsupported_reasons),
            value_axis_title: finish_one(self.value_axis_title, unsupported_reasons),
            legend: finish_one(self.legend, unsupported_reasons),
            category_axis_labels: finish_one(self.category_axis_labels, unsupported_reasons),
            value_axis_labels: finish_one(self.value_axis_labels, unsupported_reasons),
            data_labels: finish_one(self.data_labels, unsupported_reasons),
        }
    }
}

#[derive(Debug)]
struct ChartTextContext {
    kind: ChartKind,
    axis: Option<ChartAxisContext>,
    axis_roles: Vec<ChartAxisContext>,
    axis_occurrence: usize,
    series_depth: usize,
    title_role: Option<ChartTextSemanticRole>,
    title_depth: usize,
    legend_depth: usize,
    data_labels_depth: usize,
    display_units_label_depth: usize,
}

impl ChartTextContext {
    fn new(kind: ChartKind, axis_roles: &[ChartAxisContext]) -> Self {
        Self {
            kind,
            axis: None,
            axis_roles: axis_roles.to_vec(),
            axis_occurrence: 0,
            series_depth: 0,
            title_role: None,
            title_depth: 0,
            legend_depth: 0,
            data_labels_depth: 0,
            display_units_label_depth: 0,
        }
    }

    fn start(&mut self, name: &[u8]) {
        match name {
            b"ser" => self.series_depth = self.series_depth.saturating_add(1),
            b"catAx" | b"dateAx" | b"valAx" => {
                self.axis = self
                    .axis_roles
                    .get(self.axis_occurrence)
                    .copied()
                    .or(match name {
                        b"catAx" | b"dateAx" => Some(ChartAxisContext::Category),
                        b"valAx" if matches!(self.kind, ChartKind::Scatter | ChartKind::Bubble) => {
                            Some(ChartAxisContext::Value)
                        }
                        b"valAx" => Some(ChartAxisContext::Value),
                        _ => None,
                    });
                self.axis_occurrence = self.axis_occurrence.saturating_add(1);
            }
            b"title" if self.series_depth == 0 => {
                if self.title_depth == 0 {
                    self.title_role = Some(match self.axis {
                        Some(ChartAxisContext::Category) => {
                            ChartTextSemanticRole::CategoryAxisTitle
                        }
                        Some(ChartAxisContext::Value) => ChartTextSemanticRole::ValueAxisTitle,
                        None => ChartTextSemanticRole::ChartTitle,
                    });
                }
                self.title_depth = self.title_depth.saturating_add(1);
            }
            b"legend" => self.legend_depth = self.legend_depth.saturating_add(1),
            b"dLbls" | b"dLbl" => {
                self.data_labels_depth = self.data_labels_depth.saturating_add(1);
            }
            b"dispUnitsLbl" => {
                self.display_units_label_depth = self.display_units_label_depth.saturating_add(1);
            }
            _ => {}
        }
    }

    fn end(&mut self, name: &[u8]) {
        match name {
            b"ser" => self.series_depth = self.series_depth.saturating_sub(1),
            b"catAx" | b"dateAx" | b"valAx" => self.axis = None,
            b"title" if self.title_depth > 0 => {
                self.title_depth -= 1;
                if self.title_depth == 0 {
                    self.title_role = None;
                }
            }
            b"legend" => self.legend_depth = self.legend_depth.saturating_sub(1),
            b"dLbls" | b"dLbl" => {
                self.data_labels_depth = self.data_labels_depth.saturating_sub(1);
            }
            b"dispUnitsLbl" => {
                self.display_units_label_depth = self.display_units_label_depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    fn text_body_role(&self, rich: bool) -> Option<ChartTextSemanticRole> {
        if self.display_units_label_depth > 0 {
            return None;
        }
        if self.title_depth > 0 {
            return self.title_role;
        }
        if self.legend_depth > 0 {
            Some(ChartTextSemanticRole::Legend)
        } else if self.data_labels_depth > 0 {
            Some(ChartTextSemanticRole::DataLabels)
        } else if rich {
            None
        } else {
            match self.axis {
                Some(ChartAxisContext::Category) => Some(ChartTextSemanticRole::CategoryAxisLabels),
                Some(ChartAxisContext::Value) => Some(ChartTextSemanticRole::ValueAxisLabels),
                None if self.series_depth == 0 => Some(ChartTextSemanticRole::ChartDefault),
                None => None,
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ChartTextColorCapture {
    color: Color,
}

#[derive(Debug, Clone, Default)]
struct ChartTextPropertyCapture {
    style: PartialChartTextStyle,
    unsupported: bool,
    in_solid_fill: bool,
    solid_fill_seen: bool,
    color_transform_count: usize,
    color: Option<ChartTextColorCapture>,
}

// A single transform is evaluated exactly enough for the retained RGB model.
// Multiple sequential transforms would require preserving higher-precision
// DrawingML color state between operations, so they fail closed.
const MAX_CHART_TEXT_COLOR_TRANSFORMS: usize = 1;

const MIN_OOXML_CHART_TEXT_SIZE: u32 = 100;
const MAX_OOXML_CHART_TEXT_SIZE: u32 = 400_000;

pub(super) fn chart_text_bounded_size(value: &str) -> Option<u32> {
    value
        .parse::<u32>()
        .ok()
        .filter(|value| (MIN_OOXML_CHART_TEXT_SIZE..=MAX_OOXML_CHART_TEXT_SIZE).contains(value))
}

fn chart_text_bounded_kerning(value: &str) -> Option<u32> {
    value
        .parse::<u32>()
        .ok()
        .filter(|value| *value <= MAX_OOXML_CHART_TEXT_SIZE)
}

fn parse_chart_bool_attr(value: &str) -> Option<bool> {
    match value {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn parse_chart_boolean_element(
    element: &quick_xml::events::BytesStart<'_>,
) -> std::result::Result<bool, ()> {
    match unique_attr(element, b"val")? {
        Some(value) => parse_chart_bool_attr(&value).ok_or(()),
        None => Ok(true),
    }
}

fn resolve_chart_latin_typeface(value: &str, theme: &ThemeColors) -> Option<String> {
    match value.trim() {
        "+mn-lt" => Some(theme.chart_default_latin_font_family().to_string()),
        "+mj-lt" => Some(theme.chart_major_latin_font_family().to_string()),
        value if value.starts_with('+') => None,
        value => bounded_imported_chart_latin_font_family(value),
    }
}

#[derive(Debug, Clone, Default)]
struct ChartTextColorMap(HashMap<String, String>);

impl ChartTextColorMap {
    fn resolve<'a>(&'a self, value: &'a str) -> &'a str {
        self.0.get(value).map(String::as_str).unwrap_or(value)
    }
}

fn parse_chart_text_color_map(xml: &str) -> (ChartTextColorMap, bool) {
    const SOURCE_SLOTS: [&str; 12] = [
        "bg1", "tx1", "bg2", "tx2", "accent1", "accent2", "accent3", "accent4", "accent5",
        "accent6", "hlink", "folHlink",
    ];
    const TARGET_SLOTS: [&str; 12] = [
        "lt1", "dk1", "lt2", "dk2", "accent1", "accent2", "accent3", "accent4", "accent5",
        "accent6", "hlink", "folHlink",
    ];

    fn read_override_mapping(
        element: &quick_xml::events::BytesStart<'_>,
        map: &mut ChartTextColorMap,
    ) -> bool {
        let mut unsupported = false;
        for slot in SOURCE_SLOTS {
            match unique_attr(element, slot.as_bytes()) {
                Ok(Some(value)) if TARGET_SLOTS.contains(&value.as_str()) => {
                    map.0.insert(slot.to_string(), value);
                }
                Ok(Some(_)) | Ok(None) | Err(()) => unsupported = true,
            }
        }
        for attribute in element.attributes() {
            match attribute {
                Ok(attribute) => {
                    let qualified_name = attribute.key.as_ref();
                    let name = local(qualified_name);
                    if !SOURCE_SLOTS.iter().any(|slot| slot.as_bytes() == name)
                        && qualified_name != b"xmlns"
                        && !qualified_name.starts_with(b"xmlns:")
                    {
                        unsupported = true;
                    }
                }
                Err(_) => unsupported = true,
            }
        }
        unsupported
    }

    let mut reader = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut chart_space_depth = None;
    let mut override_depth = None;
    let mut seen_override = false;
    let mut override_child_seen = false;
    let mut map = ChartTextColorMap::default();
    let mut unsupported = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if name == b"chartSpace" && chart_space_depth.is_none() {
                    chart_space_depth = Some(depth);
                } else if chart_space_depth.is_some_and(|root| depth == root + 1)
                    && name == b"clrMapOvr"
                {
                    if seen_override {
                        unsupported = true;
                    }
                    seen_override = true;
                    override_depth = Some(depth);
                    override_child_seen = false;
                } else if override_depth.is_some_and(|parent| depth == parent + 1) {
                    if override_child_seen {
                        unsupported = true;
                    }
                    override_child_seen = true;
                    match name {
                        b"masterClrMapping" => {}
                        b"overrideClrMapping" => {
                            unsupported |= read_override_mapping(&element, &mut map);
                        }
                        _ => unsupported = true,
                    }
                } else if override_depth.is_some_and(|parent| depth > parent + 1) {
                    unsupported = true;
                }
                depth = depth.saturating_add(1);
            }
            Ok(Event::Empty(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if chart_space_depth.is_some_and(|root| depth == root + 1) && name == b"clrMapOvr" {
                    seen_override = true;
                    unsupported = true;
                } else if override_depth.is_some_and(|parent| depth == parent + 1) {
                    if override_child_seen {
                        unsupported = true;
                    }
                    override_child_seen = true;
                    match name {
                        b"masterClrMapping" => {}
                        b"overrideClrMapping" => {
                            unsupported |= read_override_mapping(&element, &mut map);
                        }
                        _ => unsupported = true,
                    }
                } else if override_depth.is_some_and(|parent| depth > parent + 1) {
                    unsupported = true;
                }
            }
            Ok(Event::End(element)) => {
                depth = depth.saturating_sub(1);
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if name == b"clrMapOvr" && override_depth == Some(depth) {
                    unsupported |= !override_child_seen;
                    override_depth = None;
                    override_child_seen = false;
                }
                if name == b"chartSpace" && chart_space_depth == Some(depth) {
                    chart_space_depth = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                unsupported = true;
                break;
            }
            _ => {}
        }
    }
    unsupported |= override_depth.is_some();
    (map, unsupported)
}

fn chart_scheme_color(
    theme: &ThemeColors,
    color_map: &ChartTextColorMap,
    value: &str,
) -> Option<Color> {
    let value = color_map.resolve(value);
    let fallback = match value.as_bytes() {
        b"bg1" | b"lt1" => Some(Color::rgb(255, 255, 255)),
        b"tx1" | b"dk1" => Some(Color::rgb(0, 0, 0)),
        b"bg2" | b"lt2" => Some(Color::rgb(238, 236, 225)),
        b"tx2" | b"dk2" => Some(Color::rgb(31, 73, 125)),
        _ => None,
    };
    let slot_name = match value.as_bytes() {
        b"bg1" => b"lt1".as_slice(),
        b"tx1" => b"dk1".as_slice(),
        b"bg2" => b"lt2".as_slice(),
        b"tx2" => b"dk2".as_slice(),
        value => value,
    };
    theme_color_slot(slot_name)
        .and_then(|slot| theme.color(slot, None))
        .or_else(|| {
            let index = match value.as_bytes() {
                b"accent1" => 0,
                b"accent2" => 1,
                b"accent3" => 2,
                b"accent4" => 3,
                b"accent5" => 4,
                b"accent6" => 5,
                _ => return fallback,
            };
            theme.chart_palette().get(index).copied()
        })
        .or(fallback)
}

fn rgb_to_hsl(color: Color) -> (f64, f64, f64) {
    let [red, green, blue] = color.as_rgb();
    let red = f64::from(red) / 255.0;
    let green = f64::from(green) / 255.0;
    let blue = f64::from(blue) / 255.0;
    let maximum = red.max(green).max(blue);
    let minimum = red.min(green).min(blue);
    let lightness = (maximum + minimum) / 2.0;
    let difference = maximum - minimum;
    if difference == 0.0 {
        return (0.0, 0.0, lightness);
    }
    let saturation = difference / (1.0 - (2.0 * lightness - 1.0).abs());
    let hue = if maximum == red {
        ((green - blue) / difference).rem_euclid(6.0)
    } else if maximum == green {
        (blue - red) / difference + 2.0
    } else {
        (red - green) / difference + 4.0
    } / 6.0;
    (hue, saturation, lightness)
}

fn hsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> Color {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let hue_sector = hue.rem_euclid(1.0) * 6.0;
    let intermediate = chroma * (1.0 - (hue_sector.rem_euclid(2.0) - 1.0).abs());
    let (red, green, blue) = if hue_sector < 1.0 {
        (chroma, intermediate, 0.0)
    } else if hue_sector < 2.0 {
        (intermediate, chroma, 0.0)
    } else if hue_sector < 3.0 {
        (0.0, chroma, intermediate)
    } else if hue_sector < 4.0 {
        (0.0, intermediate, chroma)
    } else if hue_sector < 5.0 {
        (intermediate, 0.0, chroma)
    } else {
        (chroma, 0.0, intermediate)
    };
    let match_value = lightness - chroma / 2.0;
    let channel = |value: f64| ((value + match_value) * 255.0).round().clamp(0.0, 255.0) as u8;
    Color::rgb(channel(red), channel(green), channel(blue))
}

pub(super) fn apply_chart_luminance(color: Color, modulation: u32, offset: u32) -> Color {
    let (hue, saturation, lightness) = rgb_to_hsl(color);
    let lightness = (lightness * f64::from(modulation) / 100_000.0 + f64::from(offset) / 100_000.0)
        .clamp(0.0, 1.0);
    hsl_to_rgb(hue, saturation, lightness)
}

fn parse_chart_color_transform(value: Option<String>) -> Option<u32> {
    value
        .as_deref()?
        .parse::<u32>()
        .ok()
        .filter(|value| *value <= 100_000)
}

pub(super) fn parse_chart_rgb(value: &str) -> Option<Color> {
    if value.len() != 6 || !value.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    Some(Color::rgb(
        u8::from_str_radix(&value[0..2], 16).ok()?,
        u8::from_str_radix(&value[2..4], 16).ok()?,
        u8::from_str_radix(&value[4..6], 16).ok()?,
    ))
}

fn chart_text_unique_attr(
    element: &quick_xml::events::BytesStart<'_>,
    name: &[u8],
    unsupported: &mut bool,
) -> Option<String> {
    match unique_attr(element, name) {
        Ok(value) => value,
        Err(()) => {
            *unsupported = true;
            None
        }
    }
}

pub(super) fn chart_text_attributes_are_subset(
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

fn chart_text_partial_from_attributes(
    element: &quick_xml::events::BytesStart<'_>,
) -> ChartTextPropertyCapture {
    let mut capture = ChartTextPropertyCapture::default();
    if !chart_text_attributes_are_subset(
        element,
        &[
            b"sz",
            b"b",
            b"i",
            b"u",
            b"strike",
            b"kern",
            b"baseline",
            b"spc",
            b"cap",
            b"normalizeH",
            b"kumimoji",
        ],
    ) {
        capture.unsupported = true;
    }
    if let Some(value) = chart_text_unique_attr(element, b"sz", &mut capture.unsupported) {
        match chart_text_bounded_size(&value) {
            Some(value) => capture.style.size_hundredths_of_point = Some(value),
            None => capture.unsupported = true,
        }
    }
    if let Some(value) = chart_text_unique_attr(element, b"b", &mut capture.unsupported) {
        match parse_chart_bool_attr(&value) {
            Some(value) => capture.style.bold = Some(value),
            None => capture.unsupported = true,
        }
    }
    if let Some(value) = chart_text_unique_attr(element, b"i", &mut capture.unsupported) {
        match parse_chart_bool_attr(&value) {
            Some(value) => capture.style.italic = Some(value),
            None => capture.unsupported = true,
        }
    }
    if let Some(value) = chart_text_unique_attr(element, b"u", &mut capture.unsupported) {
        match value.as_str() {
            "none" => capture.style.underline = Some(false),
            "sng" => capture.style.underline = Some(true),
            _ => capture.unsupported = true,
        }
    }
    if let Some(value) = chart_text_unique_attr(element, b"strike", &mut capture.unsupported) {
        match value.as_str() {
            "noStrike" => capture.style.strikethrough = Some(false),
            "sngStrike" => capture.style.strikethrough = Some(true),
            _ => capture.unsupported = true,
        }
    }
    if let Some(value) = chart_text_unique_attr(element, b"kern", &mut capture.unsupported) {
        match chart_text_bounded_kerning(&value) {
            Some(value) => capture.style.kerning_minimum_hundredths_of_point = Some(value),
            None => capture.unsupported = true,
        }
    }
    for name in [b"baseline".as_slice(), b"spc".as_slice()] {
        if chart_text_unique_attr(element, name, &mut capture.unsupported)
            .as_deref()
            .is_some_and(|value| value.parse::<i32>().ok() != Some(0))
        {
            capture.unsupported = true;
        }
    }
    if chart_text_unique_attr(element, b"cap", &mut capture.unsupported)
        .as_deref()
        .is_some_and(|value| value != "none")
    {
        capture.unsupported = true;
    }
    for name in [b"normalizeH".as_slice(), b"kumimoji".as_slice()] {
        if let Some(value) = chart_text_unique_attr(element, name, &mut capture.unsupported) {
            if parse_chart_bool_attr(&value) != Some(false) {
                capture.unsupported = true;
            }
        }
    }
    capture
}

fn update_chart_text_property_capture(
    capture: &mut ChartTextPropertyCapture,
    element: &quick_xml::events::BytesStart<'_>,
    theme: &ThemeColors,
    color_map: &ChartTextColorMap,
    empty: bool,
) {
    let qualified_name = element.name();
    let name = local(qualified_name.as_ref());
    match name {
        b"latin" => {
            if !chart_text_attributes_are_subset(element, &[b"typeface"]) {
                capture.unsupported = true;
            }
            let family = unique_attr(element, b"typeface")
                .ok()
                .flatten()
                .as_deref()
                .and_then(|value| resolve_chart_latin_typeface(value, theme));
            if family.is_none() || capture.style.latin_font_family.is_some() {
                capture.unsupported = true;
            } else {
                capture.style.latin_font_family = family;
            }
        }
        b"ea" | b"cs" => {
            if !chart_text_attributes_are_subset(element, &[b"typeface"]) {
                capture.unsupported = true;
            }
            match unique_attr(element, b"typeface") {
                Ok(None) => {}
                Ok(Some(value)) if value.trim().is_empty() => {}
                _ => capture.unsupported = true,
            }
        }
        b"solidFill" => {
            if !chart_text_attributes_are_subset(element, &[]) {
                capture.unsupported = true;
            }
            if capture.solid_fill_seen || capture.in_solid_fill {
                capture.unsupported = true;
            }
            capture.solid_fill_seen = true;
            capture.in_solid_fill = true;
            if empty {
                capture.unsupported = true;
                capture.in_solid_fill = false;
            }
        }
        b"srgbClr" | b"schemeClr" | b"sysClr" if capture.in_solid_fill => {
            let allowed: &[&[u8]] = if name == b"sysClr" {
                &[b"val", b"lastClr"]
            } else {
                &[b"val"]
            };
            if !chart_text_attributes_are_subset(element, allowed) {
                capture.unsupported = true;
            }
            let duplicate_color = capture.color.is_some() || capture.style.color.is_some();
            if duplicate_color {
                capture.unsupported = true;
            }
            let color = match name {
                b"srgbClr" => unique_attr(element, b"val")
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(parse_chart_rgb),
                b"schemeClr" => unique_attr(element, b"val")
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(|value| chart_scheme_color(theme, color_map, value)),
                b"sysClr" => unique_attr(element, b"lastClr")
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(parse_chart_rgb),
                _ => unreachable!("guarded chart text color"),
            };
            match color {
                Some(color) if !duplicate_color => {
                    capture.color = Some(ChartTextColorCapture { color });
                }
                Some(_) => {}
                None => capture.unsupported = true,
            }
            if empty {
                if let Some(color) = capture.color.take() {
                    capture.style.color = Some(color.color);
                }
            }
        }
        b"lumMod" | b"lumOff" | b"tint" | b"shade" if capture.color.is_some() => {
            if !chart_text_attributes_are_subset(element, &[b"val"]) {
                capture.unsupported = true;
            }
            capture.color_transform_count = capture.color_transform_count.saturating_add(1);
            if capture.color_transform_count > MAX_CHART_TEXT_COLOR_TRANSFORMS {
                capture.unsupported = true;
                return;
            }
            let Some(value) = unique_attr(element, b"val")
                .ok()
                .flatten()
                .and_then(|value| parse_chart_color_transform(Some(value)))
            else {
                capture.unsupported = true;
                return;
            };
            let color = capture.color.as_mut().expect("chart color checked above");
            color.color = match name {
                b"lumMod" => apply_chart_luminance(color.color, value, 0),
                b"lumOff" => apply_chart_luminance(color.color, 100_000, value),
                b"tint" => apply_chart_luminance(color.color, value, 100_000 - value),
                b"shade" => apply_chart_luminance(color.color, value, 0),
                _ => unreachable!("guarded luminance transform"),
            };
        }
        b"lumMod" | b"lumOff" | b"tint" | b"shade" => capture.unsupported = true,
        b"noFill" | b"gradFill" | b"pattFill" | b"blipFill" | b"grpFill" | b"highlight"
        | b"uLn" | b"uLnTx" | b"uFill" | b"uFillTx" | b"rtl" | b"sym" | b"ln" | b"effectLst"
        | b"effectDag" | b"scene3d" | b"sp3d" | b"glow" | b"outerShdw" | b"innerShdw"
        | b"reflection" | b"softEdge" | b"hlinkClick" | b"hlinkMouseOver" => {
            capture.unsupported = true;
        }
        b"alpha" | b"alphaMod" | b"alphaOff" | b"blue" | b"blueMod" | b"blueOff" | b"green"
        | b"greenMod" | b"greenOff" | b"hue" | b"hueMod" | b"hueOff" | b"lum" | b"red"
        | b"redMod" | b"redOff" | b"sat" | b"satMod" | b"satOff" | b"comp" | b"gamma" | b"gray"
        | b"inv" | b"invGamma" => capture.unsupported = true,
        _ => capture.unsupported = true,
    }
}

fn finish_chart_text_property_element(capture: &mut ChartTextPropertyCapture, name: &[u8]) {
    match name {
        b"srgbClr" | b"schemeClr" | b"sysClr" if capture.in_solid_fill => {
            if let Some(color) = capture.color.take() {
                capture.style.color = Some(color.color);
            } else {
                capture.unsupported = true;
            }
        }
        b"solidFill" => {
            if capture.style.color.is_none() {
                capture.unsupported = true;
            }
            capture.in_solid_fill = false;
            capture.color = None;
        }
        _ => {}
    }
}

pub(super) const MAX_CHART_TEXT_STYLE_FACTS_PER_ROLE: usize = 16;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ChartTextRotationState {
    #[default]
    Inherit,
    Automatic,
    Degrees(i16),
}

#[derive(Debug, Clone, Default)]
enum PartialChartTextStyleObservation {
    #[default]
    Unseen,
    Uniform {
        style: PartialChartTextStyle,
        rotation: ChartTextRotationState,
    },
    Mixed,
    Unsupported,
}

impl PartialChartTextStyleObservation {
    fn observe(&mut self, style: PartialChartTextStyle, rotation: ChartTextRotationState) {
        match self {
            Self::Unseen => *self = Self::Uniform { style, rotation },
            Self::Uniform {
                style: previous_style,
                rotation: previous_rotation,
            } if *previous_style == style && *previous_rotation == rotation => {}
            Self::Uniform { .. } => *self = Self::Mixed,
            Self::Mixed | Self::Unsupported => {}
        }
    }

    fn mark_unsupported(&mut self) {
        *self = Self::Unsupported;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaintedChartTextStyleFact {
    style: PartialChartTextStyle,
    rotation: ChartTextRotationState,
    unsupported: bool,
}

#[derive(Debug, Clone, Copy)]
enum ChartTextPropertyTarget {
    List(usize),
    Paragraph,
    Run,
}

#[derive(Debug)]
struct UnifiedChartTextBody {
    rich: bool,
    role: ChartTextSemanticRole,
    rotation: ChartTextRotationState,
    body_unsupported: bool,
    body_properties_seen: bool,
    body_properties_open: bool,
    autofit_seen: bool,
    list_style_seen: bool,
    list_style_open: bool,
    paragraph_open: bool,
    paragraph_properties_open: bool,
    paragraph_content_started: bool,
    run_open: bool,
    list_styles: [PartialChartTextStyle; 9],
    list_unsupported: [bool; 9],
    list_property_seen: [bool; 9],
    list_level_context: Option<usize>,
    paragraph_level: usize,
    paragraph_style: PartialChartTextStyle,
    paragraph_unsupported: bool,
    paragraph_property_seen: bool,
    run_style: PartialChartTextStyle,
    run_unsupported: bool,
    run_property_seen: bool,
    in_text: bool,
    current_run_painted: bool,
    current_paragraph_painted: bool,
    paragraph_seen: bool,
    painted_paragraphs: usize,
    default_candidate: Option<PartialChartTextStyle>,
    default_candidate_mixed: bool,
    default_candidate_unsupported: bool,
}

impl UnifiedChartTextBody {
    fn new(rich: bool, role: ChartTextSemanticRole) -> Self {
        Self {
            rich,
            role,
            rotation: ChartTextRotationState::Inherit,
            body_unsupported: false,
            body_properties_seen: false,
            body_properties_open: false,
            autofit_seen: false,
            list_style_seen: false,
            list_style_open: false,
            paragraph_open: false,
            paragraph_properties_open: false,
            paragraph_content_started: false,
            run_open: false,
            list_styles: std::array::from_fn(|_| PartialChartTextStyle::default()),
            list_unsupported: [false; 9],
            list_property_seen: [false; 9],
            list_level_context: None,
            paragraph_level: 0,
            paragraph_style: PartialChartTextStyle::default(),
            paragraph_unsupported: false,
            paragraph_property_seen: false,
            run_style: PartialChartTextStyle::default(),
            run_unsupported: false,
            run_property_seen: false,
            in_text: false,
            current_run_painted: false,
            current_paragraph_painted: false,
            paragraph_seen: false,
            painted_paragraphs: 0,
            default_candidate: None,
            default_candidate_mixed: false,
            default_candidate_unsupported: false,
        }
    }

    fn reset_paragraph(&mut self) {
        self.paragraph_level = 0;
        self.paragraph_style = PartialChartTextStyle::default();
        self.paragraph_unsupported = false;
        self.paragraph_property_seen = false;
        self.paragraph_content_started = false;
        self.run_style = PartialChartTextStyle::default();
        self.run_unsupported = false;
        self.run_property_seen = false;
        self.current_run_painted = false;
        self.current_paragraph_painted = false;
    }

    fn reset_run(&mut self) {
        self.run_style = PartialChartTextStyle::default();
        self.run_unsupported = false;
        self.run_property_seen = false;
        self.current_run_painted = false;
    }

    fn effective_partial(&self, include_run: bool) -> PartialChartTextStyle {
        let mut style = self.list_styles[self.paragraph_level].clone();
        style.merge_from(&self.paragraph_style);
        if include_run {
            style.merge_from(&self.run_style);
        }
        style
    }

    fn observe_default_candidate(&mut self) {
        if self.rich {
            return;
        }
        self.default_candidate_unsupported |=
            self.list_unsupported[self.paragraph_level] || self.paragraph_unsupported;
        let candidate = self.effective_partial(false);
        match self.default_candidate.as_ref() {
            Some(previous) if previous != &candidate => self.default_candidate_mixed = true,
            None => self.default_candidate = Some(candidate),
            _ => {}
        }
    }

    fn property_target(&self, run: bool) -> ChartTextPropertyTarget {
        if run {
            ChartTextPropertyTarget::Run
        } else if let Some(level) = self.list_level_context {
            ChartTextPropertyTarget::List(level)
        } else {
            ChartTextPropertyTarget::Paragraph
        }
    }

    fn apply_property(
        &mut self,
        target: ChartTextPropertyTarget,
        capture: ChartTextPropertyCapture,
    ) {
        match target {
            ChartTextPropertyTarget::List(level) => {
                if self.list_property_seen[level] {
                    self.list_unsupported[level] = true;
                }
                self.list_property_seen[level] = true;
                self.list_unsupported[level] |= capture.unsupported;
                self.list_styles[level].merge_from(&capture.style);
            }
            ChartTextPropertyTarget::Paragraph => {
                if self.paragraph_property_seen {
                    self.paragraph_unsupported = true;
                }
                self.paragraph_property_seen = true;
                self.paragraph_unsupported |= capture.unsupported;
                self.paragraph_style.merge_from(&capture.style);
            }
            ChartTextPropertyTarget::Run => {
                if self.run_property_seen {
                    self.run_unsupported = true;
                }
                self.run_property_seen = true;
                self.run_unsupported |= capture.unsupported;
                self.run_style.merge_from(&capture.style);
            }
        }
    }

    fn current_fact(&mut self) -> PaintedChartTextStyleFact {
        self.current_run_painted = true;
        if !self.current_paragraph_painted {
            self.current_paragraph_painted = true;
            self.painted_paragraphs = self.painted_paragraphs.saturating_add(1);
        }
        PaintedChartTextStyleFact {
            style: self.effective_partial(true),
            rotation: self.rotation,
            unsupported: self.body_unsupported
                || self.list_unsupported[self.paragraph_level]
                || self.paragraph_unsupported
                || self.run_unsupported
                || self.painted_paragraphs > 1,
        }
    }
}

fn chart_text_list_level(name: &[u8]) -> Option<usize> {
    match name {
        b"lvl1pPr" => Some(0),
        b"lvl2pPr" => Some(1),
        b"lvl3pPr" => Some(2),
        b"lvl4pPr" => Some(3),
        b"lvl5pPr" => Some(4),
        b"lvl6pPr" => Some(5),
        b"lvl7pPr" => Some(6),
        b"lvl8pPr" => Some(7),
        b"lvl9pPr" => Some(8),
        _ => None,
    }
}

fn normalize_imported_chart_rotation(degrees: i32) -> Option<i16> {
    let normalized = degrees.rem_euclid(360);
    i16::try_from(if normalized > 180 {
        normalized - 360
    } else {
        normalized
    })
    .ok()
}

fn parse_chart_body_rotation_state(
    element: &quick_xml::events::BytesStart<'_>,
    role: ChartTextSemanticRole,
) -> std::result::Result<ChartTextRotationState, ()> {
    if !chart_text_attributes_are_subset(element, &[b"vert", b"rot"]) {
        return Err(());
    }
    if unique_attr(element, b"vert")?
        .as_deref()
        .is_some_and(|value| value != "horz")
    {
        return Err(());
    }
    let Some(value) = unique_attr(element, b"rot")? else {
        return Ok(ChartTextRotationState::Inherit);
    };
    if value == "-60000000" {
        return if matches!(
            role,
            ChartTextSemanticRole::CategoryAxisLabels | ChartTextSemanticRole::ValueAxisLabels
        ) {
            Ok(ChartTextRotationState::Automatic)
        } else {
            Err(())
        };
    }
    let value = value.parse::<i32>().map_err(|_| ())?;
    if value % 60_000 != 0 {
        return Err(());
    }
    normalize_imported_chart_rotation(value / 60_000)
        .map(ChartTextRotationState::Degrees)
        .ok_or(())
}

fn resolve_chart_text_rotation(
    inherited: Option<i16>,
    overlay: ChartTextRotationState,
) -> Option<i16> {
    match overlay {
        ChartTextRotationState::Inherit => inherited,
        ChartTextRotationState::Automatic => None,
        ChartTextRotationState::Degrees(degrees) => Some(degrees),
    }
}

fn chart_text_norm_autofit_is_neutral(element: &quick_xml::events::BytesStart<'_>) -> bool {
    if !chart_text_attributes_are_subset(element, &[b"fontScale", b"lnSpcReduction"]) {
        return false;
    }
    let font_scale = match unique_attr(element, b"fontScale") {
        Ok(None) => 100_000,
        Ok(Some(value)) => match value.parse::<u32>() {
            Ok(value) => value,
            Err(_) => return false,
        },
        Err(()) => return false,
    };
    let line_spacing_reduction = match unique_attr(element, b"lnSpcReduction") {
        Ok(None) => 0,
        Ok(Some(value)) => match value.parse::<u32>() {
            Ok(value) => value,
            Err(_) => return false,
        },
        Err(()) => return false,
    };
    font_scale == 100_000 && line_spacing_reduction == 0
}

fn apply_chart_text_paragraph_properties(
    body: &mut UnifiedChartTextBody,
    element: &quick_xml::events::BytesStart<'_>,
) {
    if !chart_text_attributes_are_subset(element, &[b"lvl"]) {
        body.paragraph_unsupported = true;
    }
    match unique_parsed_attr::<usize>(element, b"lvl") {
        Ok(Some(level)) if level <= 8 => body.paragraph_level = level,
        Ok(None) => {}
        _ => body.paragraph_unsupported = true,
    }
}

fn resolve_unified_chart_text_style(
    role: ChartTextSemanticRole,
    theme: &ThemeColors,
    color_map: &ChartTextColorMap,
    chart_default: Option<(&PartialChartTextStyle, ChartTextRotationState)>,
    role_default: Option<(&PartialChartTextStyle, ChartTextRotationState)>,
    fact: Option<&PaintedChartTextStyleFact>,
) -> ChartTextStyle {
    let mut style = role.default_style(theme, color_map);
    let mut rotation = None;
    if let Some((partial, state)) = chart_default {
        partial.apply_to(&mut style);
        rotation = resolve_chart_text_rotation(rotation, state);
    }
    if let Some((partial, state)) = role_default {
        partial.apply_to(&mut style);
        rotation = resolve_chart_text_rotation(rotation, state);
    }
    if let Some(fact) = fact {
        fact.style.apply_to(&mut style);
        rotation = resolve_chart_text_rotation(rotation, fact.rotation);
    }
    style.rotation_degrees = rotation;
    style
}

fn push_chart_text_style_fact(
    role: ChartTextSemanticRole,
    fact: PaintedChartTextStyleFact,
    facts: &mut [Vec<PaintedChartTextStyleFact>; ChartTextSemanticRole::COUNT],
    role_unsupported: &mut [bool; ChartTextSemanticRole::COUNT],
    limit_exceeded: &mut bool,
) {
    let target = &mut facts[role.index()];
    if target.last() == Some(&fact) {
        return;
    }
    match target.len().cmp(&MAX_CHART_TEXT_STYLE_FACTS_PER_ROLE) {
        std::cmp::Ordering::Equal => {
            role_unsupported[role.index()] = true;
            *limit_exceeded = true;
        }
        std::cmp::Ordering::Less => target.push(fact),
        std::cmp::Ordering::Greater => {}
    }
}

fn parse_chart_text_styles_unified(
    xml: &str,
    kind: ChartKind,
    axis_roles: &[ChartAxisContext],
    theme: &ThemeColors,
    unsupported_reasons: &mut Vec<ChartUnsupportedReason>,
    limit_exceeded: &mut bool,
) -> ChartTextStyles {
    let (color_map, invalid_color_map) = parse_chart_text_color_map(xml);
    if invalid_color_map {
        add_chart_unsupported(
            unsupported_reasons,
            ChartUnsupportedReason::UnsupportedTextStyle,
        );
        return ChartTextStyles::default();
    }
    let mut reader = Reader::from_str(xml);
    let mut context = ChartTextContext::new(kind, axis_roles);
    let mut defaults: [PartialChartTextStyleObservation; ChartTextSemanticRole::COUNT] =
        std::array::from_fn(|_| PartialChartTextStyleObservation::default());
    let mut facts: [Vec<PaintedChartTextStyleFact>; ChartTextSemanticRole::COUNT] =
        std::array::from_fn(|_| Vec::new());
    let mut role_unsupported = [false; ChartTextSemanticRole::COUNT];
    let mut body: Option<UnifiedChartTextBody> = None;
    let mut property: Option<(ChartTextPropertyTarget, ChartTextPropertyCapture, usize)> = None;
    let mut ignored_end_paragraph_depth = 0usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if ignored_end_paragraph_depth > 0 {
                    ignored_end_paragraph_depth = ignored_end_paragraph_depth.saturating_add(1);
                    continue;
                }
                if let Some((_, capture, depth)) = property.as_mut() {
                    update_chart_text_property_capture(capture, &element, theme, &color_map, false);
                    *depth = depth.saturating_add(1);
                    continue;
                }
                context.start(name);
                if name == b"txPr" || name == b"rich" {
                    let rich = name == b"rich";
                    if let Some(active) = body.as_mut() {
                        active.body_unsupported = true;
                    } else {
                        body = context
                            .text_body_role(rich)
                            .map(|role| UnifiedChartTextBody::new(rich, role));
                    }
                } else if let Some(body) = body.as_mut() {
                    if name == b"endParaRPr" {
                        if !body.paragraph_open || body.run_open {
                            body.body_unsupported = true;
                        }
                        ignored_end_paragraph_depth = 1;
                    } else if name == b"bodyPr" {
                        if body.body_properties_seen || body.list_style_open || body.paragraph_open
                        {
                            body.body_unsupported = true;
                        }
                        body.body_properties_seen = true;
                        body.body_properties_open = true;
                        match parse_chart_body_rotation_state(&element, body.role) {
                            Ok(rotation) => body.rotation = rotation,
                            Err(()) => body.body_unsupported = true,
                        }
                    } else if name == b"lstStyle" {
                        if body.list_style_seen || body.body_properties_open || body.paragraph_open
                        {
                            body.body_unsupported = true;
                        }
                        if !chart_text_attributes_are_subset(&element, &[]) {
                            body.body_unsupported = true;
                        }
                        body.list_style_seen = true;
                        body.list_style_open = true;
                    } else if let Some(level) = chart_text_list_level(name) {
                        if !body.list_style_open
                            || body.list_level_context.is_some()
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                        body.list_level_context = Some(level);
                    } else if name == b"p" {
                        if body.body_properties_open
                            || body.list_style_open
                            || body.paragraph_open
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                        body.paragraph_open = true;
                        body.paragraph_seen = true;
                        body.reset_paragraph();
                    } else if name == b"pPr" {
                        if !body.paragraph_open
                            || body.paragraph_properties_open
                            || body.paragraph_content_started
                            || body.run_open
                        {
                            body.paragraph_unsupported = true;
                        }
                        body.paragraph_properties_open = true;
                        apply_chart_text_paragraph_properties(body, &element);
                    } else if name == b"r" || name == b"fld" {
                        if !body.paragraph_open
                            || body.paragraph_properties_open
                            || body.run_open
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                        body.paragraph_content_started = true;
                        body.run_open = true;
                        body.reset_run();
                    } else if name == b"defRPr" || name == b"rPr" {
                        let valid_parent = if name == b"rPr" {
                            body.run_open && !body.current_run_painted && !body.in_text
                        } else {
                            body.paragraph_properties_open || body.list_level_context.is_some()
                        };
                        if valid_parent {
                            let target = body.property_target(name == b"rPr");
                            property =
                                Some((target, chart_text_partial_from_attributes(&element), 1));
                        } else {
                            body.body_unsupported = true;
                        }
                    } else if name == b"t" {
                        if !body.run_open || body.in_text {
                            body.body_unsupported = true;
                        }
                        body.in_text = true;
                    } else if name == b"br" {
                        if !body.paragraph_open || body.paragraph_properties_open || body.run_open {
                            body.body_unsupported = true;
                        }
                        body.paragraph_content_started = true;
                        role_unsupported[body.role.index()] = true;
                    } else if matches!(name, b"noAutofit" | b"normAutofit" | b"spAutoFit") {
                        if !body.body_properties_open || body.autofit_seen {
                            body.body_unsupported = true;
                        }
                        body.autofit_seen = true;
                        match name {
                            b"noAutofit" => {
                                body.body_unsupported |=
                                    !chart_text_attributes_are_subset(&element, &[]);
                            }
                            b"normAutofit" => {
                                body.body_unsupported |=
                                    !chart_text_norm_autofit_is_neutral(&element);
                            }
                            b"spAutoFit" => body.body_unsupported = true,
                            _ => unreachable!("guarded chart text autofit"),
                        }
                    } else {
                        body.body_unsupported = true;
                    }
                }
            }
            Ok(Event::Empty(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if ignored_end_paragraph_depth > 0 {
                    continue;
                }
                if let Some((_, capture, _)) = property.as_mut() {
                    update_chart_text_property_capture(capture, &element, theme, &color_map, true);
                    continue;
                }
                if name == b"txPr" || name == b"rich" {
                    if let Some(role) = context.text_body_role(name == b"rich") {
                        role_unsupported[role.index()] = true;
                    }
                } else if let Some(body) = body.as_mut() {
                    if name == b"endParaRPr" {
                        if !body.paragraph_open || body.run_open {
                            body.body_unsupported = true;
                        }
                    } else if name == b"bodyPr" {
                        if body.body_properties_seen || body.list_style_open || body.paragraph_open
                        {
                            body.body_unsupported = true;
                        }
                        body.body_properties_seen = true;
                        match parse_chart_body_rotation_state(&element, body.role) {
                            Ok(rotation) => body.rotation = rotation,
                            Err(()) => body.body_unsupported = true,
                        }
                    } else if name == b"lstStyle" {
                        if body.list_style_seen
                            || body.body_properties_open
                            || body.paragraph_open
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                        body.list_style_seen = true;
                    } else if chart_text_list_level(name).is_some() {
                        if !body.list_style_open
                            || body.list_level_context.is_some()
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                    } else if name == b"p" {
                        if body.body_properties_open
                            || body.list_style_open
                            || body.paragraph_open
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                        body.paragraph_seen = true;
                        body.reset_paragraph();
                        body.observe_default_candidate();
                        body.reset_paragraph();
                    } else if name == b"pPr" {
                        if !body.paragraph_open
                            || body.paragraph_properties_open
                            || body.paragraph_content_started
                            || body.run_open
                        {
                            body.paragraph_unsupported = true;
                        }
                        apply_chart_text_paragraph_properties(body, &element);
                    } else if name == b"r" || name == b"fld" {
                        if !body.paragraph_open
                            || body.paragraph_properties_open
                            || body.run_open
                            || !chart_text_attributes_are_subset(&element, &[])
                        {
                            body.body_unsupported = true;
                        }
                        body.paragraph_content_started = true;
                    } else if name == b"defRPr" || name == b"rPr" {
                        let valid_parent = if name == b"rPr" {
                            body.run_open && !body.current_run_painted && !body.in_text
                        } else {
                            body.paragraph_properties_open || body.list_level_context.is_some()
                        };
                        if valid_parent {
                            let target = body.property_target(name == b"rPr");
                            body.apply_property(
                                target,
                                chart_text_partial_from_attributes(&element),
                            );
                        } else {
                            body.body_unsupported = true;
                        }
                    } else if name == b"t" {
                        if !body.run_open || !chart_text_attributes_are_subset(&element, &[]) {
                            body.body_unsupported = true;
                        }
                    } else if name == b"br" {
                        if !body.paragraph_open || body.paragraph_properties_open || body.run_open {
                            body.body_unsupported = true;
                        }
                        body.paragraph_content_started = true;
                        role_unsupported[body.role.index()] = true;
                    } else if matches!(name, b"noAutofit" | b"normAutofit" | b"spAutoFit") {
                        if !body.body_properties_open || body.autofit_seen {
                            body.body_unsupported = true;
                        }
                        body.autofit_seen = true;
                        match name {
                            b"noAutofit" => {
                                body.body_unsupported |=
                                    !chart_text_attributes_are_subset(&element, &[]);
                            }
                            b"normAutofit" => {
                                body.body_unsupported |=
                                    !chart_text_norm_autofit_is_neutral(&element);
                            }
                            b"spAutoFit" => body.body_unsupported = true,
                            _ => unreachable!("guarded chart text autofit"),
                        }
                    } else {
                        body.body_unsupported = true;
                    }
                }
            }
            Ok(Event::Text(text)) => {
                if let Some(body) = body.as_mut().filter(|body| body.in_text) {
                    if !text_of(&text).is_empty() {
                        let role = body.role;
                        let fact = body.current_fact();
                        push_chart_text_style_fact(
                            role,
                            fact,
                            &mut facts,
                            &mut role_unsupported,
                            limit_exceeded,
                        );
                    }
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                if let Some(body) = body.as_mut().filter(|body| body.in_text) {
                    with_general_ref_text(&reference, |text| {
                        if !text.is_empty() {
                            let role = body.role;
                            let fact = body.current_fact();
                            push_chart_text_style_fact(
                                role,
                                fact,
                                &mut facts,
                                &mut role_unsupported,
                                limit_exceeded,
                            );
                        }
                    });
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(body) = body.as_mut().filter(|body| body.in_text) {
                    if !text.as_ref().is_empty() {
                        let role = body.role;
                        let fact = body.current_fact();
                        push_chart_text_style_fact(
                            role,
                            fact,
                            &mut facts,
                            &mut role_unsupported,
                            limit_exceeded,
                        );
                    }
                }
            }
            Ok(Event::End(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if ignored_end_paragraph_depth > 0 {
                    ignored_end_paragraph_depth = ignored_end_paragraph_depth.saturating_sub(1);
                    continue;
                }
                if let Some((_, capture, depth)) = property.as_mut() {
                    *depth = depth.saturating_sub(1);
                    if *depth > 0 {
                        finish_chart_text_property_element(capture, name);
                        continue;
                    }
                    let (target, capture, _) = property.take().expect("text property is active");
                    if let Some(body) = body.as_mut() {
                        body.apply_property(target, capture);
                    }
                    continue;
                }
                match name {
                    b"t" => {
                        if let Some(body) = body.as_mut() {
                            if !body.in_text {
                                body.body_unsupported = true;
                            }
                            body.in_text = false;
                        }
                    }
                    b"r" | b"fld" => {
                        if let Some(body) = body.as_mut() {
                            if !body.run_open {
                                body.body_unsupported = true;
                            }
                            if body.run_unsupported && body.current_run_painted {
                                role_unsupported[body.role.index()] = true;
                            }
                            body.run_open = false;
                            body.reset_run();
                        }
                    }
                    b"pPr" => {
                        if let Some(body) = body.as_mut() {
                            if !body.paragraph_properties_open {
                                body.paragraph_unsupported = true;
                            }
                            body.paragraph_properties_open = false;
                        }
                    }
                    b"p" => {
                        if let Some(body) = body.as_mut() {
                            if !body.paragraph_open
                                || body.paragraph_properties_open
                                || body.run_open
                            {
                                body.body_unsupported = true;
                            }
                            if body.current_paragraph_painted
                                && (body.paragraph_unsupported
                                    || body.list_unsupported[body.paragraph_level])
                            {
                                role_unsupported[body.role.index()] = true;
                            }
                            body.observe_default_candidate();
                            body.paragraph_open = false;
                            body.reset_paragraph();
                        }
                    }
                    b"bodyPr" => {
                        if let Some(body) = body.as_mut() {
                            if !body.body_properties_open {
                                body.body_unsupported = true;
                            }
                            body.body_properties_open = false;
                        }
                    }
                    b"lstStyle" => {
                        if let Some(body) = body.as_mut() {
                            if !body.list_style_open || body.list_level_context.is_some() {
                                body.body_unsupported = true;
                            }
                            body.list_style_open = false;
                        }
                    }
                    b"lvl1pPr" | b"lvl2pPr" | b"lvl3pPr" | b"lvl4pPr" | b"lvl5pPr" | b"lvl6pPr"
                    | b"lvl7pPr" | b"lvl8pPr" | b"lvl9pPr" => {
                        if let Some(body) = body.as_mut() {
                            body.list_level_context = None;
                        }
                    }
                    b"txPr" | b"rich" => {
                        if let Some(mut completed) = body.take() {
                            completed.body_unsupported |= completed.body_properties_open
                                || completed.list_style_open
                                || completed.list_level_context.is_some()
                                || completed.paragraph_open
                                || completed.paragraph_properties_open
                                || completed.run_open
                                || completed.in_text
                                || !completed.body_properties_seen
                                || !completed.paragraph_seen;
                            if completed.painted_paragraphs > 1 {
                                role_unsupported[completed.role.index()] = true;
                            }
                            if !completed.rich {
                                if completed.default_candidate.is_none() {
                                    completed.observe_default_candidate();
                                }
                                let observation = &mut defaults[completed.role.index()];
                                if completed.body_unsupported
                                    || completed.default_candidate_mixed
                                    || completed.default_candidate_unsupported
                                {
                                    observation.mark_unsupported();
                                } else {
                                    observation.observe(
                                        completed.default_candidate.unwrap_or_default(),
                                        completed.rotation,
                                    );
                                }
                            } else if completed.body_unsupported && completed.painted_paragraphs > 0
                            {
                                role_unsupported[completed.role.index()] = true;
                            }
                        }
                    }
                    _ => {}
                }
                context.end(name);
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                add_chart_unsupported(
                    unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedTextStyle,
                );
                return ChartTextStyles::default();
            }
            _ => {}
        }
    }
    if body.is_some() || property.is_some() || ignored_end_paragraph_depth > 0 {
        add_chart_unsupported(
            unsupported_reasons,
            ChartUnsupportedReason::UnsupportedTextStyle,
        );
        return ChartTextStyles::default();
    }

    let chart_default = match &defaults[ChartTextSemanticRole::ChartDefault.index()] {
        PartialChartTextStyleObservation::Uniform { style, rotation } => Some((style, *rotation)),
        PartialChartTextStyleObservation::Unseen => None,
        PartialChartTextStyleObservation::Mixed => {
            add_chart_unsupported(unsupported_reasons, ChartUnsupportedReason::MixedTextStyle);
            return ChartTextStyles::default();
        }
        PartialChartTextStyleObservation::Unsupported => {
            add_chart_unsupported(
                unsupported_reasons,
                ChartUnsupportedReason::UnsupportedTextStyle,
            );
            return ChartTextStyles::default();
        }
    };

    let mut resolved = ChartTextStyleObservations::default();
    for role in [
        ChartTextSemanticRole::ChartTitle,
        ChartTextSemanticRole::CategoryAxisTitle,
        ChartTextSemanticRole::ValueAxisTitle,
        ChartTextSemanticRole::Legend,
        ChartTextSemanticRole::CategoryAxisLabels,
        ChartTextSemanticRole::ValueAxisLabels,
        ChartTextSemanticRole::DataLabels,
    ] {
        let role_default = match &defaults[role.index()] {
            PartialChartTextStyleObservation::Uniform { style, rotation } => {
                Some((style, *rotation))
            }
            PartialChartTextStyleObservation::Unseen => None,
            PartialChartTextStyleObservation::Mixed => {
                resolved.get_mut(role).mark_mixed();
                continue;
            }
            PartialChartTextStyleObservation::Unsupported => {
                resolved.get_mut(role).mark_unsupported();
                continue;
            }
        };
        if role_unsupported[role.index()] {
            resolved.get_mut(role).mark_unsupported();
            continue;
        }
        if facts[role.index()].is_empty() {
            if chart_default.is_some() || role_default.is_some() {
                resolved
                    .get_mut(role)
                    .observe(resolve_unified_chart_text_style(
                        role,
                        theme,
                        &color_map,
                        chart_default,
                        role_default,
                        None,
                    ));
            }
            continue;
        }
        for fact in &facts[role.index()] {
            if fact.unsupported {
                resolved.get_mut(role).mark_unsupported();
                break;
            }
            resolved
                .get_mut(role)
                .observe(resolve_unified_chart_text_style(
                    role,
                    theme,
                    &color_map,
                    chart_default,
                    role_default,
                    Some(fact),
                ));
        }
    }
    resolved.finish(unsupported_reasons)
}

#[allow(clippy::too_many_arguments)]
fn append_chart_text(
    current_series: &mut Option<ParsedChartSeries>,
    capture_series_field: Option<ChartSeriesField>,
    capture_cache_value: bool,
    cache_value: &mut String,
    title_target: Option<ChartTitleTarget>,
    in_title_text: bool,
    title_text: &mut String,
    title_text_valid: &mut bool,
    text: &str,
    limit_exceeded: &mut bool,
    cache_value_valid: &mut bool,
) {
    if capture_cache_value {
        let remaining = MAX_XLSX_CHART_CACHE_VALUE_BYTES.saturating_sub(cache_value.len());
        if text.len() <= remaining {
            cache_value.push_str(text);
        } else {
            *limit_exceeded = true;
            *cache_value_valid = false;
        }
    } else if let Some(field) = capture_series_field {
        if let Some(series) = current_series.as_mut() {
            let invalid_bit = match field {
                ChartSeriesField::Name => 1 << 0,
                ChartSeriesField::Categories => 1 << 1,
                ChartSeriesField::Values => 1 << 2,
                ChartSeriesField::BubbleSizes => 1 << 3,
            };
            if series.invalid_text_fields & invalid_bit != 0 {
                return;
            }
            let slot = match field {
                ChartSeriesField::Name => &mut series.name,
                ChartSeriesField::Categories => &mut series.categories,
                ChartSeriesField::Values => &mut series.values,
                ChartSeriesField::BubbleSizes => &mut series.bubble_sizes,
            };
            let current_len = slot.as_ref().map_or(0, String::len);
            if text.len() <= MAX_XLSX_CHART_TEXT_FIELD_BYTES.saturating_sub(current_len) {
                slot.get_or_insert_with(String::new).push_str(text);
            } else {
                *slot = None;
                series.invalid_text_fields |= invalid_bit;
                *limit_exceeded = true;
            }
        }
    } else if title_target.is_some() && in_title_text {
        if *title_text_valid
            && text.len() <= MAX_XLSX_CHART_TEXT_FIELD_BYTES.saturating_sub(title_text.len())
        {
            title_text.push_str(text);
        } else {
            title_text.clear();
            *title_text_valid = false;
            *limit_exceeded = true;
        }
    }
}

fn chart_cache_points_mut(
    cache: &mut ChartSeriesCache,
    field: ChartSeriesField,
) -> &mut Vec<ChartCachedPoint> {
    match field {
        ChartSeriesField::Name => &mut cache.name,
        ChartSeriesField::Categories => &mut cache.categories,
        ChartSeriesField::Values => &mut cache.values,
        ChartSeriesField::BubbleSizes => &mut cache.bubble_sizes,
    }
}

fn chart_kind_element(name: &[u8]) -> Option<ChartKind> {
    match name {
        b"barChart" => Some(ChartKind::Bar),
        b"lineChart" => Some(ChartKind::Line),
        b"pieChart" => Some(ChartKind::Pie),
        b"scatterChart" => Some(ChartKind::Scatter),
        b"areaChart" => Some(ChartKind::Area),
        b"doughnutChart" => Some(ChartKind::Doughnut),
        b"radarChart" => Some(ChartKind::Radar),
        b"bubbleChart" => Some(ChartKind::Bubble),
        _ => None,
    }
}

fn chart_3d_kind_element(name: &[u8]) -> Option<ChartKind> {
    match name {
        b"bar3DChart" => Some(ChartKind::Bar),
        b"line3DChart" => Some(ChartKind::Line),
        b"pie3DChart" => Some(ChartKind::Pie),
        b"area3DChart" => Some(ChartKind::Area),
        _ => None,
    }
}

fn add_chart_unsupported(
    reasons: &mut Vec<ChartUnsupportedReason>,
    reason: ChartUnsupportedReason,
) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

fn add_chart_series_style_loss(style: &mut ChartSeriesStyle, loss: ChartSeriesStyleLossKind) {
    if !style.losses.contains(&loss) {
        style.losses.push(loss);
    }
}

fn add_chart_frame_style_loss(
    losses: &mut Vec<ChartFrameStyleLossKind>,
    loss: ChartFrameStyleLossKind,
) {
    if !losses.contains(&loss) {
        losses.push(loss);
    }
}

fn retain_chart_marker_symbol(style: &mut ChartSeriesStyle, value: Option<&str>) {
    style.marker = match value {
        Some("none") => ChartMarkerSymbol::None,
        Some("circle") => ChartMarkerSymbol::Circle,
        Some("square") => ChartMarkerSymbol::Square,
        Some("diamond") => ChartMarkerSymbol::Diamond,
        Some("triangle") => ChartMarkerSymbol::Triangle,
        Some("auto") | None => ChartMarkerSymbol::Automatic,
        Some(_) => {
            add_chart_series_style_loss(style, ChartSeriesStyleLossKind::UnsupportedMarkerSymbol);
            ChartMarkerSymbol::Automatic
        }
    };
}

fn retain_chart_marker_size(style: &mut ChartSeriesStyle, value: Option<&str>) {
    match value.and_then(|value| value.parse::<u8>().ok()) {
        Some(size @ 2..=72) => style.marker_size = Some(size),
        _ => add_chart_series_style_loss(style, ChartSeriesStyleLossKind::InvalidMarkerSize),
    }
}

fn retain_chart_series_line_width(style: &mut ChartSeriesStyle, value: Option<&str>) {
    let Some(value) = value else {
        // LibreOffice's DrawingML chart import initializes an authored `a:ln`
        // to one point when the optional width is absent.
        style.line_width_emu = Some(12_700);
        return;
    };
    match value.parse::<u32>() {
        Ok(width) if width <= MAX_OOXML_CHART_LINE_WIDTH_EMU => {
            style.line_width_emu = Some(width);
        }
        _ => add_chart_series_style_loss(style, ChartSeriesStyleLossKind::InvalidLineWidth),
    }
}

fn chart_series_line_color(
    name: &[u8],
    element: &quick_xml::events::BytesStart<'_>,
    theme: &ThemeColors,
    color_map: &ChartTextColorMap,
) -> Option<Color> {
    match name {
        b"srgbClr" => chart_text_attributes_are_subset(element, &[b"val"])
            .then(|| unique_attr(element, b"val").ok().flatten())
            .flatten()
            .as_deref()
            .and_then(parse_chart_rgb),
        b"sysClr" => {
            if !chart_text_attributes_are_subset(element, &[b"val", b"lastClr"])
                || !matches!(unique_attr(element, b"val"), Ok(Some(value)) if !value.is_empty())
            {
                return None;
            }
            unique_attr(element, b"lastClr")
                .ok()
                .flatten()
                .as_deref()
                .and_then(parse_chart_rgb)
        }
        b"schemeClr" => chart_text_attributes_are_subset(element, &[b"val"])
            .then(|| unique_attr(element, b"val").ok().flatten())
            .flatten()
            .as_deref()
            .and_then(|value| chart_scheme_color(theme, color_map, value)),
        _ => None,
    }
}

fn observe_chart_kind(
    kind: &mut Option<ChartKind>,
    next: ChartKind,
    reasons: &mut Vec<ChartUnsupportedReason>,
) {
    match *kind {
        Some(previous) if previous != next => {
            add_chart_unsupported(reasons, ChartUnsupportedReason::Combo);
        }
        None => *kind = Some(next),
        _ => {}
    }
}

fn is_external_chart_reference(reference: &str) -> bool {
    let Some(open) = reference.find('[') else {
        return false;
    };
    let Some(close) = reference[open + 1..]
        .find(']')
        .map(|index| index + open + 1)
    else {
        return false;
    };
    reference[close + 1..].contains('!')
}

fn chart_plot_option_supported(
    kind: Option<ChartKind>,
    name: &[u8],
    element: &quick_xml::events::BytesStart<'_>,
) -> Option<bool> {
    if !matches!(
        name,
        b"grouping"
            | b"overlap"
            | b"gapWidth"
            | b"smooth"
            | b"varyColors"
            | b"firstSliceAng"
            | b"holeSize"
            | b"explosion"
            | b"showNegBubbles"
            | b"bubble3D"
            | b"showDLblsOverMax"
            | b"dispBlanksAs"
            | b"plotVisOnly"
            | b"scatterStyle"
            | b"radarStyle"
            | b"gapDepth"
            | b"bubbleScale"
            | b"sizeRepresents"
            | b"secondPieSize"
            | b"splitType"
            | b"splitPos"
            | b"custSplit"
            | b"ofPieType"
            | b"serLines"
            | b"dropLines"
            | b"hiLowLines"
            | b"upDownBars"
            | b"shape"
    ) {
        return None;
    }
    if !chart_text_attributes_are_subset(element, &[b"val"]) {
        return Some(false);
    }
    let value = || unique_attr(element, b"val");
    let numeric = || {
        value()
            .ok()
            .flatten()
            .and_then(|value| value.parse::<i64>().ok())
    };
    let boolean = || parse_chart_boolean_element(element).ok();
    Some(match name {
        b"grouping" => match (kind, value()) {
            (Some(ChartKind::Bar), Ok(Some(value))) => value == "clustered",
            (Some(ChartKind::Line | ChartKind::Area), Ok(Some(value))) => value == "standard",
            _ => false,
        },
        b"overlap" => kind == Some(ChartKind::Bar) && numeric() == Some(0),
        b"gapWidth" => kind == Some(ChartKind::Bar) && numeric() == Some(150),
        b"smooth" => {
            matches!(kind, Some(ChartKind::Line | ChartKind::Scatter)) && boolean() == Some(false)
        }
        b"varyColors" => {
            boolean() == Some(matches!(kind, Some(ChartKind::Pie | ChartKind::Doughnut)))
        }
        b"firstSliceAng" => {
            matches!(kind, Some(ChartKind::Pie | ChartKind::Doughnut)) && numeric() == Some(0)
        }
        b"holeSize" => kind == Some(ChartKind::Doughnut) && numeric() == Some(50),
        b"explosion" => {
            matches!(kind, Some(ChartKind::Pie | ChartKind::Doughnut)) && numeric() == Some(0)
        }
        b"showNegBubbles" | b"bubble3D" => {
            kind == Some(ChartKind::Bubble) && boolean() == Some(false)
        }
        b"showDLblsOverMax" => boolean() == Some(false),
        b"dispBlanksAs" => matches!(value(), Ok(Some(value)) if value == "gap"),
        b"plotVisOnly" => boolean() == Some(true),
        b"scatterStyle" => {
            kind == Some(ChartKind::Scatter)
                && matches!(value(), Ok(Some(value)) if value == "marker")
        }
        b"radarStyle" => {
            kind == Some(ChartKind::Radar)
                && matches!(value(), Ok(Some(value)) if value == "standard")
        }
        b"gapDepth" | b"bubbleScale" | b"sizeRepresents" | b"secondPieSize" | b"splitType"
        | b"splitPos" | b"custSplit" | b"ofPieType" | b"serLines" | b"dropLines"
        | b"hiLowLines" | b"upDownBars" | b"shape" => false,
        _ => return None,
    })
}

fn retain_chart_series_position(
    series: &mut ParsedChartSeries,
    name: &[u8],
    element: &quick_xml::events::BytesStart<'_>,
) -> bool {
    let seen = if name == b"idx" {
        &mut series.source_index_seen
    } else {
        &mut series.source_order_seen
    };
    if *seen {
        return false;
    }
    *seen = true;
    unique_attr(element, b"val")
        .ok()
        .flatten()
        .and_then(|value| value.parse::<usize>().ok())
        == Some(series.source_position)
}

#[derive(Debug, Default)]
struct ChartDataLabelCapture {
    show_value: Option<bool>,
    deleted: Option<bool>,
    show_legend_key: Option<bool>,
    show_category_name: Option<bool>,
    show_series_name: Option<bool>,
    show_percent: Option<bool>,
    show_bubble_size: Option<bool>,
    show_leader_lines: Option<bool>,
    unsupported_formatting: bool,
    unsupported: bool,
}

impl ChartDataLabelCapture {
    fn set_boolean(
        target: &mut Option<bool>,
        element: &quick_xml::events::BytesStart<'_>,
    ) -> std::result::Result<(), ()> {
        if target.is_some() {
            return Err(());
        }
        let value = match unique_attr(element, b"val")? {
            Some(value) => parse_chart_bool_attr(&value).ok_or(())?,
            None => true,
        };
        *target = Some(value);
        Ok(())
    }

    fn observe(&mut self, element: &quick_xml::events::BytesStart<'_>) {
        let qualified_name = element.name();
        let name = local(qualified_name.as_ref());
        let result = match name {
            b"showVal" => Self::set_boolean(&mut self.show_value, element),
            b"delete" => Self::set_boolean(&mut self.deleted, element),
            b"showLegendKey" => Self::set_boolean(&mut self.show_legend_key, element),
            b"showCatName" => Self::set_boolean(&mut self.show_category_name, element),
            b"showSerName" => Self::set_boolean(&mut self.show_series_name, element),
            b"showPercent" => Self::set_boolean(&mut self.show_percent, element),
            b"showBubbleSize" => Self::set_boolean(&mut self.show_bubble_size, element),
            b"showLeaderLines" => Self::set_boolean(&mut self.show_leader_lines, element),
            b"dLblPos" | b"numFmt" | b"separator" | b"tx" | b"leaderLines" | b"spPr"
            | b"layout" => {
                self.unsupported_formatting = true;
                Ok(())
            }
            b"txPr" => Ok(()),
            b"dLbl" | b"extLst" => {
                self.unsupported = true;
                Ok(())
            }
            _ => {
                self.unsupported = true;
                Ok(())
            }
        };
        if result.is_err() {
            self.unsupported = true;
        }
    }

    fn finish(self) -> std::result::Result<bool, ()> {
        let unsupported_content = [
            self.show_legend_key,
            self.show_category_name,
            self.show_series_name,
            self.show_percent,
            self.show_bubble_size,
            self.show_leader_lines,
        ]
        .into_iter()
        .flatten()
        .any(|value| value);
        let deleted = self.deleted.unwrap_or(false);
        let visible = !deleted && self.show_value.unwrap_or(false);
        if self.unsupported
            || (!deleted && unsupported_content)
            || (visible && self.unsupported_formatting)
        {
            Err(())
        } else {
            Ok(visible)
        }
    }
}

pub(super) fn parse_chart_data_labels(xml: &str) -> (bool, bool) {
    fn retain_policy(
        target: Option<usize>,
        policy: std::result::Result<bool, ()>,
        global: &mut Option<bool>,
        per_series: &mut Vec<Option<bool>>,
        unsupported: &mut bool,
    ) {
        let Ok(policy) = policy else {
            *unsupported = true;
            return;
        };
        match target {
            Some(index) => {
                if per_series.len() <= index {
                    per_series.resize(index + 1, None);
                }
                if per_series[index].replace(policy).is_some() {
                    *unsupported = true;
                }
            }
            None => {
                if global.replace(policy).is_some() {
                    *unsupported = true;
                }
            }
        }
    }

    let mut reader = Reader::from_str(xml);
    let mut current_series = None;
    let mut next_series = 0usize;
    let mut capture: Option<(usize, Option<usize>, ChartDataLabelCapture)> = None;
    let mut global = None;
    let mut per_series = Vec::<Option<bool>>::new();
    let mut unsupported = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if let Some((depth, _, policy)) = capture.as_mut() {
                    if *depth == 1 {
                        policy.observe(&element);
                    }
                    *depth = depth.saturating_add(1);
                } else if name == b"dLbls" {
                    capture = Some((1, current_series, ChartDataLabelCapture::default()));
                } else if name == b"ser" {
                    if next_series < MAX_XLSX_CHART_SERIES_PER_WORKBOOK {
                        current_series = Some(next_series);
                        next_series += 1;
                    } else {
                        current_series = None;
                        unsupported = true;
                    }
                }
            }
            Ok(Event::Empty(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if let Some((depth, _, policy)) = capture.as_mut() {
                    if *depth == 1 {
                        policy.observe(&element);
                    }
                } else if name == b"dLbls" {
                    retain_policy(
                        current_series,
                        Ok(false),
                        &mut global,
                        &mut per_series,
                        &mut unsupported,
                    );
                }
            }
            Ok(Event::End(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if let Some((depth, _, _)) = capture.as_mut() {
                    *depth = depth.saturating_sub(1);
                    if *depth == 0 {
                        let (_, target, policy) = capture.take().expect("label capture is active");
                        retain_policy(
                            target,
                            policy.finish(),
                            &mut global,
                            &mut per_series,
                            &mut unsupported,
                        );
                    }
                } else if name == b"ser" {
                    current_series = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                unsupported = true;
                break;
            }
            _ => {}
        }
    }
    if capture.is_some() {
        unsupported = true;
    }

    let series_count = next_series.max(per_series.len());
    let effective = if series_count == 0 {
        vec![global.unwrap_or(false)]
    } else {
        (0..series_count)
            .map(|index| {
                per_series
                    .get(index)
                    .copied()
                    .flatten()
                    .or(global)
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>()
    };
    if effective.iter().any(|value| *value != effective[0]) {
        unsupported = true;
    }
    (
        effective.first().copied().unwrap_or(false) && !unsupported,
        unsupported,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawChartAxisKind {
    Category,
    Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawChartAxisPosition {
    Bottom,
    Left,
}

#[derive(Debug)]
struct RawChartAxis {
    id: Option<u32>,
    cross_axis_id: Option<u32>,
    kind: RawChartAxisKind,
    visible: bool,
    visibility_valid: bool,
    major_gridlines: bool,
    unsupported_presentation: bool,
    scaling_open: bool,
    major_gridlines_open: bool,
    tick_label_position_seen: bool,
    position: Option<RawChartAxisPosition>,
    number_format_seen: bool,
    crosses_seen: bool,
    auto_seen: bool,
    label_alignment_seen: bool,
    label_offset_seen: bool,
    cross_between_seen: bool,
    cross_between_shifted: Option<bool>,
}

#[derive(Debug)]
struct RawChartPlot {
    kind: ChartKind,
    axis_ids: Vec<u32>,
}

#[derive(Debug, Default)]
pub(super) struct ChartAxisSemantics {
    pub(super) axis_roles: Vec<ChartAxisContext>,
    pub(super) category_visible: Option<bool>,
    pub(super) value_visible: Option<bool>,
    pub(super) category_major_gridlines: bool,
    pub(super) value_major_gridlines: bool,
    category_position: Option<RawChartAxisPosition>,
    value_position: Option<RawChartAxisPosition>,
    pub(super) category_axis_shifted: Option<bool>,
    pub(super) invalid_visibility: bool,
    pub(super) unsupported_topology: bool,
    pub(super) unsupported_presentation: bool,
}

pub(super) fn parse_chart_axis_semantics(xml: &str) -> ChartAxisSemantics {
    fn element_u32(element: &quick_xml::events::BytesStart<'_>) -> std::result::Result<u32, ()> {
        unique_attr(element, b"val")?
            .ok_or(())?
            .parse::<u32>()
            .map_err(|_| ())
    }

    fn element_bool(element: &quick_xml::events::BytesStart<'_>) -> std::result::Result<bool, ()> {
        match unique_attr(element, b"val")? {
            Some(value) => parse_chart_bool_attr(&value).ok_or(()),
            None => Ok(true),
        }
    }

    fn observe_axis_presentation(
        axis: &mut RawChartAxis,
        name: &[u8],
        element: &quick_xml::events::BytesStart<'_>,
        start: bool,
    ) {
        let value = || unique_attr(element, b"val");
        match name {
            b"majorGridlines" => {
                axis.major_gridlines = true;
                axis.major_gridlines_open = start;
                axis.unsupported_presentation |= !chart_text_attributes_are_subset(element, &[]);
            }
            b"scaling" => {
                axis.scaling_open = start;
                axis.unsupported_presentation |= !chart_text_attributes_are_subset(element, &[]);
            }
            b"tickLblPos" => {
                if axis.tick_label_position_seen {
                    axis.unsupported_presentation = true;
                }
                axis.tick_label_position_seen = true;
                match value() {
                    Ok(Some(value)) if value == "nextTo" => {}
                    Ok(Some(_)) | Ok(None) | Err(()) => axis.unsupported_presentation = true,
                }
            }
            b"axPos" => {
                if axis.position.is_some() || !chart_text_attributes_are_subset(element, &[b"val"])
                {
                    axis.unsupported_presentation = true;
                }
                let position = match value() {
                    Ok(Some(value)) if value == "b" => Some(RawChartAxisPosition::Bottom),
                    Ok(Some(value)) if value == "l" => Some(RawChartAxisPosition::Left),
                    Ok(Some(_)) | Ok(None) | Err(()) => None,
                };
                if position.is_none() {
                    axis.unsupported_presentation = true;
                } else if axis.position.is_none() {
                    axis.position = position;
                }
            }
            b"numFmt" => {
                if axis.number_format_seen {
                    axis.unsupported_presentation = true;
                }
                axis.number_format_seen = true;
                let format_code = unique_attr(element, b"formatCode");
                let source_linked = unique_attr(element, b"sourceLinked");
                let source_linked_is_default = matches!(source_linked, Ok(None))
                    || matches!(source_linked, Ok(Some(ref value)) if value == "1" || value == "true");
                if !chart_text_attributes_are_subset(element, &[b"formatCode", b"sourceLinked"])
                    || !matches!(format_code, Ok(Some(ref value)) if value == "General")
                    || !source_linked_is_default
                {
                    axis.unsupported_presentation = true;
                }
            }
            b"crosses" => {
                if axis.crosses_seen || !chart_text_attributes_are_subset(element, &[b"val"]) {
                    axis.unsupported_presentation = true;
                }
                axis.crosses_seen = true;
                // `autoZero` is the canonical default emitted by spreadsheet
                // producers. It carries the same retained semantics as an
                // omitted crossing policy; explicit coordinates and the
                // non-default edge policies remain unsupported.
                if !matches!(value(), Ok(Some(ref value)) if value == "autoZero") {
                    axis.unsupported_presentation = true;
                }
            }
            b"auto" => {
                if axis.auto_seen
                    || axis.kind != RawChartAxisKind::Category
                    || !chart_text_attributes_are_subset(element, &[b"val"])
                    || element_bool(element) != Ok(true)
                {
                    axis.unsupported_presentation = true;
                }
                axis.auto_seen = true;
            }
            b"lblAlgn" => {
                if axis.label_alignment_seen
                    || axis.kind != RawChartAxisKind::Category
                    || !chart_text_attributes_are_subset(element, &[b"val"])
                    || !matches!(value(), Ok(Some(ref value)) if value == "ctr")
                {
                    axis.unsupported_presentation = true;
                }
                axis.label_alignment_seen = true;
            }
            b"lblOffset" => {
                if axis.label_offset_seen
                    || axis.kind != RawChartAxisKind::Category
                    || !chart_text_attributes_are_subset(element, &[b"val"])
                    || !matches!(value(), Ok(Some(ref value)) if value == "100")
                {
                    axis.unsupported_presentation = true;
                }
                axis.label_offset_seen = true;
            }
            b"crossBetween" => {
                if axis.cross_between_seen
                    || axis.kind != RawChartAxisKind::Value
                    || !chart_text_attributes_are_subset(element, &[b"val"])
                {
                    axis.unsupported_presentation = true;
                }
                axis.cross_between_seen = true;
                match value() {
                    Ok(Some(value)) if value == "between" => {
                        axis.cross_between_shifted = Some(true);
                    }
                    Ok(Some(value)) if value == "midCat" => {
                        axis.cross_between_shifted = Some(false);
                    }
                    Ok(Some(_)) | Ok(None) | Err(()) => {
                        axis.unsupported_presentation = true;
                    }
                }
            }
            b"majorTickMark" | b"minorTickMark" => match value() {
                Ok(Some(value)) if value == "none" => {}
                Ok(Some(_)) | Ok(None) | Err(()) => axis.unsupported_presentation = true,
            },
            b"axId" | b"delete" | b"crossAx" | b"title" | b"txPr" => {}
            b"minorGridlines" | b"majorUnit" | b"minorUnit" | b"tickLblSkip" | b"tickMarkSkip"
            | b"crossesAt" | b"dispUnits" | b"spPr" | b"extLst" | b"noMultiLvlLbl" => {
                axis.unsupported_presentation = true;
            }
            _ => axis.unsupported_presentation = true,
        }
    }

    fn observe_axis_scaling_child(
        axis: &mut RawChartAxis,
        name: &[u8],
        element: &quick_xml::events::BytesStart<'_>,
    ) {
        match name {
            b"orientation" => match unique_attr(element, b"val") {
                Ok(Some(value)) if value == "minMax" => {}
                Ok(Some(_)) | Ok(None) | Err(()) => axis.unsupported_presentation = true,
            },
            b"logBase" | b"min" | b"max" | b"extLst" => {
                axis.unsupported_presentation = true;
            }
            _ => axis.unsupported_presentation = true,
        }
    }

    let mut reader = Reader::from_str(xml);
    let mut plots = Vec::<RawChartPlot>::new();
    let mut plot: Option<(usize, RawChartPlot)> = None;
    let mut axes = Vec::<RawChartAxis>::new();
    let mut axis: Option<(usize, RawChartAxis, bool)> = None;
    let mut malformed = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if let Some((depth, current, delete_seen)) = axis.as_mut() {
                    let current_depth = *depth;
                    if current_depth == 1 {
                        match name {
                            b"axId" => match element_u32(&element) {
                                Ok(id) if current.id.replace(id).is_none() => {}
                                _ => malformed = true,
                            },
                            b"delete" => {
                                if *delete_seen {
                                    current.visibility_valid = false;
                                } else {
                                    *delete_seen = true;
                                    match element_bool(&element) {
                                        Ok(deleted) => current.visible = !deleted,
                                        Err(()) => current.visibility_valid = false,
                                    }
                                }
                            }
                            b"crossAx" => match element_u32(&element) {
                                Ok(id) if current.cross_axis_id.replace(id).is_none() => {}
                                _ => malformed = true,
                            },
                            _ => observe_axis_presentation(current, name, &element, true),
                        }
                    } else if current.scaling_open && current_depth == 2 {
                        observe_axis_scaling_child(current, name, &element);
                    } else if current.major_gridlines_open && current_depth >= 2 {
                        current.unsupported_presentation = true;
                    }
                    *depth = depth.saturating_add(1);
                } else if matches!(name, b"catAx" | b"dateAx" | b"valAx") {
                    axis = Some((
                        1,
                        RawChartAxis {
                            id: None,
                            cross_axis_id: None,
                            kind: if name == b"valAx" {
                                RawChartAxisKind::Value
                            } else {
                                RawChartAxisKind::Category
                            },
                            visible: true,
                            visibility_valid: true,
                            major_gridlines: false,
                            unsupported_presentation: name == b"dateAx",
                            scaling_open: false,
                            major_gridlines_open: false,
                            tick_label_position_seen: false,
                            position: None,
                            number_format_seen: false,
                            crosses_seen: false,
                            auto_seen: false,
                            label_alignment_seen: false,
                            label_offset_seen: false,
                            cross_between_seen: false,
                            cross_between_shifted: None,
                        },
                        false,
                    ));
                } else if let Some((depth, current)) = plot.as_mut() {
                    let direct_child = *depth == 1;
                    *depth = depth.saturating_add(1);
                    if direct_child && name == b"axId" {
                        match element_u32(&element) {
                            Ok(id) if current.axis_ids.len() < MAX_XLSX_CHART_AXIS_ITEMS => {
                                current.axis_ids.push(id);
                            }
                            Err(()) => malformed = true,
                            Ok(_) => malformed = true,
                        }
                    }
                } else if let Some(kind) =
                    chart_kind_element(name).or_else(|| chart_3d_kind_element(name))
                {
                    plot = Some((
                        1,
                        RawChartPlot {
                            kind,
                            axis_ids: Vec::new(),
                        },
                    ));
                }
            }
            Ok(Event::Empty(element)) => {
                let qualified_name = element.name();
                let name = local(qualified_name.as_ref());
                if let Some((depth, current, delete_seen)) = axis.as_mut() {
                    if *depth == 1 {
                        match name {
                            b"axId" => match element_u32(&element) {
                                Ok(id) if current.id.replace(id).is_none() => {}
                                _ => malformed = true,
                            },
                            b"delete" => {
                                if *delete_seen {
                                    current.visibility_valid = false;
                                } else {
                                    *delete_seen = true;
                                    match element_bool(&element) {
                                        Ok(deleted) => current.visible = !deleted,
                                        Err(()) => current.visibility_valid = false,
                                    }
                                }
                            }
                            b"crossAx" => match element_u32(&element) {
                                Ok(id) if current.cross_axis_id.replace(id).is_none() => {}
                                _ => malformed = true,
                            },
                            _ => observe_axis_presentation(current, name, &element, false),
                        }
                    } else if current.scaling_open && *depth == 2 {
                        observe_axis_scaling_child(current, name, &element);
                    } else if current.major_gridlines_open && *depth >= 2 {
                        current.unsupported_presentation = true;
                    }
                } else if matches!(name, b"catAx" | b"dateAx" | b"valAx") {
                    if axes.len() < MAX_XLSX_CHART_AXIS_ITEMS {
                        axes.push(RawChartAxis {
                            id: None,
                            cross_axis_id: None,
                            kind: if name == b"valAx" {
                                RawChartAxisKind::Value
                            } else {
                                RawChartAxisKind::Category
                            },
                            visible: true,
                            visibility_valid: true,
                            major_gridlines: false,
                            unsupported_presentation: name == b"dateAx",
                            scaling_open: false,
                            major_gridlines_open: false,
                            tick_label_position_seen: false,
                            position: None,
                            number_format_seen: false,
                            crosses_seen: false,
                            auto_seen: false,
                            label_alignment_seen: false,
                            label_offset_seen: false,
                            cross_between_seen: false,
                            cross_between_shifted: None,
                        });
                    }
                    malformed = true;
                } else if let Some((depth, current)) = plot.as_mut() {
                    if *depth == 1 && name == b"axId" {
                        match element_u32(&element) {
                            Ok(id) if current.axis_ids.len() < MAX_XLSX_CHART_AXIS_ITEMS => {
                                current.axis_ids.push(id);
                            }
                            Err(()) => malformed = true,
                            Ok(_) => malformed = true,
                        }
                    }
                } else if let Some(kind) =
                    chart_kind_element(name).or_else(|| chart_3d_kind_element(name))
                {
                    if plots.len() < MAX_XLSX_CHART_AXIS_ITEMS {
                        plots.push(RawChartPlot {
                            kind,
                            axis_ids: Vec::new(),
                        });
                    } else {
                        malformed = true;
                    }
                }
            }
            Ok(Event::End(element)) => {
                if let Some((depth, current, _)) = axis.as_mut() {
                    let qualified_name = element.name();
                    let name = local(qualified_name.as_ref());
                    if *depth == 2 && name == b"scaling" {
                        current.scaling_open = false;
                    }
                    if *depth == 2 && name == b"majorGridlines" {
                        current.major_gridlines_open = false;
                    }
                    *depth = depth.saturating_sub(1);
                    if *depth == 0 {
                        let (_, completed, _) = axis.take().expect("axis capture is active");
                        if axes.len() < MAX_XLSX_CHART_AXIS_ITEMS {
                            axes.push(completed);
                        } else {
                            malformed = true;
                        }
                    }
                } else if let Some((depth, _)) = plot.as_mut() {
                    *depth = depth.saturating_sub(1);
                    if *depth == 0 {
                        let (_, completed) = plot.take().expect("plot capture is active");
                        if plots.len() < MAX_XLSX_CHART_AXIS_ITEMS {
                            plots.push(completed);
                        } else {
                            malformed = true;
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                malformed = true;
                break;
            }
            _ => {}
        }
    }
    if axis.is_some() || plot.is_some() {
        malformed = true;
    }

    let mut semantics = ChartAxisSemantics {
        invalid_visibility: axes.iter().any(|axis| !axis.visibility_valid),
        unsupported_topology: malformed || plots.len() > 1,
        unsupported_presentation: axes.iter().any(|axis| axis.unsupported_presentation),
        ..Default::default()
    };
    if axes.is_empty() {
        if plots.first().is_some_and(|plot| {
            !plot.axis_ids.is_empty()
                || matches!(
                    plot.kind,
                    ChartKind::Bar
                        | ChartKind::Line
                        | ChartKind::Scatter
                        | ChartKind::Area
                        | ChartKind::Radar
                        | ChartKind::Bubble
                )
        }) {
            semantics.unsupported_topology = true;
        }
        return semantics;
    }

    let mut id_to_index = HashMap::<u32, usize>::new();
    for (index, axis) in axes.iter().enumerate() {
        let Some(id) = axis.id else {
            semantics.unsupported_topology = true;
            continue;
        };
        if id_to_index.insert(id, index).is_some() {
            semantics.unsupported_topology = true;
        }
    }

    let plot_kind = plots.first().map(|plot| plot.kind);
    let axis_based_plot = matches!(
        plot_kind,
        Some(
            ChartKind::Bar
                | ChartKind::Line
                | ChartKind::Scatter
                | ChartKind::Area
                | ChartKind::Radar
                | ChartKind::Bubble
        )
    );
    if !axis_based_plot || plots.is_empty() {
        semantics.unsupported_topology = true;
    }
    if let Some(plot) = plots.first() {
        let mut unique_axis_ids = plot.axis_ids.clone();
        unique_axis_ids.sort_unstable();
        unique_axis_ids.dedup();
        if plot.axis_ids.len() != 2 || unique_axis_ids.len() != 2 {
            semantics.unsupported_topology = true;
        }
    }
    let mut roles = vec![None; axes.len()];
    if matches!(plot_kind, Some(ChartKind::Scatter | ChartKind::Bubble)) {
        let Some(plot) = plots.first() else {
            semantics.unsupported_topology = true;
            return semantics;
        };
        if plot.axis_ids.len() != 2 {
            semantics.unsupported_topology = true;
        } else {
            for (role, id) in [ChartAxisContext::Category, ChartAxisContext::Value]
                .into_iter()
                .zip(plot.axis_ids.iter())
            {
                match id_to_index.get(id).copied() {
                    Some(index)
                        if axes[index].kind == RawChartAxisKind::Value
                            && roles[index].replace(role).is_none() => {}
                    _ => semantics.unsupported_topology = true,
                }
            }
        }
    } else {
        for (index, axis) in axes.iter().enumerate() {
            roles[index] = Some(match axis.kind {
                RawChartAxisKind::Category => ChartAxisContext::Category,
                RawChartAxisKind::Value => ChartAxisContext::Value,
            });
        }
        if let Some(plot) = plots.first() {
            if plot.axis_ids.iter().any(|id| !id_to_index.contains_key(id)) {
                semantics.unsupported_topology = true;
            }
        }
    }

    let category_count = roles
        .iter()
        .filter(|role| **role == Some(ChartAxisContext::Category))
        .count();
    let value_count = roles
        .iter()
        .filter(|role| **role == Some(ChartAxisContext::Value))
        .count();
    if category_count != 1 || value_count != 1 || roles.iter().any(Option::is_none) {
        semantics.unsupported_topology = true;
    } else {
        let category_index = roles
            .iter()
            .position(|role| *role == Some(ChartAxisContext::Category))
            .expect("validated category axis role");
        let value_index = roles
            .iter()
            .position(|role| *role == Some(ChartAxisContext::Value))
            .expect("validated value axis role");
        let category_axis = &axes[category_index];
        let value_axis = &axes[value_index];
        if category_axis.cross_axis_id != value_axis.id
            || value_axis.cross_axis_id != category_axis.id
        {
            semantics.unsupported_topology = true;
        }
    }

    semantics.axis_roles = roles
        .into_iter()
        .zip(axes.iter())
        .map(|(role, axis)| {
            let role = role.unwrap_or(match axis.kind {
                RawChartAxisKind::Category => ChartAxisContext::Category,
                RawChartAxisKind::Value => ChartAxisContext::Value,
            });
            match role {
                ChartAxisContext::Category => {
                    semantics.category_visible = Some(axis.visible);
                    semantics.category_major_gridlines |= axis.major_gridlines;
                    semantics.category_position = axis.position;
                }
                ChartAxisContext::Value => {
                    semantics.value_visible = Some(axis.visible);
                    semantics.value_major_gridlines |= axis.major_gridlines;
                    semantics.value_position = axis.position;
                    if axis.cross_between_shifted.is_some() {
                        semantics.category_axis_shifted = axis.cross_between_shifted;
                    }
                }
            }
            role
        })
        .collect();
    if semantics.category_axis_shifted.is_none() {
        semantics.category_axis_shifted = match plot_kind {
            Some(ChartKind::Bar | ChartKind::Line) => Some(true),
            Some(ChartKind::Area | ChartKind::Radar) => Some(false),
            _ => None,
        };
    }
    semantics
}

#[cfg(any(test, feature = "xlsb"))]
#[cfg(test)]
pub(crate) fn parse_chart(
    xml: &str,
    from: (u32, u16),
    to: (u32, u16),
    chart_cache_points_remaining: &mut usize,
    chart_series_remaining: &mut usize,
) -> Option<ParsedChart> {
    parse_chart_with_theme(
        xml,
        from,
        to,
        chart_cache_points_remaining,
        chart_series_remaining,
        &ThemeColors::default(),
    )
}

pub(crate) fn parse_chart_with_theme(
    xml: &str,
    from: (u32, u16),
    to: (u32, u16),
    chart_cache_points_remaining: &mut usize,
    chart_series_remaining: &mut usize,
    theme: &ThemeColors,
) -> Option<ParsedChart> {
    if xml.len() > usize::try_from(MAX_XLSX_CHART_XML_BYTES).unwrap_or(usize::MAX) {
        return None;
    }
    let unsupported_markup = !chart_markup_is_supported(xml);
    let axis_semantics = parse_chart_axis_semantics(xml);
    let (chart_color_map, unsupported_chart_color_map) = parse_chart_text_color_map(xml);
    let mut r = Reader::from_str(xml);
    let mut kind: Option<ChartKind> = None;
    let mut title: Option<String> = None;
    let mut category_axis_title: Option<String> = None;
    let mut value_axis_title: Option<String> = None;
    let mut title_text = String::new();
    let mut title_text_valid = true;
    let mut title_target: Option<ChartTitleTarget> = None;
    let mut in_title_text = false;
    let mut legend = false;
    let (data_labels, unsupported_data_labels) = parse_chart_data_labels(xml);
    let mut series = Vec::new();
    let mut series_caches = Vec::new();
    let mut series_styles = Vec::new();
    let mut current_series: Option<ParsedChartSeries> = None;
    let mut source_series_position = 0usize;
    let mut series_field: Option<ChartSeriesField> = None;
    let mut capture_series_field: Option<ChartSeriesField> = None;
    let mut series_cache_depth = 0usize;
    let mut cache_field: Option<ChartSeriesField> = None;
    let mut cache_point_index: Option<u32> = None;
    let mut cache_value = String::new();
    let mut cache_value_valid = true;
    let mut capture_cache_value = false;
    let mut limit_exceeded = false;
    let mut unsupported_reasons = Vec::new();
    if unsupported_markup {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedMarkup,
        );
    }
    if !theme.source_valid() {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedChartStyle,
        );
    }
    if unsupported_data_labels {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedDataLabels,
        );
    }
    if unsupported_chart_color_map {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedChartStyle,
        );
    }
    if axis_semantics.invalid_visibility {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::InvalidAxisVisibility,
        );
    }
    if axis_semantics.unsupported_topology {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedAxisTopology,
        );
    }
    if axis_semantics.unsupported_presentation {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedAxisPresentation,
        );
    }
    let mut frame_fill = ChartFrameFill::Automatic;
    let mut frame_style_losses = Vec::new();
    let category_major_gridlines = axis_semantics.category_major_gridlines;
    let value_major_gridlines = axis_semantics.value_major_gridlines;
    let mut bar_direction = ChartBarDirection::Column;
    let mut bar_chart_depth = 0usize;
    let mut chart_depth = 0usize;
    let mut axis_context: Option<ChartAxisContext> = None;
    let mut axis_occurrence = 0usize;
    let mut in_legend = false;
    let mut marker_depth = 0usize;
    let mut marker_symbol_seen = false;
    let mut marker_size_seen = false;
    let mut data_point_depth = 0usize;
    let mut data_label_container_depth = 0usize;
    let mut trendline_depth = 0usize;
    let mut error_bars_depth = 0usize;
    let mut series_shape_depth = 0usize;
    let mut series_shape_seen = false;
    let mut series_line_depth = 0usize;
    let mut series_line_seen = false;
    let mut series_line_paint_seen = false;
    let mut series_line_color_seen = false;
    let mut series_line_solid_fill_depth = 0usize;
    let mut frame_shape_depth = 0usize;
    let mut frame_shape_seen = false;
    let mut frame_fill_choice_seen = false;
    let mut frame_solid_fill_color_seen = false;
    let mut frame_line_depth = 0usize;
    let mut frame_solid_fill_depth = 0usize;
    let mut frame_solid_fill_resolved = false;

    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name().as_ref()) {
                b"chart" => chart_depth = chart_depth.saturating_add(1),
                name if chart_kind_element(name).is_some() => {
                    let observed = chart_kind_element(name).expect("guarded chart kind");
                    observe_chart_kind(&mut kind, observed, &mut unsupported_reasons);
                    if observed == ChartKind::Bar {
                        bar_chart_depth = bar_chart_depth.saturating_add(1);
                    }
                }
                name if chart_3d_kind_element(name).is_some() => {
                    observe_chart_kind(
                        &mut kind,
                        chart_3d_kind_element(name).expect("guarded 3-D chart kind"),
                        &mut unsupported_reasons,
                    );
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::ThreeDimensional,
                    );
                }
                b"stockChart" | b"surfaceChart" | b"surface3DChart" | b"ofPieChart" => {
                    let fallback = match local(e.name().as_ref()) {
                        b"stockChart" => ChartKind::Line,
                        b"ofPieChart" => ChartKind::Pie,
                        _ => ChartKind::Area,
                    };
                    observe_chart_kind(&mut kind, fallback, &mut unsupported_reasons);
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedKind,
                    );
                    if local(e.name().as_ref()) == b"surface3DChart" {
                        add_chart_unsupported(
                            &mut unsupported_reasons,
                            ChartUnsupportedReason::ThreeDimensional,
                        );
                    }
                }
                b"view3D" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::ThreeDimensional,
                ),
                b"pivotSource" => {
                    add_chart_unsupported(&mut unsupported_reasons, ChartUnsupportedReason::Pivot)
                }
                b"externalData" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::ExternalData,
                ),
                b"barDir" if bar_chart_depth > 0 => match unique_attr(&e, b"val") {
                    Ok(Some(value)) if value == "bar" => {
                        bar_direction = ChartBarDirection::Horizontal;
                    }
                    Ok(Some(value)) if value == "col" => {
                        bar_direction = ChartBarDirection::Column;
                    }
                    _ => add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    ),
                },
                name if chart_plot_option_supported(kind, name, &e) == Some(false) => {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                    if name == b"bubble3D" {
                        add_chart_unsupported(
                            &mut unsupported_reasons,
                            ChartUnsupportedReason::ThreeDimensional,
                        );
                    }
                }
                b"style"
                    if chart_depth == 0
                        && current_series.is_none()
                        && !matches!(
                            unique_attr(&e, b"val"),
                            Ok(Some(value)) if value == "2"
                        ) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedChartStyle,
                    );
                }
                b"catAx" | b"dateAx" | b"valAx" => {
                    axis_context = axis_semantics
                        .axis_roles
                        .get(axis_occurrence)
                        .copied()
                        .or_else(|| {
                            (local(e.name().as_ref()) != b"valAx")
                                .then_some(ChartAxisContext::Category)
                                .or(Some(ChartAxisContext::Value))
                        });
                    axis_occurrence = axis_occurrence.saturating_add(1);
                }
                b"title" if current_series.is_none() => {
                    let target = match axis_context {
                        Some(ChartAxisContext::Category) if category_axis_title.is_none() => {
                            Some(ChartTitleTarget::CategoryAxis)
                        }
                        Some(ChartAxisContext::Value) if value_axis_title.is_none() => {
                            Some(ChartTitleTarget::ValueAxis)
                        }
                        None if title.is_none() => Some(ChartTitleTarget::Main),
                        _ => None,
                    };
                    if let Some(target) = target {
                        title_target = Some(target);
                        title_text.clear();
                        title_text_valid = true;
                    }
                }
                b"legend" => {
                    legend = true;
                    in_legend = true;
                }
                b"legendPos"
                    if !matches!(
                        unique_attr(&e, b"val"),
                        Ok(Some(value)) if value == "r"
                    ) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedLegend,
                    );
                }
                b"legendEntry" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedLegend,
                ),
                b"overlay" if parse_chart_boolean_element(&e) != Ok(false) => {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedLegend,
                    );
                }
                b"manualLayout" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedPlotSemantics,
                ),
                b"ser" if current_series.is_some() => return None,
                b"ser" => {
                    current_series = Some(ParsedChartSeries {
                        source_position: source_series_position,
                        ..ParsedChartSeries::default()
                    });
                    source_series_position = source_series_position.saturating_add(1);
                    series_field = None;
                    capture_series_field = None;
                    series_cache_depth = 0;
                    marker_depth = 0;
                    marker_symbol_seen = false;
                    marker_size_seen = false;
                    data_point_depth = 0;
                    data_label_container_depth = 0;
                    trendline_depth = 0;
                    error_bars_depth = 0;
                    series_shape_depth = 0;
                    series_shape_seen = false;
                    series_line_depth = 0;
                    series_line_seen = false;
                    series_line_paint_seen = false;
                    series_line_color_seen = false;
                    series_line_solid_fill_depth = 0;
                }
                b"marker" if current_series.is_some() => marker_depth = 1,
                b"dPt" if current_series.is_some() => {
                    data_point_depth = 1;
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"dLbls" | b"dLbl" if current_series.is_some() => {
                    data_label_container_depth = data_label_container_depth.saturating_add(1);
                }
                b"trendline" if current_series.is_some() => {
                    trendline_depth = 1;
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"errBars" if current_series.is_some() => {
                    error_bars_depth = 1;
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"invertIfNegative" | b"pictureOptions" if current_series.is_some() => {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"spPr" if in_legend => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedLegend,
                ),
                b"spPr"
                    if chart_depth == 0 && current_series.is_none() && frame_shape_depth == 0 =>
                {
                    if frame_shape_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_shape_seen = true;
                    frame_shape_depth = 1;
                }
                b"spPr"
                    if current_series.is_some()
                        && (marker_depth > 0
                            || data_point_depth > 0
                            || trendline_depth > 0
                            || error_bars_depth > 0) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"spPr"
                    if current_series.is_some()
                        && marker_depth == 0
                        && data_point_depth == 0
                        && trendline_depth == 0
                        && error_bars_depth == 0
                        && series_shape_depth == 0 =>
                {
                    if series_shape_seen {
                        if let Some(series) = current_series.as_mut() {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                    series_shape_seen = true;
                    series_shape_depth = 1;
                }
                b"spPr" if current_series.is_none() && chart_depth > 0 => {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"ln" if frame_shape_depth > 0 => {
                    frame_line_depth = 1;
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"solidFill" if frame_shape_depth > 0 && frame_line_depth == 0 => {
                    if frame_fill_choice_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_fill_choice_seen = true;
                    frame_solid_fill_depth = 1;
                    frame_solid_fill_resolved = false;
                    frame_solid_fill_color_seen = false;
                }
                b"noFill" if frame_shape_depth > 0 && frame_line_depth == 0 => {
                    if frame_fill_choice_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_fill_choice_seen = true;
                    frame_fill = ChartFrameFill::NoFill;
                }
                b"srgbClr" | b"schemeClr" if frame_solid_fill_depth > 0 => {
                    let qualified_name = e.name();
                    let name = local(qualified_name.as_ref());
                    if frame_solid_fill_color_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_solid_fill_color_seen = true;
                    if let Some(color) = chart_series_line_color(name, &e, theme, &chart_color_map)
                    {
                        frame_fill = ChartFrameFill::Solid(color);
                        frame_solid_fill_resolved = true;
                    } else {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                }
                b"sysClr" if frame_solid_fill_depth > 0 => {
                    if frame_solid_fill_color_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_solid_fill_color_seen = true;
                    if let Some(color) =
                        chart_series_line_color(b"sysClr", &e, theme, &chart_color_map)
                    {
                        frame_fill = ChartFrameFill::Solid(color);
                        frame_solid_fill_resolved = true;
                    } else {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                }
                b"gradFill" | b"pattFill" | b"blipFill" | b"grpFill"
                    if frame_shape_depth > 0 && frame_line_depth == 0 =>
                {
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"alpha" | b"alphaMod" | b"alphaOff" | b"blue" | b"blueMod" | b"blueOff"
                | b"comp" | b"gamma" | b"gray" | b"green" | b"greenMod" | b"greenOff" | b"hue"
                | b"hueMod" | b"hueOff" | b"inv" | b"invGamma" | b"lum" | b"lumMod" | b"lumOff"
                | b"red" | b"redMod" | b"redOff" | b"sat" | b"satMod" | b"satOff" | b"shade"
                | b"tint"
                    if frame_solid_fill_depth > 0 =>
                {
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"ln" if series_shape_depth > 0 => {
                    series_line_depth = 1;
                    if let Some(series) = current_series.as_mut() {
                        if series_line_seen || !chart_text_attributes_are_subset(&e, &[b"w"]) {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_seen = true;
                        series_line_paint_seen = false;
                        series_line_color_seen = false;
                        retain_chart_series_line_width(
                            &mut series.style,
                            attr(&e, b"w").as_deref(),
                        );
                    }
                }
                b"solidFill" if series_line_depth > 0 => {
                    series_line_solid_fill_depth = 1;
                    if let Some(series) = current_series.as_mut() {
                        if series_line_paint_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_paint_seen = true;
                        series_line_color_seen = false;
                        series.style.line_visible = true;
                    }
                }
                b"noFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if series_line_paint_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_paint_seen = true;
                        series.style.line_visible = false;
                        series.style.line_color = None;
                    }
                }
                b"srgbClr" | b"schemeClr" if series_line_solid_fill_depth > 0 => {
                    let qualified_name = e.name();
                    let name = local(qualified_name.as_ref());
                    if let Some(series) = current_series.as_mut() {
                        if series_line_color_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_color_seen = true;
                        if let Some(color) =
                            chart_series_line_color(name, &e, theme, &chart_color_map)
                        {
                            series.style.line_color = Some(color);
                        } else {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"sysClr" if series_line_solid_fill_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if series_line_color_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_color_seen = true;
                        if let Some(color) =
                            chart_series_line_color(b"sysClr", &e, theme, &chart_color_map)
                        {
                            series.style.line_color = Some(color);
                        } else {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"gradFill" | b"pattFill" | b"blipFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"prstDash"
                    if series_line_depth > 0
                        && (!chart_text_attributes_are_subset(&e, &[b"val"])
                            || !matches!(
                                unique_attr(&e, b"val"),
                                Ok(Some(value)) if value == "solid"
                            )) =>
                {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"alpha" | b"alphaMod" | b"alphaOff" | b"blue" | b"blueMod" | b"blueOff"
                | b"comp" | b"gamma" | b"gray" | b"green" | b"greenMod" | b"greenOff" | b"hue"
                | b"hueMod" | b"hueOff" | b"inv" | b"invGamma" | b"lum" | b"lumMod" | b"lumOff"
                | b"red" | b"redMod" | b"redOff" | b"sat" | b"satMod" | b"satOff" | b"shade"
                | b"tint"
                    if series_line_solid_fill_depth > 0 =>
                {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"custDash" | b"round" | b"bevel" | b"miter" | b"headEnd" | b"tailEnd"
                    if series_line_depth > 0 =>
                {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"solidFill" | b"noFill" | b"gradFill" | b"pattFill" | b"blipFill" | b"grpFill"
                    if series_shape_depth > 0 && series_line_depth == 0 =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"symbol" if marker_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if marker_symbol_seen || !chart_text_attributes_are_subset(&e, &[b"val"]) {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedMarkerSymbol,
                            );
                        }
                        marker_symbol_seen = true;
                        let value = unique_attr(&e, b"val").ok().flatten();
                        retain_chart_marker_symbol(&mut series.style, value.as_deref());
                    }
                }
                b"size" if marker_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if marker_size_seen || !chart_text_attributes_are_subset(&e, &[b"val"]) {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::InvalidMarkerSize,
                            );
                        }
                        marker_size_seen = true;
                        let value = unique_attr(&e, b"val").ok().flatten();
                        retain_chart_marker_size(&mut series.style, value.as_deref());
                    }
                }
                b"idx" | b"order"
                    if current_series.is_some()
                        && data_point_depth == 0
                        && data_label_container_depth == 0
                        && marker_depth == 0
                        && trendline_depth == 0
                        && error_bars_depth == 0
                        && series_cache_depth == 0
                        && current_series.as_mut().is_some_and(|series| {
                            !retain_chart_series_position(series, local(e.name().as_ref()), &e)
                        }) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"tx" if current_series.is_some() => series_field = Some(ChartSeriesField::Name),
                b"cat" | b"xVal" if current_series.is_some() => {
                    series_field = Some(ChartSeriesField::Categories);
                }
                b"val" | b"yVal" if current_series.is_some() => {
                    series_field = Some(ChartSeriesField::Values);
                }
                b"bubbleSize" if current_series.is_some() => {
                    series_field = Some(ChartSeriesField::BubbleSizes);
                }
                b"strCache" | b"numCache" | b"strLit" | b"numLit" if current_series.is_some() => {
                    if series_cache_depth == 0 {
                        cache_field = series_field;
                    }
                    series_cache_depth += 1;
                }
                b"multiLvlStrCache" if current_series.is_some() => {
                    // Multi-level categories cannot be represented faithfully by
                    // the flat public Series API. Keep the A1 reference and
                    // deliberately leave this cache unusable.
                    if series_cache_depth == 0 {
                        cache_field = None;
                    }
                    series_cache_depth += 1;
                }
                b"pt" if current_series.is_some() && series_cache_depth > 0 => {
                    cache_point_index = attr(&e, b"idx").and_then(|value| value.parse().ok());
                    cache_value.clear();
                    cache_value_valid = true;
                }
                b"f" if current_series.is_some() => {
                    capture_series_field = series_field;
                }
                b"v" if current_series.is_some()
                    && series_cache_depth > 0
                    && cache_point_index.is_some() =>
                {
                    capture_cache_value = true;
                }
                b"v" if current_series.is_some() && series_cache_depth == 0 => {
                    capture_series_field = series_field;
                }
                b"t" | b"v" if title_target.is_some() => in_title_text = true,
                _ => {}
            },
            Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                name if chart_kind_element(name).is_some() => observe_chart_kind(
                    &mut kind,
                    chart_kind_element(name).expect("guarded chart kind"),
                    &mut unsupported_reasons,
                ),
                name if chart_3d_kind_element(name).is_some() => {
                    observe_chart_kind(
                        &mut kind,
                        chart_3d_kind_element(name).expect("guarded 3-D chart kind"),
                        &mut unsupported_reasons,
                    );
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::ThreeDimensional,
                    );
                }
                b"stockChart" | b"surfaceChart" | b"surface3DChart" | b"ofPieChart" => {
                    let qualified_name = e.name();
                    let name = local(qualified_name.as_ref());
                    let fallback = match name {
                        b"stockChart" => ChartKind::Line,
                        b"ofPieChart" => ChartKind::Pie,
                        _ => ChartKind::Area,
                    };
                    observe_chart_kind(&mut kind, fallback, &mut unsupported_reasons);
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedKind,
                    );
                    if name == b"surface3DChart" {
                        add_chart_unsupported(
                            &mut unsupported_reasons,
                            ChartUnsupportedReason::ThreeDimensional,
                        );
                    }
                }
                b"view3D" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::ThreeDimensional,
                ),
                b"pivotSource" => {
                    add_chart_unsupported(&mut unsupported_reasons, ChartUnsupportedReason::Pivot)
                }
                b"externalData" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::ExternalData,
                ),
                b"barDir" if bar_chart_depth > 0 => match unique_attr(&e, b"val") {
                    Ok(Some(value)) if value == "bar" => {
                        bar_direction = ChartBarDirection::Horizontal;
                    }
                    Ok(Some(value)) if value == "col" => {
                        bar_direction = ChartBarDirection::Column;
                    }
                    _ => add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    ),
                },
                name if chart_plot_option_supported(kind, name, &e) == Some(false) => {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                    if name == b"bubble3D" {
                        add_chart_unsupported(
                            &mut unsupported_reasons,
                            ChartUnsupportedReason::ThreeDimensional,
                        );
                    }
                }
                b"style"
                    if chart_depth == 0
                        && current_series.is_none()
                        && !matches!(
                            unique_attr(&e, b"val"),
                            Ok(Some(value)) if value == "2"
                        ) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedChartStyle,
                    );
                }
                b"legend" => legend = true,
                b"legendPos"
                    if !matches!(
                        unique_attr(&e, b"val"),
                        Ok(Some(value)) if value == "r"
                    ) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedLegend,
                    );
                }
                b"legendEntry" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedLegend,
                ),
                b"overlay" if parse_chart_boolean_element(&e) != Ok(false) => {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedLegend,
                    );
                }
                b"manualLayout" => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedPlotSemantics,
                ),
                b"ser" => {
                    source_series_position = source_series_position.saturating_add(1);
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"dPt" | b"trendline" | b"errBars" | b"invertIfNegative" | b"pictureOptions"
                    if current_series.is_some() =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"spPr"
                    if current_series.is_some()
                        && (marker_depth > 0
                            || data_point_depth > 0
                            || trendline_depth > 0
                            || error_bars_depth > 0) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"spPr" if in_legend => add_chart_unsupported(
                    &mut unsupported_reasons,
                    ChartUnsupportedReason::UnsupportedLegend,
                ),
                b"idx" | b"order"
                    if current_series.is_some()
                        && data_point_depth == 0
                        && data_label_container_depth == 0
                        && marker_depth == 0
                        && trendline_depth == 0
                        && error_bars_depth == 0
                        && series_cache_depth == 0
                        && current_series.as_mut().is_some_and(|series| {
                            !retain_chart_series_position(series, local(e.name().as_ref()), &e)
                        }) =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                b"symbol" if marker_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if marker_symbol_seen || !chart_text_attributes_are_subset(&e, &[b"val"]) {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedMarkerSymbol,
                            );
                        }
                        marker_symbol_seen = true;
                        let value = unique_attr(&e, b"val").ok().flatten();
                        retain_chart_marker_symbol(&mut series.style, value.as_deref());
                    }
                }
                b"size" if marker_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if marker_size_seen || !chart_text_attributes_are_subset(&e, &[b"val"]) {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::InvalidMarkerSize,
                            );
                        }
                        marker_size_seen = true;
                        let value = unique_attr(&e, b"val").ok().flatten();
                        retain_chart_marker_size(&mut series.style, value.as_deref());
                    }
                }
                b"ln" if frame_shape_depth > 0 => {
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"noFill" if frame_shape_depth > 0 && frame_line_depth == 0 => {
                    if frame_fill_choice_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_fill_choice_seen = true;
                    frame_fill = ChartFrameFill::NoFill;
                }
                b"solidFill" if frame_shape_depth > 0 && frame_line_depth == 0 => {
                    frame_fill_choice_seen = true;
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"srgbClr" | b"schemeClr" if frame_solid_fill_depth > 0 => {
                    let qualified_name = e.name();
                    let name = local(qualified_name.as_ref());
                    if frame_solid_fill_color_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_solid_fill_color_seen = true;
                    if let Some(color) = chart_series_line_color(name, &e, theme, &chart_color_map)
                    {
                        frame_fill = ChartFrameFill::Solid(color);
                        frame_solid_fill_resolved = true;
                    } else {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                }
                b"sysClr" if frame_solid_fill_depth > 0 => {
                    if frame_solid_fill_color_seen {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_solid_fill_color_seen = true;
                    if let Some(color) =
                        chart_series_line_color(b"sysClr", &e, theme, &chart_color_map)
                    {
                        frame_fill = ChartFrameFill::Solid(color);
                        frame_solid_fill_resolved = true;
                    } else {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                }
                b"gradFill" | b"pattFill" | b"blipFill" | b"grpFill"
                    if frame_shape_depth > 0 && frame_line_depth == 0 =>
                {
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"alpha" | b"alphaMod" | b"alphaOff" | b"blue" | b"blueMod" | b"blueOff"
                | b"comp" | b"gamma" | b"gray" | b"green" | b"greenMod" | b"greenOff" | b"hue"
                | b"hueMod" | b"hueOff" | b"inv" | b"invGamma" | b"lum" | b"lumMod" | b"lumOff"
                | b"red" | b"redMod" | b"redOff" | b"sat" | b"satMod" | b"satOff" | b"shade"
                | b"tint"
                    if frame_solid_fill_depth > 0 =>
                {
                    add_chart_frame_style_loss(
                        &mut frame_style_losses,
                        ChartFrameStyleLossKind::UnsupportedPaint,
                    );
                }
                b"ln" if series_shape_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if series_line_seen || !chart_text_attributes_are_subset(&e, &[b"w"]) {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_seen = true;
                        retain_chart_series_line_width(
                            &mut series.style,
                            attr(&e, b"w").as_deref(),
                        );
                    }
                }
                b"noFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if series_line_paint_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_paint_seen = true;
                        series.style.line_visible = false;
                        series.style.line_color = None;
                    }
                }
                b"srgbClr" | b"schemeClr" if series_line_solid_fill_depth > 0 => {
                    let qualified_name = e.name();
                    let name = local(qualified_name.as_ref());
                    if let Some(series) = current_series.as_mut() {
                        if series_line_color_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_color_seen = true;
                        if let Some(color) =
                            chart_series_line_color(name, &e, theme, &chart_color_map)
                        {
                            series.style.line_color = Some(color);
                        } else {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"sysClr" if series_line_solid_fill_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        if series_line_color_seen {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                        series_line_color_seen = true;
                        if let Some(color) =
                            chart_series_line_color(b"sysClr", &e, theme, &chart_color_map)
                        {
                            series.style.line_color = Some(color);
                        } else {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                }
                b"gradFill" | b"pattFill" | b"blipFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"prstDash"
                    if series_line_depth > 0
                        && (!chart_text_attributes_are_subset(&e, &[b"val"])
                            || !matches!(
                                unique_attr(&e, b"val"),
                                Ok(Some(value)) if value == "solid"
                            )) =>
                {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"alpha" | b"alphaMod" | b"alphaOff" | b"blue" | b"blueMod" | b"blueOff"
                | b"comp" | b"gamma" | b"gray" | b"green" | b"greenMod" | b"greenOff" | b"hue"
                | b"hueMod" | b"hueOff" | b"inv" | b"invGamma" | b"lum" | b"lumMod" | b"lumOff"
                | b"red" | b"redMod" | b"redOff" | b"sat" | b"satMod" | b"satOff" | b"shade"
                | b"tint"
                    if series_line_solid_fill_depth > 0 =>
                {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"solidFill" if series_line_depth > 0 => {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"custDash" | b"round" | b"bevel" | b"miter" | b"headEnd" | b"tailEnd"
                    if series_line_depth > 0 =>
                {
                    if let Some(series) = current_series.as_mut() {
                        add_chart_series_style_loss(
                            &mut series.style,
                            ChartSeriesStyleLossKind::UnsupportedLinePaint,
                        );
                    }
                }
                b"solidFill" | b"noFill" | b"gradFill" | b"pattFill" | b"blipFill" | b"grpFill"
                    if series_shape_depth > 0 && series_line_depth == 0 =>
                {
                    add_chart_unsupported(
                        &mut unsupported_reasons,
                        ChartUnsupportedReason::UnsupportedPlotSemantics,
                    );
                }
                _ => {}
            },
            Ok(Event::Text(t)) => {
                append_chart_text(
                    &mut current_series,
                    capture_series_field,
                    capture_cache_value,
                    &mut cache_value,
                    title_target,
                    in_title_text,
                    &mut title_text,
                    &mut title_text_valid,
                    &text_of(&t),
                    &mut limit_exceeded,
                    &mut cache_value_valid,
                );
            }
            Ok(Event::GeneralRef(reference)) => {
                with_general_ref_text(&reference, |text| {
                    append_chart_text(
                        &mut current_series,
                        capture_series_field,
                        capture_cache_value,
                        &mut cache_value,
                        title_target,
                        in_title_text,
                        &mut title_text,
                        &mut title_text_valid,
                        text,
                        &mut limit_exceeded,
                        &mut cache_value_valid,
                    );
                });
            }
            Ok(Event::CData(t)) => {
                let text = String::from_utf8_lossy(t.into_inner().as_ref()).into_owned();
                append_chart_text(
                    &mut current_series,
                    capture_series_field,
                    capture_cache_value,
                    &mut cache_value,
                    title_target,
                    in_title_text,
                    &mut title_text,
                    &mut title_text_valid,
                    &text,
                    &mut limit_exceeded,
                    &mut cache_value_valid,
                );
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"chart" if chart_depth > 0 => chart_depth -= 1,
                b"legend" => in_legend = false,
                b"barChart" if bar_chart_depth > 0 => {
                    bar_chart_depth -= 1;
                }
                b"marker" if marker_depth > 0 => marker_depth = 0,
                b"dPt" if data_point_depth > 0 => data_point_depth = 0,
                b"dLbls" | b"dLbl" if data_label_container_depth > 0 => {
                    data_label_container_depth -= 1;
                }
                b"trendline" if trendline_depth > 0 => trendline_depth = 0,
                b"errBars" if error_bars_depth > 0 => error_bars_depth = 0,
                b"solidFill" if frame_solid_fill_depth > 0 => {
                    if !frame_solid_fill_resolved {
                        add_chart_frame_style_loss(
                            &mut frame_style_losses,
                            ChartFrameStyleLossKind::UnsupportedPaint,
                        );
                    }
                    frame_solid_fill_depth = 0;
                    frame_solid_fill_resolved = false;
                    frame_solid_fill_color_seen = false;
                }
                b"ln" if frame_line_depth > 0 => frame_line_depth = 0,
                b"spPr" if frame_shape_depth > 0 => {
                    frame_shape_depth = 0;
                    frame_line_depth = 0;
                    frame_solid_fill_depth = 0;
                    frame_solid_fill_resolved = false;
                    frame_solid_fill_color_seen = false;
                }
                b"solidFill" if series_line_solid_fill_depth > 0 => {
                    if !series_line_color_seen {
                        if let Some(series) = current_series.as_mut() {
                            add_chart_series_style_loss(
                                &mut series.style,
                                ChartSeriesStyleLossKind::UnsupportedLinePaint,
                            );
                        }
                    }
                    series_line_solid_fill_depth = 0;
                    series_line_color_seen = false;
                }
                b"ln" if series_line_depth > 0 => {
                    series_line_depth = 0;
                    series_line_solid_fill_depth = 0;
                    series_line_paint_seen = false;
                    series_line_color_seen = false;
                }
                b"spPr" if series_shape_depth > 0 => {
                    series_shape_depth = 0;
                    series_line_depth = 0;
                    series_line_solid_fill_depth = 0;
                    series_line_paint_seen = false;
                    series_line_color_seen = false;
                }
                b"v" if capture_cache_value => capture_cache_value = false,
                b"t" | b"v" if in_title_text => in_title_text = false,
                b"title" if title_target.is_some() => {
                    let text = title_text.trim();
                    if title_text_valid && !text.is_empty() {
                        match title_target.expect("title target checked above") {
                            ChartTitleTarget::Main => title = Some(text.to_string()),
                            ChartTitleTarget::CategoryAxis => {
                                category_axis_title = Some(text.to_string());
                            }
                            ChartTitleTarget::ValueAxis => {
                                value_axis_title = Some(text.to_string());
                            }
                        }
                    }
                    title_target = None;
                    in_title_text = false;
                    title_text.clear();
                    title_text_valid = true;
                }
                b"catAx" | b"dateAx" | b"valAx" => axis_context = None,
                b"f" | b"v" if capture_series_field.is_some() => capture_series_field = None,
                b"pt" if series_cache_depth > 0 => {
                    if cache_value_valid {
                        if let (Some(field), Some(index), Some(parsed)) =
                            (cache_field, cache_point_index, current_series.as_mut())
                        {
                            if *chart_cache_points_remaining == 0 {
                                limit_exceeded = true;
                            } else {
                                chart_cache_points_mut(&mut parsed.cache, field).push(
                                    ChartCachedPoint {
                                        index,
                                        value: std::mem::take(&mut cache_value),
                                    },
                                );
                                *chart_cache_points_remaining -= 1;
                            }
                        }
                    }
                    cache_point_index = None;
                    cache_value.clear();
                    cache_value_valid = true;
                    capture_cache_value = false;
                }
                b"strCache" | b"numCache" | b"strLit" | b"numLit" | b"multiLvlStrCache"
                    if series_cache_depth > 0 =>
                {
                    series_cache_depth -= 1;
                    if series_cache_depth == 0 {
                        cache_field = None;
                        cache_point_index = None;
                        cache_value.clear();
                        cache_value_valid = true;
                        capture_cache_value = false;
                    }
                }
                b"tx" | b"cat" | b"xVal" | b"val" | b"yVal" | b"bubbleSize"
                    if current_series.is_some() =>
                {
                    series_field = None;
                }
                b"ser" => {
                    if let Some(parsed) = current_series.take() {
                        if !parsed.source_index_seen || !parsed.source_order_seen {
                            add_chart_unsupported(
                                &mut unsupported_reasons,
                                ChartUnsupportedReason::UnsupportedPlotSemantics,
                            );
                        }
                        if [
                            parsed.name.as_deref(),
                            parsed.categories.as_deref(),
                            parsed.values.as_deref(),
                            parsed.bubble_sizes.as_deref(),
                        ]
                        .into_iter()
                        .flatten()
                        .any(is_external_chart_reference)
                        {
                            add_chart_unsupported(
                                &mut unsupported_reasons,
                                ChartUnsupportedReason::ExternalData,
                            );
                        }
                        if let Some(values) = parsed.values {
                            if *chart_series_remaining > 0 {
                                series.push(Series {
                                    name: parsed.name,
                                    categories: parsed.categories,
                                    values,
                                    bubble_sizes: parsed.bubble_sizes,
                                });
                                series_caches.push(parsed.cache);
                                series_styles.push(parsed.style);
                                *chart_series_remaining -= 1;
                            } else {
                                limit_exceeded = true;
                            }
                        }
                    }
                    series_field = None;
                    capture_series_field = None;
                    series_cache_depth = 0;
                    cache_field = None;
                    cache_point_index = None;
                    cache_value.clear();
                    cache_value_valid = true;
                    capture_cache_value = false;
                    marker_depth = 0;
                    marker_symbol_seen = false;
                    marker_size_seen = false;
                    data_point_depth = 0;
                    data_label_container_depth = 0;
                    trendline_depth = 0;
                    error_bars_depth = 0;
                    series_shape_depth = 0;
                    series_shape_seen = false;
                    series_line_depth = 0;
                    series_line_seen = false;
                    series_line_paint_seen = false;
                    series_line_color_seen = false;
                    series_line_solid_fill_depth = 0;
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }

    let kind = kind?;
    let (expected_category_position, expected_value_position) =
        if kind == ChartKind::Bar && bar_direction == ChartBarDirection::Horizontal {
            (RawChartAxisPosition::Left, RawChartAxisPosition::Bottom)
        } else {
            (RawChartAxisPosition::Bottom, RawChartAxisPosition::Left)
        };
    if axis_semantics
        .category_position
        .is_some_and(|position| position != expected_category_position)
        || axis_semantics
            .value_position
            .is_some_and(|position| position != expected_value_position)
    {
        add_chart_unsupported(
            &mut unsupported_reasons,
            ChartUnsupportedReason::UnsupportedAxisPresentation,
        );
    }
    let text_styles = parse_chart_text_styles_unified(
        xml,
        kind,
        &axis_semantics.axis_roles,
        theme,
        &mut unsupported_reasons,
        &mut limit_exceeded,
    );
    let (x_axis_title, y_axis_title) =
        if kind == ChartKind::Bar && bar_direction == ChartBarDirection::Horizontal {
            (value_axis_title, category_axis_title)
        } else {
            (category_axis_title, value_axis_title)
        };

    Some(ParsedChart {
        chart: Chart {
            kind,
            title,
            series,
            legend,
            data_labels,
            x_axis_title,
            y_axis_title,
            from,
            to,
        },
        series_caches,
        series_styles,
        text_styles,
        frame_fill,
        frame_style_losses,
        category_major_gridlines,
        value_major_gridlines,
        category_axis_visible: axis_semantics.category_visible,
        category_axis_shifted: axis_semantics.category_axis_shifted,
        value_axis_visible: axis_semantics.value_visible,
        limit_exceeded,
        unsupported_reasons,
        bar_direction,
    })
}
