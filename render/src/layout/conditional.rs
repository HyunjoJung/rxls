//! Bounded conditional-rule evaluation, text-layout overlays, and cell paint resolution.

use std::collections::{BTreeMap, BTreeSet};

use rxls::{Cell, CellStyle, CfRule, Color, DisplayCell, DvOp, Font, Sheet, StyleLossKind};

use crate::error::{LimitKind, RenderError};
use crate::scene::{Fixed, Rect, RectNode, Rgb, SceneNode};
use crate::typography::CellLineLayoutPolicy;

use super::geometry::verified_implicit_ooxml;
use super::text::multiply_fixed;
use super::{
    calc_cell_vertical_margin, enforce, parse_a1_reference, push_node, rgb, A1Reference,
    CalcLinePlacementPolicy, CellCoordinate, ConditionalOutcome, ConditionalPaint, DataBarPaint,
    Region, RenderOptions, RenderStyleSnapshot, SparseDisplayCellIndex, WarningCode, Warnings,
    MAX_WORKSHEET_COLUMN, MAX_WORKSHEET_ROW,
};

pub(super) fn has_conditional_text_layout_overlay(sheet: &Sheet) -> bool {
    sheet.conditional_format_metadata().iter().any(|metadata| {
        let has_unsafe_losses = metadata
            .style_losses
            .iter()
            .any(|loss| loss.kind != StyleLossKind::UnresolvedColor);
        let unresolved_color_without_retained_font = metadata
            .style_losses
            .iter()
            .any(|loss| loss.kind == StyleLossKind::UnresolvedColor)
            && metadata
                .differential_style
                .as_ref()
                .is_none_or(|style| style.font.is_none());
        metadata.differential_style.as_ref().map_or(
            has_unsafe_losses || unresolved_color_without_retained_font,
            |style| {
                unresolved_color_without_retained_font
                    || conditional_style_affects_text_layout(style, has_unsafe_losses)
            },
        )
    })
}

pub(super) fn calc_line_layout_available(sheet: &Sheet, options: &RenderOptions) -> bool {
    verified_implicit_ooxml(sheet, options) && !has_conditional_text_layout_overlay(sheet)
}

pub(super) fn conditional_style_is_geometry_safe_color_only(
    style: &CellStyle,
    has_unsafe_losses: bool,
) -> bool {
    if has_unsafe_losses
        || style.align.is_some()
        || style.num_fmt.is_some()
        || style.protection.is_some()
    {
        return false;
    }
    let Some(font) = style.font.as_ref() else {
        return false;
    };
    let mut font_without_color = font.clone();
    if font_without_color.color.take().is_none() || font_without_color != Font::default() {
        return false;
    }
    true
}

pub(super) fn conditional_style_affects_text_layout(
    style: &CellStyle,
    has_unsafe_losses: bool,
) -> bool {
    has_unsafe_losses
        || style.align.is_some()
        || style.num_fmt.is_some()
        || (style.font.is_some()
            && !conditional_style_is_geometry_safe_color_only(style, has_unsafe_losses))
}

fn conditional_metadata_requires_text_measurement(
    metadata: Option<&rxls::ConditionalFormatMetadata>,
) -> bool {
    let Some(metadata) = metadata else {
        return false;
    };
    let has_unsafe_losses = metadata
        .style_losses
        .iter()
        .any(|loss| loss.kind != StyleLossKind::UnresolvedColor);
    let unresolved_color_without_retained_font = metadata
        .style_losses
        .iter()
        .any(|loss| loss.kind == StyleLossKind::UnresolvedColor)
        && metadata
            .differential_style
            .as_ref()
            .is_none_or(|style| style.font.is_none());
    has_unsafe_losses
        || unresolved_color_without_retained_font
        || metadata.differential_style.as_ref().is_some_and(|style| {
            style.font.is_some() || style.align.is_some() || style.num_fmt.is_some()
        })
}

fn conditional_metadata_text_measurement_is_unresolved(
    metadata: Option<&rxls::ConditionalFormatMetadata>,
) -> bool {
    let Some(metadata) = metadata else {
        return false;
    };
    metadata
        .style_losses
        .iter()
        .any(|loss| loss.kind != StyleLossKind::UnresolvedColor)
        || (metadata
            .style_losses
            .iter()
            .any(|loss| loss.kind == StyleLossKind::UnresolvedColor)
            && metadata
                .differential_style
                .as_ref()
                .is_none_or(|style| style.font.is_none()))
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ColorOnlyConditionalActivation {
    Known(BTreeSet<CellCoordinate>),
    Unknown,
}

#[cfg(test)]
impl ColorOnlyConditionalActivation {
    pub(super) fn requires_individual_metrics(&self, source: CellCoordinate) -> bool {
        match self {
            Self::Known(active) => active.contains(&source),
            Self::Unknown => true,
        }
    }
}

#[cfg(test)]
pub(super) fn active_color_only_conditional_cells(
    sheet: &Sheet,
    candidates: &[DisplayCell<'_>],
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<ColorOnlyConditionalActivation, RenderError> {
    let mut relevant_rule = None;
    for (index, metadata) in sheet.conditional_format_metadata().iter().enumerate() {
        if conditional_metadata_text_measurement_is_unresolved(Some(metadata)) {
            return Ok(ColorOnlyConditionalActivation::Unknown);
        }
        let Some(style) = metadata.differential_style.as_ref() else {
            continue;
        };
        let has_unsafe_losses = metadata
            .style_losses
            .iter()
            .any(|loss| loss.kind != StyleLossKind::UnresolvedColor);
        if !conditional_style_is_geometry_safe_color_only(style, has_unsafe_losses) {
            continue;
        }
        if relevant_rule.replace(index).is_some() {
            return Ok(ColorOnlyConditionalActivation::Unknown);
        }
    }
    let Some(rule_index) = relevant_rule else {
        return Ok(ColorOnlyConditionalActivation::Known(BTreeSet::new()));
    };
    let Some(conditional) = sheet.conditional_formats().get(rule_index) else {
        return Ok(ColorOnlyConditionalActivation::Unknown);
    };
    let CfRule::CellIs {
        op,
        formula1,
        formula2,
        ..
    } = &conditional.rule
    else {
        return Ok(ColorOnlyConditionalActivation::Unknown);
    };
    let (first_row, first_col, last_row, last_col) = conditional.sqref;
    if first_row > last_row || first_col > last_col {
        return Ok(ColorOnlyConditionalActivation::Unknown);
    }
    let Some(first) = parse_conditional_operand(formula1) else {
        warnings.add(WarningCode::ConditionalFormattingDeferred, None);
        return Ok(ColorOnlyConditionalActivation::Unknown);
    };
    let second = match op {
        DvOp::Between | DvOp::NotBetween => {
            let Some(second) = formula2.as_deref().and_then(parse_conditional_operand) else {
                warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                return Ok(ColorOnlyConditionalActivation::Unknown);
            };
            Some(second)
        }
        _ => None,
    };
    let mut evaluations = 0_u64;
    let mut active = BTreeSet::new();
    for &cell in candidates {
        bump_conditional_evaluations(&mut evaluations, options)?;
        let source = CellCoordinate {
            row: cell.row,
            col: cell.col,
        };
        if !coordinate_in_range(source, conditional.sqref) {
            continue;
        }
        let Some(value) = numeric_cell_value(cell.value) else {
            continue;
        };
        let Some(first) = resolve_conditional_operand(&first, sheet, source, conditional.sqref)
        else {
            warnings.add(WarningCode::ConditionalFormattingDeferred, None);
            return Ok(ColorOnlyConditionalActivation::Unknown);
        };
        let second = match second.as_ref() {
            Some(second) => {
                let Some(value) =
                    resolve_conditional_operand(second, sheet, source, conditional.sqref)
                else {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    return Ok(ColorOnlyConditionalActivation::Unknown);
                };
                Some(value)
            }
            None => None,
        };
        if compare_conditional(value, *op, first, second) {
            active.insert(source);
        }
    }
    Ok(ColorOnlyConditionalActivation::Known(active))
}

#[derive(Debug, Clone)]
pub(super) struct ConditionalLayoutCell {
    pub(super) effective_style: Option<CellStyle>,
    pub(super) active_style: Option<CellStyle>,
}

pub(super) fn resolve_conditional_layout_cells(
    sheet: &Sheet,
    candidates: &[DisplayCell<'_>],
    style_snapshot: &RenderStyleSnapshot,
    options: &RenderOptions,
    evaluations: &mut u64,
) -> Result<BTreeMap<CellCoordinate, ConditionalLayoutCell>, RenderError> {
    if sheet.conditional_formats().is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut display_cells = BTreeMap::new();
    for &cell in candidates {
        if cell.row > MAX_WORKSHEET_ROW || cell.col > MAX_WORKSHEET_COLUMN {
            continue;
        }
        display_cells.insert(
            CellCoordinate {
                row: cell.row,
                col: cell.col,
            },
            cell,
        );
    }
    let mut regions = display_cells
        .keys()
        .copied()
        .map(|source| Region {
            source,
            rect: Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_raw(1),
                height: Fixed::from_raw(1),
            },
            is_merged: false,
            line_layout_policy: CellLineLayoutPolicy::Native,
            line_placement_policy: CalcLinePlacementPolicy::Native,
            calc_wrap_space: None,
            style: style_snapshot
                .owned_style(source)
                .or_else(|| sheet.resolved_cell_style(source.row, source.col)),
            conditional: ConditionalPaint::default(),
            text: String::new(),
            rich_text: None,
            hyperlink: None,
            numeric_default: false,
            text_can_overflow: false,
            fixed_height_row: false,
            ods_fixed_height_row: false,
            print_vertical_overflow: false,
            vertical_margin: calc_cell_vertical_margin(sheet),
        })
        .collect::<Vec<_>>();
    let mut measurement_warnings = Warnings::default();
    let deferred = resolve_conditional_paints(
        sheet,
        &display_cells,
        &mut regions,
        options,
        &mut measurement_warnings,
        evaluations,
        true,
    )?;
    if deferred {
        return Err(RenderError::Typography {
            reason: "conditional_text_layout_unresolved",
        });
    }
    let mut resolved = BTreeMap::new();
    for region in regions {
        if region
            .conditional
            .style
            .as_ref()
            .is_some_and(|style| style.num_fmt.is_some())
        {
            return Err(RenderError::Typography {
                reason: "conditional_number_format_layout_unresolved",
            });
        }
        resolved.insert(
            region.source,
            ConditionalLayoutCell {
                effective_style: region.style,
                active_style: region.conditional.style,
            },
        );
    }
    Ok(resolved)
}

pub(super) fn resolve_conditional_paints(
    sheet: &Sheet,
    display_cells: &BTreeMap<CellCoordinate, DisplayCell<'_>>,
    regions: &mut [Region],
    options: &RenderOptions,
    warnings: &mut Warnings,
    evaluations: &mut u64,
    retain_layout_fields: bool,
) -> Result<bool, RenderError> {
    let mut paints = BTreeMap::<CellCoordinate, ConditionalPaint>::new();
    let mut stopped = BTreeSet::<CellCoordinate>::new();
    let mut deferred_text_layout = false;
    let metadata = sheet.conditional_format_metadata();
    let mut rule_order = (0..sheet.conditional_formats().len()).collect::<Vec<_>>();
    rule_order.sort_by_key(|&index| {
        let authored_priority = u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1);
        (
            metadata
                .get(index)
                .and_then(|metadata| metadata.priority)
                .unwrap_or(authored_priority),
            index,
        )
    });
    for rule_index in rule_order {
        let conditional = &sheet.conditional_formats()[rule_index];
        let rule_metadata = metadata.get(rule_index);
        let text_measurement_relevant =
            conditional_metadata_requires_text_measurement(rule_metadata);
        let text_measurement_unresolved =
            conditional_metadata_text_measurement_is_unresolved(rule_metadata);
        let stop_if_true = rule_metadata.is_some_and(|metadata| metadata.stop_if_true);
        if let Some(metadata) = rule_metadata {
            for loss in &metadata.style_losses {
                warnings.add_count(
                    WarningCode::ConditionalFormattingDeferred,
                    u64::from(loss.occurrences),
                    None,
                );
            }
        }
        let differential_style = rule_metadata
            .and_then(|metadata| metadata.differential_style.as_ref())
            .cloned()
            .map(|mut style| {
                if style.num_fmt.is_some() {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    if !retain_layout_fields {
                        style.num_fmt = None;
                    }
                }
                if style.protection.is_some() {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    if !retain_layout_fields {
                        style.protection = None;
                    }
                }
                style
            });
        let has_imported_differential =
            rule_metadata.is_some_and(|metadata| metadata.differential_style.is_some());
        let range = conditional.sqref;
        let measurement_range_intersects = text_measurement_relevant
            && regions
                .iter()
                .any(|region| coordinate_in_range(region.source, range));
        if range.0 > range.2 || range.1 > range.3 {
            warnings.add(WarningCode::ConditionalFormattingDeferred, None);
            deferred_text_layout |= measurement_range_intersects;
            continue;
        }
        match &conditional.rule {
            CfRule::CellIs {
                op,
                formula1,
                formula2,
                fill,
            } => {
                let Some(first) = parse_conditional_operand(formula1) else {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    deferred_text_layout |= measurement_range_intersects;
                    continue;
                };
                let second = match op {
                    DvOp::Between | DvOp::NotBetween => {
                        let Some(second) = formula2.as_deref().and_then(parse_conditional_operand)
                        else {
                            warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                            deferred_text_layout |= measurement_range_intersects;
                            continue;
                        };
                        Some(second)
                    }
                    _ => None,
                };
                let mut matches = Vec::new();
                let mut deferred = false;
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(value) = display_cells
                        .get(&region.source)
                        .and_then(|cell| numeric_cell_value(cell.value))
                    else {
                        continue;
                    };
                    let Some(first) =
                        resolve_conditional_operand(&first, sheet, region.source, range)
                    else {
                        deferred = true;
                        break;
                    };
                    let second = match second.as_ref() {
                        Some(second) => {
                            let Some(value) =
                                resolve_conditional_operand(second, sheet, region.source, range)
                            else {
                                deferred = true;
                                break;
                            };
                            Some(value)
                        }
                        None => None,
                    };
                    if compare_conditional(value, *op, first, second) {
                        matches.push(region.source);
                    }
                }
                if deferred {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    deferred_text_layout |= measurement_range_intersects;
                    continue;
                }
                for coordinate in matches {
                    apply_conditional_paint(
                        &mut paints,
                        &mut stopped,
                        coordinate,
                        ConditionalOutcome {
                            style: Some(conditional_fill_overlay(
                                rgb(*fill),
                                differential_style.as_ref(),
                                has_imported_differential,
                            )),
                            data_bar: None,
                            stop_if_true,
                            text_measurement_unresolved,
                        },
                        &mut deferred_text_layout,
                    );
                }
            }
            CfRule::ColorScale2 { min, max } => {
                let values = conditional_numeric_values(sheet, range, evaluations, options)?;
                let Some((minimum, maximum)) = numeric_bounds(&values) else {
                    continue;
                };
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(value) = display_cells
                        .get(&region.source)
                        .and_then(|cell| numeric_cell_value(cell.value))
                    else {
                        continue;
                    };
                    let ratio = normalized_ppm(value, minimum, maximum);
                    apply_conditional_paint(
                        &mut paints,
                        &mut stopped,
                        region.source,
                        ConditionalOutcome {
                            style: Some(conditional_fill_overlay(
                                interpolate_rgb(rgb(*min), rgb(*max), ratio),
                                differential_style.as_ref(),
                                has_imported_differential,
                            )),
                            data_bar: None,
                            stop_if_true,
                            text_measurement_unresolved,
                        },
                        &mut deferred_text_layout,
                    );
                }
            }
            CfRule::ColorScale3 { min, mid, max } => {
                let mut values = conditional_numeric_values(sheet, range, evaluations, options)?;
                if values.is_empty() {
                    continue;
                }
                values.sort_by(f64::total_cmp);
                let minimum = values[0];
                let maximum = values[values.len() - 1];
                let midpoint = percentile_50(&values);
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(value) = display_cells
                        .get(&region.source)
                        .and_then(|cell| numeric_cell_value(cell.value))
                    else {
                        continue;
                    };
                    let color = if value <= midpoint {
                        interpolate_rgb(
                            rgb(*min),
                            rgb(*mid),
                            normalized_ppm(value, minimum, midpoint),
                        )
                    } else {
                        interpolate_rgb(
                            rgb(*mid),
                            rgb(*max),
                            normalized_ppm(value, midpoint, maximum),
                        )
                    };
                    apply_conditional_paint(
                        &mut paints,
                        &mut stopped,
                        region.source,
                        ConditionalOutcome {
                            style: Some(conditional_fill_overlay(
                                color,
                                differential_style.as_ref(),
                                has_imported_differential,
                            )),
                            data_bar: None,
                            stop_if_true,
                            text_measurement_unresolved,
                        },
                        &mut deferred_text_layout,
                    );
                }
            }
            CfRule::DataBar { color } => {
                let values = conditional_numeric_values(sheet, range, evaluations, options)?;
                let Some((minimum, maximum)) = numeric_bounds(&values) else {
                    continue;
                };
                warnings.add(WarningCode::ConditionalDataBarSimplified, None);
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(value) = display_cells
                        .get(&region.source)
                        .and_then(|cell| numeric_cell_value(cell.value))
                    else {
                        continue;
                    };
                    apply_conditional_paint(
                        &mut paints,
                        &mut stopped,
                        region.source,
                        ConditionalOutcome {
                            style: differential_style.clone(),
                            data_bar: Some(DataBarPaint {
                                color: rgb(*color),
                                width_ppm: normalized_ppm(value, minimum, maximum),
                            }),
                            stop_if_true,
                            text_measurement_unresolved,
                        },
                        &mut deferred_text_layout,
                    );
                }
            }
            CfRule::TopBottom {
                rank,
                bottom,
                percent,
                fill,
            } => {
                let mut values = conditional_numeric_values(sheet, range, evaluations, options)?;
                if values.is_empty() || *rank == 0 {
                    continue;
                }
                values.sort_by(f64::total_cmp);
                let selected = if *percent {
                    let percentage = u64::from((*rank).min(100));
                    ((values.len() as u64)
                        .checked_mul(percentage)
                        .ok_or(RenderError::CoordinateOverflow)?
                        .saturating_add(99)
                        / 100) as usize
                } else {
                    usize::try_from(*rank).unwrap_or(usize::MAX)
                }
                .max(1)
                .min(values.len());
                let threshold = if *bottom {
                    values[selected - 1]
                } else {
                    values[values.len() - selected]
                };
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(value) = display_cells
                        .get(&region.source)
                        .and_then(|cell| numeric_cell_value(cell.value))
                    else {
                        continue;
                    };
                    if (*bottom && value <= threshold) || (!*bottom && value >= threshold) {
                        apply_conditional_paint(
                            &mut paints,
                            &mut stopped,
                            region.source,
                            ConditionalOutcome {
                                style: Some(conditional_fill_overlay(
                                    rgb(*fill),
                                    differential_style.as_ref(),
                                    has_imported_differential,
                                )),
                                data_bar: None,
                                stop_if_true,
                                text_measurement_unresolved,
                            },
                            &mut deferred_text_layout,
                        );
                    }
                }
            }
            CfRule::AboveAverage { below, fill } => {
                let values = conditional_numeric_values(sheet, range, evaluations, options)?;
                if values.is_empty() {
                    continue;
                }
                let sum = values.iter().try_fold(0.0_f64, |sum, value| {
                    let next = sum + value;
                    next.is_finite().then_some(next)
                });
                let Some(sum) = sum else {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    deferred_text_layout |= measurement_range_intersects;
                    continue;
                };
                let average = sum / values.len() as f64;
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(value) = display_cells
                        .get(&region.source)
                        .and_then(|cell| numeric_cell_value(cell.value))
                    else {
                        continue;
                    };
                    if (*below && value < average) || (!*below && value > average) {
                        apply_conditional_paint(
                            &mut paints,
                            &mut stopped,
                            region.source,
                            ConditionalOutcome {
                                style: Some(conditional_fill_overlay(
                                    rgb(*fill),
                                    differential_style.as_ref(),
                                    has_imported_differential,
                                )),
                                data_bar: None,
                                stop_if_true,
                                text_measurement_unresolved,
                            },
                            &mut deferred_text_layout,
                        );
                    }
                }
            }
            CfRule::DuplicateValues { unique, fill } => {
                let Some(keys) = conditional_value_keys(sheet, range, evaluations, options)? else {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    deferred_text_layout |= measurement_range_intersects;
                    continue;
                };
                let mut counts = BTreeMap::<ConditionalValueKey, u64>::new();
                for key in keys.values() {
                    let count = counts.entry(key.clone()).or_default();
                    *count = count
                        .checked_add(1)
                        .ok_or(RenderError::CoordinateOverflow)?;
                }
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(key) = keys.get(&region.source) else {
                        continue;
                    };
                    let count = counts.get(key).copied().unwrap_or(0);
                    if (*unique && count == 1) || (!*unique && count > 1) {
                        apply_conditional_paint(
                            &mut paints,
                            &mut stopped,
                            region.source,
                            ConditionalOutcome {
                                style: Some(conditional_fill_overlay(
                                    rgb(*fill),
                                    differential_style.as_ref(),
                                    has_imported_differential,
                                )),
                                data_bar: None,
                                stop_if_true,
                                text_measurement_unresolved,
                            },
                            &mut deferred_text_layout,
                        );
                    }
                }
            }
            CfRule::Expression { formula, fill } => {
                let Some(expression) = parse_conditional_expression(formula) else {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    deferred_text_layout |= measurement_range_intersects;
                    continue;
                };
                let mut matches = Vec::new();
                let mut deferred = false;
                for region in regions.iter() {
                    if stopped.contains(&region.source) {
                        continue;
                    }
                    bump_conditional_evaluations(evaluations, options)?;
                    if !coordinate_in_range(region.source, range) {
                        continue;
                    }
                    let Some(left) =
                        resolve_conditional_operand(&expression.left, sheet, region.source, range)
                    else {
                        deferred = true;
                        break;
                    };
                    let Some(right) =
                        resolve_conditional_operand(&expression.right, sheet, region.source, range)
                    else {
                        deferred = true;
                        break;
                    };
                    if expression.op.compare(left, right) {
                        matches.push(region.source);
                    }
                }
                if deferred {
                    warnings.add(WarningCode::ConditionalFormattingDeferred, None);
                    deferred_text_layout |= measurement_range_intersects;
                    continue;
                }
                for coordinate in matches {
                    apply_conditional_paint(
                        &mut paints,
                        &mut stopped,
                        coordinate,
                        ConditionalOutcome {
                            style: Some(conditional_fill_overlay(
                                rgb(*fill),
                                differential_style.as_ref(),
                                has_imported_differential,
                            )),
                            data_bar: None,
                            stop_if_true,
                            text_measurement_unresolved,
                        },
                        &mut deferred_text_layout,
                    );
                }
            }
        }
    }
    for region in regions {
        let Some(paint) = paints.remove(&region.source) else {
            continue;
        };
        if let Some(overlay) = paint.style.as_ref() {
            region.style = Some(match region.style.take() {
                Some(base) => base.merge(overlay),
                None => overlay.clone(),
            });
        }
        region.conditional = paint;
    }
    Ok(deferred_text_layout)
}

fn bump_conditional_evaluations(
    evaluations: &mut u64,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    *evaluations = evaluations
        .checked_add(1)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::ConditionalEvaluations,
        options.limits.max_conditional_evaluations,
        *evaluations,
    )
}

fn conditional_numeric_values(
    sheet: &Sheet,
    range: (u32, u16, u32, u16),
    evaluations: &mut u64,
    options: &RenderOptions,
) -> Result<Vec<f64>, RenderError> {
    let mut values = Vec::new();
    // Aggregate conditional rules are defined over their authored sqref, not
    // the clipped scene. The sparse sheet iterator visits only retained cells,
    // while each visit still consumes the shared conditional-evaluation budget.
    for cell in SparseDisplayCellIndex::new(sheet).range(range) {
        bump_conditional_evaluations(evaluations, options)?;
        if let Some(value) = numeric_cell_value(cell.value) {
            values.push(value);
        }
    }
    Ok(values)
}

fn coordinate_in_range(
    coordinate: CellCoordinate,
    (first_row, first_col, last_row, last_col): (u32, u16, u32, u16),
) -> bool {
    (first_row..=last_row).contains(&coordinate.row)
        && (first_col..=last_col).contains(&coordinate.col)
}

pub(super) fn numeric_cell_value(cell: &Cell) -> Option<f64> {
    let mut cell = cell;
    for _ in 0..=64 {
        match cell {
            Cell::Number(value) | Cell::Date(value) if value.is_finite() => return Some(*value),
            Cell::Formula { cached, .. } => cell = cached,
            Cell::Number(_) | Cell::Date(_) | Cell::Text(_) | Cell::Bool(_) | Cell::Error(_) => {
                return None;
            }
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum ConditionalOperand {
    Literal(f64),
    Reference(A1Reference),
}

pub(super) fn parse_conditional_operand(formula: &str) -> Option<ConditionalOperand> {
    let formula = formula.trim().strip_prefix('=').unwrap_or(formula.trim());
    if let Ok(value) = formula.parse::<f64>() {
        return value
            .is_finite()
            .then_some(ConditionalOperand::Literal(value));
    }
    parse_a1_reference(formula).map(ConditionalOperand::Reference)
}

fn resolve_conditional_operand(
    operand: &ConditionalOperand,
    sheet: &Sheet,
    target: CellCoordinate,
    origin: (u32, u16, u32, u16),
) -> Option<f64> {
    match operand {
        ConditionalOperand::Literal(value) => Some(*value),
        ConditionalOperand::Reference(reference) => {
            conditional_reference_coordinate(reference, sheet, target, origin)
                .and_then(|coordinate| sheet.cell(coordinate.row, coordinate.col))
                .and_then(numeric_cell_value)
        }
    }
}

pub(super) fn conditional_reference_coordinate(
    reference: &A1Reference,
    sheet: &Sheet,
    target: CellCoordinate,
    origin: (u32, u16, u32, u16),
) -> Option<CellCoordinate> {
    if reference.sheet.as_deref().is_some_and(|name| {
        name != sheet.name
            && !(name.is_ascii() && sheet.name.is_ascii() && name.eq_ignore_ascii_case(&sheet.name))
    }) {
        return None;
    }
    let row = if reference.row_absolute {
        reference.row
    } else {
        offset_a1_axis(
            u64::from(target.row),
            u64::from(reference.row),
            u64::from(origin.0),
            u64::from(MAX_WORKSHEET_ROW),
        )? as u32
    };
    let col = if reference.col_absolute {
        reference.col
    } else {
        offset_a1_axis(
            u64::from(target.col),
            u64::from(reference.col),
            u64::from(origin.1),
            u64::from(MAX_WORKSHEET_COLUMN),
        )? as u16
    };
    Some(CellCoordinate { row, col })
}

fn offset_a1_axis(target: u64, reference: u64, origin: u64, maximum: u64) -> Option<u64> {
    let value = i128::from(target)
        .checked_add(i128::from(reference))?
        .checked_sub(i128::from(origin))?;
    (0..=i128::from(maximum))
        .contains(&value)
        .then_some(value as u64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionalComparison {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

impl ConditionalComparison {
    fn compare(self, left: f64, right: f64) -> bool {
        match self {
            Self::Equal => left == right,
            Self::NotEqual => left != right,
            Self::Less => left < right,
            Self::LessOrEqual => left <= right,
            Self::Greater => left > right,
            Self::GreaterOrEqual => left >= right,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct ConditionalExpression {
    pub(super) left: ConditionalOperand,
    op: ConditionalComparison,
    pub(super) right: ConditionalOperand,
}

pub(super) fn parse_conditional_expression(formula: &str) -> Option<ConditionalExpression> {
    let formula = formula.trim().strip_prefix('=').unwrap_or(formula.trim());
    let bytes = formula.as_bytes();
    let mut quoted = false;
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\'' {
            if quoted && bytes.get(cursor + 1) == Some(&b'\'') {
                cursor += 2;
                continue;
            }
            quoted = !quoted;
            cursor += 1;
            continue;
        }
        if quoted {
            cursor += 1;
            continue;
        }
        let (op, width) = match (bytes[cursor], bytes.get(cursor + 1).copied()) {
            (b'<', Some(b'>')) => (ConditionalComparison::NotEqual, 2),
            (b'<', Some(b'=')) => (ConditionalComparison::LessOrEqual, 2),
            (b'>', Some(b'=')) => (ConditionalComparison::GreaterOrEqual, 2),
            (b'=', _) => (ConditionalComparison::Equal, 1),
            (b'<', _) => (ConditionalComparison::Less, 1),
            (b'>', _) => (ConditionalComparison::Greater, 1),
            _ => {
                cursor += 1;
                continue;
            }
        };
        let left = parse_conditional_operand(&formula[..cursor])?;
        let right = parse_conditional_operand(&formula[cursor + width..])?;
        return Some(ConditionalExpression { left, op, right });
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ConditionalValueKey {
    Number(u64),
    Text(String),
    Bool(bool),
}

fn conditional_value_key(cell: &Cell) -> Option<ConditionalValueKey> {
    let mut cell = cell;
    for _ in 0..=64 {
        match cell {
            Cell::Number(value) | Cell::Date(value) if value.is_finite() => {
                let value = if *value == 0.0 { 0.0 } else { *value };
                return Some(ConditionalValueKey::Number(value.to_bits()));
            }
            Cell::Text(value)
                if value.is_ascii() && !value.contains('*') && !value.contains('?') =>
            {
                return Some(ConditionalValueKey::Text(value.to_ascii_lowercase()));
            }
            Cell::Bool(value) => return Some(ConditionalValueKey::Bool(*value)),
            Cell::Formula { cached, .. } => cell = cached,
            Cell::Number(_) | Cell::Date(_) | Cell::Text(_) | Cell::Error(_) => return None,
        }
    }
    None
}

fn conditional_value_keys(
    sheet: &Sheet,
    range: (u32, u16, u32, u16),
    evaluations: &mut u64,
    options: &RenderOptions,
) -> Result<Option<BTreeMap<CellCoordinate, ConditionalValueKey>>, RenderError> {
    if range.2 > MAX_WORKSHEET_ROW || range.3 > MAX_WORKSHEET_COLUMN {
        return Ok(None);
    }
    let rows = u64::from(range.2) - u64::from(range.0) + 1;
    let columns = u64::from(range.3) - u64::from(range.1) + 1;
    let cells = rows
        .checked_mul(columns)
        .ok_or(RenderError::CoordinateOverflow)?;
    let actual = evaluations
        .checked_add(cells)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::ConditionalEvaluations,
        options.limits.max_conditional_evaluations,
        actual,
    )?;
    *evaluations = actual;

    // Duplicate/unique classification likewise uses the complete authored
    // range. This rule intentionally remains exact-only: unsupported or blank
    // members still defer the whole result instead of guessing.
    let mut keys = BTreeMap::new();
    for row in range.0..=range.2 {
        for col in range.1..=range.3 {
            let coordinate = CellCoordinate { row, col };
            let Some(key) = sheet.cell(row, col).and_then(conditional_value_key) else {
                return Ok(None);
            };
            keys.insert(coordinate, key);
        }
    }
    Ok(Some(keys))
}

fn compare_conditional(value: f64, op: DvOp, first: f64, second: Option<f64>) -> bool {
    match op {
        DvOp::Between => second.is_some_and(|second| first <= value && value <= second),
        DvOp::NotBetween => second.is_some_and(|second| value < first || value > second),
        DvOp::Equal => value == first,
        DvOp::NotEqual => value != first,
        DvOp::GreaterThan => value > first,
        DvOp::LessThan => value < first,
        DvOp::GreaterThanOrEqual => value >= first,
        DvOp::LessThanOrEqual => value <= first,
    }
}

pub(super) fn numeric_bounds(values: &[f64]) -> Option<(f64, f64)> {
    let mut values = values.iter().copied();
    let first = values.next()?;
    Some(values.fold((first, first), |(minimum, maximum), value| {
        (minimum.min(value), maximum.max(value))
    }))
}

fn percentile_50(sorted: &[f64]) -> f64 {
    let upper = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        sorted[upper - 1] / 2.0 + sorted[upper] / 2.0
    } else {
        sorted[upper]
    }
}

fn normalized_ppm(value: f64, minimum: f64, maximum: f64) -> u32 {
    if maximum <= minimum {
        return 1_000_000;
    }
    (((value - minimum) / (maximum - minimum)).clamp(0.0, 1.0) * 1_000_000.0).round() as u32
}

fn interpolate_rgb(start: Rgb, end: Rgb, ratio_ppm: u32) -> Rgb {
    let channel = |start: u8, end: u8| {
        let delta = i64::from(end) - i64::from(start);
        let scaled = i64::from(start) * 1_000_000 + delta * i64::from(ratio_ppm);
        u8::try_from(((scaled + 500_000) / 1_000_000).clamp(0, 255)).unwrap_or(start)
    };
    Rgb::new(
        channel(start.red, end.red),
        channel(start.green, end.green),
        channel(start.blue, end.blue),
    )
}

fn conditional_fill_overlay(
    color: Rgb,
    differential_style: Option<&CellStyle>,
    has_imported_differential: bool,
) -> CellStyle {
    if has_imported_differential {
        return differential_style.cloned().unwrap_or_default();
    }
    CellStyle::new().fill(Color::rgb(color.red, color.green, color.blue))
}

pub(super) fn apply_conditional_paint(
    paints: &mut BTreeMap<CellCoordinate, ConditionalPaint>,
    stopped: &mut BTreeSet<CellCoordinate>,
    coordinate: CellCoordinate,
    outcome: ConditionalOutcome,
    deferred_text_layout: &mut bool,
) {
    let ConditionalOutcome {
        style,
        data_bar,
        stop_if_true,
        text_measurement_unresolved,
    } = outcome;
    if stopped.contains(&coordinate) {
        return;
    }
    *deferred_text_layout |= text_measurement_unresolved;
    let paint = paints.entry(coordinate).or_default();
    if let Some(lower_priority) = style {
        paint.style = Some(match paint.style.take() {
            // The existing overlay came from a higher-priority rule. Merge it
            // last so each of its explicitly represented properties wins,
            // while the lower-priority rule may still supply missing ones.
            Some(higher_priority) => lower_priority.merge(&higher_priority),
            None => lower_priority,
        });
    }
    if paint.data_bar.is_none() {
        paint.data_bar = data_bar;
    }
    if stop_if_true {
        stopped.insert(coordinate);
    }
}

pub(super) fn push_data_bar(
    nodes: &mut Vec<SceneNode>,
    rect: Rect,
    paint: DataBarPaint,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    if paint.width_ppm == 0 || rect.width.raw() <= 0 || rect.height.raw() <= 0 {
        return Ok(());
    }
    let horizontal_inset = Fixed::from_pixels(1).max(Fixed::from_raw(1));
    let vertical_inset = Fixed::from_raw((rect.height.raw() / 5).max(1));
    let inner_width = rect
        .width
        .checked_sub(multiply_fixed(horizontal_inset, 2)?)
        .ok_or(RenderError::CoordinateOverflow)?
        .max(Fixed::from_raw(1));
    let inner_height = rect
        .height
        .checked_sub(multiply_fixed(vertical_inset, 2)?)
        .ok_or(RenderError::CoordinateOverflow)?
        .max(Fixed::from_raw(1));
    let width_raw = i128::from(inner_width.raw())
        .checked_mul(i128::from(paint.width_ppm))
        .and_then(|value| value.checked_div(1_000_000))
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(RenderError::CoordinateOverflow)?;
    if width_raw <= 0 {
        return Ok(());
    }
    push_node(
        nodes,
        SceneNode::Rect(RectNode {
            rect: Rect {
                x: rect
                    .x
                    .checked_add(horizontal_inset)
                    .ok_or(RenderError::CoordinateOverflow)?,
                y: rect
                    .y
                    .checked_add(vertical_inset)
                    .ok_or(RenderError::CoordinateOverflow)?,
                width: Fixed::from_raw(width_raw),
                height: inner_height,
            },
            fill: Some(paint.color),
            stroke: None,
            stroke_width: Fixed::ZERO,
        }),
        options,
    )
}
