mod cells;
mod rules;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use quick_xml::events::Event;
use quick_xml::Reader;

use super::refs::{parse_range, parse_ref, shift_formula, SheetRange};
use super::style::Styles;
use super::theme::{color_attr, ThemeColors};
use super::{
    attr, attr_false, attr_true, local, parse_bool_attr, text_of, with_general_ref_text,
    SharedString, MAX_XLSX_COLUMN_INDEX, MAX_XLSX_ROW_INDEX,
};
use crate::model::{parse_decimal_ratio_u64, CellStyleOverlay, ImportedAxisMeasure};
use crate::{
    CellEntry, CellStyle, Color, CondFormat, ConditionalFormatMetadata, DataValidation,
    FormatScript, HeaderFooterKind, PageSetup, PrintLossKind, PrintMetadata, PrintPageOrder,
    ProtectionOptions, Sparkline, SparklineKind,
};

use cells::build_cell;
pub(super) use cells::retained_cell_cost;
#[cfg(test)]
pub(super) use cells::RETAINED_CELL_RECORD_BYTES;
use rules::{
    parse_conditional_rule, parse_data_validation, push_current_conditional_format,
    push_current_data_validation, PendingCfRule,
};

/// `<sheetData><row><c r t s><f>formula</f><v>…</v>|<is><t>…</t></is></c>` →
/// typed cells, plus the sheet's `<mergeCells>` ranges and the unresolved
/// `<hyperlinks>` as `(row, col, r:id)` (the caller resolves each `r:id` via the
/// worksheet rels).
#[derive(Debug, Default)]
pub(super) struct ParsedSheet {
    pub(super) cells: Vec<CellEntry>,
    pub(super) direct_cell_formats: BTreeMap<(u32, u16), CellStyleOverlay>,
    pub(super) rich: BTreeMap<(u32, u16), Vec<crate::TextRun>>,
    pub(super) merges: Vec<(u32, u16, u32, u16)>,
    pub(super) hyperlink_refs: Vec<(u32, u16, String)>,
    pub(super) freeze: Option<(u32, u16)>,
    pub(super) autofilter: Option<(u32, u16, u32, u16)>,
    pub(super) data_validations: Vec<DataValidation>,
    pub(super) cond_formats: Vec<CondFormat>,
    pub(super) cond_format_metadata: Vec<ConditionalFormatMetadata>,
    pub(super) page_setup: Option<PageSetup>,
    pub(super) print_metadata: PrintMetadata,
    pub(super) sparklines: Vec<Sparkline>,
    pub(super) tab_color: Option<Color>,
    pub(super) print_gridlines: bool,
    pub(super) print_headings: bool,
    pub(super) row_outline: BTreeMap<u32, u8>,
    pub(super) col_outline: BTreeMap<u16, u8>,
    pub(super) col_widths: BTreeMap<u16, f32>,
    pub(super) row_heights: BTreeMap<u32, f32>,
    pub(super) automatic_row_height_candidates: BTreeSet<u32>,
    pub(super) imported_column_axis_measures: BTreeMap<u16, ImportedAxisMeasure>,
    pub(super) imported_row_axis_measures: BTreeMap<u32, ImportedAxisMeasure>,
    pub(super) col_formats: BTreeMap<u16, CellStyle>,
    pub(super) row_formats: BTreeMap<u32, CellStyle>,
    pub(super) hidden_cols: BTreeSet<u16>,
    pub(super) hidden_rows: BTreeSet<u32>,
    pub(super) default_rows_hidden: bool,
    pub(super) explicit_visible_rows: BTreeSet<u32>,
    pub(super) default_row_height: Option<f32>,
    pub(super) automatic_default_row_height_candidate: bool,
    pub(super) default_col_width: Option<f32>,
    pub(super) imported_default_row_axis_measure: Option<ImportedAxisMeasure>,
    pub(super) imported_default_column_axis_measure: Option<ImportedAxisMeasure>,
    pub(super) base_col_width: Option<f32>,
    pub(super) defaulted_base_col_width: bool,
    pub(super) collapsed_rows: BTreeSet<u32>,
    pub(super) outline_summary_below: Option<bool>,
    pub(super) outline_summary_right: Option<bool>,
    pub(super) protect: bool,
    pub(super) protect_options: Option<ProtectionOptions>,
    pub(super) hide_gridlines: bool,
    pub(super) zoom: Option<u16>,
    pub(super) show_headers: Option<bool>,
    pub(super) right_to_left: bool,
    pub(super) tab_selected: bool,
}

fn scaled_ratio_u32(numerator: u64, denominator: u64, scale: u32) -> Option<u32> {
    let scaled = u128::from(numerator).checked_mul(u128::from(scale))?;
    (scaled % u128::from(denominator) == 0)
        .then(|| u32::try_from(scaled / u128::from(denominator)).ok())
        .flatten()
}

fn parse_point_axis_measure(value: &str) -> Option<ImportedAxisMeasure> {
    let (numerator, denominator) = parse_decimal_ratio_u64(value)?;
    if numerator == 0 {
        return None;
    }
    Some(
        scaled_ratio_u32(numerator, denominator, 20)
            .map(ImportedAxisMeasure::Twips)
            .unwrap_or(ImportedAxisMeasure::PointRatio(numerator, denominator)),
    )
}

fn parse_character_axis_measure(value: &str) -> Option<ImportedAxisMeasure> {
    let (numerator, denominator) = parse_decimal_ratio_u64(value)?;
    if numerator == 0 {
        return None;
    }
    Some(ImportedAxisMeasure::CharacterWidthRatio(
        numerator,
        denominator,
    ))
}

#[derive(Clone, Copy, Debug)]
enum HeaderFooterField {
    Header,
    Footer,
}

#[derive(Clone, Copy, Debug)]
struct HeaderFooterCapture {
    kind: HeaderFooterKind,
    legacy: Option<HeaderFooterField>,
}

#[derive(Clone, Copy, Debug)]
enum PageBreakAxis {
    Row,
    Column,
}

pub(super) fn parse_sheet(
    xml: &str,
    shared: &[SharedString],
    styles: &Styles,
    theme: &ThemeColors,
    date1904: bool,
    budget: &mut usize,
) -> ParsedSheet {
    if !crate::xml_reference_work_within_budget(xml) {
        *budget = 0;
        return ParsedSheet::default();
    }
    let mut r = Reader::from_str(xml);
    let mut parsed = ParsedSheet::default();
    // Current cell state.
    let mut rc: Option<(u32, u16)> = None;
    let mut ctype = String::new();
    let mut style_idx = 0usize;
    let mut value = String::new();
    let mut inline_value = String::new();
    let mut inline_text_seen = false;
    let mut inline_run: Option<crate::TextRun> = None;
    let mut inline_runs = Vec::<crate::TextRun>::new();
    let mut formula = String::new();
    // Shared-formula state: si → (master formula text, base row, base col).
    let mut shared_masters: HashMap<u32, (String, u32, u16)> = HashMap::new();
    // Array-formula state: declared rectangular range → anchor formula text.
    let mut array_formulas: Vec<(SheetRange, String)> = Vec::new();
    let mut f_si: Option<u32> = None;
    let mut f_array_ref: Option<SheetRange> = None;
    let mut in_v = false;
    let mut in_f = false;
    let mut in_is_t = false;
    let mut in_rph = false; // East Asian phonetic (ruby) guide — excluded from value
    let mut current_dv: Option<DataValidation> = None;
    let mut current_dv_extra_ranges: Vec<SheetRange> = Vec::new();
    let mut in_dv_formula1 = false;
    let mut in_dv_formula2 = false;
    let mut current_cf_ranges: Vec<SheetRange> = Vec::new();
    let mut current_cf: Option<PendingCfRule> = None;
    let mut in_cf_formula = false;
    let mut header_footer_capture: Option<HeaderFooterCapture> = None;
    let mut page_break_axis: Option<PageBreakAxis> = None;
    let mut current_sparkline_kind = SparklineKind::Line;
    let mut current_sparkline_range = String::new();
    let mut current_sparkline_location = String::new();
    let mut in_sparkline = false;
    let mut in_sparkline_formula = false;
    let mut in_sparkline_sqref = false;
    // Implicit position tracking: the `r` attribute on `<row>`/`<c>` is optional
    // in [ISO/IEC 29500]; when omitted, position is implicit (cells fill
    // left-to-right, rows top-to-bottom). Some writers (LibreOffice, EPPlus, …)
    // omit it. Without this, every `r`-less cell would be dropped.
    let mut cur_row: Option<u32> = Some(0);
    let mut cur_col: Option<u16> = Some(0);
    let mut row_started = false;
    let mut selected_sheet_view_rank = 0u8;
    let mut in_selected_sheet_view = false;
    loop {
        match r.read_event() {
            // A self-closing `<f/>` (a shared-formula follower has no formula text)
            // must NOT open formula-text capture: otherwise pretty-printing
            // whitespace between `<f/>` and `<v>` is captured as the formula and the
            // follower is mis-registered as a master. Capture only formula metadata.
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == b"f" => {
                let formula_kind = attr(&e, b"t");
                f_si = if formula_kind.as_deref() == Some("shared") {
                    attr(&e, b"si").and_then(|s| s.parse::<u32>().ok())
                } else {
                    None
                };
                f_array_ref = if formula_kind.as_deref() == Some("array") {
                    attr(&e, b"ref").as_deref().and_then(parse_range)
                } else {
                    None
                };
                in_f = false;
            }
            Ok(Event::Empty(e))
                if matches!(
                    local(e.name().as_ref()),
                    b"dataValidation" | b"formula1" | b"formula2"
                ) => {}
            Ok(Event::Empty(e)) if header_footer_field(local(e.name().as_ref())).is_some() => {
                if let Some((kind, field, preferred)) =
                    header_footer_field(local(e.name().as_ref()))
                {
                    begin_header_footer_capture(&mut parsed, kind, field, preferred);
                }
            }
            Ok(Event::Empty(e))
                if matches!(local(e.name().as_ref()), b"rowBreaks" | b"colBreaks") =>
            {
                parsed.print_metadata.mark_source();
                page_break_axis = None;
            }
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == b"conditionalFormatting" => {
                push_current_conditional_format(&mut parsed, current_cf.take());
                current_cf_ranges.clear();
            }
            Ok(Event::Empty(e)) if local(e.name().as_ref()) == b"cfRule" => {
                push_current_conditional_format(&mut parsed, current_cf.take());
                let current = parse_conditional_rule(&e, &current_cf_ranges, styles);
                push_current_conditional_format(&mut parsed, current);
            }
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"row" => {
                    cur_row = match attr(&e, b"r") {
                        Some(value) => value
                            .parse::<u32>()
                            .ok()
                            .and_then(|row| row.checked_sub(1))
                            .filter(|row| *row <= MAX_XLSX_ROW_INDEX),
                        None if row_started => cur_row
                            .and_then(|row| row.checked_add(1))
                            .filter(|row| *row <= MAX_XLSX_ROW_INDEX),
                        None => Some(0),
                    };
                    row_started = true;
                    cur_col = Some(0);
                    if let Some(cur_row) = cur_row {
                        if let Some(level) = attr(&e, b"outlineLevel")
                            .and_then(|s| s.parse::<u8>().ok())
                            .filter(|level| (1..=7).contains(level))
                        {
                            parsed.row_outline.insert(cur_row, level);
                        }
                        if attr(&e, b"collapsed").as_deref().is_some_and(attr_true) {
                            parsed.collapsed_rows.insert(cur_row);
                        }
                        if let Some(source_height) = attr(&e, b"ht") {
                            let exact_measure = parse_point_axis_measure(&source_height);
                            if let Ok(height) = source_height.trim().parse::<f32>() {
                                if height.is_finite() && height > 0.0 {
                                    parsed.row_heights.insert(cur_row, height);
                                    if let Some(measure) = exact_measure {
                                        parsed.imported_row_axis_measures.insert(cur_row, measure);
                                    } else {
                                        parsed.imported_row_axis_measures.remove(&cur_row);
                                    }
                                    if attr(&e, b"customHeight").as_deref().is_some_and(attr_true) {
                                        parsed.automatic_row_height_candidates.remove(&cur_row);
                                    } else {
                                        parsed.automatic_row_height_candidates.insert(cur_row);
                                    }
                                }
                            }
                        }
                        if attr(&e, b"hidden").as_deref().is_some_and(attr_true) {
                            parsed.hidden_rows.insert(cur_row);
                            parsed.explicit_visible_rows.remove(&cur_row);
                        } else {
                            parsed.hidden_rows.remove(&cur_row);
                            parsed.explicit_visible_rows.insert(cur_row);
                        }
                        if let Some(style) = attr(&e, b"s")
                            .and_then(|value| value.parse::<usize>().ok())
                            .and_then(|index| styles.cell_style(index))
                        {
                            parsed.row_formats.insert(cur_row, style.clone());
                        }
                    }
                }
                b"sheetFormatPr" => {
                    let default_row_height = attr(&e, b"defaultRowHeight");
                    parsed.default_row_height = default_row_height
                        .as_deref()
                        .and_then(|source| source.trim().parse::<f32>().ok())
                        .filter(|height| height.is_finite() && *height > 0.0);
                    parsed.automatic_default_row_height_candidate =
                        parsed.default_row_height.is_some()
                            && !attr(&e, b"customHeight").as_deref().is_some_and(attr_true);
                    // Preserve the compatibility float independently of exact
                    // source geometry. Valid xsd:double lexical values can
                    // exceed the bounded rational provenance representation.
                    parsed.imported_default_row_axis_measure = parsed
                        .default_row_height
                        .and(default_row_height.as_deref())
                        .and_then(parse_point_axis_measure);
                    let default_col_width = attr(&e, b"defaultColWidth");
                    parsed.default_col_width = default_col_width
                        .as_deref()
                        .and_then(|s| s.trim().parse::<f32>().ok())
                        .filter(|width| width.is_finite() && *width > 0.0);
                    // ECMA-376 defaults baseColWidth to 8 when the element is
                    // present. Keep that import branch separate from the
                    // 8.5-character constructor default used when the entire
                    // element is absent, without changing the compatibility
                    // character-width projection.
                    let base_col_width = attr(&e, b"baseColWidth");
                    parsed.defaulted_base_col_width = base_col_width
                        .as_deref()
                        .is_none_or(|value| value.parse::<i32>().is_err());
                    parsed.base_col_width = match base_col_width {
                        Some(value) => value
                            .parse::<i32>()
                            .ok()
                            .filter(|width| *width > 0)
                            .map(|width| width as f32),
                        None => None,
                    };
                    parsed.imported_default_column_axis_measure =
                        if parsed.default_col_width.is_some() {
                            default_col_width
                                .as_deref()
                                .and_then(parse_character_axis_measure)
                        } else {
                            parsed.base_col_width.and_then(|characters| {
                                let characters = characters as u32;
                                characters
                                    .checked_mul(256)
                                    .map(ImportedAxisMeasure::CharacterBaseWidth256)
                            })
                        };
                    parsed.default_rows_hidden = attr(&e, b"zeroHeight")
                        .as_deref()
                        .and_then(parse_bool_attr)
                        .unwrap_or(false);
                }
                b"outlinePr" => {
                    if let Some(value) = attr(&e, b"summaryBelow")
                        .as_deref()
                        .and_then(parse_bool_attr)
                    {
                        parsed.outline_summary_below = Some(value);
                    }
                    if let Some(value) = attr(&e, b"summaryRight")
                        .as_deref()
                        .and_then(parse_bool_attr)
                    {
                        parsed.outline_summary_right = Some(value);
                    }
                }
                b"pageSetUpPr" => {
                    // ECMA-376 §18.3.1.65 makes this the mode switch; Annex A.2
                    // defaults a missing fitToPage attribute to false.
                    let fit_to_page = print_bool_attr(&e, b"fitToPage", &mut parsed.print_metadata);
                    parsed.print_metadata.set_fit_to_page(fit_to_page);
                }
                b"col" => {
                    let first = attr(&e, b"min")
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(1)
                        .max(1);
                    let last = attr(&e, b"max")
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(first)
                        .min(16_384);
                    if first <= last {
                        let width_source = attr(&e, b"width");
                        let width = width_source
                            .as_deref()
                            .and_then(|s| s.trim().parse::<f32>().ok());
                        let width_measure = width_source
                            .as_deref()
                            .and_then(parse_character_axis_measure);
                        let hidden = attr(&e, b"hidden").as_deref().is_some_and(attr_true);
                        let style = attr(&e, b"style")
                            .and_then(|value| value.parse::<usize>().ok())
                            .and_then(|index| styles.cell_style(index));
                        for col in first..=last {
                            if let Ok(col) = u16::try_from(col - 1) {
                                if let Some(width) = width {
                                    parsed.col_widths.insert(col, width);
                                    match width_measure {
                                        Some(measure) => {
                                            parsed
                                                .imported_column_axis_measures
                                                .insert(col, measure);
                                        }
                                        None => {
                                            parsed.imported_column_axis_measures.remove(&col);
                                        }
                                    }
                                }
                                if hidden {
                                    parsed.hidden_cols.insert(col);
                                }
                                if let Some(style) = style {
                                    parsed.col_formats.insert(col, style.clone());
                                }
                            }
                        }
                    }
                    if let Some(level) = attr(&e, b"outlineLevel")
                        .and_then(|s| s.parse::<u8>().ok())
                        .filter(|level| (1..=7).contains(level))
                    {
                        if first <= last {
                            for col in first..=last {
                                if let Ok(col) = u16::try_from(col - 1) {
                                    parsed.col_outline.insert(col, level);
                                }
                            }
                        }
                    }
                }
                b"sheetProtection" => {
                    if let Some(options) = parse_sheet_protection(&e) {
                        parsed.protect = true;
                        parsed.protect_options = options;
                    }
                }
                b"sheetView" => {
                    if attr(&e, b"tabSelected").as_deref().is_some_and(attr_true) {
                        parsed.tab_selected = true;
                    }
                    let rank = sheet_view_rank(&e);
                    in_selected_sheet_view = rank > selected_sheet_view_rank;
                    if in_selected_sheet_view {
                        selected_sheet_view_rank = rank;
                        clear_sheet_view_metadata(&mut parsed);
                        if attr(&e, b"showGridLines")
                            .as_deref()
                            .is_some_and(attr_false)
                        {
                            parsed.hide_gridlines = true;
                        }
                        if let Some(show_headers) = attr(&e, b"showRowColHeaders")
                            .as_deref()
                            .and_then(parse_bool_attr)
                        {
                            parsed.show_headers = Some(show_headers);
                        }
                        if attr(&e, b"rightToLeft").as_deref().is_some_and(attr_true) {
                            parsed.right_to_left = true;
                        }
                        if let Some(zoom) =
                            attr(&e, b"zoomScale").and_then(|s| s.parse::<u16>().ok())
                        {
                            parsed.zoom = Some(zoom);
                        }
                    }
                }
                b"pane"
                    if in_selected_sheet_view
                        && attr(&e, b"state")
                            .as_deref()
                            .is_some_and(|state| matches!(state, "frozen" | "frozenSplit")) =>
                {
                    let row = attr(&e, b"ySplit")
                        .as_deref()
                        .and_then(parse_split_u32)
                        .unwrap_or(0);
                    let col = attr(&e, b"xSplit")
                        .as_deref()
                        .and_then(parse_split_u16)
                        .unwrap_or(0);
                    if row > 0 || col > 0 {
                        parsed.freeze = Some((row, col));
                    }
                }
                b"tabColor" => {
                    parsed.tab_color = color_attr(&e, theme, &styles.indexed_colors);
                }
                b"c" => {
                    // Use the explicit `r` when present (and resync the implicit
                    // column to it); otherwise fall back to the running position.
                    // A cell reference never repairs an invalid enclosing row:
                    // only a later valid `<row r>` may resynchronize row order.
                    let pos = match (cur_row, attr(&e, b"r")) {
                        (Some(_), Some(reference)) => match parse_ref(&reference) {
                            Some((row, col)) => {
                                cur_col = col
                                    .checked_add(1)
                                    .filter(|column| *column <= MAX_XLSX_COLUMN_INDEX);
                                Some((row, col))
                            }
                            None => {
                                cur_col = None;
                                None
                            }
                        },
                        (Some(row), None) => cur_col.map(|col| {
                            cur_col = col
                                .checked_add(1)
                                .filter(|column| *column <= MAX_XLSX_COLUMN_INDEX);
                            (row, col)
                        }),
                        (None, _) => None,
                    };
                    rc = pos;
                    ctype = attr(&e, b"t").unwrap_or_default();
                    style_idx = attr(&e, b"s").and_then(|s| s.parse().ok()).unwrap_or(0);
                    value.clear();
                    inline_value.clear();
                    inline_text_seen = false;
                    inline_run = None;
                    inline_runs.clear();
                    formula.clear();
                    f_si = None;
                    f_array_ref = None;
                    // Reset text-capture flags so a stray one (e.g. a self-closing
                    // `<f/>` that never fires an End) cannot leak into this cell.
                    (in_v, in_f, in_is_t, in_rph) = (false, false, false, false);
                }
                // A `<v>` is a sibling of `<f>`, never inside it: entering `<v>`
                // clears `in_f` so a self-closing `<f/>` (shared-formula follower,
                // no End event) can't capture the value text as formula text.
                b"v" => (in_v, in_f) = (true, false),
                b"f" if in_sparkline => in_sparkline_formula = true,
                b"f" => {
                    in_f = true; // formula text (sibling of <v>)
                                 // A shared formula carries `t="shared" si="N"`; the master also
                                 // has the formula text + a `ref`, followers are empty `<f/>`.
                    let formula_kind = attr(&e, b"t");
                    f_si = if formula_kind.as_deref() == Some("shared") {
                        attr(&e, b"si").and_then(|s| s.parse::<u32>().ok())
                    } else {
                        None
                    };
                    f_array_ref = if formula_kind.as_deref() == Some("array") {
                        attr(&e, b"ref").as_deref().and_then(parse_range)
                    } else {
                        None
                    };
                }
                b"rPh" => in_rph = true, // phonetic/ruby guide in an inline string
                b"r" if ctype == "inlineStr" && rc.is_some() && !in_rph => {
                    inline_run = Some(crate::TextRun::default());
                }
                b"rFont" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.name = attr(&e, b"val");
                }
                b"sz" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.size_pt = attr(&e, b"val")
                        .and_then(|value| value.parse::<f32>().ok())
                        .map(|value| value.round().clamp(1.0, f32::from(u16::MAX)) as u16);
                }
                b"color" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.color =
                        color_attr(&e, theme, &styles.indexed_colors);
                }
                b"b" if inline_run.is_some() => inline_run.as_mut().expect("run").font.bold = true,
                b"i" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.italic = true;
                }
                b"u" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.underline = true;
                }
                b"strike" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.strikethrough = true;
                }
                b"vertAlign" if inline_run.is_some() => {
                    inline_run.as_mut().expect("run").font.script =
                        match attr(&e, b"val").as_deref() {
                            Some("superscript") => FormatScript::Superscript,
                            Some("subscript") => FormatScript::Subscript,
                            _ => FormatScript::None,
                        };
                }
                b"t" => {
                    in_is_t = true; // inline-string text (within <is>)
                    if !in_rph {
                        inline_text_seen = true;
                    }
                }
                // `<mergeCell ref="A1:C3"/>` — usually self-closing (Empty), but
                // accept Start too.
                b"mergeCell" => {
                    if let Some(rng) = attr(&e, b"ref").as_deref().and_then(parse_range) {
                        parsed.merges.push(rng);
                    }
                }
                b"autoFilter" => {
                    if let Some(rng) = attr(&e, b"ref").as_deref().and_then(parse_range) {
                        parsed.autofilter = Some(rng);
                    }
                }
                b"printOptions" => {
                    let gridlines = print_bool_attr(&e, b"gridLines", &mut parsed.print_metadata);
                    let headings = print_bool_attr(&e, b"headings", &mut parsed.print_metadata);
                    let horizontal =
                        print_bool_attr(&e, b"horizontalCentered", &mut parsed.print_metadata);
                    let vertical =
                        print_bool_attr(&e, b"verticalCentered", &mut parsed.print_metadata);
                    parsed.print_metadata.set_print_gridlines(gridlines);
                    parsed.print_metadata.set_print_headings(headings);
                    parsed.print_metadata.set_center_horizontally(horizontal);
                    parsed.print_metadata.set_center_vertically(vertical);
                    if gridlines {
                        parsed.print_gridlines = true;
                    }
                    if headings {
                        parsed.print_headings = true;
                    }
                    if horizontal {
                        page_setup_mut(&mut parsed).center_horizontally = true;
                    }
                    if vertical {
                        page_setup_mut(&mut parsed).center_vertically = true;
                    }
                }
                b"pageMargins" => {
                    let margins = (
                        attr_f64(&e, b"left"),
                        attr_f64(&e, b"right"),
                        attr_f64(&e, b"top"),
                        attr_f64(&e, b"bottom"),
                        attr_f64(&e, b"header"),
                        attr_f64(&e, b"footer"),
                    );
                    if let (
                        Some(left),
                        Some(right),
                        Some(top),
                        Some(bottom),
                        Some(header),
                        Some(footer),
                    ) = margins
                    {
                        page_setup_mut(&mut parsed).margins =
                            Some((left, right, top, bottom, header, footer));
                    }
                }
                b"pageSetup" => {
                    // A missing pageSetUpPr/fitToPage is percentage mode, even
                    // when stale fit counts remain on pageSetup.
                    if parsed.print_metadata.fit_to_page().is_none() {
                        parsed.print_metadata.set_fit_to_page(false);
                    }
                    match attr(&e, b"pageOrder").as_deref() {
                        Some("overThenDown") => parsed
                            .print_metadata
                            .set_page_order(PrintPageOrder::OverThenDown),
                        Some("downThenOver") | None => parsed
                            .print_metadata
                            .set_page_order(PrintPageOrder::DownThenOver),
                        Some(_) => parsed
                            .print_metadata
                            .add_loss(PrintLossKind::UnsupportedProperty),
                    }
                    let fit_to_width =
                        print_fit_count_attr(&e, b"fitToWidth", &mut parsed.print_metadata);
                    let fit_to_height =
                        print_fit_count_attr(&e, b"fitToHeight", &mut parsed.print_metadata);
                    let ps = page_setup_mut(&mut parsed);
                    ps.landscape = attr(&e, b"orientation")
                        .as_deref()
                        .is_some_and(|orientation| orientation.eq_ignore_ascii_case("landscape"));
                    ps.paper_size = attr_u16(&e, b"paperSize");
                    ps.scale = attr_u16(&e, b"scale");
                    ps.fit_to_width = fit_to_width;
                    ps.fit_to_height = fit_to_height;
                    ps.first_page_number = attr(&e, b"useFirstPageNumber")
                        .as_deref()
                        .is_some_and(attr_true)
                        .then(|| attr_u16(&e, b"firstPageNumber"))
                        .flatten();
                }
                b"headerFooter" => {
                    let different_odd_even = header_footer_bool_attr(
                        &e,
                        b"differentOddEven",
                        false,
                        &mut parsed.print_metadata,
                    );
                    let different_first = header_footer_bool_attr(
                        &e,
                        b"differentFirst",
                        false,
                        &mut parsed.print_metadata,
                    );
                    let scale_with_document = header_footer_bool_attr(
                        &e,
                        b"scaleWithDoc",
                        true,
                        &mut parsed.print_metadata,
                    );
                    let align_with_margins = header_footer_bool_attr(
                        &e,
                        b"alignWithMargins",
                        true,
                        &mut parsed.print_metadata,
                    );
                    parsed.print_metadata.set_header_footer_flag(
                        Some(different_odd_even),
                        Some(different_first),
                        Some(scale_with_document),
                        Some(align_with_margins),
                    );
                }
                b"oddHeader" | b"firstHeader" | b"evenHeader" | b"oddFooter" | b"firstFooter"
                | b"evenFooter" => {
                    if let Some((kind, field, preferred)) =
                        header_footer_field(local(e.name().as_ref()))
                    {
                        header_footer_capture = Some(begin_header_footer_capture(
                            &mut parsed,
                            kind,
                            field,
                            preferred,
                        ));
                    }
                }
                b"rowBreaks" => page_break_axis = Some(PageBreakAxis::Row),
                b"colBreaks" => page_break_axis = Some(PageBreakAxis::Column),
                b"brk" if page_break_axis.is_some() => {
                    parse_manual_page_break(&e, page_break_axis, &mut parsed.print_metadata);
                }
                b"sparklineGroup" => {
                    current_sparkline_kind = attr(&e, b"type")
                        .as_deref()
                        .map(parse_sparkline_kind)
                        .unwrap_or(SparklineKind::Line);
                }
                b"sparkline" => {
                    in_sparkline = true;
                    current_sparkline_range.clear();
                    current_sparkline_location.clear();
                }
                b"dataValidation" => {
                    push_current_data_validation(
                        &mut parsed,
                        current_dv.take(),
                        &mut current_dv_extra_ranges,
                    );
                    if let Some((dv, ranges)) = parse_data_validation(&e) {
                        current_dv = Some(dv);
                        current_dv_extra_ranges = ranges;
                    }
                }
                b"conditionalFormatting" => {
                    push_current_conditional_format(&mut parsed, current_cf.take());
                    current_cf_ranges = attr(&e, b"sqref")
                        .map(|sqref| sqref.split_whitespace().filter_map(parse_range).collect())
                        .unwrap_or_default();
                }
                b"cfRule" => {
                    push_current_conditional_format(&mut parsed, current_cf.take());
                    current_cf = parse_conditional_rule(&e, &current_cf_ranges, styles);
                }
                b"formula" if current_cf.is_some() => {
                    if let Some(cf) = current_cf.as_mut() {
                        cf.formulas.push(String::new());
                    }
                    in_cf_formula = true;
                }
                b"sqref" if in_sparkline => in_sparkline_sqref = true,
                b"color" if current_cf.is_some() => {
                    if let (Some(cf), Some(color)) = (
                        current_cf.as_mut(),
                        color_attr(&e, theme, &styles.indexed_colors),
                    ) {
                        cf.colors.push(color);
                    }
                }
                b"formula1" if current_dv.is_some() => in_dv_formula1 = true,
                b"formula2" if current_dv.is_some() => {
                    if let Some(dv) = current_dv.as_mut() {
                        dv.formula2.get_or_insert_with(String::new);
                    }
                    in_dv_formula2 = true;
                }
                // `<hyperlink ref="A1" r:id="rIdN"/>` — the `ref` may be a single
                // cell or a range; anchor at its top-left. The URL lives in the
                // worksheet rels (`r:id` → local "id"), resolved by the caller.
                b"hyperlink" => {
                    if let (Some((r0, c0, r1, c1)), Some(rid)) = (
                        attr(&e, b"ref").as_deref().and_then(parse_range),
                        attr(&e, b"id"),
                    ) {
                        // A `ref` may be a range (`A1:A3`) — surface every cell, not
                        // just the top-left, bounded so a whole-column ref can't
                        // amplify into millions of entries.
                        let mut n = 0usize;
                        'hl: for row in r0..=r1 {
                            for col in c0..=c1 {
                                if n >= (1 << 16) {
                                    break 'hl;
                                }
                                parsed.hyperlink_refs.push((row, col, rid.clone()));
                                n += 1;
                            }
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Text(t)) if header_footer_capture.is_some() => {
                if let Some(capture) = header_footer_capture {
                    append_header_footer_text(&mut parsed, capture, &text_of(&t));
                }
            }
            Ok(Event::Text(t)) if in_sparkline_formula => {
                current_sparkline_range.push_str(&text_of(&t));
            }
            Ok(Event::Text(t)) if in_sparkline_sqref => {
                current_sparkline_location.push_str(&text_of(&t));
            }
            Ok(Event::Text(t)) if in_cf_formula => {
                if let Some(formula) = current_cf.as_mut().and_then(|cf| cf.formulas.last_mut()) {
                    formula.push_str(&text_of(&t));
                }
            }
            Ok(Event::Text(t)) if in_dv_formula1 => {
                if let Some(dv) = current_dv.as_mut() {
                    dv.formula1.push_str(&text_of(&t));
                }
            }
            Ok(Event::Text(t)) if in_dv_formula2 => {
                if let Some(dv) = current_dv.as_mut() {
                    if let Some(formula2) = dv.formula2.as_mut() {
                        formula2.push_str(&text_of(&t));
                    }
                }
            }
            Ok(Event::Text(t)) if in_f => formula.push_str(&text_of(&t)),
            Ok(Event::Text(t)) if in_v => value.push_str(&text_of(&t)),
            Ok(Event::Text(t)) if in_is_t && !in_rph => {
                let text = text_of(&t);
                inline_value.push_str(&text);
                if let Some(run) = inline_run.as_mut() {
                    run.text.push_str(&text);
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                with_general_ref_text(&reference, |text| {
                    if let Some(capture) = header_footer_capture {
                        append_header_footer_text(&mut parsed, capture, text);
                    } else if in_sparkline_formula {
                        current_sparkline_range.push_str(text);
                    } else if in_sparkline_sqref {
                        current_sparkline_location.push_str(text);
                    } else if in_cf_formula {
                        if let Some(formula) =
                            current_cf.as_mut().and_then(|cf| cf.formulas.last_mut())
                        {
                            formula.push_str(text);
                        }
                    } else if in_dv_formula1 {
                        if let Some(dv) = current_dv.as_mut() {
                            dv.formula1.push_str(text);
                        }
                    } else if in_dv_formula2 {
                        if let Some(formula2) =
                            current_dv.as_mut().and_then(|dv| dv.formula2.as_mut())
                        {
                            formula2.push_str(text);
                        }
                    } else if in_f {
                        formula.push_str(text);
                    } else if in_v {
                        value.push_str(text);
                    } else if in_is_t && !in_rph {
                        inline_value.push_str(text);
                        if let Some(run) = inline_run.as_mut() {
                            run.text.push_str(text);
                        }
                    }
                });
            }
            Ok(Event::CData(t)) if header_footer_capture.is_some() => {
                if let Some(capture) = header_footer_capture {
                    let bytes = t.into_inner();
                    let text = String::from_utf8_lossy(bytes.as_ref());
                    append_header_footer_text(&mut parsed, capture, text.as_ref());
                }
            }
            Ok(Event::CData(t)) if in_sparkline_formula => {
                current_sparkline_range.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::CData(t)) if in_sparkline_sqref => {
                current_sparkline_location
                    .push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::CData(t)) if in_cf_formula => {
                if let Some(formula) = current_cf.as_mut().and_then(|cf| cf.formulas.last_mut()) {
                    formula.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                }
            }
            Ok(Event::CData(t)) if in_dv_formula1 => {
                if let Some(dv) = current_dv.as_mut() {
                    dv.formula1
                        .push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                }
            }
            Ok(Event::CData(t)) if in_dv_formula2 => {
                if let Some(dv) = current_dv.as_mut() {
                    if let Some(formula2) = dv.formula2.as_mut() {
                        formula2.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                    }
                }
            }
            Ok(Event::CData(t)) if in_v => {
                value.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::CData(t)) if in_is_t && !in_rph => {
                let bytes = t.into_inner();
                let text = String::from_utf8_lossy(bytes.as_ref());
                inline_value.push_str(&text);
                if let Some(run) = inline_run.as_mut() {
                    run.text.push_str(&text);
                }
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"v" => in_v = false,
                b"f" if in_sparkline_formula => in_sparkline_formula = false,
                b"f" => in_f = false,
                b"rPh" => in_rph = false,
                b"t" => in_is_t = false,
                b"r" if inline_run.is_some() => {
                    let completed = inline_run.take().expect("run");
                    if !completed.text.is_empty() {
                        inline_runs.push(completed);
                    }
                }
                b"sheetView" => in_selected_sheet_view = false,
                b"oddHeader" | b"firstHeader" | b"evenHeader" | b"oddFooter" | b"firstFooter"
                | b"evenFooter" => header_footer_capture = None,
                b"rowBreaks" | b"colBreaks" => page_break_axis = None,
                b"sqref" if in_sparkline_sqref => in_sparkline_sqref = false,
                b"sparkline" => {
                    in_sparkline = false;
                    if let Some(sparkline) = parse_sparkline(
                        current_sparkline_kind,
                        &current_sparkline_range,
                        &current_sparkline_location,
                    ) {
                        parsed.sparklines.push(sparkline);
                    }
                    current_sparkline_range.clear();
                    current_sparkline_location.clear();
                }
                b"sparklineGroup" => current_sparkline_kind = SparklineKind::Line,
                b"formula" => in_cf_formula = false,
                b"cfRule" => {
                    in_cf_formula = false;
                    push_current_conditional_format(&mut parsed, current_cf.take());
                }
                b"conditionalFormatting" => {
                    push_current_conditional_format(&mut parsed, current_cf.take());
                    current_cf_ranges.clear();
                }
                b"formula1" => in_dv_formula1 = false,
                b"formula2" => in_dv_formula2 = false,
                b"dataValidation" => {
                    in_dv_formula1 = false;
                    in_dv_formula2 = false;
                    push_current_data_validation(
                        &mut parsed,
                        current_dv.take(),
                        &mut current_dv_extra_ranges,
                    );
                }
                b"c" => {
                    if let Some((row, col)) = rc.take() {
                        let cell_value = if ctype == "inlineStr" && inline_text_seen {
                            inline_value.as_str()
                        } else {
                            value.as_str()
                        };
                        // Resolve a shared formula: a master (`si` + formula text)
                        // registers itself; a follower (`si`, empty text) rebuilds the
                        // formula by shifting the master's relative refs to this cell.
                        let mut resolved = match f_si {
                            Some(si) if !formula.is_empty() => {
                                shared_masters.insert(si, (formula.clone(), row, col));
                                formula.clone()
                            }
                            Some(si) => match shared_masters.get(&si) {
                                Some((mf, br, bc)) => shift_formula(
                                    mf,
                                    i64::from(row) - i64::from(*br),
                                    i64::from(col) - i64::from(*bc),
                                ),
                                None => formula.clone(),
                            },
                            None => formula.clone(),
                        };
                        if resolved.is_empty() {
                            if let Some((_, array_formula)) =
                                array_formulas.iter().rev().find(|((r0, c0, r1, c1), _)| {
                                    row >= *r0 && row <= *r1 && col >= *c0 && col <= *c1
                                })
                            {
                                resolved = array_formula.clone();
                            }
                        }
                        if !formula.is_empty() {
                            if let Some(array_ref) = f_array_ref.take() {
                                array_formulas.push((array_ref, resolved.clone()));
                            }
                        } else {
                            f_array_ref = None;
                        }
                        if let Some(entry) = build_cell(
                            row, col, &ctype, style_idx, cell_value, &resolved, shared, styles,
                            date1904,
                        ) {
                            // Account for the complete retained cell rather than
                            // only its rendered text. A custom number format can
                            // deliberately render a real value as an empty string;
                            // that value must remain available through the typed
                            // API without giving empty-display cells a zero-cost
                            // path around the workbook allocation budget.
                            let retained_cost = retained_cell_cost(&entry);
                            if retained_cost > *budget {
                                *budget = 0;
                                break;
                            }
                            *budget -= retained_cost;
                            // Coordinate sidecars follow the retained cell stream's
                            // last-write-wins contract too. Clear an earlier
                            // duplicate before installing the final cell's rich
                            // text or direct-format overlay.
                            parsed.rich.remove(&(row, col));
                            if ctype == "s" {
                                if let Some(runs) = value
                                    .trim()
                                    .parse::<usize>()
                                    .ok()
                                    .and_then(|index| shared.get(index))
                                    .map(|shared| shared.runs.clone())
                                    .filter(|runs| !runs.is_empty())
                                {
                                    parsed.rich.insert((row, col), runs);
                                }
                            } else if ctype == "inlineStr" && !inline_runs.is_empty() {
                                parsed.rich.insert((row, col), inline_runs.clone());
                            }
                            parsed.direct_cell_formats.remove(&(row, col));
                            if style_idx != 0 {
                                if let Some(overlay) = styles.cell_style_overlay(style_idx) {
                                    parsed
                                        .direct_cell_formats
                                        .insert((row, col), overlay.clone());
                                }
                            }
                            parsed.cells.push(entry);
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    push_current_conditional_format(&mut parsed, current_cf.take());
    parsed
}

fn print_bool_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    metadata: &mut PrintMetadata,
) -> bool {
    match attr(e, key).as_deref() {
        Some(value) => parse_bool_attr(value).unwrap_or_else(|| {
            metadata.add_loss(PrintLossKind::UnsupportedProperty);
            false
        }),
        None => false,
    }
}

fn header_footer_bool_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    default: bool,
    metadata: &mut PrintMetadata,
) -> bool {
    match attr(e, key).as_deref() {
        Some(value) => parse_bool_attr(value).unwrap_or_else(|| {
            metadata.add_loss(PrintLossKind::MalformedHeaderFooter);
            default
        }),
        None => default,
    }
}

fn parse_sheet_protection(
    e: &quick_xml::events::BytesStart<'_>,
) -> Option<Option<ProtectionOptions>> {
    if attr(e, b"sheet").as_deref().is_some_and(attr_false) {
        return None;
    }

    let mut options = ProtectionOptions::default();
    for (key, field) in [
        (b"sort".as_slice(), &mut options.sort),
        (b"autoFilter".as_slice(), &mut options.auto_filter),
        (b"formatCells".as_slice(), &mut options.format_cells),
        (b"formatColumns".as_slice(), &mut options.format_columns),
        (b"formatRows".as_slice(), &mut options.format_rows),
        (b"insertColumns".as_slice(), &mut options.insert_columns),
        (b"insertRows".as_slice(), &mut options.insert_rows),
        (
            b"insertHyperlinks".as_slice(),
            &mut options.insert_hyperlinks,
        ),
        (b"deleteColumns".as_slice(), &mut options.delete_columns),
        (b"deleteRows".as_slice(), &mut options.delete_rows),
        (b"pivotTables".as_slice(), &mut options.pivot_tables),
    ] {
        if attr(e, key).as_deref().is_some_and(attr_false) {
            *field = true;
        }
    }

    if options == ProtectionOptions::default() {
        Some(None)
    } else {
        Some(Some(options))
    }
}

fn sheet_view_rank(e: &quick_xml::events::BytesStart<'_>) -> u8 {
    match attr(e, b"workbookViewId").as_deref() {
        Some("0") | None => 2,
        Some(_) => 1,
    }
}

fn clear_sheet_view_metadata(parsed: &mut ParsedSheet) {
    parsed.freeze = None;
    parsed.hide_gridlines = false;
    parsed.zoom = None;
    parsed.show_headers = None;
    parsed.right_to_left = false;
}

fn page_setup_mut(parsed: &mut ParsedSheet) -> &mut PageSetup {
    parsed.page_setup.get_or_insert_with(PageSetup::default)
}

fn header_footer_field(name: &[u8]) -> Option<(HeaderFooterKind, HeaderFooterField, bool)> {
    match name {
        b"oddHeader" => Some((HeaderFooterKind::OddHeader, HeaderFooterField::Header, true)),
        b"firstHeader" => Some((
            HeaderFooterKind::FirstHeader,
            HeaderFooterField::Header,
            false,
        )),
        b"evenHeader" => Some((
            HeaderFooterKind::EvenHeader,
            HeaderFooterField::Header,
            false,
        )),
        b"oddFooter" => Some((HeaderFooterKind::OddFooter, HeaderFooterField::Footer, true)),
        b"firstFooter" => Some((
            HeaderFooterKind::FirstFooter,
            HeaderFooterField::Footer,
            false,
        )),
        b"evenFooter" => Some((
            HeaderFooterKind::EvenFooter,
            HeaderFooterField::Footer,
            false,
        )),
        _ => None,
    }
}

fn begin_header_footer_capture(
    parsed: &mut ParsedSheet,
    kind: HeaderFooterKind,
    field: HeaderFooterField,
    preferred: bool,
) -> HeaderFooterCapture {
    parsed.print_metadata.set_header_footer(kind, String::new());
    let page_setup = page_setup_mut(parsed);
    let slot = match field {
        HeaderFooterField::Header => &mut page_setup.header,
        HeaderFooterField::Footer => &mut page_setup.footer,
    };
    let legacy = if preferred || slot.is_none() {
        *slot = Some(String::new());
        Some(field)
    } else {
        None
    };
    HeaderFooterCapture { kind, legacy }
}

fn append_header_footer_text(parsed: &mut ParsedSheet, capture: HeaderFooterCapture, text: &str) {
    parsed
        .print_metadata
        .append_header_footer(capture.kind, text);
    match capture.legacy {
        Some(HeaderFooterField::Header) => {
            if let Some(header) = page_setup_mut(parsed).header.as_mut() {
                header.push_str(text);
            }
        }
        Some(HeaderFooterField::Footer) => {
            if let Some(footer) = page_setup_mut(parsed).footer.as_mut() {
                footer.push_str(text);
            }
        }
        None => {}
    }
}

fn parse_manual_page_break(
    e: &quick_xml::events::BytesStart<'_>,
    axis: Option<PageBreakAxis>,
    metadata: &mut PrintMetadata,
) {
    let manual = match attr(e, b"man").as_deref() {
        Some(value) => match parse_bool_attr(value) {
            Some(value) => value,
            None => {
                metadata.add_loss(PrintLossKind::InvalidPageBreak);
                return;
            }
        },
        None => false,
    };
    if !manual {
        return;
    }
    let Some(id) = attr(e, b"id").and_then(|value| value.parse::<u32>().ok()) else {
        metadata.add_loss(PrintLossKind::InvalidPageBreak);
        return;
    };
    match axis {
        Some(PageBreakAxis::Row) => metadata.push_manual_row_break(id),
        Some(PageBreakAxis::Column) => match u16::try_from(id) {
            Ok(col) => metadata.push_manual_col_break(col),
            Err(_) => metadata.add_loss(PrintLossKind::InvalidPageBreak),
        },
        None => metadata.add_loss(PrintLossKind::InvalidPageBreak),
    }
}

fn attr_f64(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<f64> {
    attr(e, key).and_then(|s| s.parse::<f64>().ok())
}

fn attr_u16(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<u16> {
    attr(e, key).and_then(|s| s.parse::<u16>().ok())
}

fn print_fit_count_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    metadata: &mut PrintMetadata,
) -> Option<u16> {
    let value = attr(e, key)?;
    match value.parse::<u32>() {
        Ok(value) => match u16::try_from(value) {
            Ok(value) => Some(value),
            Err(_) => {
                // OOXML permits the full unsignedInt range, while the stable
                // public PageSetup model is u16. Saturation preserves a large,
                // effectively unconstrained target instead of turning it into
                // an omitted dimension, whose active-fit default is one page.
                metadata.add_loss(PrintLossKind::LimitExceeded);
                Some(u16::MAX)
            }
        },
        Err(_) => {
            metadata.add_loss(PrintLossKind::UnsupportedProperty);
            None
        }
    }
}

fn parse_sparkline_kind(value: &str) -> SparklineKind {
    match value {
        "column" => SparklineKind::Column,
        "stacked" => SparklineKind::WinLoss,
        _ => SparklineKind::Line,
    }
}

fn parse_sparkline(kind: SparklineKind, range: &str, location: &str) -> Option<Sparkline> {
    let range = range.trim();
    if range.is_empty() {
        return None;
    }
    let (row, col) = location.split_whitespace().next().and_then(parse_ref)?;
    Some(Sparkline {
        location: (row, col),
        range: range.to_string(),
        kind,
    })
}

fn parse_split_u32(value: &str) -> Option<u32> {
    let n = value.parse::<f64>().ok()?;
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > f64::from(u32::MAX) {
        return None;
    }
    Some(n as u32)
}

fn parse_split_u16(value: &str) -> Option<u16> {
    u16::try_from(parse_split_u32(value)?).ok()
}
