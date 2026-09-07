//! Script-aware cell text preparation, shaping, line placement, and glyph outlines.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use rxls::{FormatScript, HAlign, VAlign};

use crate::error::{LimitKind, RenderError};
use crate::font::{
    BaseDirection, FontId, FontOutlineCommand, FontPack, FontPackError, FontRequest, ShapeOptions,
    ShapedText, StyledFontRequest, FONT_OUTLINE_UNITS,
};
use crate::scene::{
    Fixed, GlyphCluster, GlyphClusterMetrics, GlyphPaint, GlyphRunNode, GlyphSemanticGroup,
    LineNode, PathCommand, Rect, Rgb, SceneFontFace, ShapedGlyph, TextAnchor, TextBaseline,
    TextStyle, FIXED_UNITS_PER_PIXEL,
};
use crate::typography::{wrap_text_lines, CellLineLayoutPolicy};
use unicode_bidi::{bidi_class, BidiClass, BidiInfo};
use unicode_script::{Script, UnicodeScript};

use super::geometry::points_to_fixed;
use super::{
    calc_ooxml_row_height_from_points, enforce, rgb, round_signed_ratio, round_unsigned_ratio,
    sum_fixed, CalcAutomaticMetricSource, CalcImportProvenance, CalcLinePlacementPolicy,
    CalcWrapSpace, Region, RenderOptions, TypographyStats, WarningCode, Warnings,
    AUTO_ROW_VERTICAL_PADDING_PIXELS, CALC_CTL_FONT_PROBES, CALC_CTL_LOGICAL_FAMILY,
    CALC_DEVICE_DPI, CALC_OPTIMAL_HEIGHT_SAMPLE_PIXELS, CALC_OPTIMAL_HEIGHT_SAMPLE_TWIPS,
    CALC_WORKSHEET_KERNING, MM100_PER_INCH, TWIPS_PER_CSS_PIXEL,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum CalcScriptClass {
    Western,
    Asian,
    Complex,
}

fn calc_uax_script_class(script: Script) -> Option<CalcScriptClass> {
    match script {
        Script::Common | Script::Inherited | Script::Unknown => None,
        Script::Bopomofo
        | Script::Han
        | Script::Hangul
        | Script::Hiragana
        | Script::Katakana
        | Script::Khitan_Small_Script
        | Script::Tangut
        | Script::Yi => Some(CalcScriptClass::Asian),
        Script::Armenian
        | Script::Braille
        | Script::Canadian_Aboriginal
        | Script::Cherokee
        | Script::Coptic
        | Script::Cypriot
        | Script::Cyrillic
        | Script::Georgian
        | Script::Glagolitic
        | Script::Gothic
        | Script::Greek
        | Script::Latin
        | Script::Ogham
        | Script::Old_Hungarian
        | Script::Old_Italic
        | Script::Osmanya
        | Script::Runic
        | Script::Shavian => Some(CalcScriptClass::Western),
        _ => Some(CalcScriptClass::Complex),
    }
}

// Mirrors LibreOffice `i18nutil::GetScriptClass`: compatibility code-point
// overrides and Unicode block classifications precede the UAX #24 fallback.
pub(super) fn calc_script_class(character: char) -> Option<CalcScriptClass> {
    let codepoint = character as u32;
    if matches!(
        codepoint,
        0x0001
            | 0x0002
            | 0x0020
            | 0x00a0
            | 0x00b2
            | 0x00b3
            | 0x00b9
            | 0x02c7
            | 0x02ca
            | 0x02cb
            | 0x02d9
    ) {
        return None;
    }
    if (0x2c80..=0x2ce3).contains(&codepoint) {
        return Some(CalcScriptClass::Western);
    }
    match codepoint {
        // Basic Latin through Spacing Modifier Letters.
        0x0000..=0x02ff
        // Greek, Cyrillic, and Armenian compatibility blocks.
        | 0x0370..=0x03ff
        | 0x0400..=0x04ff
        | 0x0530..=0x058f
        // Georgian.
        | 0x10a0..=0x10ff
        // Cherokee through Runic.
        | 0x13a0..=0x16ff
        // Latin Extended Additional and Greek Extended.
        | 0x1e00..=0x1fff
        // Latin Extended-C and Latin Extended-D.
        | 0x2c60..=0x2c7f
        | 0xa720..=0xa7ff => Some(CalcScriptClass::Western),

        // Hebrew through Myanmar, retaining the original ICU block set.
        0x0590..=0x05ff
        | 0x0600..=0x06ff
        | 0x0700..=0x074f
        | 0x0780..=0x07bf
        | 0x0900..=0x097f
        | 0x0980..=0x09ff
        | 0x0a00..=0x0a7f
        | 0x0a80..=0x0aff
        | 0x0b00..=0x0b7f
        | 0x0b80..=0x0bff
        | 0x0c00..=0x0c7f
        | 0x0c80..=0x0cff
        | 0x0d00..=0x0d7f
        | 0x0d80..=0x0dff
        | 0x0e00..=0x0e7f
        | 0x0e80..=0x0eff
        | 0x0f00..=0x0fff
        | 0x1000..=0x109f
        // Ethiopic, Khmer, Mongolian, and Arabic presentation forms.
        | 0x1200..=0x137f
        | 0x1780..=0x17ff
        | 0x1800..=0x18af
        | 0xfb50..=0xfdff
        | 0xfe70..=0xfeff => Some(CalcScriptClass::Complex),

        // Hangul Jamo.
        0x1100..=0x11ff
        // CJK Radicals Supplement through Hangul Syllables, retaining the
        // original ICU block set rather than treating every intervening scalar
        // value as Asian.
        | 0x2e80..=0x2eff
        | 0x2f00..=0x2fdf
        | 0x2ff0..=0x2fff
        | 0x3000..=0x303f
        | 0x3040..=0x309f
        | 0x30a0..=0x30ff
        | 0x3100..=0x312f
        | 0x3130..=0x318f
        | 0x3190..=0x319f
        | 0x31a0..=0x31bf
        | 0x3200..=0x32ff
        | 0x3300..=0x33ff
        | 0x3400..=0x4dbf
        | 0x4e00..=0x9fff
        | 0xa000..=0xa48f
        | 0xa490..=0xa4cf
        | 0xac00..=0xd7af
        // Later compatibility blocks named explicitly by Calc.
        | 0xf900..=0xfaff
        | 0xfe30..=0xfe4f
        | 0xff00..=0xffef
        | 0x20000..=0x2a6df
        | 0x2f800..=0x2fa1f
        | 0x31c0..=0x31ef => Some(CalcScriptClass::Asian),

        // Number Forms is explicitly weak in the compatibility table.
        0x2150..=0x218f => None,
        _ => calc_uax_script_class(character.script()),
    }
}

/// Whether `text` mixes Calc script classes inside one cell.
///
/// Calc keeps an internally-uniform cell on its pattern font height; only a
/// cell that itself spans script classes selects a taller face for part of the
/// run and grows the automatic row.
pub(super) fn has_mixed_calc_script_classes(text: &str) -> bool {
    let mut resolved = None;
    for script in text.chars().filter_map(calc_script_class) {
        if resolved.is_some_and(|resolved| resolved != script) {
            return true;
        }
        resolved = Some(script);
    }
    false
}

/// Partition an ambiguous Calc cell the way EditEngine retains script runs.
///
/// Weak characters inherit the preceding strong script, matching Calc's
/// script-change scanner. Leading weak characters inherit the first strong
/// script. A uniform cell stays on the faster DrawStrings path and therefore
/// carries no semantic groups.
pub(super) fn calc_edit_engine_semantic_groups(
    text: &str,
    lines: &[PreparedLine],
) -> Result<Vec<GlyphSemanticGroup>, RenderError> {
    let Some(first_script) = text.chars().find_map(calc_script_class) else {
        return Ok(Vec::new());
    };
    let mut current_script = first_script;
    let mut previous_script = first_script;
    let mut boundaries = BTreeSet::from([0_usize, text.len()]);
    for (byte_start, character) in text.char_indices() {
        let script = calc_script_class(character).unwrap_or(previous_script);
        if script != current_script {
            boundaries.insert(byte_start);
            current_script = script;
        }
        previous_script = script;
    }
    if boundaries.len() == 2 {
        return Ok(Vec::new());
    }
    for line in lines {
        boundaries.insert(line.source.start);
        boundaries.insert(line.source.end);
    }
    boundaries
        .into_iter()
        .collect::<Vec<_>>()
        .windows(2)
        .map(|pair| {
            Ok(GlyphSemanticGroup {
                source_start: u64::try_from(pair[0])
                    .map_err(|_| RenderError::CoordinateOverflow)?,
                source_end: u64::try_from(pair[1]).map_err(|_| RenderError::CoordinateOverflow)?,
            })
        })
        .collect()
}

/// Preserve Calc retention groups and append authoritative visual-line records.
///
/// A fixed-height, non-rotated wrapped ODS cell retains the prepared source
/// prefix as one semantic group. A trailing group completes the source
/// partition without making text beyond Calc's printer cutoff visible.
fn glyph_semantic_groups(
    region: &Region,
    lines: &[PreparedLine],
    block_height: Fixed,
    sheet_right_to_left: bool,
) -> Result<Vec<GlyphSemanticGroup>, RenderError> {
    let alignment = region.style.as_ref().and_then(|style| style.align.as_ref());
    let retains_clipped_ods_prefix = region.ods_fixed_height_row
        && lines.len() > 1
        && block_height > region.rect.height
        && alignment.is_some_and(|alignment| alignment.wrap && alignment.rotation == 0);
    let mixed_script_cell = has_mixed_calc_script_classes(&region.text);
    let non_rotated = alignment.is_none_or(|alignment| alignment.rotation == 0);
    let right_to_left = lines
        .iter()
        .any(|line| line.shaped.base_direction == BaseDirection::RightToLeft);
    let single_visual_line = lines
        .iter()
        .filter(|line| line.source.start < line.source.end)
        .count()
        == 1;
    // On an LTR sheet, Calc emits a non-rotated mixed RTL EditEngine line as
    // one logical text object. Once its RTL portion intersects the cell clip,
    // PDF semantics keep the complete source, including a Western-number run
    // painted beyond the left blocker. A mirrored RTL sheet instead clips that
    // Western run in visual order. Fixed-height mixed LTR cells retain the
    // complete line in either sheet direction.
    let filters_mixed_rtl_groups = mixed_script_cell
        && single_visual_line
        && non_rotated
        && right_to_left
        && sheet_right_to_left;
    let retains_complete_mixed_script_cell = mixed_script_cell
        && single_visual_line
        && non_rotated
        && ((!right_to_left && region.fixed_height_row) || (right_to_left && !sheet_right_to_left));
    let mut records = if retains_clipped_ods_prefix {
        let prefix_end = lines.last().map(|line| line.source.end).unwrap_or_default();
        let text_len = region.text.len();
        let mut groups = vec![GlyphSemanticGroup {
            source_start: 0,
            source_end: u64::try_from(prefix_end).map_err(|_| RenderError::CoordinateOverflow)?,
        }];
        if prefix_end < text_len {
            groups.push(GlyphSemanticGroup {
                source_start: u64::try_from(prefix_end)
                    .map_err(|_| RenderError::CoordinateOverflow)?,
                source_end: u64::try_from(text_len).map_err(|_| RenderError::CoordinateOverflow)?,
            });
        }
        groups
    } else if retains_complete_mixed_script_cell {
        vec![GlyphSemanticGroup {
            source_start: 0,
            source_end: u64::try_from(region.text.len())
                .map_err(|_| RenderError::CoordinateOverflow)?,
        }]
    } else if filters_mixed_rtl_groups {
        let mut rtl_records = Vec::new();
        for group in calc_edit_engine_semantic_groups(&region.text, lines)? {
            let start =
                usize::try_from(group.source_start).map_err(|_| RenderError::CoordinateOverflow)?;
            let end =
                usize::try_from(group.source_end).map_err(|_| RenderError::CoordinateOverflow)?;
            let source = region.text.get(start..end).ok_or(RenderError::Typography {
                reason: "invalid_semantic_group",
            })?;
            if source
                .chars()
                .any(|character| matches!(bidi_class(character), BidiClass::R | BidiClass::AL))
            {
                rtl_records.push(group);
                continue;
            }
            for (offset, character) in source.char_indices() {
                let scalar_start = start
                    .checked_add(offset)
                    .ok_or(RenderError::CoordinateOverflow)?;
                let scalar_end = scalar_start
                    .checked_add(character.len_utf8())
                    .ok_or(RenderError::CoordinateOverflow)?;
                rtl_records.push(GlyphSemanticGroup {
                    source_start: u64::try_from(scalar_start)
                        .map_err(|_| RenderError::CoordinateOverflow)?,
                    source_end: u64::try_from(scalar_end)
                        .map_err(|_| RenderError::CoordinateOverflow)?,
                });
            }
        }
        rtl_records
    } else {
        calc_edit_engine_semantic_groups(&region.text, lines)?
    };
    if !lines.iter().any(|line| line.source.start < line.source.end) {
        return Ok(records);
    }
    let text_len = u64::try_from(region.text.len()).map_err(|_| RenderError::CoordinateOverflow)?;
    records.push(GlyphSemanticGroup {
        source_start: text_len,
        source_end: text_len,
    });
    for line in lines
        .iter()
        .filter(|line| line.source.start < line.source.end)
    {
        records.push(GlyphSemanticGroup {
            source_start: u64::try_from(line.source.start)
                .map_err(|_| RenderError::CoordinateOverflow)?,
            source_end: u64::try_from(line.source.end)
                .map_err(|_| RenderError::CoordinateOverflow)?,
        });
    }
    Ok(records)
}

fn glyph_semantic_groups_with_calc_shortening(
    region: &Region,
    prepared: &PreparedText,
    block_height: Fixed,
    clip_bounds: Rect,
    output_bounds: Option<Rect>,
    style: &TextStyle,
    sheet_right_to_left: bool,
) -> Result<Vec<GlyphSemanticGroup>, RenderError> {
    let mut records =
        glyph_semantic_groups(region, &prepared.lines, block_height, sheet_right_to_left)?;
    let Some(divider_index) = records
        .iter()
        .position(|record| record.source_start == record.source_end)
    else {
        return Ok(records);
    };
    // Script-partitioned cells use Calc's EditEngine semantics. The
    // proportional source shortening below belongs only to DrawStrings.
    if divider_index != 0 || records.len() != 2 || prepared.lines.len() != 1 {
        return Ok(records);
    }
    let line = &prepared.lines[0];
    let alignment = region.style.as_ref().and_then(|style| style.align.as_ref());
    let uses_edit_engine = region.rich_text.is_some()
        || alignment.is_some_and(|alignment| {
            alignment.wrap || alignment.shrink_to_fit || alignment.rotation != 0
        })
        || region.text.chars().any(calc_draw_strings_edit_character);
    if uses_edit_engine
        || region.numeric_default
        // Calc retains the full source on RTL sheets because its logical clip
        // lengths are mirrored before the visual DrawStrings shortening test.
        || sheet_right_to_left
        || style.rotation_degrees.rem_euclid(360) != 0
        || !matches!(style.anchor, TextAnchor::Start | TextAnchor::End)
    {
        return Ok(records);
    }
    let Some(output_bounds) = output_bounds else {
        // Chart, heading, header, and footer text does not use Calc's
        // worksheet-cell DrawStrings path.
        return Ok(records);
    };
    let reaches_unbounded_output_edge = match style.anchor {
        TextAnchor::Start => {
            clip_bounds.x.checked_add(clip_bounds.width)
                == output_bounds.x.checked_add(output_bounds.width)
        }
        TextAnchor::End => clip_bounds.x == output_bounds.x,
        TextAnchor::Middle => false,
    };
    if reaches_unbounded_output_edge {
        return Ok(records);
    }

    let visible_width = calc_draw_strings_visible_width(
        clip_bounds.width,
        region.vertical_margin.max(Fixed::ZERO),
        prepared.horizontal_padding,
    )?;
    if visible_width <= Fixed::ZERO || line.width <= visible_width || line.width <= Fixed::ZERO {
        return Ok(records);
    }
    let source = region
        .text
        .get(line.source.clone())
        .ok_or(RenderError::Typography {
            reason: "invalid_semantic_line",
        })?;
    let utf16_len = source.encode_utf16().count();
    if utf16_len == 0 {
        return Ok(records);
    }
    // ScOutputData::DrawStrings shortens clipped text by this proportional
    // UTF-16 formula before recording the printer metafile/PDF text array.
    let short_utf16 = calc_draw_strings_short_utf16_len(visible_width, line.width, utf16_len)?;
    if short_utf16 >= utf16_len {
        return Ok(records);
    }

    let record = records.get_mut(1).ok_or(RenderError::Typography {
        reason: "missing_semantic_line",
    })?;
    match style.anchor {
        TextAnchor::Start => {
            let retained = utf16_prefix_byte_len(source, short_utf16);
            record.source_end = u64::try_from(
                line.source
                    .start
                    .checked_add(retained)
                    .ok_or(RenderError::CoordinateOverflow)?,
            )
            .map_err(|_| RenderError::CoordinateOverflow)?;
        }
        TextAnchor::End => {
            let retained = utf16_suffix_byte_len(source, short_utf16);
            record.source_start = u64::try_from(
                line.source
                    .end
                    .checked_sub(retained)
                    .ok_or(RenderError::CoordinateOverflow)?,
            )
            .map_err(|_| RenderError::CoordinateOverflow)?;
        }
        TextAnchor::Middle => unreachable!("centered text was rejected above"),
    }
    Ok(records)
}

pub(super) fn calc_draw_strings_visible_width(
    clip_width: Fixed,
    base_margin: Fixed,
    aligned_margin: Fixed,
) -> Result<Fixed, RenderError> {
    // Calc adds indent to the aligned edge only. `aligned_margin` contains the
    // base ATTR_MARGIN plus indent, while the opposite edge keeps base_margin.
    clip_width
        .checked_sub(base_margin)
        .and_then(|width| width.checked_sub(aligned_margin))
        .map(|width| width.max(Fixed::from_raw(1)))
        .and_then(|width| width.checked_sub(Fixed::from_pixels(1)))
        .ok_or(RenderError::CoordinateOverflow)
}

pub(super) fn calc_draw_strings_short_utf16_len(
    visible_width: Fixed,
    text_width: Fixed,
    utf16_len: usize,
) -> Result<usize, RenderError> {
    let visible =
        u128::try_from(visible_width.raw()).map_err(|_| RenderError::CoordinateOverflow)?;
    let width = u128::try_from(text_width.raw()).map_err(|_| RenderError::CoordinateOverflow)?;
    visible
        .checked_mul(utf16_len as u128)
        .and_then(|value| value.checked_div(width))
        .and_then(|value| value.checked_add(1))
        .and_then(|value| usize::try_from(value).ok())
        .map(|value| value.min(utf16_len))
        .ok_or(RenderError::CoordinateOverflow)
}

pub(super) fn expand_draw_strings_cutoff_to_cluster_boundary(
    record: &mut GlyphSemanticGroup,
    clusters: &[GlyphCluster],
    anchor: TextAnchor,
) {
    match anchor {
        TextAnchor::Start => {
            if let Some(cluster) = clusters.iter().find(|cluster| {
                cluster.source_start < record.source_end && record.source_end < cluster.source_end
            }) {
                record.source_end = cluster.source_end;
            }
        }
        TextAnchor::End => {
            if let Some(cluster) = clusters.iter().find(|cluster| {
                cluster.source_start < record.source_start
                    && record.source_start < cluster.source_end
            }) {
                record.source_start = cluster.source_start;
            }
        }
        TextAnchor::Middle => {}
    }
}

pub(super) fn expand_draw_strings_cutoff_to_visible_cluster(
    record: &mut GlyphSemanticGroup,
    clusters: &[GlyphCluster],
    cluster_metrics: &[GlyphClusterMetrics],
    clip_bounds: Rect,
    base_margin: Fixed,
    anchor: TextAnchor,
    calc_single_page_layout: bool,
) -> Result<(), RenderError> {
    if calc_single_page_layout {
        return Ok(());
    }
    let inset = base_margin
        .max(Fixed::ZERO)
        .checked_add(Fixed::from_pixels(1))
        .ok_or(RenderError::CoordinateOverflow)?;
    match anchor {
        TextAnchor::Start => {
            let visible_end = clip_bounds
                .x
                .checked_add(clip_bounds.width)
                .and_then(|right| right.checked_sub(inset))
                .ok_or(RenderError::CoordinateOverflow)?;
            if let Some((cluster, _)) =
                clusters
                    .iter()
                    .zip(cluster_metrics)
                    .find(|(cluster, metrics)| {
                        cluster.source_start == record.source_end && metrics.origin_x < visible_end
                    })
            {
                record.source_end = cluster.source_end;
            }
        }
        TextAnchor::End => {
            let visible_start = clip_bounds
                .x
                .checked_add(inset)
                .ok_or(RenderError::CoordinateOverflow)?;
            if let Some((cluster, _)) =
                clusters
                    .iter()
                    .zip(cluster_metrics)
                    .rev()
                    .find(|(cluster, metrics)| {
                        let trailing = metrics
                            .origin_x
                            .checked_add(metrics.advance_x)
                            .unwrap_or(metrics.origin_x);
                        cluster.source_end == record.source_start && trailing > visible_start
                    })
            {
                record.source_start = cluster.source_start;
            }
        }
        TextAnchor::Middle => {}
    }
    Ok(())
}

pub(super) fn calc_draw_strings_edit_character(character: char) -> bool {
    matches!(
        character,
        '\u{00a0}' | '\u{00ad}' | '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{2011}' | '\u{2060}'
    )
}

pub(super) fn utf16_prefix_byte_len(text: &str, units: usize) -> usize {
    let mut consumed = 0_usize;
    for (offset, character) in text.char_indices() {
        consumed = consumed.saturating_add(character.len_utf16());
        if consumed >= units {
            return offset + character.len_utf8();
        }
    }
    text.len()
}

pub(super) fn utf16_suffix_byte_len(text: &str, units: usize) -> usize {
    let mut consumed = 0_usize;
    for (offset, character) in text.char_indices().rev() {
        consumed = consumed.saturating_add(character.len_utf16());
        if consumed >= units {
            return text.len() - offset;
        }
    }
    text.len()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct CalcScriptClassSummary {
    pub(super) first: Option<CalcScriptClass>,
    pub(super) mixed: bool,
    pub(super) has_western: bool,
    pub(super) has_asian: bool,
    pub(super) has_complex: bool,
}

impl CalcScriptClassSummary {
    pub(super) fn record(&mut self, script: CalcScriptClass) {
        self.has_western |= script == CalcScriptClass::Western;
        self.has_asian |= script == CalcScriptClass::Asian;
        self.has_complex |= script == CalcScriptClass::Complex;
        if self.first.is_some_and(|first| first != script) {
            self.mixed = true;
        } else if self.first.is_none() {
            self.first = Some(script);
        }
    }

    pub(super) fn merge(&mut self, other: Self) {
        self.has_western |= other.has_western;
        self.has_asian |= other.has_asian;
        self.has_complex |= other.has_complex;
        self.mixed |= other.mixed;
        if let Some(script) = other.first {
            self.record(script);
        }
    }
}

fn calc_script_class_summary(text: &str) -> CalcScriptClassSummary {
    let mut summary = CalcScriptClassSummary::default();
    for script in text.chars().filter_map(calc_script_class) {
        summary.record(script);
    }
    summary
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct CalcCellScriptAnalysis {
    pub(super) edit_engine_uses_only_complex_role: bool,
}

pub(super) fn account_automatic_text_bytes(
    text: &str,
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<(), RenderError> {
    stats.text_bytes = stats
        .text_bytes
        .checked_add(text.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::TextBytes,
        options.limits.max_text_bytes,
        stats.text_bytes,
    )
}

pub(super) fn calc_script_class_summary_bounded(
    text: &str,
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<CalcScriptClassSummary, RenderError> {
    let base_work = stats.text_work;
    let mut scanned = 0_u64;
    for _ in text.chars() {
        scanned = scanned
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        let actual = base_work
            .checked_add(scanned)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(LimitKind::TextRuns, options.limits.max_text_runs, actual)?;
    }
    stats.text_work = base_work
        .checked_add(scanned)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(calc_script_class_summary(text))
}

fn is_explicit_bidi_control(class: BidiClass) -> bool {
    matches!(
        class,
        BidiClass::LRE
            | BidiClass::RLE
            | BidiClass::LRO
            | BidiClass::RLO
            | BidiClass::PDF
            | BidiClass::LRI
            | BidiClass::RLI
            | BidiClass::FSI
            | BidiClass::PDI
    )
}

/// Whether Calc's `MakeScriptChangeScanner` assigns every scalar in one
/// ambiguous plain-text cell to the COMPLEX role.
///
/// `unicode-bidi` supplies the same UAX #9 logical embedding levels consumed
/// by LibreOffice's `MakeDirectionChangeScanner`. Within an RTL level, or an
/// embedded LTR level that has no strong LTR scalar, EditEngine promotes every
/// non-Asian script class (including ASCII punctuation and digits) to COMPLEX.
/// Explicit embeddings, overrides, and isolates fail closed because faithfully
/// replaying their directional-status stack is outside this source-specific
/// automatic-row contract.
pub(super) fn calc_edit_engine_uses_only_complex_role(
    text: &str,
    summary: CalcScriptClassSummary,
    options: &RenderOptions,
) -> Result<bool, RenderError> {
    if !summary.mixed || !summary.has_complex || text.is_empty() {
        return Ok(false);
    }
    enforce(
        LimitKind::TextBytes,
        options.limits.max_text_bytes,
        text.len() as u64,
    )?;
    if text.chars().map(bidi_class).any(is_explicit_bidi_control) {
        return Ok(false);
    }

    let bidi = BidiInfo::new(text, None);
    let Some(paragraph) = bidi.paragraphs.first() else {
        return Ok(false);
    };
    if bidi.paragraphs.len() != 1 || paragraph.range != (0..text.len()) {
        return Ok(false);
    }

    let mut previous = text
        .chars()
        .find_map(calc_script_class)
        .unwrap_or(CalcScriptClass::Western);
    let mut run_start = 0_usize;
    while run_start < text.len() {
        let Some(level) = bidi.levels.get(run_start).copied() else {
            return Ok(false);
        };
        let mut run_end = text.len();
        for (relative, _) in text[run_start..].char_indices().skip(1) {
            let index = run_start
                .checked_add(relative)
                .ok_or(RenderError::CoordinateOverflow)?;
            if bidi.levels.get(index).copied() != Some(level) {
                run_end = index;
                break;
            }
        }
        if run_end <= run_start || !text.is_char_boundary(run_end) {
            return Ok(false);
        }
        let run = &text[run_start..run_end];
        let embedded_ltr_has_strong = level.is_ltr()
            && level.number() > 1
            && run
                .chars()
                .map(bidi_class)
                .any(|class| class == BidiClass::L);
        for character in run.chars() {
            let mut script = calc_script_class(character);
            if (level.is_rtl() || (level.number() > 0 && !embedded_ltr_has_strong))
                && script != Some(CalcScriptClass::Asian)
            {
                script = Some(CalcScriptClass::Complex);
            } else if script.is_none() {
                script = Some(previous);
            }
            let Some(script) = script else {
                return Ok(false);
            };
            if script != CalcScriptClass::Complex {
                return Ok(false);
            }
            previous = script;
        }
        run_start = run_end;
    }
    Ok(true)
}

pub(super) fn text_style(region: &Region, options: &RenderOptions) -> TextStyle {
    let style = region.style.as_ref();
    let font = style.and_then(|style| style.font.as_ref());
    let alignment = style.and_then(|style| style.align.as_ref());
    let anchor = match alignment.and_then(|alignment| alignment.horizontal) {
        Some(HAlign::Left) => TextAnchor::Start,
        Some(HAlign::Center) => TextAnchor::Middle,
        Some(HAlign::Right) => TextAnchor::End,
        None if region.numeric_default => TextAnchor::End,
        None if text_base_direction(&region.text, false) == BaseDirection::RightToLeft => {
            TextAnchor::End
        }
        None => TextAnchor::Start,
    };
    let baseline = match alignment.and_then(|alignment| alignment.vertical) {
        Some(VAlign::Top) => TextBaseline::Top,
        Some(VAlign::Middle) => TextBaseline::Middle,
        // ECMA-376 Part 1 section 18.8.1 gives `vertical` a default of
        // `bottom`, which is what Excel and Calc both render for a cell that
        // declares no vertical alignment.
        Some(VAlign::Bottom) | None => TextBaseline::Bottom,
    };
    let size = font
        .and_then(|font| font.size_pt)
        .and_then(|points| points_to_fixed(points as f32))
        .unwrap_or(options.default_font_size);
    TextStyle {
        family: font
            .and_then(|font| font.name.clone())
            .unwrap_or_else(|| options.default_font_family.clone()),
        size,
        color: font
            .and_then(|font| font.color)
            .map(rgb)
            .unwrap_or(Rgb::BLACK),
        bold: font.is_some_and(|font| font.bold),
        italic: font.is_some_and(|font| font.italic),
        underline: font.is_some_and(|font| font.underline),
        strikethrough: font.is_some_and(|font| font.strikethrough),
        anchor,
        baseline,
        rotation_degrees: alignment.map_or(0, |alignment| alignment.rotation),
    }
}

pub(super) fn text_base_direction(text: &str, sheet_right_to_left: bool) -> BaseDirection {
    for character in text.chars() {
        match bidi_class(character) {
            BidiClass::L => return BaseDirection::LeftToRight,
            BidiClass::R | BidiClass::AL => return BaseDirection::RightToLeft,
            _ => {}
        }
    }
    if sheet_right_to_left {
        BaseDirection::RightToLeft
    } else {
        BaseDirection::Auto
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedRunStyle {
    pub(super) family: String,
    pub(super) size: Fixed,
    pub(super) color: Rgb,
    pub(super) bold: bool,
    pub(super) italic: bool,
    pub(super) underline: bool,
    pub(super) strikethrough: bool,
    pub(super) script: FormatScript,
}

impl ResolvedRunStyle {
    pub(super) fn request(&self) -> FontRequest<'_> {
        FontRequest {
            family: &self.family,
            weight: if self.bold { 700 } else { 400 },
            italic: self.italic,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct StyledSourceSpan {
    pub(super) source: Range<usize>,
    pub(super) style_index: usize,
}

pub(super) struct PreparedLine {
    pub(super) source: Range<usize>,
    pub(super) advance_end: usize,
    pub(super) shaped: ShapedText,
    pub(super) width: Fixed,
    pub(super) metrics: CombinedLineMetrics,
}

pub(super) struct PreparedText {
    pub(super) styles: Vec<ResolvedRunStyle>,
    pub(super) lines: Vec<PreparedLine>,
    pub(super) line_layout_policy: CellLineLayoutPolicy,
    pub(super) line_placement_policy: CalcLinePlacementPolicy,
    pub(super) horizontal_padding: Fixed,
    pub(super) available_width: Fixed,
    pub(super) max_width: Fixed,
    pub(super) missing_glyphs: u64,
    pub(super) family_substituted: bool,
}

pub(super) fn measure_automatic_cell_height(
    pack: &FontPack,
    region: &Region,
    sheet_right_to_left: bool,
    options: &RenderOptions,
    stats: &mut TypographyStats,
    calc_metric_source: Option<CalcAutomaticMetricSource>,
    calc_pattern_points: Option<u16>,
) -> Result<Fixed, RenderError> {
    let style = text_style(region, options);
    let prepared = prepare_styled_text(
        pack,
        region,
        &style,
        sheet_right_to_left,
        CALC_WORKSHEET_KERNING,
        options,
        stats,
    )?;
    if let Some(source) = calc_metric_source {
        if let Some(height) = calc_verified_automatic_cell_height(
            pack,
            &prepared,
            &region.text,
            source,
            calc_pattern_points,
            options,
            stats,
        )? {
            return Ok(height);
        }
    }
    match prepared.line_layout_policy {
        CellLineLayoutPolicy::Native | CellLineLayoutPolicy::OdsNative => sum_fixed(
            prepared
                .lines
                .iter()
                .map(|line| line_height_from_metrics(line.metrics, prepared.line_placement_policy))
                .collect::<Result<Vec<_>, _>>()?,
        )?
        .checked_add(Fixed::from_pixels(AUTO_ROW_VERTICAL_PADDING_PIXELS))
        .ok_or(RenderError::CoordinateOverflow),
        CellLineLayoutPolicy::CalcEditEngine => calc_automatic_cell_height(&prepared.lines),
    }
}

fn calc_verified_automatic_cell_height(
    pack: &FontPack,
    prepared: &PreparedText,
    text: &str,
    source: CalcAutomaticMetricSource,
    pattern_points: Option<u16>,
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<Option<Fixed>, RenderError> {
    let style = prepared.styles.first().ok_or(RenderError::Typography {
        reason: "missing_text_style",
    })?;
    let resolution = pack.resolve(style.request());
    if !(resolution.exact_family || resolution.declared_alias) || !resolution.exact_style {
        return Ok(None);
    }
    // A single line whose cell stays on the pattern font resolves through the
    // same formula the implicit row height already uses, so an automatic row
    // and an untouched one agree by construction. Multi-line runs keep the
    // per-line metric accumulation below, because Calc stacks measured lines.
    if prepared.lines.len() == 1 {
        if let Some(height) = pattern_points.and_then(calc_ooxml_row_height_from_points) {
            return Ok(Some(height));
        }
    }
    let font_id = match source {
        CalcAutomaticMetricSource::RequestedFont => resolution.id,
        CalcAutomaticMetricSource::PreparedAsianOrRequested => {
            match prepared_asian_face(prepared, text, options, stats)? {
                PreparedAsianFace::None => resolution.id,
                PreparedAsianFace::Verified(font_id)
                    if pack.weight(font_id).map_err(map_font_error)?
                        == if style.bold { 700 } else { 400 }
                        && pack.is_italic(font_id).map_err(map_font_error)? == style.italic =>
                {
                    font_id
                }
                PreparedAsianFace::Verified(_) | PreparedAsianFace::Unverified => return Ok(None),
            }
        }
        CalcAutomaticMetricSource::CalcComplexRole => {
            let Some(font_id) = calc_verified_complex_role_face(pack, style, resolution.id)? else {
                return Ok(None);
            };
            font_id
        }
    };
    let metrics = single_face_line_metrics(
        pack,
        font_id,
        style,
        CellLineLayoutPolicy::CalcEditEngine,
        1,
        1,
    )?;
    let line_height = calc_line_height_mm100(metrics)?;
    let total = line_height
        .checked_mul(prepared.lines.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    calc_automatic_height_from_engine_mm100(total).map(Some)
}

fn calc_face_has_complex_role_coverage(
    pack: &FontPack,
    font_id: FontId,
) -> Result<bool, RenderError> {
    for probe in CALC_CTL_FONT_PROBES {
        if pack
            .face_supports_text(font_id, probe)
            .map_err(map_font_error)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn calc_verified_complex_role_face(
    pack: &FontPack,
    style: &ResolvedRunStyle,
    requested_font_id: FontId,
) -> Result<Option<FontId>, RenderError> {
    // OOXML import keeps a requested family in the CTL slot whenever the
    // resolved face passes Calc's complex-script classification. The logical
    // document default is consulted only when that slot was left untouched.
    if calc_face_has_complex_role_coverage(pack, requested_font_id)? {
        return Ok(Some(requested_font_id));
    }
    let resolution = pack.resolve(FontRequest {
        family: CALC_CTL_LOGICAL_FAMILY,
        weight: if style.bold { 700 } else { 400 },
        italic: style.italic,
    });
    if !(resolution.exact_family || resolution.declared_alias) || !resolution.exact_style {
        return Ok(None);
    }
    if !calc_face_has_complex_role_coverage(pack, resolution.id)? {
        return Ok(None);
    }
    Ok(Some(resolution.id))
}

fn apply_calc_complex_role_shaping_style(
    pack: &FontPack,
    region: &Region,
    styles: &mut [ResolvedRunStyle],
    options: &RenderOptions,
) -> Result<(), RenderError> {
    if region.rich_text.is_some()
        || !matches!(
            region.line_placement_policy,
            CalcLinePlacementPolicy::Imported(
                CalcImportProvenance::Biff
                    | CalcImportProvenance::Xlsb
                    | CalcImportProvenance::Xlsx
            )
        )
    {
        return Ok(());
    }
    let summary = calc_script_class_summary(&region.text);
    if !calc_edit_engine_uses_only_complex_role(&region.text, summary, options)? {
        return Ok(());
    }
    let style = styles.first_mut().ok_or(RenderError::Typography {
        reason: "missing_text_style",
    })?;
    let requested = pack.resolve(style.request());
    if !(requested.exact_family || requested.declared_alias) || !requested.exact_style {
        return Ok(());
    }
    let Some(complex_role) = calc_verified_complex_role_face(pack, style, requested.id)? else {
        return Ok(());
    };
    if complex_role != requested.id {
        // Calc assigns the complete RTL embedding, including Western digits,
        // to the COMPLEX font role. The verified pack owns the deterministic
        // Tahoma alias; script fallback still selects an Arabic face for the
        // Arabic glyphs while neutral digits remain on this role face.
        style.family = CALC_CTL_LOGICAL_FAMILY.to_string();
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PreparedAsianFace {
    None,
    Verified(FontId),
    Unverified,
}

pub(super) fn prepared_asian_face(
    prepared: &PreparedText,
    text: &str,
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<PreparedAsianFace, RenderError> {
    let mut selected = None;
    for line in &prepared.lines {
        for run in &line.shaped.runs {
            let Some(start) = line.source.start.checked_add(run.source.start) else {
                return Ok(PreparedAsianFace::Unverified);
            };
            let Some(end) = line.source.start.checked_add(run.source.end) else {
                return Ok(PreparedAsianFace::Unverified);
            };
            if start > end || end > line.advance_end {
                return Ok(PreparedAsianFace::Unverified);
            }
            let Some(source) = text.get(start..end) else {
                return Ok(PreparedAsianFace::Unverified);
            };
            let summary = calc_script_class_summary_bounded(source, options, stats)?;
            if !summary.has_asian {
                continue;
            }
            if run.style_index != 0 || run.glyphs.iter().any(|glyph| glyph.glyph_id == 0) {
                return Ok(PreparedAsianFace::Unverified);
            }
            if selected.is_some_and(|font_id| font_id != run.font_id) {
                return Ok(PreparedAsianFace::Unverified);
            }
            selected = Some(run.font_id);
        }
    }
    Ok(selected.map_or(PreparedAsianFace::None, PreparedAsianFace::Verified))
}

pub(super) fn account_shaping(
    pack: &FontPack,
    shaped: &ShapedText,
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<(), RenderError> {
    for selected in &shaped.selected_faces {
        stats.record_face(pack, selected.font_id, selected.substituted)?;
    }
    stats.shaped_glyphs = stats
        .shaped_glyphs
        .checked_add(shaped.glyph_count as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::Glyphs,
        options.limits.max_glyphs,
        stats.shaped_glyphs,
    )?;
    stats.shaped_runs = stats
        .shaped_runs
        .checked_add(shaped.runs.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::TextRuns,
        options.limits.max_text_runs,
        stats.shaped_runs,
    )
}

pub(super) fn line_height_from_metrics(
    metrics: CombinedLineMetrics,
    policy: CalcLinePlacementPolicy,
) -> Result<Fixed, RenderError> {
    let height = if policy.uses_placement_line_height() {
        metrics
            .placement_ascent
            .checked_sub(metrics.placement_descent)
            .ok_or(RenderError::CoordinateOverflow)?
    } else {
        metrics
            .ascent
            .checked_sub(metrics.descent)
            .ok_or(RenderError::CoordinateOverflow)?
            .checked_add(metrics.line_gap)
            .ok_or(RenderError::CoordinateOverflow)?
    };
    Ok(height.max(Fixed::from_raw(1)))
}

pub(super) fn calc_line_height_mm100(metrics: CombinedLineMetrics) -> Result<u64, RenderError> {
    let ascent =
        u64::try_from(metrics.calc_ascent_pixels).map_err(|_| RenderError::CoordinateOverflow)?;
    let descent = metrics.calc_descent_pixels.unsigned_abs();
    let formatter = round_unsigned_ratio(ascent, MM100_PER_INCH, CALC_DEVICE_DPI)
        .and_then(|value| {
            round_unsigned_ratio(descent, MM100_PER_INCH, CALC_DEVICE_DPI)
                .and_then(|descent| value.checked_add(descent))
        })
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(metrics.calc_portion_height_mm100.max(formatter).max(1))
}

pub(super) fn calc_automatic_cell_height(lines: &[PreparedLine]) -> Result<Fixed, RenderError> {
    let total_mm100 = lines.iter().try_fold(0_u64, |total, line| {
        total
            .checked_add(calc_line_height_mm100(line.metrics)?)
            .ok_or(RenderError::CoordinateOverflow)
    })?;
    calc_automatic_height_from_engine_mm100(total_mm100)
}

pub(super) fn calc_automatic_height_from_engine_mm100(
    total_mm100: u64,
) -> Result<Fixed, RenderError> {
    let text_pixels = round_unsigned_ratio(total_mm100, CALC_DEVICE_DPI, MM100_PER_INCH)
        .ok_or(RenderError::CoordinateOverflow)?;
    let row_pixels = text_pixels
        .checked_add(
            u64::try_from(AUTO_ROW_VERTICAL_PADDING_PIXELS)
                .map_err(|_| RenderError::CoordinateOverflow)?,
        )
        .ok_or(RenderError::CoordinateOverflow)?;
    let row_twips = row_pixels
        .checked_mul(CALC_OPTIMAL_HEIGHT_SAMPLE_TWIPS)
        .and_then(|value| value.checked_div(CALC_OPTIMAL_HEIGHT_SAMPLE_PIXELS))
        .ok_or(RenderError::CoordinateOverflow)?;
    let raw = round_unsigned_ratio(
        row_twips,
        FIXED_UNITS_PER_PIXEL as u64,
        u64::try_from(TWIPS_PER_CSS_PIXEL).map_err(|_| RenderError::CoordinateOverflow)?,
    )
    .and_then(|value| i64::try_from(value).ok())
    .ok_or(RenderError::CoordinateOverflow)?;
    Ok(Fixed::from_raw(raw.max(1)))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_glyph_run(
    pack: &FontPack,
    region: &Region,
    layout_bounds: Rect,
    clip_bounds: Rect,
    output_bounds: Option<Rect>,
    style: &TextStyle,
    sheet_right_to_left: bool,
    kerning: bool,
    calc_single_page_layout: bool,
    options: &RenderOptions,
    stats: &mut TypographyStats,
    warnings: &mut Warnings,
) -> Result<GlyphRunNode, RenderError> {
    let alignment = region.style.as_ref().and_then(|style| style.align.as_ref());
    let mut prepared = prepare_styled_text(
        pack,
        region,
        style,
        sheet_right_to_left,
        kerning,
        options,
        stats,
    )?;
    warnings.add_count(
        WarningCode::MissingGlyph,
        prepared.missing_glyphs,
        Some(region.source),
    );
    if prepared.family_substituted {
        warnings.add(WarningCode::FontFamilySubstituted, Some(region.source));
    }

    let mut scale_numerator = 1_i64;
    let mut scale_denominator = 1_i64;
    if alignment.is_some_and(|alignment| alignment.shrink_to_fit)
        && !alignment.is_some_and(|alignment| alignment.wrap)
        && prepared.max_width > prepared.available_width
        && prepared.max_width.raw() > 0
    {
        scale_numerator = prepared.available_width.raw().max(1);
        scale_denominator = prepared.max_width.raw();
        let floor = options.min_shrink_font_size.max(Fixed::from_raw(1));
        if scale_ratio(prepared.styles[0].size, scale_numerator, scale_denominator)? < floor {
            scale_numerator = floor.raw();
            scale_denominator = prepared.styles[0].size.raw().max(1);
        }
        for line in &mut prepared.lines {
            line.width = styled_shaped_width(
                pack,
                &line.shaped,
                &prepared.styles,
                scale_numerator,
                scale_denominator,
            )?;
            line.metrics = styled_line_metrics(
                pack,
                &line.shaped,
                &prepared.styles,
                prepared.line_layout_policy,
                prepared.line_placement_policy,
                region.text.get(line.source.start..line.advance_end).ok_or(
                    RenderError::Typography {
                        reason: "invalid_line_source_range",
                    },
                )?,
                scale_numerator,
                scale_denominator,
                options,
            )?;
        }
    }

    let mut line_heights = prepared
        .lines
        .iter()
        .map(|line| line_height_from_metrics(line.metrics, prepared.line_placement_policy))
        .collect::<Result<Vec<_>, _>>()?;
    let full_block_height = sum_fixed(line_heights.iter().copied())?;
    if region.ods_fixed_height_row
        && full_block_height > region.rect.height
        && alignment.is_some_and(|alignment| alignment.wrap && alignment.rotation == 0)
    {
        let mut covered_height = Fixed::ZERO;
        let mut covered_lines = 0_usize;
        while covered_lines < line_heights.len() && covered_height < region.rect.height {
            covered_height = covered_height
                .checked_add(line_heights[covered_lines])
                .ok_or(RenderError::CoordinateOverflow)?;
            covered_lines += 1;
        }
        // Calc's fixed-row printer keeps one leading line beyond the lines
        // needed to cover the row. Bottom alignment can place that line above
        // the clip while preserving the final visible line at the row edge.
        let retained_lines = covered_lines.saturating_add(1).min(line_heights.len());
        prepared.lines.truncate(retained_lines);
        line_heights.truncate(retained_lines);
    }
    let block_height = sum_fixed(line_heights.iter().copied())?;
    // An implicitly aligned wrapped ODS cell gets no special positioning: Calc
    // resolves the absent alignment to its ordinary bottom default and lets the
    // beginning translate above the row clip, exactly as an explicit bottom
    // alignment does. Measured against the pinned oracle on a fixed-height row,
    // the first words of an implicitly aligned cell and of an explicitly
    // bottom-aligned cell are both pushed off the top, while an explicitly
    // top-aligned cell keeps its first word at the row top.
    let top = vertical_block_top(layout_bounds, block_height, style.baseline)?;

    let mut commands = Vec::new();
    let mut clusters = Vec::new();
    let mut cluster_metrics = Vec::new();
    let mut paints = Vec::new();
    let mut decorations = Vec::new();
    let mut glyphs = Vec::new();
    let mut font_faces = Vec::new();
    let mut line_top = top;
    for (line, line_height) in prepared.lines.iter().zip(line_heights) {
        let placement_ascent = if prepared.line_placement_policy.uses_placement_ascent() {
            line.metrics.placement_ascent
        } else {
            line.metrics.ascent
        };
        let baseline = line_top
            .checked_add(placement_ascent)
            .ok_or(RenderError::CoordinateOverflow)?;
        let line_x = horizontal_line_start(
            layout_bounds,
            prepared.horizontal_padding,
            line.width,
            style.anchor,
        )?;
        append_styled_shaped_outlines(
            pack,
            &region.text,
            line.source.start,
            &line.shaped,
            line_x,
            baseline,
            &prepared.styles,
            scale_numerator,
            scale_denominator,
            options,
            stats,
            &mut commands,
            &mut clusters,
            &mut cluster_metrics,
            &mut paints,
            &mut decorations,
            &mut glyphs,
            &mut font_faces,
        )?;
        if line.advance_end < line.source.end {
            if region.text.get(line.advance_end..line.source.end) != Some(" ") {
                return Err(RenderError::Typography {
                    reason: "invalid_suppressed_line_suffix",
                });
            }
            let command_index =
                u64::try_from(commands.len()).map_err(|_| RenderError::CoordinateOverflow)?;
            let source_start =
                u64::try_from(line.advance_end).map_err(|_| RenderError::CoordinateOverflow)?;
            let source_end =
                u64::try_from(line.source.end).map_err(|_| RenderError::CoordinateOverflow)?;
            clusters.push(GlyphCluster {
                source_start,
                source_end,
                command_start: command_index,
                command_end: command_index,
            });
            let origin_x = if line.shaped.base_direction == BaseDirection::RightToLeft {
                line_x
            } else {
                line_x
                    .checked_add(line.width)
                    .ok_or(RenderError::CoordinateOverflow)?
            };
            cluster_metrics.push(GlyphClusterMetrics {
                origin_x,
                advance_x: Fixed::ZERO,
                baseline_y: baseline,
                ascent: line.metrics.ascent.max(Fixed::from_raw(1)),
                descent: line.metrics.descent.min(Fixed::ZERO),
            });
        }
        line_top = line_top
            .checked_add(line_height)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    let (pivot_x, pivot_y) = rotation_pivot(layout_bounds, prepared.horizontal_padding, style)?;
    let semantic_groups = glyph_semantic_groups_with_calc_shortening(
        region,
        &prepared,
        block_height,
        clip_bounds,
        output_bounds,
        style,
        sheet_right_to_left,
    )?;
    let mut semantic_groups = semantic_groups;
    if semantic_groups.len() == 2
        && semantic_groups[0].source_start == semantic_groups[0].source_end
    {
        expand_draw_strings_cutoff_to_cluster_boundary(
            &mut semantic_groups[1],
            &clusters,
            style.anchor,
        );
        if style.bold {
            expand_draw_strings_cutoff_to_visible_cluster(
                &mut semantic_groups[1],
                &clusters,
                &cluster_metrics,
                clip_bounds,
                region.vertical_margin,
                style.anchor,
                calc_single_page_layout,
            )?;
        }
    }
    let node = GlyphRunNode {
        glyphs,
        font_faces,
        text: region.text.clone(),
        clip_bounds,
        commands,
        clusters,
        cluster_metrics,
        semantic_groups,
        paints,
        decorations,
        color: style.color,
        rotation_degrees: style.rotation_degrees,
        pivot_x,
        pivot_y,
        hyperlink: region.hyperlink.clone(),
    };
    if !node.metadata_is_valid() {
        return Err(RenderError::Typography {
            reason: "invalid_glyph_metadata",
        });
    }
    Ok(node)
}

pub(super) fn prepare_styled_text(
    pack: &FontPack,
    region: &Region,
    base: &TextStyle,
    sheet_right_to_left: bool,
    kerning: bool,
    options: &RenderOptions,
    stats: &mut TypographyStats,
) -> Result<PreparedText, RenderError> {
    let (mut styles, spans) = resolve_rich_styles(region, base)?;
    apply_calc_complex_role_shaping_style(pack, region, &mut styles, options)?;
    let direction = text_base_direction(&region.text, sheet_right_to_left);
    let primary_size = styled_font_size(&styles[0], 1, 1)?;
    let horizontal_padding =
        outlined_horizontal_padding(pack, styles[0].request(), primary_size, region)?;
    let available_width = inner_width(region.rect.width, horizontal_padding)?;
    let calc_wrap_space = match region.line_layout_policy {
        CellLineLayoutPolicy::Native | CellLineLayoutPolicy::OdsNative => None,
        CellLineLayoutPolicy::CalcEditEngine => {
            Some(region.calc_wrap_space.ok_or(RenderError::Typography {
                reason: "missing_calc_wrap_space",
            })?)
        }
    };
    let line_available_width = match calc_wrap_space {
        Some(space) => space.line_width()?,
        None => available_width,
    };
    let scalar_count = region.text.chars().count() as u64;
    let work = scalar_count
        .checked_mul(2)
        .and_then(|value| value.checked_add(1))
        .ok_or(RenderError::CoordinateOverflow)?;
    stats.text_work = stats
        .text_work
        .checked_add(work)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::TextRuns,
        options.limits.max_text_runs,
        stats.text_work,
    )?;
    let remaining_lines = options
        .limits
        .max_text_lines
        .saturating_sub(stats.text_lines);
    let wrap = region
        .style
        .as_ref()
        .and_then(|style| style.align.as_ref())
        .is_some_and(|alignment| alignment.wrap);
    let wrapped_lines = wrap_text_lines(
        &region.text,
        wrap,
        region.line_layout_policy,
        line_available_width,
        remaining_lines,
        work,
        |range| {
            let shaped = shape_styled_range(
                pack,
                &region.text,
                range,
                &spans,
                &styles,
                direction,
                kerning,
                options,
            )?;
            let width = styled_shaped_width(pack, &shaped, &styles, 1, 1)?;
            match calc_wrap_space {
                Some(_) => CalcWrapSpace::measure_physical_width(width, primary_size),
                None => Ok(width),
            }
        },
    )?;

    let mut lines = Vec::with_capacity(wrapped_lines.len());
    let mut max_width = Fixed::ZERO;
    let mut missing_glyphs = 0_u64;
    let mut family_substituted = false;
    for line in wrapped_lines {
        let source = line.source;
        let line_text =
            region
                .text
                .get(source.start..line.advance_end)
                .ok_or(RenderError::Typography {
                    reason: "invalid_line_source_range",
                })?;
        let shaped = shape_styled_range(
            pack,
            &region.text,
            source.start..line.advance_end,
            &spans,
            &styles,
            direction,
            kerning,
            options,
        )?;
        account_shaping(pack, &shaped, options, stats)?;
        let width = styled_shaped_width(pack, &shaped, &styles, 1, 1)?;
        let metrics = styled_line_metrics(
            pack,
            &shaped,
            &styles,
            region.line_layout_policy,
            region.line_placement_policy,
            line_text,
            1,
            1,
            options,
        )?;
        max_width = max_width.max(width);
        missing_glyphs = missing_glyphs.saturating_add(shaped.missing_glyphs as u64);
        family_substituted |= !shaped.requested_family_matched;
        lines.push(PreparedLine {
            source,
            advance_end: line.advance_end,
            shaped,
            width,
            metrics,
        });
    }
    stats.text_lines = stats
        .text_lines
        .checked_add(lines.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::TextLines,
        options.limits.max_text_lines,
        stats.text_lines,
    )?;
    Ok(PreparedText {
        styles,
        lines,
        line_layout_policy: region.line_layout_policy,
        line_placement_policy: region.line_placement_policy,
        horizontal_padding,
        available_width,
        max_width,
        missing_glyphs,
        family_substituted,
    })
}

pub(super) fn resolve_rich_styles(
    region: &Region,
    base: &TextStyle,
) -> Result<(Vec<ResolvedRunStyle>, Vec<StyledSourceSpan>), RenderError> {
    let cell_script = region
        .style
        .as_ref()
        .and_then(|style| style.font.as_ref())
        .map_or(FormatScript::None, |font| font.script);
    let base_style = ResolvedRunStyle {
        family: base.family.clone(),
        size: base.size,
        color: base.color,
        bold: base.bold,
        italic: base.italic,
        underline: base.underline,
        strikethrough: base.strikethrough,
        script: cell_script,
    };
    let mut styles = vec![base_style.clone()];
    let Some(runs) = region.rich_text.as_deref() else {
        return Ok((
            styles,
            vec![StyledSourceSpan {
                source: 0..region.text.len(),
                style_index: 0,
            }],
        ));
    };
    let mut spans: Vec<StyledSourceSpan> = Vec::new();
    let mut cursor = 0_usize;
    for run in runs {
        let end = cursor
            .checked_add(run.text.len())
            .ok_or(RenderError::CoordinateOverflow)?;
        if end > region.text.len()
            || !region.text.is_char_boundary(cursor)
            || !region.text.is_char_boundary(end)
        {
            return Err(RenderError::Typography {
                reason: "invalid_rich_text_range",
            });
        }
        let candidate = ResolvedRunStyle {
            family: run
                .font
                .name
                .clone()
                .unwrap_or_else(|| base_style.family.clone()),
            size: run
                .font
                .size_pt
                .and_then(|points| points_to_fixed(points as f32))
                .unwrap_or(base_style.size),
            color: run.font.color.map(rgb).unwrap_or(base_style.color),
            bold: base_style.bold || run.font.bold,
            italic: base_style.italic || run.font.italic,
            underline: base_style.underline || run.font.underline,
            strikethrough: base_style.strikethrough || run.font.strikethrough,
            script: if run.font.script == FormatScript::None {
                base_style.script
            } else {
                run.font.script
            },
        };
        let style_index = styles
            .iter()
            .position(|style| style == &candidate)
            .unwrap_or_else(|| {
                styles.push(candidate);
                styles.len() - 1
            });
        if cursor != end {
            if let Some(last) = spans.last_mut() {
                if last.style_index == style_index && last.source.end == cursor {
                    last.source.end = end;
                } else {
                    spans.push(StyledSourceSpan {
                        source: cursor..end,
                        style_index,
                    });
                }
            } else {
                spans.push(StyledSourceSpan {
                    source: cursor..end,
                    style_index,
                });
            }
        }
        cursor = end;
    }
    if cursor != region.text.len() || (!region.text.is_empty() && spans.is_empty()) {
        return Err(RenderError::Typography {
            reason: "invalid_rich_text_range",
        });
    }
    Ok((styles, spans))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn shape_styled_range(
    pack: &FontPack,
    text: &str,
    source: Range<usize>,
    spans: &[StyledSourceSpan],
    styles: &[ResolvedRunStyle],
    direction: BaseDirection,
    kerning: bool,
    options: &RenderOptions,
) -> Result<ShapedText, RenderError> {
    let value = text.get(source.clone()).ok_or(RenderError::Typography {
        reason: "invalid_rich_text_range",
    })?;
    let requests = spans
        .iter()
        .filter_map(|span| {
            let start = span.source.start.max(source.start);
            let end = span.source.end.min(source.end);
            (start < end).then_some((start, end, span.style_index))
        })
        .map(|(start, end, style_index)| {
            let style = styles.get(style_index).ok_or(RenderError::Typography {
                reason: "invalid_rich_style_index",
            })?;
            Ok(StyledFontRequest {
                source: start - source.start..end - source.start,
                request: style.request(),
                style_index,
            })
        })
        .collect::<Result<Vec<_>, RenderError>>()?;
    let glyph_limit = usize::try_from(options.limits.max_glyphs).unwrap_or(usize::MAX);
    let run_limit = usize::try_from(options.limits.max_text_runs).unwrap_or(usize::MAX);
    pack.shape_styled(
        value,
        &requests,
        ShapeOptions {
            direction,
            max_glyphs: glyph_limit,
            max_runs: run_limit,
            kerning,
        },
    )
    .map_err(map_font_error)
}

pub(super) fn styled_shaped_width(
    pack: &FontPack,
    shaped: &ShapedText,
    styles: &[ResolvedRunStyle],
    scale_numerator: i64,
    scale_denominator: i64,
) -> Result<Fixed, RenderError> {
    let mut width = Fixed::ZERO;
    for run in &shaped.runs {
        let style = styles.get(run.style_index).ok_or(RenderError::Typography {
            reason: "invalid_rich_style_index",
        })?;
        let metrics = pack.metrics(run.font_id).map_err(map_font_error)?;
        let advance = run.glyphs.iter().try_fold(0_i64, |sum, glyph| {
            sum.checked_add(i64::from(glyph.x_advance))
                .ok_or(RenderError::CoordinateOverflow)
        })?;
        let advance =
            i64::try_from(advance.unsigned_abs()).map_err(|_| RenderError::CoordinateOverflow)?;
        width = width
            .checked_add(scale_font_units(
                advance,
                styled_font_size(style, scale_numerator, scale_denominator)?,
                metrics.units_per_em,
                1,
            )?)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(width)
}

fn styled_font_size(
    style: &ResolvedRunStyle,
    scale_numerator: i64,
    scale_denominator: i64,
) -> Result<Fixed, RenderError> {
    let size = scale_ratio(style.size, scale_numerator, scale_denominator)?;
    match style.script {
        FormatScript::None => Ok(size),
        FormatScript::Superscript | FormatScript::Subscript => scale_ratio(size, 13, 20),
    }
}

fn styled_script_shift(
    style: &ResolvedRunStyle,
    scale_numerator: i64,
    scale_denominator: i64,
) -> Result<Fixed, RenderError> {
    let size = scale_ratio(style.size, scale_numerator, scale_denominator)?;
    match style.script {
        FormatScript::None => Ok(Fixed::ZERO),
        FormatScript::Superscript => negate_fixed(scale_ratio(size, 7, 20)?),
        FormatScript::Subscript => scale_ratio(size, 1, 5),
    }
}

pub(super) fn shape_text_with_kerning(
    pack: &FontPack,
    text: &str,
    request: FontRequest<'_>,
    direction: BaseDirection,
    kerning: bool,
    options: &RenderOptions,
) -> Result<ShapedText, RenderError> {
    let glyph_limit = usize::try_from(options.limits.max_glyphs).unwrap_or(usize::MAX);
    let run_limit = usize::try_from(options.limits.max_text_runs).unwrap_or(usize::MAX);
    pack.shape(
        text,
        request,
        ShapeOptions {
            direction,
            max_glyphs: glyph_limit,
            max_runs: run_limit,
            kerning,
        },
    )
    .map_err(map_font_error)
}

pub(super) fn shaped_width(
    pack: &FontPack,
    shaped: &ShapedText,
    font_size: Fixed,
) -> Result<Fixed, RenderError> {
    let mut width = Fixed::ZERO;
    for run in &shaped.runs {
        let metrics = pack.metrics(run.font_id).map_err(map_font_error)?;
        let advance = run
            .glyphs
            .iter()
            .try_fold(0_i64, |sum, glyph| {
                sum.checked_add(i64::from(glyph.x_advance))
                    .ok_or(RenderError::CoordinateOverflow)
            })?
            .unsigned_abs();
        let advance = i64::try_from(advance).map_err(|_| RenderError::CoordinateOverflow)?;
        width = width
            .checked_add(scale_font_units(
                advance,
                font_size,
                metrics.units_per_em,
                1,
            )?)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(width)
}

pub(super) fn outlined_horizontal_padding(
    pack: &FontPack,
    request: FontRequest<'_>,
    font_size: Fixed,
    region: &Region,
) -> Result<Fixed, RenderError> {
    // `ATTR_MARGIN` is one attribute covering all four cell edges, so Calc
    // insets text horizontally by the same value it uses vertically: 20 twips
    // ordinarily and the BIFF importer's 40 for an imported BIFF default. The
    // backend-neutral `horizontal_padding` option is a coarser 3 px, which
    // narrows the wrapping width by 2.5 pt against Calc and breaks lines Calc
    // keeps whole.
    let base = region.vertical_margin.max(Fixed::ZERO);
    let indent = region
        .style
        .as_ref()
        .and_then(|style| style.align.as_ref())
        .map_or(0_u8, |alignment| alignment.indent);
    if indent == 0 {
        return Ok(base);
    }
    let (font_id, digit_width) = pack.max_digit_width(request).map_err(map_font_error)?;
    let metrics = pack.metrics(font_id).map_err(map_font_error)?;
    let indent_width =
        scale_font_units(i64::from(digit_width), font_size, metrics.units_per_em, 1)?;
    base.checked_add(multiply_fixed(indent_width, i64::from(indent))?)
        .ok_or(RenderError::CoordinateOverflow)
}

pub(super) fn inner_width(width: Fixed, padding: Fixed) -> Result<Fixed, RenderError> {
    let inset = multiply_fixed(padding, 2)?;
    Ok(width
        .checked_sub(inset)
        .ok_or(RenderError::CoordinateOverflow)?
        .max(Fixed::from_raw(1)))
}

#[derive(Debug, Clone, Copy)]
pub(super) struct CombinedLineMetrics {
    pub(super) ascent: Fixed,
    pub(super) descent: Fixed,
    pub(super) line_gap: Fixed,
    pub(super) placement_ascent: Fixed,
    pub(super) placement_descent: Fixed,
    pub(super) calc_ascent_pixels: i64,
    pub(super) calc_descent_pixels: i64,
    pub(super) calc_portion_height_mm100: u64,
}

pub(super) fn single_face_line_metrics(
    pack: &FontPack,
    font_id: FontId,
    style: &ResolvedRunStyle,
    policy: CellLineLayoutPolicy,
    scale_numerator: i64,
    scale_denominator: i64,
) -> Result<CombinedLineMetrics, RenderError> {
    let metrics = pack.metrics(font_id).map_err(map_font_error)?;
    let font_size = styled_font_size(style, scale_numerator, scale_denominator)?;
    let shift = styled_script_shift(style, scale_numerator, scale_denominator)?;
    let ascent = scale_font_units(
        i64::from(metrics.ascent),
        font_size,
        metrics.units_per_em,
        1,
    )?
    .checked_sub(shift)
    .ok_or(RenderError::CoordinateOverflow)?;
    let descent = scale_font_units(
        i64::from(metrics.descent),
        font_size,
        metrics.units_per_em,
        1,
    )?
    .checked_sub(shift)
    .ok_or(RenderError::CoordinateOverflow)?;
    let line_gap = scale_font_units(
        i64::from(metrics.line_gap.max(0)),
        font_size,
        metrics.units_per_em,
        1,
    )?;
    let (calc_ascent_pixels, calc_descent_pixels, calc_portion_height_mm100) =
        if policy == CellLineLayoutPolicy::CalcEditEngine {
            let device_em_pixels = round_unsigned_ratio(
                u64::try_from(font_size.raw()).map_err(|_| RenderError::CoordinateOverflow)?,
                1,
                FIXED_UNITS_PER_PIXEL as u64,
            )
            .ok_or(RenderError::CoordinateOverflow)?;
            let calc_ascent_pixels = round_signed_ratio(
                i128::from(metrics.ascent),
                i128::from(device_em_pixels),
                i128::from(metrics.units_per_em),
            )?;
            let calc_descent_pixels = round_signed_ratio(
                i128::from(metrics.descent),
                i128::from(device_em_pixels),
                i128::from(metrics.units_per_em),
            )?;
            let calc_portion_pixels = calc_ascent_pixels
                .checked_sub(calc_descent_pixels)
                .and_then(|value| u64::try_from(value).ok())
                .ok_or(RenderError::CoordinateOverflow)?;
            let calc_portion_height_mm100 =
                round_unsigned_ratio(calc_portion_pixels, MM100_PER_INCH, CALC_DEVICE_DPI)
                    .ok_or(RenderError::CoordinateOverflow)?;
            (
                calc_ascent_pixels,
                calc_descent_pixels,
                calc_portion_height_mm100,
            )
        } else {
            (0, 0, 0)
        };
    Ok(CombinedLineMetrics {
        ascent,
        descent,
        line_gap,
        placement_ascent: ascent,
        placement_descent: descent,
        calc_ascent_pixels,
        calc_descent_pixels,
        calc_portion_height_mm100,
    })
}

pub(super) fn combine_styled_line_metrics(
    combined: &mut Option<CombinedLineMetrics>,
    ink: CombinedLineMetrics,
    placement: CombinedLineMetrics,
) {
    let candidate = CombinedLineMetrics {
        ascent: ink.ascent,
        descent: ink.descent,
        line_gap: ink.line_gap,
        placement_ascent: placement.ascent,
        placement_descent: placement.descent,
        calc_ascent_pixels: placement.calc_ascent_pixels,
        calc_descent_pixels: placement.calc_descent_pixels,
        calc_portion_height_mm100: placement.calc_portion_height_mm100,
    };
    match combined {
        Some(combined) => {
            combined.ascent = combined.ascent.max(candidate.ascent);
            combined.descent = combined.descent.min(candidate.descent);
            combined.line_gap = combined.line_gap.max(candidate.line_gap);
            combined.placement_ascent = combined.placement_ascent.max(candidate.placement_ascent);
            combined.placement_descent =
                combined.placement_descent.min(candidate.placement_descent);
            combined.calc_ascent_pixels = combined
                .calc_ascent_pixels
                .max(candidate.calc_ascent_pixels);
            combined.calc_descent_pixels = combined
                .calc_descent_pixels
                .min(candidate.calc_descent_pixels);
            combined.calc_portion_height_mm100 = combined
                .calc_portion_height_mm100
                .max(candidate.calc_portion_height_mm100);
        }
        None => *combined = Some(candidate),
    }
}

fn replace_line_placement_metrics(
    mut ink: CombinedLineMetrics,
    placement: CombinedLineMetrics,
) -> CombinedLineMetrics {
    ink.placement_ascent = placement.ascent;
    ink.placement_descent = placement.descent;
    ink.calc_ascent_pixels = placement.calc_ascent_pixels;
    ink.calc_descent_pixels = placement.calc_descent_pixels;
    ink.calc_portion_height_mm100 = placement.calc_portion_height_mm100;
    ink
}

fn calc_selected_asian_role_face(shaped: &ShapedText, line_text: &str) -> Option<FontId> {
    let mut selected = None;
    for run in &shaped.runs {
        let source = line_text.get(run.source.clone())?;
        if !calc_script_class_summary(source).has_asian {
            continue;
        }
        if run.style_index != 0 || run.glyphs.iter().any(|glyph| glyph.glyph_id == 0) {
            return None;
        }
        if selected.is_some_and(|font_id| font_id != run.font_id) {
            return None;
        }
        selected = Some(run.font_id);
    }
    selected
}

#[allow(clippy::too_many_arguments)]
fn calc_imported_line_placement_metrics(
    pack: &FontPack,
    shaped: &ShapedText,
    line_text: &str,
    style: &ResolvedRunStyle,
    provenance: CalcImportProvenance,
    layout_policy: CellLineLayoutPolicy,
    scale_numerator: i64,
    scale_denominator: i64,
    options: &RenderOptions,
) -> Result<Option<CombinedLineMetrics>, RenderError> {
    let summary = calc_script_class_summary(line_text);
    let only_complex = calc_edit_engine_uses_only_complex_role(line_text, summary, options)?;
    let requested_font_id = pack.resolve(style.request()).id;
    let mut roles = BTreeSet::new();
    if only_complex {
        roles.insert(CalcScriptClass::Complex);
    } else {
        if summary.has_western {
            roles.insert(CalcScriptClass::Western);
        }
        if summary.has_asian {
            roles.insert(CalcScriptClass::Asian);
        }
        if summary.has_complex {
            roles.insert(CalcScriptClass::Complex);
        }
    }
    if roles.is_empty() {
        roles.insert(CalcScriptClass::Western);
    }

    let mut combined = None;
    for role in roles {
        let font_id = match role {
            CalcScriptClass::Western => requested_font_id,
            CalcScriptClass::Asian => {
                calc_selected_asian_role_face(shaped, line_text).unwrap_or(requested_font_id)
            }
            CalcScriptClass::Complex => match provenance {
                CalcImportProvenance::Ods => requested_font_id,
                CalcImportProvenance::Biff
                | CalcImportProvenance::Xlsb
                | CalcImportProvenance::Xlsx => {
                    let Some(font_id) =
                        calc_verified_complex_role_face(pack, style, requested_font_id)?
                    else {
                        return Ok(None);
                    };
                    font_id
                }
            },
        };
        let metrics = single_face_line_metrics(
            pack,
            font_id,
            style,
            layout_policy,
            scale_numerator,
            scale_denominator,
        )?;
        combine_styled_line_metrics(&mut combined, metrics, metrics);
    }
    Ok(combined)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn styled_line_metrics(
    pack: &FontPack,
    shaped: &ShapedText,
    styles: &[ResolvedRunStyle],
    layout_policy: CellLineLayoutPolicy,
    placement_policy: CalcLinePlacementPolicy,
    line_text: &str,
    scale_numerator: i64,
    scale_denominator: i64,
    options: &RenderOptions,
) -> Result<CombinedLineMetrics, RenderError> {
    let mut ink = None;
    let mut include_ink = |font_id: FontId, style: &ResolvedRunStyle| -> Result<(), RenderError> {
        let metrics = single_face_line_metrics(
            pack,
            font_id,
            style,
            layout_policy,
            scale_numerator,
            scale_denominator,
        )?;
        combine_styled_line_metrics(&mut ink, metrics, metrics);
        Ok(())
    };
    if shaped.runs.is_empty() {
        let primary = styles.first().ok_or(RenderError::Typography {
            reason: "missing_text_style",
        })?;
        include_ink(pack.resolve(primary.request()).id, primary)?;
    } else {
        for run in &shaped.runs {
            let style = styles.get(run.style_index).ok_or(RenderError::Typography {
                reason: "invalid_rich_style_index",
            })?;
            include_ink(run.font_id, style)?;
        }
    }
    let ink = ink.ok_or(RenderError::Typography {
        reason: "missing_line_metrics",
    })?;
    let placement = match placement_policy {
        CalcLinePlacementPolicy::Native => return Ok(ink),
        #[cfg(test)]
        CalcLinePlacementPolicy::RequestedFace => {
            let mut requested = None;
            if shaped.runs.is_empty() {
                let primary = styles.first().ok_or(RenderError::Typography {
                    reason: "missing_text_style",
                })?;
                let metrics = single_face_line_metrics(
                    pack,
                    pack.resolve(primary.request()).id,
                    primary,
                    layout_policy,
                    scale_numerator,
                    scale_denominator,
                )?;
                combine_styled_line_metrics(&mut requested, metrics, metrics);
            } else {
                for run in &shaped.runs {
                    let style = styles.get(run.style_index).ok_or(RenderError::Typography {
                        reason: "invalid_rich_style_index",
                    })?;
                    let metrics = single_face_line_metrics(
                        pack,
                        pack.resolve(style.request()).id,
                        style,
                        layout_policy,
                        scale_numerator,
                        scale_denominator,
                    )?;
                    combine_styled_line_metrics(&mut requested, metrics, metrics);
                }
            }
            requested
        }
        CalcLinePlacementPolicy::Imported(provenance) => {
            let Some(style) = (styles.len() == 1).then(|| &styles[0]) else {
                return Ok(ink);
            };
            calc_imported_line_placement_metrics(
                pack,
                shaped,
                line_text,
                style,
                provenance,
                layout_policy,
                scale_numerator,
                scale_denominator,
                options,
            )?
        }
    };
    Ok(placement.map_or(ink, |placement| {
        replace_line_placement_metrics(ink, placement)
    }))
}

pub(super) fn vertical_block_top(
    rect: Rect,
    block_height: Fixed,
    baseline: TextBaseline,
) -> Result<Fixed, RenderError> {
    let remaining = rect
        .height
        .checked_sub(block_height)
        .ok_or(RenderError::CoordinateOverflow)?;
    match baseline {
        TextBaseline::Top => Ok(rect.y),
        TextBaseline::Middle => rect
            .y
            .checked_add(Fixed::from_raw(remaining.raw() / 2))
            .ok_or(RenderError::CoordinateOverflow),
        TextBaseline::Bottom => rect
            .y
            .checked_add(remaining)
            .ok_or(RenderError::CoordinateOverflow),
    }
}

pub(super) fn calc_cell_text_layout_bounds(
    rect: Rect,
    baseline: TextBaseline,
    vertical_margin: Fixed,
) -> Result<Rect, RenderError> {
    // Calc applies the corresponding ATTR_MARGIN edge as a positional offset;
    // it does not resize the cell clip. Translating the backend-neutral layout
    // bounds preserves the original clip and keeps the source-derived inset
    // even when a deliberately short row cannot contain the margin. Equal top
    // and bottom defaults cancel for middle alignment, so its bounds stay byte-
    // for-byte unchanged.
    let y = match baseline {
        TextBaseline::Top => rect
            .y
            .checked_add(vertical_margin)
            .ok_or(RenderError::CoordinateOverflow)?,
        TextBaseline::Middle => rect.y,
        TextBaseline::Bottom => rect
            .y
            .checked_sub(vertical_margin)
            .ok_or(RenderError::CoordinateOverflow)?,
    };
    Ok(Rect { y, ..rect })
}

fn horizontal_line_start(
    rect: Rect,
    padding: Fixed,
    line_width: Fixed,
    anchor: TextAnchor,
) -> Result<Fixed, RenderError> {
    let right = rect
        .x
        .checked_add(rect.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    match anchor {
        TextAnchor::Start => rect
            .x
            .checked_add(padding)
            .ok_or(RenderError::CoordinateOverflow),
        TextAnchor::Middle => rect
            .x
            .checked_add(Fixed::from_raw(
                rect.width
                    .raw()
                    .checked_sub(line_width.raw())
                    .ok_or(RenderError::CoordinateOverflow)?
                    / 2,
            ))
            .ok_or(RenderError::CoordinateOverflow),
        TextAnchor::End => right
            .checked_sub(padding)
            .and_then(|value| value.checked_sub(line_width))
            .ok_or(RenderError::CoordinateOverflow),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn append_styled_shaped_outlines(
    pack: &FontPack,
    text: &str,
    line_source_start: usize,
    shaped: &ShapedText,
    line_x: Fixed,
    baseline: Fixed,
    styles: &[ResolvedRunStyle],
    scale_numerator: i64,
    scale_denominator: i64,
    options: &RenderOptions,
    stats: &mut TypographyStats,
    output: &mut Vec<PathCommand>,
    clusters: &mut Vec<GlyphCluster>,
    cluster_metrics: &mut Vec<GlyphClusterMetrics>,
    paints: &mut Vec<GlyphPaint>,
    decorations: &mut Vec<LineNode>,
    glyphs: &mut Vec<ShapedGlyph>,
    font_faces: &mut Vec<SceneFontFace>,
) -> Result<(), RenderError> {
    let mut visual_cursor = line_x;
    for run in &shaped.runs {
        let style = styles.get(run.style_index).ok_or(RenderError::Typography {
            reason: "invalid_rich_style_index",
        })?;
        let metrics = pack.metrics(run.font_id).map_err(map_font_error)?;
        let font_size = styled_font_size(style, scale_numerator, scale_denominator)?;
        let nominal_ascent = scale_font_units(
            i64::from(metrics.ascent),
            font_size,
            metrics.units_per_em,
            1,
        )?;
        let nominal_descent = scale_font_units(
            i64::from(metrics.descent),
            font_size,
            metrics.units_per_em,
            1,
        )?;
        if nominal_ascent <= Fixed::ZERO
            || nominal_descent > Fixed::ZERO
            || nominal_ascent <= nominal_descent
        {
            return Err(RenderError::Typography {
                reason: "invalid_font_metrics",
            });
        }
        let run_baseline = baseline
            .checked_add(styled_script_shift(
                style,
                scale_numerator,
                scale_denominator,
            )?)
            .ok_or(RenderError::CoordinateOverflow)?;
        let signed_advance = run.glyphs.iter().try_fold(0_i64, |sum, glyph| {
            sum.checked_add(i64::from(glyph.x_advance))
                .ok_or(RenderError::CoordinateOverflow)
        })?;
        let run_width = scale_font_units(
            i64::try_from(signed_advance.unsigned_abs())
                .map_err(|_| RenderError::CoordinateOverflow)?,
            font_size,
            metrics.units_per_em,
            1,
        )?;
        let mut pen = if signed_advance < 0 {
            visual_cursor
                .checked_add(run_width)
                .ok_or(RenderError::CoordinateOverflow)?
        } else {
            visual_cursor
        };
        let synthetic_italic =
            style.italic && !pack.is_italic(run.font_id).map_err(map_font_error)?;
        let synthetic_bold = style.bold && pack.weight(run.font_id).map_err(map_font_error)? < 600;
        let synthetic_style = synthetic_italic || synthetic_bold;
        let face_index = {
            let identity = pack
                .selected_face_identity(run.font_id)
                .map_err(map_font_error)?;
            let existing = font_faces
                .iter()
                .position(|face| face.face_sha256 == identity.face_sha256);
            match existing {
                Some(index) => index,
                None => {
                    font_faces.push(SceneFontFace {
                        family: identity.family.to_string(),
                        weight: identity.weight,
                        italic: identity.italic,
                        units_per_em: metrics.units_per_em,
                        face_sha256: identity.face_sha256.to_string(),
                    });
                    font_faces.len() - 1
                }
            }
        };
        let face_index = u32::try_from(face_index).map_err(|_| RenderError::CoordinateOverflow)?;
        let run_command_start = output.len() as u64;
        let mut logical_cluster_starts = run
            .glyphs
            .iter()
            .map(|glyph| {
                usize::try_from(glyph.cluster).map_err(|_| RenderError::Typography {
                    reason: "invalid_glyph_cluster",
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        logical_cluster_starts.sort_unstable();
        logical_cluster_starts.dedup();
        let mut logical_cluster_ends = HashMap::with_capacity(logical_cluster_starts.len());
        for pair in logical_cluster_starts.windows(2) {
            logical_cluster_ends.insert(pair[0], pair[1]);
        }
        if let Some(&last) = logical_cluster_starts.last() {
            logical_cluster_ends.insert(last, run.source.end);
        }
        let mut glyph_index = 0_usize;
        while glyph_index < run.glyphs.len() {
            let shaped_glyph_start = glyphs.len();
            let cluster_start = usize::try_from(run.glyphs[glyph_index].cluster).map_err(|_| {
                RenderError::Typography {
                    reason: "invalid_glyph_cluster",
                }
            })?;
            let mut group_end = glyph_index + 1;
            while group_end < run.glyphs.len()
                && run.glyphs[group_end].cluster == run.glyphs[glyph_index].cluster
            {
                group_end += 1;
            }
            let cluster_origin_x = pen;
            let command_start = output.len() as u64;
            for glyph in &run.glyphs[glyph_index..group_end] {
                let x_offset = scale_font_units(
                    i64::from(glyph.x_offset),
                    font_size,
                    metrics.units_per_em,
                    1,
                )?;
                let y_offset = scale_font_units(
                    i64::from(glyph.y_offset),
                    font_size,
                    metrics.units_per_em,
                    1,
                )?;
                let origin_x = pen
                    .checked_add(x_offset)
                    .ok_or(RenderError::CoordinateOverflow)?;
                let origin_y = run_baseline
                    .checked_sub(y_offset)
                    .ok_or(RenderError::CoordinateOverflow)?;
                glyphs.push(ShapedGlyph {
                    face: face_index,
                    // Filled with the actual merged-or-new cluster index once
                    // the group's command range and source range are known.
                    cluster: u32::MAX,
                    glyph_id: glyph.glyph_id,
                    origin_x,
                    origin_y,
                    size: font_size,
                    synthetic: synthetic_style,
                });
                let remaining = options
                    .limits
                    .max_path_commands
                    .saturating_sub(stats.path_commands);
                let outline = match pack.outline(run.font_id, glyph.glyph_id, remaining) {
                    Ok(outline) => outline,
                    Err(FontPackError::LimitExceeded { limit, actual, .. }) => {
                        if limit == remaining {
                            return Err(RenderError::LimitExceeded {
                                kind: LimitKind::PathCommands,
                                limit: options.limits.max_path_commands,
                                actual: stats.path_commands.saturating_add(actual),
                            });
                        }
                        return Err(RenderError::Typography {
                            reason: "glyph_outline_complexity",
                        });
                    }
                    Err(error) => return Err(map_font_error(error)),
                };
                let outline_multiplier = if synthetic_bold { 2_u64 } else { 1_u64 };
                let outline_commands = (outline.len() as u64)
                    .checked_mul(outline_multiplier)
                    .ok_or(RenderError::CoordinateOverflow)?;
                stats.path_commands = stats
                    .path_commands
                    .checked_add(outline_commands)
                    .ok_or(RenderError::CoordinateOverflow)?;
                enforce(
                    LimitKind::PathCommands,
                    options.limits.max_path_commands,
                    stats.path_commands,
                )?;
                let bold_offset = scale_ratio(font_size, 1, 32)?.max(Fixed::from_raw(1));
                for copy in 0..outline_multiplier {
                    let copy_origin_x = if copy == 0 {
                        origin_x
                    } else {
                        origin_x
                            .checked_add(bold_offset)
                            .ok_or(RenderError::CoordinateOverflow)?
                    };
                    for command in &outline {
                        output.push(transform_outline_command(
                            *command,
                            copy_origin_x,
                            origin_y,
                            font_size,
                            metrics.units_per_em,
                            synthetic_italic,
                        )?);
                    }
                }
                let advance = scale_font_units(
                    i64::from(glyph.x_advance),
                    font_size,
                    metrics.units_per_em,
                    1,
                )?;
                pen = pen
                    .checked_add(advance)
                    .ok_or(RenderError::CoordinateOverflow)?;
            }
            let metrics = GlyphClusterMetrics {
                origin_x: cluster_origin_x,
                advance_x: pen
                    .checked_sub(cluster_origin_x)
                    .ok_or(RenderError::CoordinateOverflow)?,
                baseline_y: run_baseline,
                ascent: nominal_ascent,
                descent: nominal_descent,
            };
            let cluster_end = logical_cluster_ends.get(&cluster_start).copied().ok_or(
                RenderError::Typography {
                    reason: "invalid_glyph_cluster",
                },
            )?;
            let source_start = line_source_start
                .checked_add(cluster_start)
                .ok_or(RenderError::CoordinateOverflow)?;
            let source_end = line_source_start
                .checked_add(cluster_end)
                .ok_or(RenderError::CoordinateOverflow)?;
            if cluster_start < run.source.start
                || cluster_end > run.source.end
                || cluster_start >= cluster_end
                || source_end > text.len()
                || !text.is_char_boundary(source_start)
                || !text.is_char_boundary(source_end)
            {
                return Err(RenderError::Typography {
                    reason: "invalid_glyph_cluster",
                });
            }
            let command_end = output.len() as u64;
            let cluster_index = if let Some(previous) = clusters.last_mut() {
                if previous.source_start == source_start as u64
                    && previous.source_end == source_end as u64
                    && previous.command_end == command_start
                {
                    previous.command_end = command_end;
                    let previous_metrics =
                        cluster_metrics.last_mut().ok_or(RenderError::Typography {
                            reason: "invalid_glyph_metadata",
                        })?;
                    merge_cluster_metrics(previous_metrics, metrics)?;
                    clusters.len() - 1
                } else {
                    let index = clusters.len();
                    clusters.push(GlyphCluster {
                        source_start: source_start as u64,
                        source_end: source_end as u64,
                        command_start,
                        command_end,
                    });
                    cluster_metrics.push(metrics);
                    index
                }
            } else {
                clusters.push(GlyphCluster {
                    source_start: source_start as u64,
                    source_end: source_end as u64,
                    command_start,
                    command_end,
                });
                cluster_metrics.push(metrics);
                0
            };
            let cluster_index =
                u32::try_from(cluster_index).map_err(|_| RenderError::CoordinateOverflow)?;
            for glyph in &mut glyphs[shaped_glyph_start..] {
                glyph.cluster = cluster_index;
            }
            glyph_index = group_end;
        }
        let run_command_end = output.len() as u64;
        if run_command_start != run_command_end {
            if let Some(previous) = paints.last_mut() {
                if previous.color == style.color && previous.command_end == run_command_start {
                    previous.command_end = run_command_end;
                } else {
                    paints.push(GlyphPaint {
                        command_start: run_command_start,
                        command_end: run_command_end,
                        color: style.color,
                    });
                }
            } else {
                paints.push(GlyphPaint {
                    command_start: run_command_start,
                    command_end: run_command_end,
                    color: style.color,
                });
            }
        }
        append_decorations(
            pack,
            run.font_id,
            visual_cursor,
            run_baseline,
            run_width,
            font_size,
            style.color,
            style.underline,
            style.strikethrough,
            decorations,
        )?;
        visual_cursor = visual_cursor
            .checked_add(run_width)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(())
}

fn merge_cluster_metrics(
    previous: &mut GlyphClusterMetrics,
    current: GlyphClusterMetrics,
) -> Result<(), RenderError> {
    let previous_end = previous
        .origin_x
        .checked_add(previous.advance_x)
        .ok_or(RenderError::CoordinateOverflow)?;
    let current_end = current
        .origin_x
        .checked_add(current.advance_x)
        .ok_or(RenderError::CoordinateOverflow)?;
    let left = Fixed::from_raw(
        previous
            .origin_x
            .raw()
            .min(previous_end.raw())
            .min(current.origin_x.raw())
            .min(current_end.raw()),
    );
    let right = Fixed::from_raw(
        previous
            .origin_x
            .raw()
            .max(previous_end.raw())
            .max(current.origin_x.raw())
            .max(current_end.raw()),
    );
    let previous_top = previous
        .baseline_y
        .checked_sub(previous.ascent)
        .ok_or(RenderError::CoordinateOverflow)?;
    let current_top = current
        .baseline_y
        .checked_sub(current.ascent)
        .ok_or(RenderError::CoordinateOverflow)?;
    let previous_bottom = previous
        .baseline_y
        .checked_sub(previous.descent)
        .ok_or(RenderError::CoordinateOverflow)?;
    let current_bottom = current
        .baseline_y
        .checked_sub(current.descent)
        .ok_or(RenderError::CoordinateOverflow)?;
    let top = Fixed::from_raw(previous_top.raw().min(current_top.raw()));
    let bottom = Fixed::from_raw(previous_bottom.raw().max(current_bottom.raw()));
    previous.origin_x = left;
    previous.advance_x = right
        .checked_sub(left)
        .ok_or(RenderError::CoordinateOverflow)?;
    previous.ascent = previous
        .baseline_y
        .checked_sub(top)
        .ok_or(RenderError::CoordinateOverflow)?;
    previous.descent = previous
        .baseline_y
        .checked_sub(bottom)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(())
}

fn transform_outline_command(
    command: FontOutlineCommand,
    origin_x: Fixed,
    origin_y: Fixed,
    font_size: Fixed,
    units_per_em: u16,
    synthetic_italic: bool,
) -> Result<PathCommand, RenderError> {
    let point = |x: i32, y: i32| {
        outline_point(
            x,
            y,
            origin_x,
            origin_y,
            font_size,
            units_per_em,
            synthetic_italic,
        )
    };
    Ok(match command {
        FontOutlineCommand::MoveTo(x, y) => {
            let (x, y) = point(x, y)?;
            PathCommand::MoveTo { x, y }
        }
        FontOutlineCommand::LineTo(x, y) => {
            let (x, y) = point(x, y)?;
            PathCommand::LineTo { x, y }
        }
        FontOutlineCommand::QuadraticTo(x1, y1, x, y) => {
            let (control_x, control_y) = point(x1, y1)?;
            let (x, y) = point(x, y)?;
            PathCommand::QuadraticTo {
                control_x,
                control_y,
                x,
                y,
            }
        }
        FontOutlineCommand::CubicTo(x1, y1, x2, y2, x, y) => {
            let (control1_x, control1_y) = point(x1, y1)?;
            let (control2_x, control2_y) = point(x2, y2)?;
            let (x, y) = point(x, y)?;
            PathCommand::CubicTo {
                control1_x,
                control1_y,
                control2_x,
                control2_y,
                x,
                y,
            }
        }
        FontOutlineCommand::Close => PathCommand::Close,
    })
}

#[allow(clippy::too_many_arguments)]
fn outline_point(
    x: i32,
    y: i32,
    origin_x: Fixed,
    origin_y: Fixed,
    font_size: Fixed,
    units_per_em: u16,
    synthetic_italic: bool,
) -> Result<(Fixed, Fixed), RenderError> {
    let mut x = i64::from(x);
    let y = i64::from(y);
    if synthetic_italic {
        x = x
            .checked_add(y / 5)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    let x = origin_x
        .checked_add(scale_font_units(
            x,
            font_size,
            units_per_em,
            FONT_OUTLINE_UNITS,
        )?)
        .ok_or(RenderError::CoordinateOverflow)?;
    let y = origin_y
        .checked_sub(scale_font_units(
            y,
            font_size,
            units_per_em,
            FONT_OUTLINE_UNITS,
        )?)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok((x, y))
}

#[allow(clippy::too_many_arguments)]
fn append_decorations(
    pack: &FontPack,
    font_id: crate::font::FontId,
    x: Fixed,
    baseline: Fixed,
    width: Fixed,
    font_size: Fixed,
    color: Rgb,
    underline: bool,
    strikethrough: bool,
    output: &mut Vec<LineNode>,
) -> Result<(), RenderError> {
    if width.raw() <= 0 || (!underline && !strikethrough) {
        return Ok(());
    }
    let metrics = pack.metrics(font_id).map_err(map_font_error)?;
    let x2 = x
        .checked_add(width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let mut push_metric = |position: i16, thickness: i16| -> Result<(), RenderError> {
        let y = baseline
            .checked_sub(scale_font_units(
                i64::from(position),
                font_size,
                metrics.units_per_em,
                1,
            )?)
            .ok_or(RenderError::CoordinateOverflow)?;
        let width = scale_font_units(
            i64::from(thickness).unsigned_abs() as i64,
            font_size,
            metrics.units_per_em,
            1,
        )?
        .max(Fixed::from_raw(1));
        output.push(LineNode {
            x1: x,
            y1: y,
            x2,
            y2: y,
            color,
            width,
        });
        Ok(())
    };
    if underline {
        push_metric(metrics.underline_position, metrics.underline_thickness)?;
    }
    if strikethrough {
        push_metric(metrics.strikeout_position, metrics.strikeout_thickness)?;
    }
    Ok(())
}

fn rotation_pivot(
    rect: Rect,
    padding: Fixed,
    style: &TextStyle,
) -> Result<(Fixed, Fixed), RenderError> {
    let right = rect
        .x
        .checked_add(rect.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let x = match style.anchor {
        TextAnchor::Start => rect.x.checked_add(padding),
        TextAnchor::Middle => rect.x.checked_add(Fixed::from_raw(rect.width.raw() / 2)),
        TextAnchor::End => right.checked_sub(padding),
    }
    .ok_or(RenderError::CoordinateOverflow)?;
    let y = match style.baseline {
        TextBaseline::Top => rect.y,
        TextBaseline::Middle => rect
            .y
            .checked_add(Fixed::from_raw(rect.height.raw() / 2))
            .ok_or(RenderError::CoordinateOverflow)?,
        TextBaseline::Bottom => bottom,
    };
    Ok((x, y))
}

pub(super) fn scale_font_units(
    value: i64,
    font_size: Fixed,
    units_per_em: u16,
    coordinate_scale: i64,
) -> Result<Fixed, RenderError> {
    let denominator = i64::from(units_per_em)
        .checked_mul(coordinate_scale)
        .ok_or(RenderError::CoordinateOverflow)?;
    scale_ratio(Fixed::from_raw(value), font_size.raw(), denominator)
}

pub(super) fn scale_ratio(
    value: Fixed,
    numerator: i64,
    denominator: i64,
) -> Result<Fixed, RenderError> {
    if denominator <= 0 {
        return Err(RenderError::Typography {
            reason: "invalid_scale_denominator",
        });
    }
    let product = i128::from(value.raw())
        .checked_mul(i128::from(numerator))
        .ok_or(RenderError::CoordinateOverflow)?;
    let divisor = i128::from(denominator);
    let rounded = if product >= 0 {
        product
            .checked_add(divisor / 2)
            .ok_or(RenderError::CoordinateOverflow)?
            / divisor
    } else {
        product
            .checked_sub(divisor / 2)
            .ok_or(RenderError::CoordinateOverflow)?
            / divisor
    };
    let raw = i64::try_from(rounded).map_err(|_| RenderError::CoordinateOverflow)?;
    Ok(Fixed::from_raw(raw))
}

pub(super) fn multiply_fixed(value: Fixed, multiplier: i64) -> Result<Fixed, RenderError> {
    value
        .raw()
        .checked_mul(multiplier)
        .map(Fixed::from_raw)
        .ok_or(RenderError::CoordinateOverflow)
}

fn negate_fixed(value: Fixed) -> Result<Fixed, RenderError> {
    value
        .raw()
        .checked_neg()
        .map(Fixed::from_raw)
        .ok_or(RenderError::CoordinateOverflow)
}

pub(super) fn map_font_error(error: FontPackError) -> RenderError {
    match error {
        FontPackError::LimitExceeded {
            resource,
            limit,
            actual,
        } => {
            let kind = match resource {
                "shape_glyphs" => LimitKind::Glyphs,
                "shape_runs" => LimitKind::TextRuns,
                "outline_commands" => LimitKind::PathCommands,
                _ => return RenderError::Typography { reason: resource },
            };
            RenderError::LimitExceeded {
                kind,
                limit,
                actual,
            }
        }
        FontPackError::InvalidTextRange => RenderError::Typography {
            reason: "invalid_text_range",
        },
        FontPackError::InvalidFont => RenderError::Typography {
            reason: "invalid_verified_font",
        },
        FontPackError::Io { .. }
        | FontPackError::InvalidManifest { .. }
        | FontPackError::UnsafePath
        | FontPackError::UnexpectedFile
        | FontPackError::MissingMember
        | FontPackError::SizeMismatch
        | FontPackError::DigestMismatch => RenderError::Typography {
            reason: "font_pack_state",
        },
    }
}
