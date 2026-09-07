//! Bounded chart data resolution, axis calculation, text placement, and scene painting.

use rxls::{
    Chart, ChartBarDirection, ChartCachedPoint, ChartFrameFill, ChartKind, ChartMarkerSymbol,
    ChartSeriesStyle, ChartTextStyle, Color, DrawingMetadata, FormatScript, Sheet,
};

use crate::error::{LimitKind, RenderError};
use crate::font::helvetica_text_advance_units;
use crate::scene::{
    Fixed, PathCommand, PathNode, Rect, Rgb, SceneNode, TextAnchor, TextBaseline, TextStyle,
    FIXED_UNITS_PER_PIXEL,
};
use crate::typography::CellLineLayoutPolicy;

use super::conditional::numeric_cell_value;
use super::geometry::points_to_fixed;
use super::text::{
    account_shaping, line_height_from_metrics, multiply_fixed, scale_ratio,
    shape_text_with_kerning, shaped_width, styled_line_metrics, text_base_direction,
    ResolvedRunStyle,
};
use super::{
    build_auxiliary_text_node_with_clip_and_kerning, enforce, fixed_as_pixels, interpolate_fixed,
    parse_a1_range, pixels_as_fixed, push_chart_frame, push_chart_series_line, push_node,
    push_placeholder_line, push_solid_rect, range_belongs_to_sheet, rgb, sum_fixed,
    A1RangeReference, CalcLinePlacementPolicy, CellCoordinate, RenderOptions, TypographyStats,
    WarningCode, Warnings,
};

pub(super) fn a1_range_points(range: &A1RangeReference) -> Option<u64> {
    let rows = u64::from(range.last_row) - u64::from(range.first_row) + 1;
    let columns = u64::from(range.last_col) - u64::from(range.first_col) + 1;
    rows.checked_mul(columns)
}

fn reserve_chart_points(
    total: &mut u64,
    additional: u64,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let actual = total
        .checked_add(additional)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::ChartPoints,
        options.limits.max_chart_points,
        actual,
    )?;
    *total = actual;
    Ok(())
}

pub(super) fn resolve_numeric_a1_range(
    sheet: &Sheet,
    source: &str,
    points: &mut u64,
    options: &RenderOptions,
    require_one_dimension: bool,
) -> Result<Option<Vec<f64>>, RenderError> {
    let Some(range) = parse_a1_range(source) else {
        return Ok(None);
    };
    if !range_belongs_to_sheet(&range, sheet)
        || (require_one_dimension
            && range.first_row != range.last_row
            && range.first_col != range.last_col)
    {
        return Ok(None);
    }
    let Some(count) = a1_range_points(&range) else {
        return Err(RenderError::CoordinateOverflow);
    };
    reserve_chart_points(points, count, options)?;
    let capacity = usize::try_from(count).map_err(|_| RenderError::CoordinateOverflow)?;
    let mut values = Vec::with_capacity(capacity);
    for row in range.first_row..=range.last_row {
        for col in range.first_col..=range.last_col {
            let Some(value) = sheet.cell(row, col).and_then(numeric_cell_value) else {
                return Ok(None);
            };
            values.push(value);
        }
    }
    Ok(Some(values))
}

pub(super) fn resolve_label_a1_range(
    sheet: &Sheet,
    source: &str,
    points: &mut u64,
    options: &RenderOptions,
) -> Result<Option<Vec<String>>, RenderError> {
    let Some(range) = parse_a1_range(source) else {
        return Ok(None);
    };
    if !range_belongs_to_sheet(&range, sheet)
        || (range.first_row != range.last_row && range.first_col != range.last_col)
    {
        return Ok(None);
    }
    let Some(count) = a1_range_points(&range) else {
        return Err(RenderError::CoordinateOverflow);
    };
    reserve_chart_points(points, count, options)?;
    let capacity = usize::try_from(count).map_err(|_| RenderError::CoordinateOverflow)?;
    let mut labels = Vec::with_capacity(capacity);
    for row in range.first_row..=range.last_row {
        for col in range.first_col..=range.last_col {
            labels.push(sheet.formatted(row, col).unwrap_or("").to_string());
        }
    }
    Ok(Some(labels))
}

fn contiguous_cached_values(points: &[ChartCachedPoint]) -> Option<Vec<&str>> {
    if points.is_empty() {
        return None;
    }
    points
        .iter()
        .enumerate()
        .map(|(expected, point)| {
            (usize::try_from(point.index).ok()? == expected).then_some(point.value.as_str())
        })
        .collect()
}

fn resolve_numeric_chart_source(
    sheet: &Sheet,
    source: &str,
    cached: &[ChartCachedPoint],
    points: &mut u64,
    options: &RenderOptions,
) -> Result<Option<Vec<f64>>, RenderError> {
    let initial_points = *points;
    if let Some(values) = resolve_numeric_a1_range(sheet, source, points, options, true)? {
        return Ok(Some(values));
    }
    *points = initial_points;
    let Some(cached) = contiguous_cached_values(cached) else {
        return Ok(None);
    };
    reserve_chart_points(points, cached.len() as u64, options)?;
    let values = cached
        .into_iter()
        .map(|value| {
            value
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite())
        })
        .collect::<Option<Vec<_>>>();
    if values.is_none() {
        *points = initial_points;
    }
    Ok(values)
}

fn resolve_label_chart_source(
    sheet: &Sheet,
    source: &str,
    cached: &[ChartCachedPoint],
    points: &mut u64,
    options: &RenderOptions,
) -> Result<Option<Vec<String>>, RenderError> {
    let initial_points = *points;
    if let Some(labels) = resolve_label_a1_range(sheet, source, points, options)? {
        return Ok(Some(labels));
    }
    *points = initial_points;
    let Some(cached) = contiguous_cached_values(cached) else {
        return Ok(None);
    };
    reserve_chart_points(points, cached.len() as u64, options)?;
    Ok(Some(cached.into_iter().map(str::to_string).collect()))
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn try_push_chart(
    nodes: &mut Vec<SceneNode>,
    rect: Rect,
    chart: &Chart,
    metadata: Option<&DrawingMetadata>,
    sheet: &Sheet,
    chart_points: &mut u64,
    text_bytes: &mut u64,
    glyphs: &mut u64,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
    warnings: &mut Warnings,
    warning_cell: CellCoordinate,
) -> Result<bool, RenderError> {
    try_push_chart_with_layout(
        nodes,
        rect,
        chart,
        metadata,
        false,
        sheet,
        chart_points,
        text_bytes,
        glyphs,
        typography_stats,
        options,
        warnings,
        warning_cell,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn try_push_chart_with_layout(
    nodes: &mut Vec<SceneNode>,
    rect: Rect,
    chart: &Chart,
    metadata: Option<&DrawingMetadata>,
    calc_single_page_layout: bool,
    sheet: &Sheet,
    chart_points: &mut u64,
    text_bytes: &mut u64,
    glyphs: &mut u64,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
    warnings: &mut Warnings,
    warning_cell: CellCoordinate,
) -> Result<bool, RenderError> {
    if chart.series.is_empty()
        || metadata.is_some_and(|metadata| !metadata.chart_unsupported_reasons.is_empty())
        || rect.width < Fixed::from_pixels(120)
        || rect.height < Fixed::from_pixels(80)
    {
        return Ok(false);
    }
    let initial_points = *chart_points;
    let style_loss_count = metadata.map_or(0_u64, |metadata| {
        metadata.chart_frame_style_losses.len() as u64
            + metadata
                .chart_series_styles
                .iter()
                .map(|style| style.losses.len() as u64)
                .sum::<u64>()
    });
    warnings.add_count(
        WarningCode::ChartMetadataSimplified,
        style_loss_count,
        Some(warning_cell),
    );
    let mut series = Vec::with_capacity(chart.series.len());
    for (index, source) in chart.series.iter().enumerate() {
        let cache = metadata.and_then(|metadata| metadata.chart_series_caches.get(index));
        let value_cache = cache.map_or(&[][..], |cache| cache.values.as_slice());
        let Some(values) = resolve_numeric_chart_source(
            sheet,
            &source.values,
            value_cache,
            chart_points,
            options,
        )?
        else {
            *chart_points = initial_points;
            return Ok(false);
        };
        if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
            *chart_points = initial_points;
            return Ok(false);
        }
        let category_cache = cache.map_or(&[][..], |cache| cache.categories.as_slice());
        let (x_values, labels) = if matches!(chart.kind, ChartKind::Scatter | ChartKind::Bubble) {
            let Some(x_values) = resolve_numeric_chart_source(
                sheet,
                source.categories.as_deref().unwrap_or(""),
                category_cache,
                chart_points,
                options,
            )?
            else {
                *chart_points = initial_points;
                return Ok(false);
            };
            if x_values.len() != values.len() {
                *chart_points = initial_points;
                return Ok(false);
            }
            if x_values.iter().any(|value| !value.is_finite()) {
                *chart_points = initial_points;
                return Ok(false);
            }
            (Some(x_values), Vec::new())
        } else {
            let labels = match source.categories.as_deref() {
                Some(categories) => {
                    let Some(labels) = resolve_label_chart_source(
                        sheet,
                        categories,
                        category_cache,
                        chart_points,
                        options,
                    )?
                    else {
                        *chart_points = initial_points;
                        return Ok(false);
                    };
                    if labels.len() != values.len() {
                        *chart_points = initial_points;
                        return Ok(false);
                    }
                    labels
                }
                None if !category_cache.is_empty() => {
                    let Some(labels) = resolve_label_chart_source(
                        sheet,
                        "",
                        category_cache,
                        chart_points,
                        options,
                    )?
                    else {
                        *chart_points = initial_points;
                        return Ok(false);
                    };
                    if labels.len() != values.len() {
                        *chart_points = initial_points;
                        return Ok(false);
                    }
                    labels
                }
                None => (1..=values.len()).map(|value| value.to_string()).collect(),
            };
            (None, labels)
        };
        let point_count = values.len();
        let bubble_sizes = if chart.kind == ChartKind::Bubble {
            let bubble_cache = cache.map_or(&[][..], |cache| cache.bubble_sizes.as_slice());
            let values = match source.bubble_sizes.as_deref() {
                Some(source) => resolve_numeric_chart_source(
                    sheet,
                    source,
                    bubble_cache,
                    chart_points,
                    options,
                )?,
                None if !bubble_cache.is_empty() => {
                    resolve_numeric_chart_source(sheet, "", bubble_cache, chart_points, options)?
                }
                None => Some(vec![1.0; point_count]),
            };
            let Some(values) = values else {
                *chart_points = initial_points;
                return Ok(false);
            };
            if values.len() != point_count || values.iter().any(|value| *value <= 0.0) {
                *chart_points = initial_points;
                return Ok(false);
            }
            Some(values)
        } else {
            None
        };
        let cached_name = cache
            .and_then(|cache| match cache.name.as_slice() {
                [point] if point.index == 0 && !point.value.trim().is_empty() => Some(point),
                _ => None,
            })
            .map(|point| point.value.trim().to_string());
        series.push(ResolvedChartSeries {
            name: cached_name
                .or_else(|| source.name.clone())
                .unwrap_or_else(|| format!("Series {}", index + 1)),
            values,
            x_values,
            labels,
            bubble_sizes,
            style: metadata
                .and_then(|metadata| metadata.chart_series_styles.get(index))
                .cloned()
                .unwrap_or_default(),
        });
    }
    if matches!(chart.kind, ChartKind::Pie | ChartKind::Doughnut)
        && ((chart.kind == ChartKind::Pie && series.len() != 1)
            || series.iter().any(|series| {
                let total = series.values.iter().sum::<f64>();
                series.values.iter().any(|value| *value < 0.0) || !total.is_finite() || total <= 0.0
            }))
    {
        *chart_points = initial_points;
        return Ok(false);
    }
    if chart.kind == ChartKind::Radar && series.iter().any(|series| series.values.len() < 3) {
        *chart_points = initial_points;
        return Ok(false);
    }
    let data_label_count = series
        .iter()
        .map(|series| series.values.len())
        .sum::<usize>();
    let legend_count = if matches!(chart.kind, ChartKind::Pie | ChartKind::Doughnut) {
        series[0].labels.len()
    } else {
        series.len()
    };
    if (chart.data_labels && data_label_count > 256) || (chart.legend && legend_count > 16) {
        *chart_points = initial_points;
        return Ok(false);
    }

    let chart_title = chart.title.as_deref().filter(|text| !text.is_empty());
    let raw_x_axis_title = chart
        .x_axis_title
        .as_deref()
        .filter(|text| !text.is_empty());
    let raw_y_axis_title = chart
        .y_axis_title
        .as_deref()
        .filter(|text| !text.is_empty());
    let chart_font_family = metadata
        .and_then(|metadata| metadata.chart_default_latin_font_family.as_deref())
        .unwrap_or(&options.default_font_family);
    let typography_before_chart_text = typography_stats.clone();
    let warnings_before_chart_text = warnings.clone();

    let palette = metadata.map_or(&[][..], |metadata| metadata.chart_palette.as_slice());
    let cartesian = matches!(
        chart.kind,
        ChartKind::Bar | ChartKind::Line | ChartKind::Scatter | ChartKind::Area | ChartKind::Bubble
    );
    let horizontal_bar = chart.kind == ChartKind::Bar
        && metadata
            .is_some_and(|metadata| metadata.chart_bar_direction == ChartBarDirection::Horizontal);
    let category_axis_visible = metadata
        .and_then(|metadata| metadata.chart_category_axis_visible)
        .unwrap_or(true);
    let category_axis_shifted = metadata
        .and_then(|metadata| metadata.chart_category_axis_shifted)
        .unwrap_or(false);
    let value_axis_visible = metadata
        .and_then(|metadata| metadata.chart_value_axis_visible)
        .unwrap_or(true);
    let (horizontal_axis_visible, vertical_axis_visible) = if horizontal_bar {
        (value_axis_visible, category_axis_visible)
    } else {
        (category_axis_visible, value_axis_visible)
    };
    let x_axis_title = raw_x_axis_title.filter(|_| horizontal_axis_visible);
    let y_axis_title = raw_y_axis_title.filter(|_| vertical_axis_visible);
    let Some(text_styles) = ResolvedChartTextStyles::resolve(metadata, chart_font_family) else {
        *chart_points = initial_points;
        return Ok(false);
    };
    let (x_axis_title_style, y_axis_title_style) = text_styles.physical_axis_titles(horizontal_bar);
    let (horizontal_axis_style, vertical_axis_style) = if horizontal_bar {
        (
            &text_styles.value_axis_labels,
            &text_styles.category_axis_labels,
        )
    } else {
        (
            &text_styles.category_axis_labels,
            &text_styles.value_axis_labels,
        )
    };
    let cartesian_axis = if cartesian {
        let axis = if metadata.is_some() && chart.kind == ChartKind::Line {
            chart_calc_imported_line_value_axis(&series)
        } else {
            chart_nice_value_axis(
                &series,
                matches!(chart.kind, ChartKind::Bar | ChartKind::Area),
            )
        };
        let Some(axis) = axis else {
            *chart_points = initial_points;
            return Ok(false);
        };
        Some(axis)
    } else {
        None
    };
    let x_value_axis = if matches!(chart.kind, ChartKind::Scatter | ChartKind::Bubble) {
        let Some(axis) = chart_nice_x_axis(&series) else {
            *chart_points = initial_points;
            return Ok(false);
        };
        Some(axis)
    } else {
        None
    };
    let x_data_bounds = if matches!(chart.kind, ChartKind::Scatter | ChartKind::Bubble) {
        let Some(bounds) = chart_x_data_bounds(&series) else {
            *chart_points = initial_points;
            return Ok(false);
        };
        Some(bounds)
    } else {
        None
    };
    let radar_axis = if chart.kind == ChartKind::Radar {
        let Some(axis) = chart_nice_value_axis(&series, true) else {
            *chart_points = initial_points;
            return Ok(false);
        };
        Some(axis)
    } else {
        None
    };
    let legend_entries = if chart.legend {
        if matches!(chart.kind, ChartKind::Pie | ChartKind::Doughnut) {
            series[0]
                .labels
                .iter()
                .cloned()
                .enumerate()
                .collect::<Vec<_>>()
        } else {
            series
                .iter()
                .map(|series| series.name.clone())
                .enumerate()
                .collect::<Vec<_>>()
        }
    } else {
        Vec::new()
    };
    let has_chart_text = chart_title.is_some()
        || x_axis_title.is_some()
        || y_axis_title.is_some()
        || legend_entries.iter().any(|(_, text)| !text.is_empty())
        || chart.data_labels
        || ((cartesian || chart.kind == ChartKind::Radar)
            && (category_axis_visible || value_axis_visible));
    if options.font_pack.is_none() && has_chart_text {
        // Fontless chart text is still deterministic because measurement and
        // backend emission share the same Helvetica byte/advance mapping. It
        // cannot, however, reproduce source kerning or selected-font metrics,
        // so retain explicit provenance instead of replacing the whole chart.
        warnings.add(WarningCode::ApproximateTextMetrics, Some(warning_cell));
    }

    let value_axis_text = if value_axis_visible {
        cartesian_axis.as_ref().or(radar_axis.as_ref()).map(|axis| {
            axis.ticks
                .iter()
                .map(|value| chart_axis_number(*value, axis.major))
                .collect::<Vec<_>>()
        })
    } else {
        None
    }
    .unwrap_or_default();
    let value_axis_metrics = max_chart_text_metrics_with_style(
        value_axis_text.iter().map(String::as_str),
        &text_styles.value_axis_labels,
        options,
        typography_stats,
        warnings,
        warning_cell,
    )?;
    let category_axis_metrics = if !category_axis_visible {
        ChartTextMetrics::default()
    } else if matches!(chart.kind, ChartKind::Scatter | ChartKind::Bubble) {
        let x_axis = x_value_axis
            .as_ref()
            .expect("scatter and bubble charts have a retained x-value axis");
        let x_data_bounds = x_data_bounds.expect("scatter and bubble charts have x-data bounds");
        let labels = x_axis
            .ticks
            .iter()
            .filter(|value| **value >= x_data_bounds.0 && **value <= x_data_bounds.1)
            .map(|value| chart_axis_number(*value, x_axis.major))
            .collect::<Vec<_>>();
        max_chart_text_metrics_with_style(
            labels.iter().map(String::as_str),
            &text_styles.category_axis_labels,
            options,
            typography_stats,
            warnings,
            warning_cell,
        )?
    } else if cartesian || chart.kind == ChartKind::Radar {
        let categories = series
            .first()
            .map(|series| series.labels.as_slice())
            .unwrap_or_default();
        let stride = chart_category_label_stride(categories.len());
        max_chart_text_metrics_with_style(
            categories
                .iter()
                .enumerate()
                .filter_map(|(index, category)| {
                    chart_category_label_is_retained(index, categories.len(), stride)
                        .then_some(category.as_str())
                }),
            &text_styles.category_axis_labels,
            options,
            typography_stats,
            warnings,
            warning_cell,
        )?
    } else {
        ChartTextMetrics::default()
    };
    let (horizontal_axis_metrics, vertical_axis_metrics) = if horizontal_bar {
        (value_axis_metrics, category_axis_metrics)
    } else {
        (category_axis_metrics, value_axis_metrics)
    };
    let title_metrics = chart_title
        .map(|title| {
            measure_chart_text_with_style(
                title,
                &text_styles.chart_title,
                options,
                Some((typography_stats, warnings, warning_cell)),
            )
        })
        .transpose()?;
    let x_axis_title_metrics = x_axis_title
        .map(|title| {
            measure_chart_text_with_style(
                title,
                x_axis_title_style,
                options,
                Some((typography_stats, warnings, warning_cell)),
            )
        })
        .transpose()?;
    let y_axis_title_metrics = y_axis_title
        .map(|title| {
            measure_chart_text_with_style(
                title,
                y_axis_title_style,
                options,
                Some((typography_stats, warnings, warning_cell)),
            )
        })
        .transpose()?;
    let legend_metrics = max_chart_text_metrics_with_style(
        legend_entries.iter().map(|(_, name)| name.as_str()),
        &text_styles.legend,
        options,
        typography_stats,
        warnings,
        warning_cell,
    )?;
    let horizontal_axis_extents =
        horizontal_axis_metrics.rotated(horizontal_axis_style.rotation_degrees(0))?;
    let vertical_axis_extents =
        vertical_axis_metrics.rotated(vertical_axis_style.rotation_degrees(0))?;
    let title_extents = title_metrics
        .map(|metrics| metrics.rotated(text_styles.chart_title.rotation_degrees(0)))
        .transpose()?;
    let x_axis_title_extents = x_axis_title_metrics
        .map(|metrics| metrics.rotated(x_axis_title_style.rotation_degrees(0)))
        .transpose()?;
    let y_axis_title_extents = y_axis_title_metrics
        .map(|metrics| metrics.rotated(y_axis_title_style.rotation_degrees(-90)))
        .transpose()?;
    let legend_extents = legend_metrics.rotated(text_styles.legend.rotation_degrees(0))?;

    let text_gap = Fixed::from_pixels(4);
    let imported_chart_frame =
        metadata.is_some_and(|metadata| metadata.chart_default_latin_font_family.is_some());
    let frame_padding =
        chart_frame_padding(rect.width, imported_chart_frame && calc_single_page_layout)?;
    let calc_default_line_plot = metadata.is_some_and(|metadata| {
        metadata.chart_series_styles.len() == chart.series.len()
            && metadata.chart_series_styles.iter().all(|style| {
                style.marker == ChartMarkerSymbol::Circle && style.marker_size == Some(5)
            })
    }) && chart.kind == ChartKind::Line
        && category_axis_shifted
        && category_axis_visible
        && value_axis_visible
        && chart_title.is_none()
        && x_axis_title.is_none()
        && y_axis_title.is_none()
        && legend_entries.is_empty()
        && !chart.data_labels;
    // Calc's imported circle-marker line profile leaves a small asymmetric
    // vertical inset around the plot, independent of the axis-label bands.
    // SinglePageSheets retains the authored inset for compact chart frames,
    // but switches to its tighter replay profile once the frame exceeds five
    // CSS inches. The distinction also keeps chart labels from being merged
    // with worksheet text on adjacent baselines.
    let compact_calc_single_page_line_plot =
        calc_single_page_layout && rect.width <= Fixed::from_pixels(480);
    let calc_authored_line_plot_spacing =
        !calc_single_page_layout || compact_calc_single_page_line_plot;
    let horizontal_overhang = Fixed::from_raw(horizontal_axis_extents.width.raw() / 2);
    let vertical_axis_space = if vertical_axis_extents.width > Fixed::ZERO {
        vertical_axis_extents
            .width
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)?
    } else {
        Fixed::ZERO
    };
    let left_axis_space = vertical_axis_space.max(horizontal_overhang);
    let y_axis_title_space = y_axis_title_extents.map_or(Ok(Fixed::ZERO), |extents| {
        extents
            .width
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)
    })?;
    let left_gutter = sum_fixed([frame_padding, y_axis_title_space, left_axis_space])?;
    let vertical_axis_overhang = Fixed::from_raw(
        vertical_axis_extents
            .height
            .raw()
            .max(if chart.kind == ChartKind::Radar {
                horizontal_axis_extents.height.raw()
            } else {
                0
            })
            / 2,
    );
    let title_band = match title_extents {
        Some(extents) => extents
            .height
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)?,
        None => frame_padding,
    };
    // A shifted category axis keeps its first and last bands inside the
    // diagram, so Calc does not reserve the endpoint half-label overhang in
    // the vertical extent either.  Keep the title band, but avoid shrinking
    // the plot by the value-label half-height used by endpoint axes.
    let top_axis_overhang = if category_axis_shifted && !horizontal_bar {
        Fixed::ZERO
    } else {
        vertical_axis_overhang
    };
    let calc_top_plot_spacing = if calc_default_line_plot {
        Fixed::from_pixels(if calc_authored_line_plot_spacing {
            7
        } else {
            4
        })
    } else {
        Fixed::ZERO
    };
    let top_gutter = title_band
        .checked_add(top_axis_overhang)
        .and_then(|value| value.checked_add(calc_top_plot_spacing))
        .ok_or(RenderError::CoordinateOverflow)?;
    let horizontal_axis_space = if horizontal_axis_extents.height > Fixed::ZERO {
        horizontal_axis_extents
            .height
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)?
    } else {
        Fixed::ZERO
    };
    let x_axis_title_space = x_axis_title_extents.map_or(Ok(Fixed::ZERO), |extents| {
        extents
            .height
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)
    })?;
    let bottom_text_space = sum_fixed([horizontal_axis_space, x_axis_title_space])?;
    let calc_bottom_plot_spacing = if calc_default_line_plot {
        Fixed::from_pixels(if calc_authored_line_plot_spacing {
            4
        } else {
            1
        })
    } else {
        Fixed::ZERO
    };
    let bottom_gutter = frame_padding
        .checked_add(bottom_text_space.max(vertical_axis_overhang))
        .and_then(|value| value.checked_add(calc_bottom_plot_spacing))
        .ok_or(RenderError::CoordinateOverflow)?;

    let legend_layout = if legend_entries.is_empty() {
        None
    } else {
        let row_height = legend_extents
            .height
            .max(Fixed::from_pixels(10))
            .checked_add(Fixed::from_pixels(2))
            .ok_or(RenderError::CoordinateOverflow)?;
        let available_height = rect
            .height
            .checked_sub(top_gutter)
            .and_then(|value| value.checked_sub(bottom_gutter))
            .ok_or(RenderError::CoordinateOverflow)?;
        let rows_per_column = if available_height > Fixed::ZERO {
            usize::try_from(available_height.raw() / row_height.raw())
                .map_err(|_| RenderError::CoordinateOverflow)?
                .min(legend_entries.len())
        } else {
            0
        };
        if rows_per_column == 0 {
            *typography_stats = typography_before_chart_text;
            *warnings = warnings_before_chart_text;
            *chart_points = initial_points;
            return Ok(false);
        }
        let columns = legend_entries.len().div_ceil(rows_per_column);
        let column_width = sum_fixed([
            Fixed::from_pixels(10),
            Fixed::from_pixels(2),
            legend_extents.width,
        ])?;
        let column_step = column_width
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)?;
        let total_width = multiply_fixed(
            column_width,
            i64::try_from(columns).map_err(|_| RenderError::CoordinateOverflow)?,
        )?
        .checked_add(multiply_fixed(
            text_gap,
            i64::try_from(columns.saturating_sub(1))
                .map_err(|_| RenderError::CoordinateOverflow)?,
        )?)
        .ok_or(RenderError::CoordinateOverflow)?;
        let plot_offset = horizontal_overhang
            .checked_add(text_gap)
            .ok_or(RenderError::CoordinateOverflow)?;
        Some(ChartLegendLayout {
            row_height,
            rows_per_column,
            column_step,
            plot_offset,
            total_width,
        })
    };
    let right_gutter = match legend_layout {
        Some(layout) => sum_fixed([frame_padding, layout.plot_offset, layout.total_width])?,
        None => {
            // Calc's `crossBetween=between` category axis places every
            // category inside the plot bands.  The final category therefore
            // does not need the endpoint overhang reserved by a legacy
            // endpoint axis; retaining it shrinks the imported plot and
            // moves the last marker left of Calc's position.  Authored and
            // endpoint charts keep the historical overhang.
            let endpoint_overhang = if category_axis_shifted && !horizontal_bar {
                Fixed::ZERO
            } else {
                horizontal_overhang
            };
            sum_fixed([frame_padding, endpoint_overhang])?
        }
    };

    let left = rect
        .x
        .checked_add(left_gutter)
        .ok_or(RenderError::CoordinateOverflow)?;
    let top = rect
        .y
        .checked_add(top_gutter)
        .ok_or(RenderError::CoordinateOverflow)?;
    let right = rect
        .x
        .checked_add(rect.width)
        .and_then(|value| value.checked_sub(right_gutter))
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .and_then(|value| value.checked_sub(bottom_gutter))
        .ok_or(RenderError::CoordinateOverflow)?;
    if right <= left || bottom <= top {
        *typography_stats = typography_before_chart_text;
        *warnings = warnings_before_chart_text;
        *chart_points = initial_points;
        return Ok(false);
    }
    let plot = Rect {
        x: left,
        y: top,
        width: right
            .checked_sub(left)
            .ok_or(RenderError::CoordinateOverflow)?,
        height: bottom
            .checked_sub(top)
            .ok_or(RenderError::CoordinateOverflow)?,
    };
    let frame_fill = match metadata.map_or(ChartFrameFill::Automatic, |metadata| {
        metadata.chart_frame_fill
    }) {
        ChartFrameFill::Automatic => {
            // Imported OOXML chart spaces with no c:spPr paint are
            // transparent in Calc. Authored charts retain the historical
            // white compatibility default; an explicit unsupported paint
            // also keeps that fallback so it is not mistaken for noFill.
            let imported_implicit_no_fill = metadata.is_some_and(|metadata| {
                metadata.chart_default_latin_font_family.is_some()
                    && metadata.chart_frame_style_losses.is_empty()
            });
            (!imported_implicit_no_fill).then_some(Rgb::WHITE)
        }
        ChartFrameFill::NoFill => None,
        ChartFrameFill::Solid(color) => {
            let [red, green, blue] = color.as_rgb();
            Some(Rgb::new(red, green, blue))
        }
        _ => Some(Rgb::WHITE),
    };
    // Calc's imported OOXML chart-space frame uses the light gray DrawingML
    // default outline even when its fill is omitted or explicitly `noFill`.
    // Authored charts retain the historical neutral outline for compatibility.
    let frame_stroke = imported_chart_frame.then_some(Rgb::new(217, 217, 217));
    push_chart_frame(nodes, rect, frame_fill, frame_stroke, options)?;
    let category_data_plot = chart_category_data_plot(plot, category_axis_shifted, horizontal_bar)?;
    let mut labels = Vec::<ChartLabel>::new();
    let mut category_labels = Vec::<ChartLabel>::new();
    let mut value_labels = Vec::<ChartLabel>::new();
    let category_major_gridlines = metadata
        .and_then(|metadata| metadata.chart_category_major_gridlines)
        .unwrap_or(false)
        && category_axis_visible;
    let value_major_gridlines = metadata
        .and_then(|metadata| metadata.chart_value_major_gridlines)
        .unwrap_or(true)
        && value_axis_visible;
    let axis_lines_visible =
        metadata.is_none() || category_major_gridlines || value_major_gridlines;
    if let Some(axis) = cartesian_axis.as_ref() {
        push_cartesian_chart_axes(
            nodes,
            rect,
            plot,
            chart.kind,
            horizontal_bar,
            axis,
            x_value_axis.as_ref(),
            x_data_bounds,
            &series,
            category_axis_visible,
            category_axis_shifted,
            value_axis_visible,
            category_major_gridlines,
            value_major_gridlines,
            axis_lines_visible,
            &text_styles.category_axis_labels,
            &text_styles.value_axis_labels,
            text_bytes,
            glyphs,
            typography_stats,
            options,
            warnings,
            warning_cell,
        )?;
    }
    match chart.kind {
        ChartKind::Pie => {
            push_pie_chart(
                nodes,
                plot,
                &series[0],
                palette,
                chart.data_labels,
                &mut labels,
                typography_stats,
                options,
            )?;
        }
        ChartKind::Doughnut => {
            push_doughnut_chart(
                nodes,
                plot,
                &series,
                palette,
                chart.data_labels,
                &mut labels,
                typography_stats,
                options,
            )?;
        }
        ChartKind::Radar => {
            let axis = radar_axis
                .as_ref()
                .expect("radar charts have a retained value axis");
            push_radar_chart(
                nodes,
                plot,
                &series,
                axis,
                palette,
                category_axis_visible,
                value_axis_visible,
                category_major_gridlines,
                value_major_gridlines,
                &mut category_labels,
                &mut value_labels,
                chart.data_labels,
                &mut labels,
                typography_stats,
                options,
                warnings,
                warning_cell,
            )?;
        }
        _ => {
            let axis = cartesian_axis
                .as_ref()
                .expect("all cartesian chart kinds have a value axis");
            let bounds = (axis.minimum, axis.maximum);
            match chart.kind {
                ChartKind::Line => push_line_chart(
                    nodes,
                    category_data_plot,
                    &series,
                    bounds,
                    category_axis_shifted,
                    palette,
                    chart.data_labels,
                    &mut labels,
                    typography_stats,
                    options,
                )?,
                ChartKind::Scatter => push_scatter_chart(
                    nodes,
                    plot,
                    &series,
                    bounds,
                    x_value_axis
                        .as_ref()
                        .expect("scatter charts have a retained x-value axis"),
                    palette,
                    chart.data_labels,
                    &mut labels,
                    typography_stats,
                    options,
                )?,
                ChartKind::Bar => {
                    if horizontal_bar {
                        push_horizontal_bar_chart(
                            nodes,
                            plot,
                            &series,
                            bounds,
                            palette,
                            chart.data_labels,
                            &mut labels,
                            options,
                        )?;
                    } else {
                        push_column_chart(
                            nodes,
                            category_data_plot,
                            &series,
                            bounds,
                            palette,
                            chart.data_labels,
                            &mut labels,
                            options,
                        )?;
                    }
                }
                ChartKind::Area => push_area_chart(
                    nodes,
                    category_data_plot,
                    &series,
                    bounds,
                    category_axis_shifted,
                    palette,
                    chart.data_labels,
                    &mut labels,
                    typography_stats,
                    options,
                )?,
                ChartKind::Bubble => push_bubble_chart(
                    nodes,
                    plot,
                    &series,
                    bounds,
                    x_value_axis
                        .as_ref()
                        .expect("bubble charts have a retained x-value axis"),
                    palette,
                    chart.data_labels,
                    &mut labels,
                    typography_stats,
                    options,
                )?,
                ChartKind::Pie | ChartKind::Doughnut | ChartKind::Radar => {
                    unreachable!("non-cartesian chart handled above")
                }
            }
        }
    }

    if let Some(title) = chart_title {
        let extents = title_extents.expect("chart title extents were measured");
        let paint_bounds = Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: extents.height,
        };
        push_chart_text_with_style(
            nodes,
            title.to_string(),
            paint_bounds,
            rect,
            TextAnchor::Middle,
            0,
            &text_styles.chart_title,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    if let Some(title) = x_axis_title {
        let extents = x_axis_title_extents.expect("x-axis title extents were measured");
        let paint_bounds = Rect {
            x: left,
            y: bottom
                .checked_add(horizontal_axis_space)
                .and_then(|value| value.checked_add(text_gap))
                .ok_or(RenderError::CoordinateOverflow)?,
            width: plot.width,
            height: extents.height,
        };
        push_chart_text_with_style(
            nodes,
            title.to_string(),
            paint_bounds,
            rect,
            TextAnchor::Middle,
            0,
            x_axis_title_style,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    if let Some(title) = y_axis_title {
        let extents = y_axis_title_extents.expect("y-axis title extents were measured");
        let paint_bounds = Rect {
            x: rect
                .x
                .checked_add(frame_padding)
                .ok_or(RenderError::CoordinateOverflow)?,
            y: top,
            width: extents.width,
            height: plot.height,
        };
        push_chart_text_with_style(
            nodes,
            title.to_string(),
            paint_bounds,
            rect,
            TextAnchor::Middle,
            -90,
            y_axis_title_style,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    if let Some(layout) = legend_layout {
        for (entry_index, (index, name)) in legend_entries.into_iter().enumerate() {
            let column = entry_index / layout.rows_per_column;
            let row = entry_index % layout.rows_per_column;
            let column_offset = multiply_fixed(
                layout.column_step,
                i64::try_from(column).map_err(|_| RenderError::CoordinateOverflow)?,
            )?;
            let legend_x = right
                .checked_add(layout.plot_offset)
                .and_then(|value| value.checked_add(column_offset))
                .ok_or(RenderError::CoordinateOverflow)?;
            let row_offset = multiply_fixed(
                layout.row_height,
                i64::try_from(row).map_err(|_| RenderError::CoordinateOverflow)?,
            )?;
            let y = top
                .checked_add(row_offset)
                .ok_or(RenderError::CoordinateOverflow)?;
            let metrics = measure_chart_text_with_style(&name, &text_styles.legend, options, None)?;
            let entry_extents = metrics.rotated(text_styles.legend.rotation_degrees(0))?;
            let swatch_offset = Fixed::from_raw(
                layout
                    .row_height
                    .raw()
                    .checked_sub(Fixed::from_pixels(10).raw())
                    .ok_or(RenderError::CoordinateOverflow)?
                    / 2,
            );
            push_solid_rect(
                nodes,
                legend_x,
                y.checked_add(swatch_offset)
                    .ok_or(RenderError::CoordinateOverflow)?,
                legend_x
                    .checked_add(Fixed::from_pixels(10))
                    .ok_or(RenderError::CoordinateOverflow)?,
                y.checked_add(swatch_offset)
                    .and_then(|value| value.checked_add(Fixed::from_pixels(10)))
                    .ok_or(RenderError::CoordinateOverflow)?,
                chart_color(index, palette),
                options,
            )?;
            let paint_bounds = Rect {
                x: legend_x
                    .checked_add(Fixed::from_pixels(12))
                    .ok_or(RenderError::CoordinateOverflow)?,
                y,
                width: entry_extents.width,
                height: layout.row_height,
            };
            let (bounds, anchor) = if text_styles.legend.rotation_degrees(0).rem_euclid(360) == 0 {
                (
                    Rect {
                        x: paint_bounds.x,
                        y,
                        width: metrics.width,
                        height: metrics.height,
                    },
                    TextAnchor::Start,
                )
            } else {
                (
                    centered_chart_text_bounds(paint_bounds, metrics)?,
                    TextAnchor::Middle,
                )
            };
            push_chart_text_with_style(
                nodes,
                name,
                bounds,
                rect,
                anchor,
                0,
                &text_styles.legend,
                text_bytes,
                glyphs,
                typography_stats,
                options,
            )?;
        }
    }
    for label in value_labels {
        let metrics = measure_chart_text_with_style(
            &label.text,
            &text_styles.value_axis_labels,
            options,
            None,
        )?;
        let extents = metrics.rotated(text_styles.value_axis_labels.rotation_degrees(0))?;
        let paint_bounds = Rect {
            x: label
                .x
                .checked_sub(Fixed::from_raw(extents.width.raw() / 2))
                .ok_or(RenderError::CoordinateOverflow)?,
            y: label
                .y
                .checked_sub(Fixed::from_raw(extents.height.raw() / 2))
                .ok_or(RenderError::CoordinateOverflow)?,
            width: extents.width,
            height: extents.height,
        };
        push_chart_text_with_style(
            nodes,
            label.text,
            centered_chart_text_bounds(paint_bounds, metrics)?,
            rect,
            TextAnchor::Middle,
            0,
            &text_styles.value_axis_labels,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    for label in category_labels {
        let metrics = measure_chart_text_with_style(
            &label.text,
            &text_styles.category_axis_labels,
            options,
            None,
        )?;
        let extents = metrics.rotated(text_styles.category_axis_labels.rotation_degrees(0))?;
        let paint_bounds = Rect {
            x: label
                .x
                .checked_sub(Fixed::from_raw(extents.width.raw() / 2))
                .ok_or(RenderError::CoordinateOverflow)?,
            y: label
                .y
                .checked_sub(Fixed::from_raw(extents.height.raw() / 2))
                .ok_or(RenderError::CoordinateOverflow)?,
            width: extents.width,
            height: extents.height,
        };
        push_chart_text_with_style(
            nodes,
            label.text,
            centered_chart_text_bounds(paint_bounds, metrics)?,
            rect,
            TextAnchor::Middle,
            0,
            &text_styles.category_axis_labels,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    for label in labels {
        let metrics = measure_chart_text_with_style(
            &label.text,
            &text_styles.data_labels,
            options,
            Some((typography_stats, warnings, warning_cell)),
        )?;
        let extents = metrics.rotated(text_styles.data_labels.rotation_degrees(0))?;
        let paint_bounds = Rect {
            x: label
                .x
                .checked_sub(Fixed::from_raw(extents.width.raw() / 2))
                .ok_or(RenderError::CoordinateOverflow)?,
            y: label
                .y
                .checked_sub(Fixed::from_raw(extents.height.raw() / 2))
                .ok_or(RenderError::CoordinateOverflow)?,
            width: extents.width,
            height: extents.height,
        };
        push_chart_text_with_style(
            nodes,
            label.text,
            centered_chart_text_bounds(paint_bounds, metrics)?,
            rect,
            TextAnchor::Middle,
            0,
            &text_styles.data_labels,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    Ok(true)
}

pub(super) fn chart_frame_padding(
    width: Fixed,
    imported_calc_single_page_chart: bool,
) -> Result<Fixed, RenderError> {
    if !imported_calc_single_page_chart {
        return Ok(Fixed::from_pixels(8));
    }

    // Calc's SinglePageSheets replay places imported axis-label ink at roughly
    // two percent of the chart width. Our logical text bounds precede that ink
    // by half of the standard four-pixel chart text gap.
    Fixed::from_raw(width.raw() / 50)
        .checked_sub(Fixed::from_pixels(2))
        .map(|padding| padding.max(Fixed::ZERO))
        .ok_or(RenderError::CoordinateOverflow)
}

pub(super) struct ResolvedChartSeries {
    pub(super) name: String,
    pub(super) values: Vec<f64>,
    pub(super) x_values: Option<Vec<f64>>,
    pub(super) labels: Vec<String>,
    pub(super) bubble_sizes: Option<Vec<f64>>,
    pub(super) style: ChartSeriesStyle,
}

pub(super) struct ChartLabel {
    pub(super) text: String,
    pub(super) x: Fixed,
    pub(super) y: Fixed,
}

#[cfg(test)]
pub(super) const CALC_MISSING_THEME_CHART_LATIN_FAMILY: &str = "Liberation Sans";
const CHART_BODY_TEXT_POINTS: f32 = 10.0;
const CHART_TITLE_TEXT_POINTS: f32 = 18.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChartTextRole {
    ChartTitle,
    AxisTitle,
    AxisLabel,
    Legend,
    DataLabel,
}

impl ChartTextRole {
    pub(super) fn points(self) -> f32 {
        match self {
            Self::ChartTitle => CHART_TITLE_TEXT_POINTS,
            Self::AxisTitle | Self::AxisLabel | Self::Legend | Self::DataLabel => {
                CHART_BODY_TEXT_POINTS
            }
        }
    }

    pub(super) fn size(self) -> Fixed {
        points_to_fixed(self.points()).expect("static chart point size is valid")
    }

    pub(super) fn bold(self) -> bool {
        matches!(self, Self::ChartTitle | Self::AxisTitle)
    }

    #[cfg(test)]
    pub(super) fn style(
        self,
        family: &str,
        anchor: TextAnchor,
        rotation_degrees: i16,
    ) -> TextStyle {
        ResolvedChartTextStyle::for_role(self, family).text_style(anchor, rotation_degrees)
    }

    #[cfg(test)]
    pub(super) fn resolved_style(self, family: &str) -> ResolvedRunStyle {
        ResolvedChartTextStyle::for_role(self, family).resolved_run_style()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedChartTextStyle {
    pub(super) family: String,
    pub(super) size: Fixed,
    pub(super) size_hundredths_of_point: u32,
    pub(super) color: Rgb,
    pub(super) bold: bool,
    pub(super) italic: bool,
    pub(super) underline: bool,
    pub(super) strikethrough: bool,
    pub(super) kerning_minimum_hundredths_of_point: Option<u32>,
    pub(super) rotation_degrees: Option<i16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedChartTextStyles {
    chart_title: ResolvedChartTextStyle,
    category_axis_title: ResolvedChartTextStyle,
    value_axis_title: ResolvedChartTextStyle,
    legend: ResolvedChartTextStyle,
    category_axis_labels: ResolvedChartTextStyle,
    value_axis_labels: ResolvedChartTextStyle,
    data_labels: ResolvedChartTextStyle,
}

impl ResolvedChartTextStyles {
    fn resolve(metadata: Option<&DrawingMetadata>, fallback_family: &str) -> Option<Self> {
        let imported = metadata.map(|metadata| &metadata.chart_text_styles);
        let resolve = |style: Option<&ChartTextStyle>, role| match style {
            Some(style) => ResolvedChartTextStyle::imported(style),
            None => Some(ResolvedChartTextStyle::for_role(role, fallback_family)),
        };
        Some(Self {
            chart_title: resolve(
                imported.and_then(|styles| styles.chart_title.as_ref()),
                ChartTextRole::ChartTitle,
            )?,
            category_axis_title: resolve(
                imported.and_then(|styles| styles.category_axis_title.as_ref()),
                ChartTextRole::AxisTitle,
            )?,
            value_axis_title: resolve(
                imported.and_then(|styles| styles.value_axis_title.as_ref()),
                ChartTextRole::AxisTitle,
            )?,
            legend: resolve(
                imported.and_then(|styles| styles.legend.as_ref()),
                ChartTextRole::Legend,
            )?,
            category_axis_labels: resolve(
                imported.and_then(|styles| styles.category_axis_labels.as_ref()),
                ChartTextRole::AxisLabel,
            )?,
            value_axis_labels: resolve(
                imported.and_then(|styles| styles.value_axis_labels.as_ref()),
                ChartTextRole::AxisLabel,
            )?,
            data_labels: resolve(
                imported.and_then(|styles| styles.data_labels.as_ref()),
                ChartTextRole::DataLabel,
            )?,
        })
    }

    fn physical_axis_titles(
        &self,
        horizontal_bar: bool,
    ) -> (&ResolvedChartTextStyle, &ResolvedChartTextStyle) {
        if horizontal_bar {
            (&self.value_axis_title, &self.category_axis_title)
        } else {
            (&self.category_axis_title, &self.value_axis_title)
        }
    }
}

impl ResolvedChartTextStyle {
    pub(super) fn for_role(role: ChartTextRole, family: &str) -> Self {
        let size_hundredths_of_point = match role {
            ChartTextRole::ChartTitle => 1_800,
            ChartTextRole::AxisTitle
            | ChartTextRole::AxisLabel
            | ChartTextRole::Legend
            | ChartTextRole::DataLabel => 1_000,
        };
        Self {
            family: family.to_string(),
            size: role.size(),
            size_hundredths_of_point,
            color: Rgb::BLACK,
            bold: role.bold(),
            italic: false,
            underline: false,
            strikethrough: false,
            kerning_minimum_hundredths_of_point: None,
            rotation_degrees: None,
        }
    }

    pub(super) fn imported(style: &ChartTextStyle) -> Option<Self> {
        if style.latin_font_family.trim().is_empty()
            || style.latin_font_family.len() > 255
            || !(100..=400_000).contains(&style.size_hundredths_of_point)
            || style
                .kerning_minimum_hundredths_of_point
                .is_some_and(|value| value > 400_000)
        {
            return None;
        }
        Some(Self {
            family: style.latin_font_family.clone(),
            size: chart_text_size_to_fixed(style.size_hundredths_of_point)?,
            size_hundredths_of_point: style.size_hundredths_of_point,
            color: rgb(style.color),
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            strikethrough: style.strikethrough,
            kerning_minimum_hundredths_of_point: style.kerning_minimum_hundredths_of_point,
            rotation_degrees: style.rotation_degrees,
        })
    }

    pub(super) fn text_style(
        &self,
        anchor: TextAnchor,
        fallback_rotation_degrees: i16,
    ) -> TextStyle {
        TextStyle {
            family: self.family.clone(),
            size: self.size,
            color: self.color,
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            strikethrough: self.strikethrough,
            anchor,
            baseline: TextBaseline::Middle,
            rotation_degrees: self.rotation_degrees(fallback_rotation_degrees),
        }
    }

    pub(super) fn resolved_run_style(&self) -> ResolvedRunStyle {
        ResolvedRunStyle {
            family: self.family.clone(),
            size: self.size,
            color: self.color,
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            strikethrough: self.strikethrough,
            script: FormatScript::None,
        }
    }

    pub(super) fn kerning(&self) -> bool {
        self.kerning_minimum_hundredths_of_point
            .is_none_or(|minimum| self.size_hundredths_of_point >= minimum)
    }

    pub(super) fn rotation_degrees(&self, fallback_rotation_degrees: i16) -> i16 {
        normalize_chart_text_rotation(self.rotation_degrees.unwrap_or(fallback_rotation_degrees))
    }
}

fn normalize_chart_text_rotation(rotation_degrees: i16) -> i16 {
    let normalized = rotation_degrees.rem_euclid(360);
    if normalized > 180 {
        normalized - 360
    } else {
        normalized
    }
}

// Q62 sine coefficients for each whole degree in the closed first quadrant.
// Non-cardinal values are rounded toward positive infinity. Combining those
// coefficients with an outward integer division therefore cannot shrink the
// exact axis-aligned bounds of rotated chart text, while avoiding libm and its
// platform-dependent boundary behavior entirely.
pub(super) const CHART_TEXT_TRIG_SCALE: u128 = 1_u128 << 62;
pub(super) const CHART_TEXT_SINE_Q62_CEIL: [u64; 91] = [
    0,
    80_485_018_754_732_518,
    160_945_520_993_076_460,
    241_356_997_666_611_640,
    321_694_954_660_579_833,
    401_934_920_255_029_412,
    482_052_452_579_138_307,
    562_023_147_056_444_632,
    641_822_643_838_717_085,
    721_426_635_226_200_705,
    800_810_873_071_977_682,
    879_951_176_168_187_814,
    958_823_437_611_858_663,
    1_037_403_632_148_101_736,
    1_115_667_823_488_437_854,
    1_193_592_171_602_022_506,
    1_271_152_939_977_550_183,
    1_348_326_502_853_625_666,
    1_425_089_352_415_399_811,
    1_501_418_105_955_277_681,
    1_577_289_512_995_517_789,
    1_652_680_462_370_552_856,
    1_727_567_989_266_874_720,
    1_801_929_282_218_338_998,
    1_875_741_690_054_758_654,
    1_948_982_728_801_669_865,
    2_021_630_088_529_168_447,
    2_093_661_640_147_730_629,
    2_165_055_442_148_948_087,
    2_235_789_747_289_123_961,
    2_305_843_009_213_693_952,
    2_375_193_889_020_454_652,
    2_443_821_261_759_599_862,
    2_511_704_222_868_584_951,
    2_578_822_094_539_859_112,
    2_645_154_432_019_525_853,
    2_710_681_029_835_013_084,
    2_775_381_927_949_855_802,
    2_839_237_417_843_716_553,
    2_902_228_048_515_791_645,
    2_964_334_632_409_774_424,
    3_025_538_251_258_570_794,
    3_085_820_261_846_986_643,
    3_145_162_301_690_631_791,
    3_203_546_294_629_310_615,
    3_260_954_456_333_195_554,
    3_317_369_299_720_106_254,
    3_372_773_640_282_244_205,
    3_427_150_601_320_760_279,
    3_480_483_619_086_560_694,
    3_532_756_447_825_785_445,
    3_583_953_164_728_422_300,
    3_634_058_174_778_548_983,
    3_683_056_215_504_726_095,
    3_730_932_361_629_093_763,
    3_777_672_029_613_755_864,
    3_823_260_982_103_066_937,
    3_867_685_332_260_468_641,
    3_910_931_547_998_554_687,
    3_952_986_456_101_075_756,
    3_993_837_246_235_628_776,
    4_033_471_474_855_808_273,
    4_071_877_068_991_631_147,
    4_109_042_329_927_080_280,
    4_144_955_936_763_646_770,
    4_179_606_949_868_785_275,
    4_212_984_814_208_232_070,
    4_245_079_362_561_170_726,
    4_275_880_818_617_266_068,
    4_305_379_799_954_623_004,
    4_333_567_320_897_763_126,
    4_360_434_795_254_748_507,
    4_385_974_038_932_618_941,
    4_410_177_272_430_345_940,
    4_433_037_123_208_544_108,
    4_454_546_627_935_218_059,
    4_474_699_234_606_860_798,
    4_493_488_804_544_257_457,
    4_510_909_614_262_386_454,
    4_526_956_357_213_848_474,
    4_541_624_145_405_292_213,
    4_554_908_510_886_344_501,
    4_566_805_407_110_591_272,
    4_577_311_210_168_194_800,
    4_586_422_719_889_771_738,
    4_594_137_160_821_195_717,
    4_600_452_183_069_027_559,
    4_605_365_863_016_315_580,
    4_608_876_703_908_547_948,
    4_610_983_636_309_578_612,
    4_611_686_018_427_387_904,
];

fn chart_text_size_to_fixed(size_hundredths_of_point: u32) -> Option<Fixed> {
    if !(100..=400_000).contains(&size_hundredths_of_point) {
        return None;
    }
    let raw = u64::from(size_hundredths_of_point)
        .checked_mul(4)?
        .checked_mul(FIXED_UNITS_PER_PIXEL as u64)?
        .checked_add(150)?
        .checked_div(300)?;
    i64::try_from(raw).ok().map(Fixed::from_raw)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ChartTextMetrics {
    pub(super) width: Fixed,
    pub(super) height: Fixed,
}

#[derive(Debug, Clone, Copy)]
struct ChartLegendLayout {
    row_height: Fixed,
    rows_per_column: usize,
    column_step: Fixed,
    plot_offset: Fixed,
    total_width: Fixed,
}

impl ChartTextMetrics {
    pub(super) fn max(self, other: Self) -> Self {
        Self {
            width: self.width.max(other.width),
            height: self.height.max(other.height),
        }
    }

    pub(super) fn rotated(self, rotation_degrees: i16) -> Result<Self, RenderError> {
        let normalized = rotation_degrees.rem_euclid(360);
        match normalized {
            0 | 180 => Ok(self),
            90 | 270 => Ok(Self {
                width: self.height,
                height: self.width,
            }),
            degrees => {
                let first_half = degrees.min(360 - degrees);
                let acute = first_half.min(180 - first_half) as usize;
                let sine = CHART_TEXT_SINE_Q62_CEIL[acute];
                let cosine = CHART_TEXT_SINE_Q62_CEIL[90 - acute];
                Ok(Self {
                    width: chart_text_rotated_extent(self.width, cosine, self.height, sine)?,
                    height: chart_text_rotated_extent(self.width, sine, self.height, cosine)?,
                })
            }
        }
    }
}

fn chart_text_rotated_extent(
    primary: Fixed,
    primary_coefficient: u64,
    secondary: Fixed,
    secondary_coefficient: u64,
) -> Result<Fixed, RenderError> {
    let primary = u128::try_from(primary.raw()).map_err(|_| RenderError::CoordinateOverflow)?;
    let secondary = u128::try_from(secondary.raw()).map_err(|_| RenderError::CoordinateOverflow)?;
    let numerator = primary
        .checked_mul(u128::from(primary_coefficient))
        .and_then(|value| {
            secondary
                .checked_mul(u128::from(secondary_coefficient))
                .and_then(|secondary| value.checked_add(secondary))
        })
        .ok_or(RenderError::CoordinateOverflow)?;
    let extent = numerator.div_ceil(CHART_TEXT_TRIG_SCALE);
    Ok(Fixed::from_raw(
        i64::try_from(extent).map_err(|_| RenderError::CoordinateOverflow)?,
    ))
}

fn centered_chart_text_bounds(
    container: Rect,
    metrics: ChartTextMetrics,
) -> Result<Rect, RenderError> {
    let x_offset = container
        .width
        .raw()
        .checked_sub(metrics.width.raw())
        .ok_or(RenderError::CoordinateOverflow)?
        / 2;
    let y_offset = container
        .height
        .raw()
        .checked_sub(metrics.height.raw())
        .ok_or(RenderError::CoordinateOverflow)?
        / 2;
    Ok(Rect {
        x: container
            .x
            .checked_add(Fixed::from_raw(x_offset))
            .ok_or(RenderError::CoordinateOverflow)?,
        y: container
            .y
            .checked_add(Fixed::from_raw(y_offset))
            .ok_or(RenderError::CoordinateOverflow)?,
        width: metrics.width,
        height: metrics.height,
    })
}

#[cfg(test)]
pub(super) fn measure_chart_text(
    text: &str,
    role: ChartTextRole,
    family: &str,
    options: &RenderOptions,
    accounting: Option<(&mut TypographyStats, &mut Warnings, CellCoordinate)>,
) -> Result<ChartTextMetrics, RenderError> {
    let style = ResolvedChartTextStyle::for_role(role, family);
    measure_chart_text_with_style(text, &style, options, accounting)
}

pub(super) fn measure_chart_text_with_style(
    text: &str,
    chart_style: &ResolvedChartTextStyle,
    options: &RenderOptions,
    mut accounting: Option<(&mut TypographyStats, &mut Warnings, CellCoordinate)>,
) -> Result<ChartTextMetrics, RenderError> {
    if text.is_empty() {
        return Ok(ChartTextMetrics::default());
    }
    if let Some((stats, _, _)) = accounting.as_mut() {
        let scalar_count = text.chars().count() as u64;
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
    }
    let style = chart_style.resolved_run_style();
    let (width, height) = if let Some(pack) = options.font_pack.as_ref() {
        let shaped = shape_text_with_kerning(
            pack,
            text,
            style.request(),
            text_base_direction(text, false),
            chart_style.kerning(),
            options,
        )?;
        if let Some((stats, warnings, warning_cell)) = accounting.as_mut() {
            account_shaping(pack, &shaped, options, stats)?;
            warnings.add_count(
                WarningCode::MissingGlyph,
                shaped.missing_glyphs as u64,
                Some(*warning_cell),
            );
            if !shaped.requested_family_matched {
                warnings.add(WarningCode::FontFamilySubstituted, Some(*warning_cell));
            }
        }
        let width = shaped_width(pack, &shaped, style.size)?;
        let metrics = styled_line_metrics(
            pack,
            &shaped,
            std::slice::from_ref(&style),
            CellLineLayoutPolicy::Native,
            CalcLinePlacementPolicy::Native,
            text,
            1,
            1,
            options,
        )?;
        (
            width,
            line_height_from_metrics(metrics, CalcLinePlacementPolicy::Native)?,
        )
    } else {
        // Preserve deterministic, backend-neutral geometry for callers that
        // deliberately render without a verified pack. Hosted release paths
        // use the shaped branch above.
        let advance_units =
            helvetica_text_advance_units(text).ok_or(RenderError::CoordinateOverflow)?;
        let width = style
            .size
            .raw()
            .checked_mul(advance_units)
            .and_then(|value| value.checked_div(1_000))
            .map(Fixed::from_raw)
            .ok_or(RenderError::CoordinateOverflow)?;
        (width, scale_ratio(style.size, 6, 5)?)
    };
    if let Some((stats, _, _)) = accounting.as_mut() {
        stats.text_lines = stats
            .text_lines
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(
            LimitKind::TextLines,
            options.limits.max_text_lines,
            stats.text_lines,
        )?;
    }
    let padding = Fixed::from_pixels(4);
    Ok(ChartTextMetrics {
        width: width
            .checked_add(padding)
            .ok_or(RenderError::CoordinateOverflow)?,
        height: height
            .checked_add(padding)
            .ok_or(RenderError::CoordinateOverflow)?,
    })
}

pub(super) fn max_chart_text_metrics_with_style<'a>(
    texts: impl IntoIterator<Item = &'a str>,
    style: &ResolvedChartTextStyle,
    options: &RenderOptions,
    typography_stats: &mut TypographyStats,
    warnings: &mut Warnings,
    warning_cell: CellCoordinate,
) -> Result<ChartTextMetrics, RenderError> {
    texts
        .into_iter()
        .try_fold(ChartTextMetrics::default(), |metrics, text| {
            Ok(metrics.max(measure_chart_text_with_style(
                text,
                style,
                options,
                Some((typography_stats, warnings, warning_cell)),
            )?))
        })
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct NiceChartAxis {
    pub(super) minimum: f64,
    pub(super) maximum: f64,
    pub(super) major: f64,
    pub(super) ticks: Vec<f64>,
}

const CHART_AXIS_TARGET_INTERVALS: f64 = 8.0;
const MAX_CHART_AXIS_INTERVALS: usize = 12;
pub(super) const MAX_CHART_CATEGORY_LABELS: usize = 64;

fn nice_chart_step(value: f64) -> Option<f64> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let exponent = value.log10().floor();
    let magnitude = 10_f64.powf(exponent);
    if !magnitude.is_finite() || magnitude <= 0.0 {
        return None;
    }
    let normalized = value / magnitude;
    let factor = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    let step = factor * magnitude;
    (step.is_finite() && step > 0.0).then_some(step)
}

fn chart_nice_axis_with_target(
    values: impl Iterator<Item = f64>,
    force_zero: bool,
    target_intervals: f64,
) -> Option<NiceChartAxis> {
    if !target_intervals.is_finite() || target_intervals <= 0.0 {
        return None;
    }
    let mut raw_minimum = f64::INFINITY;
    let mut raw_maximum = f64::NEG_INFINITY;
    for value in values {
        if !value.is_finite() {
            return None;
        }
        raw_minimum = raw_minimum.min(value);
        raw_maximum = raw_maximum.max(value);
    }
    if !raw_minimum.is_finite() || !raw_maximum.is_finite() {
        return None;
    }
    if raw_maximum <= raw_minimum {
        let padding = raw_maximum.abs().max(1.0) * 0.5;
        if !padding.is_finite() || padding <= 0.0 {
            return None;
        }
        raw_minimum -= padding;
        raw_maximum += padding;
        if !raw_minimum.is_finite() || !raw_maximum.is_finite() || raw_maximum <= raw_minimum {
            return None;
        }
    }
    let include_zero = force_zero
        || (raw_minimum >= 0.0 && raw_minimum <= raw_maximum * 0.5)
        || (raw_maximum <= 0.0 && raw_maximum >= raw_minimum * 0.5);
    let data_minimum = if include_zero {
        raw_minimum.min(0.0)
    } else {
        raw_minimum
    };
    let data_maximum = if include_zero {
        raw_maximum.max(0.0)
    } else {
        raw_maximum
    };
    let span = data_maximum - data_minimum;
    if !span.is_finite() || span <= 0.0 {
        return None;
    }
    let step_input = span / target_intervals;
    let major = nice_chart_step(if step_input > 0.0 { step_input } else { span })?;
    let padding = span * 0.05;
    if !padding.is_finite() {
        return None;
    }
    let padded_minimum = if include_zero && data_minimum == 0.0 {
        0.0
    } else {
        data_minimum - padding
    };
    let padded_maximum = if include_zero && data_maximum == 0.0 {
        0.0
    } else {
        data_maximum + padding
    };
    if !padded_minimum.is_finite() || !padded_maximum.is_finite() {
        return None;
    }
    let mut minimum = (padded_minimum / major).floor() * major;
    let mut maximum = (padded_maximum / major).ceil() * major;
    if !minimum.is_finite() || !maximum.is_finite() {
        return None;
    }
    if include_zero {
        minimum = minimum.min(0.0);
        maximum = maximum.max(0.0);
    }
    if maximum <= minimum {
        maximum = minimum + major;
    }
    let axis_span = maximum - minimum;
    if !axis_span.is_finite() || axis_span <= 0.0 {
        return None;
    }
    let interval_count = (axis_span / major).round();
    if !interval_count.is_finite() || interval_count <= 0.0 {
        return None;
    }
    if interval_count > MAX_CHART_AXIS_INTERVALS as f64 {
        return None;
    }
    let intervals = interval_count as usize;
    maximum = minimum + major * intervals as f64;
    let final_span = maximum - minimum;
    if !maximum.is_finite()
        || !final_span.is_finite()
        || final_span <= 0.0
        || minimum > data_minimum
        || maximum < data_maximum
    {
        return None;
    }
    let ticks = (0..=intervals)
        .map(|index| {
            let value = minimum + major * index as f64;
            if !value.is_finite() {
                None
            } else if value.abs() < major.abs() * 1e-10 {
                Some(0.0)
            } else {
                Some(value)
            }
        })
        .collect::<Option<Vec<_>>>()?;
    if ticks.windows(2).any(|pair| pair[1] <= pair[0]) {
        return None;
    }
    Some(NiceChartAxis {
        minimum,
        maximum,
        major,
        ticks,
    })
}

fn chart_nice_axis(values: impl Iterator<Item = f64>, force_zero: bool) -> Option<NiceChartAxis> {
    chart_nice_axis_with_target(values, force_zero, CHART_AXIS_TARGET_INTERVALS)
}

pub(super) fn chart_nice_value_axis(
    series: &[ResolvedChartSeries],
    force_zero: bool,
) -> Option<NiceChartAxis> {
    chart_nice_axis(
        series
            .iter()
            .flat_map(|series| series.values.iter().copied()),
        force_zero,
    )
}

pub(super) fn chart_calc_imported_line_value_axis(
    series: &[ResolvedChartSeries],
) -> Option<NiceChartAxis> {
    // Calc changes a positive imported line chart from ten-unit to
    // twenty-unit ticks immediately above 85. Using 8.5 target intervals
    // reproduces the verified 85/86 boundary without changing authored,
    // bar, area, scatter, or bubble axes.
    chart_nice_axis_with_target(
        series
            .iter()
            .flat_map(|series| series.values.iter().copied()),
        true,
        8.5,
    )
}

pub(super) fn chart_nice_x_axis(series: &[ResolvedChartSeries]) -> Option<NiceChartAxis> {
    chart_nice_axis(
        series
            .iter()
            .filter_map(|series| series.x_values.as_ref())
            .flatten()
            .copied(),
        false,
    )
}

pub(super) fn chart_x_data_bounds(series: &[ResolvedChartSeries]) -> Option<(f64, f64)> {
    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for value in series
        .iter()
        .filter_map(|series| series.x_values.as_ref())
        .flatten()
    {
        if !value.is_finite() {
            return None;
        }
        minimum = minimum.min(*value);
        maximum = maximum.max(*value);
    }
    if !minimum.is_finite() || !maximum.is_finite() {
        return None;
    }
    if maximum <= minimum {
        let lower = minimum - 0.5;
        let upper = maximum + 0.5;
        (lower.is_finite() && upper.is_finite() && upper > lower).then_some((lower, upper))
    } else {
        Some((minimum, maximum))
    }
}

pub(super) fn chart_category_label_is_retained(index: usize, count: usize, stride: usize) -> bool {
    index < count && (index % stride.max(1) == 0 || index + 1 == count)
}

pub(super) fn chart_category_ratio(index: usize, count: usize, shifted: bool) -> f64 {
    if shifted {
        (index as f64 + 0.5) / count.max(1) as f64
    } else if count == 1 {
        0.5
    } else {
        index as f64 / (count - 1) as f64
    }
}

pub(super) fn chart_category_label_stride(count: usize) -> usize {
    if count <= MAX_CHART_CATEGORY_LABELS {
        1
    } else {
        (count - 1).div_ceil(MAX_CHART_CATEGORY_LABELS - 1)
    }
}

fn chart_axis_number(value: f64, major: f64) -> String {
    let decimal_places = if major.abs() >= 1.0 {
        0
    } else {
        (-major.abs().log10().floor() as i32 + 1).clamp(0, 12) as usize
    };
    let mut output = format!("{value:.decimal_places$}");
    if output.contains('.') {
        while output.ends_with('0') {
            output.pop();
        }
        if output.ends_with('.') {
            output.pop();
        }
    }
    if output == "-0" {
        "0".to_string()
    } else {
        output
    }
}

fn chart_color(index: usize, palette: &[Color]) -> Rgb {
    const COLORS: [Rgb; 8] = [
        Rgb::new(68, 114, 196),
        Rgb::new(237, 125, 49),
        Rgb::new(165, 165, 165),
        Rgb::new(255, 192, 0),
        Rgb::new(91, 155, 213),
        Rgb::new(112, 173, 71),
        Rgb::new(38, 68, 120),
        Rgb::new(158, 72, 14),
    ];
    if let Some(color) = palette.get(index % palette.len().max(1)) {
        let [red, green, blue] = color.as_rgb();
        Rgb::new(red, green, blue)
    } else {
        COLORS[index % COLORS.len()]
    }
}

fn light_chart_color(color: Rgb) -> Rgb {
    let lighten =
        |channel: u8| (u16::from(channel) + (u16::from(255_u8) - u16::from(channel)) * 3 / 5) as u8;
    Rgb::new(
        lighten(color.red),
        lighten(color.green),
        lighten(color.blue),
    )
}

fn chart_series_line_width(style: &ChartSeriesStyle) -> Fixed {
    let Some(width_emu) = style.line_width_emu else {
        return Fixed::from_pixels(1);
    };
    if width_emu == 0 {
        // LibreOffice imports an explicit zero chart-line width as a solid
        // zero-width drawing-layer stroke, whose width-zero convention is a
        // visible, view-dependent hairline. Normalize that convention at the
        // backend-neutral scene boundary so SVG, PDF, and raster output agree.
        return Fixed::from_pixels(1);
    }
    // 914,400 EMUs/in at 96 CSS px/in = 9,525 EMUs/CSS px.
    let raw = (u64::from(width_emu) * FIXED_UNITS_PER_PIXEL as u64 + 9_525 / 2) / 9_525;
    Fixed::from_raw(raw as i64)
}

fn chart_y(plot: Rect, value: f64, bounds: (f64, f64)) -> Result<Fixed, RenderError> {
    interpolate_fixed(
        plot.y
            .checked_add(plot.height)
            .ok_or(RenderError::CoordinateOverflow)?,
        Fixed::from_raw(-plot.height.raw()),
        (value - bounds.0) / (bounds.1 - bounds.0),
    )
}

fn chart_x(plot: Rect, ratio: f64) -> Result<Fixed, RenderError> {
    interpolate_fixed(plot.x, plot.width, ratio)
}

pub(super) fn chart_category_data_plot(
    plot: Rect,
    category_axis_shifted: bool,
    horizontal_bar: bool,
) -> Result<Rect, RenderError> {
    if !category_axis_shifted || horizontal_bar {
        return Ok(plot);
    }
    // Calc keeps the category axis and value-axis bounds at the diagram edge,
    // but places shifted series bands three pixels inside that rectangle.
    let inset = Fixed::from_pixels(3);
    let double_inset = Fixed::from_raw(
        inset
            .raw()
            .checked_mul(2)
            .ok_or(RenderError::CoordinateOverflow)?,
    );
    let width = plot
        .width
        .checked_sub(double_inset)
        .ok_or(RenderError::CoordinateOverflow)?;
    if width <= Fixed::ZERO {
        return Ok(plot);
    }
    Ok(Rect {
        x: plot
            .x
            .checked_add(inset)
            .ok_or(RenderError::CoordinateOverflow)?,
        y: plot.y,
        width,
        height: plot.height,
    })
}

#[allow(clippy::too_many_arguments)]
fn push_cartesian_chart_axes(
    nodes: &mut Vec<SceneNode>,
    chart_clip_bounds: Rect,
    plot: Rect,
    chart_kind: ChartKind,
    horizontal_bar: bool,
    axis: &NiceChartAxis,
    x_value_axis: Option<&NiceChartAxis>,
    x_data_bounds: Option<(f64, f64)>,
    series: &[ResolvedChartSeries],
    category_axis_visible: bool,
    category_axis_shifted: bool,
    value_axis_visible: bool,
    category_major_gridlines: bool,
    value_major_gridlines: bool,
    axis_lines_visible: bool,
    category_axis_style: &ResolvedChartTextStyle,
    value_axis_style: &ResolvedChartTextStyle,
    text_bytes: &mut u64,
    glyphs: &mut u64,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
    warnings: &mut Warnings,
    warning_cell: CellCoordinate,
) -> Result<(), RenderError> {
    let category_plot = chart_category_data_plot(plot, category_axis_shifted, horizontal_bar)?;
    let category_label_plot = if category_axis_shifted && !horizontal_bar {
        plot
    } else {
        category_plot
    };
    let plot_right = plot
        .x
        .checked_add(plot.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let plot_bottom = plot
        .y
        .checked_add(plot.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let grid = Rgb::new(217, 217, 217);
    let text_gap = Fixed::from_pixels(4);
    let category_label_gap = if category_axis_shifted && !horizontal_bar {
        Fixed::from_pixels(8)
    } else {
        text_gap
    };
    if value_axis_visible {
        for value in &axis.ticks {
            let label = chart_axis_number(*value, axis.major);
            let metrics = measure_chart_text_with_style(&label, value_axis_style, options, None)?;
            let extents = metrics.rotated(value_axis_style.rotation_degrees(0))?;
            if horizontal_bar {
                let x = chart_x(
                    plot,
                    (*value - axis.minimum) / (axis.maximum - axis.minimum),
                )?;
                if value_major_gridlines {
                    push_placeholder_line(nodes, x, plot.y, x, plot_bottom, grid, options)?;
                }
                let paint_bounds = Rect {
                    x: x.checked_sub(Fixed::from_raw(extents.width.raw() / 2))
                        .ok_or(RenderError::CoordinateOverflow)?,
                    y: plot_bottom
                        .checked_add(text_gap)
                        .ok_or(RenderError::CoordinateOverflow)?,
                    width: extents.width,
                    height: extents.height,
                };
                push_chart_text_with_style(
                    nodes,
                    label,
                    centered_chart_text_bounds(paint_bounds, metrics)?,
                    chart_clip_bounds,
                    TextAnchor::Middle,
                    0,
                    value_axis_style,
                    text_bytes,
                    glyphs,
                    typography_stats,
                    options,
                )?;
            } else {
                let y = chart_y(plot, *value, (axis.minimum, axis.maximum))?;
                if value_major_gridlines {
                    push_placeholder_line(nodes, plot.x, y, plot_right, y, grid, options)?;
                }
                let paint_bounds = Rect {
                    x: plot
                        .x
                        .checked_sub(text_gap)
                        .and_then(|value| value.checked_sub(extents.width))
                        .ok_or(RenderError::CoordinateOverflow)?,
                    y: y.checked_sub(Fixed::from_raw(extents.height.raw() / 2))
                        .ok_or(RenderError::CoordinateOverflow)?,
                    width: extents.width,
                    height: extents.height,
                };
                let (bounds, anchor) = if value_axis_style.rotation_degrees(0).rem_euclid(360) == 0
                {
                    (
                        Rect {
                            x: plot
                                .x
                                .checked_sub(text_gap)
                                .and_then(|value| value.checked_sub(metrics.width))
                                .ok_or(RenderError::CoordinateOverflow)?,
                            y: y.checked_sub(Fixed::from_raw(metrics.height.raw() / 2))
                                .ok_or(RenderError::CoordinateOverflow)?,
                            width: metrics.width,
                            height: metrics.height,
                        },
                        TextAnchor::End,
                    )
                } else {
                    (
                        centered_chart_text_bounds(paint_bounds, metrics)?,
                        TextAnchor::Middle,
                    )
                };
                push_chart_text_with_style(
                    nodes,
                    label,
                    bounds,
                    chart_clip_bounds,
                    anchor,
                    0,
                    value_axis_style,
                    text_bytes,
                    glyphs,
                    typography_stats,
                    options,
                )?;
            }
        }
    }
    if matches!(chart_kind, ChartKind::Scatter | ChartKind::Bubble) && category_axis_visible {
        let x_axis = x_value_axis.ok_or(RenderError::Typography {
            reason: "missing_chart_x_value_axis",
        })?;
        let x_data_bounds = x_data_bounds.ok_or(RenderError::Typography {
            reason: "missing_chart_x_data_bounds",
        })?;
        for value in x_axis
            .ticks
            .iter()
            .filter(|value| **value >= x_data_bounds.0 && **value <= x_data_bounds.1)
        {
            let x = chart_x(
                category_plot,
                (*value - x_axis.minimum) / (x_axis.maximum - x_axis.minimum),
            )?;
            if category_major_gridlines {
                push_placeholder_line(nodes, x, plot.y, x, plot_bottom, grid, options)?;
            }
            let label = chart_axis_number(*value, x_axis.major);
            let metrics =
                measure_chart_text_with_style(&label, category_axis_style, options, None)?;
            let extents = metrics.rotated(category_axis_style.rotation_degrees(0))?;
            let paint_bounds = Rect {
                x: x.checked_sub(Fixed::from_raw(extents.width.raw() / 2))
                    .ok_or(RenderError::CoordinateOverflow)?,
                y: plot_bottom
                    .checked_add(text_gap)
                    .ok_or(RenderError::CoordinateOverflow)?,
                width: extents.width,
                height: extents.height,
            };
            push_chart_text_with_style(
                nodes,
                label,
                centered_chart_text_bounds(paint_bounds, metrics)?,
                chart_clip_bounds,
                TextAnchor::Middle,
                0,
                category_axis_style,
                text_bytes,
                glyphs,
                typography_stats,
                options,
            )?;
        }
    }
    let physical_vertical_axis_visible = if horizontal_bar {
        category_axis_visible
    } else {
        value_axis_visible
    };
    let physical_horizontal_axis_visible = if horizontal_bar {
        value_axis_visible
    } else {
        category_axis_visible
    };
    if axis_lines_visible && physical_vertical_axis_visible {
        push_placeholder_line(
            nodes,
            plot.x,
            plot.y,
            plot.x,
            plot_bottom,
            Rgb::BLACK,
            options,
        )?;
    }
    if axis_lines_visible && physical_horizontal_axis_visible {
        push_placeholder_line(
            nodes,
            plot.x,
            plot_bottom,
            plot_right,
            plot_bottom,
            Rgb::BLACK,
            options,
        )?;
    }

    if matches!(chart_kind, ChartKind::Scatter | ChartKind::Bubble) {
        return Ok(());
    }
    if !category_axis_visible {
        return Ok(());
    }
    let Some(categories) = series.first().map(|series| series.labels.as_slice()) else {
        return Ok(());
    };
    if categories.is_empty() {
        return Ok(());
    }
    let stride = chart_category_label_stride(categories.len());
    let retained = categories
        .iter()
        .enumerate()
        .filter(|(index, _)| chart_category_label_is_retained(*index, categories.len(), stride))
        .count();
    warnings.add_count(
        WarningCode::ChartMetadataSimplified,
        categories.len().saturating_sub(retained) as u64,
        Some(warning_cell),
    );
    for (index, category) in categories.iter().enumerate() {
        if !chart_category_label_is_retained(index, categories.len(), stride) {
            continue;
        }
        let metrics = measure_chart_text_with_style(category, category_axis_style, options, None)?;
        let extents = metrics.rotated(category_axis_style.rotation_degrees(0))?;
        let paint_bounds = if horizontal_bar {
            let ratio = (index as f64 + 0.5) / categories.len() as f64;
            let y = interpolate_fixed(plot.y, plot.height, ratio)?;
            if category_major_gridlines {
                push_placeholder_line(nodes, plot.x, y, plot_right, y, grid, options)?;
            }
            Rect {
                x: plot
                    .x
                    .checked_sub(text_gap)
                    .and_then(|value| value.checked_sub(extents.width))
                    .ok_or(RenderError::CoordinateOverflow)?,
                y: y.checked_sub(Fixed::from_raw(extents.height.raw() / 2))
                    .ok_or(RenderError::CoordinateOverflow)?,
                width: extents.width,
                height: extents.height,
            }
        } else {
            let ratio = chart_category_ratio(
                index,
                categories.len(),
                category_axis_shifted || chart_kind == ChartKind::Bar,
            );
            let x = chart_x(category_label_plot, ratio)?;
            if category_major_gridlines {
                push_placeholder_line(nodes, x, plot.y, x, plot_bottom, grid, options)?;
            }
            Rect {
                x: x.checked_sub(Fixed::from_raw(extents.width.raw() / 2))
                    .ok_or(RenderError::CoordinateOverflow)?,
                y: plot_bottom
                    .checked_add(category_label_gap)
                    .ok_or(RenderError::CoordinateOverflow)?,
                width: extents.width,
                height: extents.height,
            }
        };
        let (bounds, anchor) =
            if horizontal_bar && category_axis_style.rotation_degrees(0).rem_euclid(360) == 0 {
                let ratio = (index as f64 + 0.5) / categories.len() as f64;
                let y = interpolate_fixed(plot.y, plot.height, ratio)?;
                (
                    Rect {
                        x: plot
                            .x
                            .checked_sub(text_gap)
                            .and_then(|value| value.checked_sub(metrics.width))
                            .ok_or(RenderError::CoordinateOverflow)?,
                        y: y.checked_sub(Fixed::from_raw(metrics.height.raw() / 2))
                            .ok_or(RenderError::CoordinateOverflow)?,
                        width: metrics.width,
                        height: metrics.height,
                    },
                    TextAnchor::End,
                )
            } else {
                (
                    centered_chart_text_bounds(paint_bounds, metrics)?,
                    TextAnchor::Middle,
                )
            };
        push_chart_text_with_style(
            nodes,
            category.clone(),
            bounds,
            chart_clip_bounds,
            anchor,
            0,
            category_axis_style,
            text_bytes,
            glyphs,
            typography_stats,
            options,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn push_line_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    bounds: (f64, f64),
    category_axis_shifted: bool,
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    for (series_index, series) in series.iter().enumerate() {
        let palette_color = chart_color(series_index, palette);
        let line_color = series.style.line_color.map_or(palette_color, |color| {
            let [red, green, blue] = color.as_rgb();
            Rgb::new(red, green, blue)
        });
        let mut previous = None;
        for (index, value) in series.values.iter().enumerate() {
            let ratio = chart_category_ratio(index, series.values.len(), category_axis_shifted);
            let x = chart_x(plot, ratio)?;
            let y = chart_y(plot, *value, bounds)?;
            if series.style.line_visible {
                if let Some((previous_x, previous_y)) = previous {
                    push_chart_series_line(
                        nodes,
                        previous_x,
                        previous_y,
                        x,
                        y,
                        line_color,
                        chart_series_line_width(&series.style),
                        options,
                    )?;
                }
            }
            previous = Some((x, y));
            push_chart_marker(
                nodes,
                x,
                y,
                line_color,
                &series.style,
                typography_stats,
                options,
            )?;
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*value),
                    x,
                    y: Fixed::from_raw(y.raw() - Fixed::from_pixels(8).raw()),
                });
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn push_scatter_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    y_bounds: (f64, f64),
    x_axis: &NiceChartAxis,
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    for (series_index, series) in series.iter().enumerate() {
        let x_values = series.x_values.as_ref().expect("scatter x values");
        let palette_color = chart_color(series_index, palette);
        let color = series.style.line_color.map_or(palette_color, |color| {
            let [red, green, blue] = color.as_rgb();
            Rgb::new(red, green, blue)
        });
        let draw_retained_line = series.style.line_visible
            && (series.style.line_width_emu.is_some() || series.style.line_color.is_some());
        let mut previous = None;
        for (x_value, y_value) in x_values.iter().zip(&series.values) {
            let x = chart_x(
                plot,
                (*x_value - x_axis.minimum) / (x_axis.maximum - x_axis.minimum),
            )?;
            let y = chart_y(plot, *y_value, y_bounds)?;
            if draw_retained_line {
                if let Some((previous_x, previous_y)) = previous {
                    push_chart_series_line(
                        nodes,
                        previous_x,
                        previous_y,
                        x,
                        y,
                        color,
                        chart_series_line_width(&series.style),
                        options,
                    )?;
                }
            }
            previous = Some((x, y));
            push_chart_marker(nodes, x, y, color, &series.style, typography_stats, options)?;
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*y_value),
                    x,
                    y: Fixed::from_raw(y.raw() - Fixed::from_pixels(8).raw()),
                });
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_column_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    bounds: (f64, f64),
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let categories = series
        .iter()
        .map(|series| series.values.len())
        .max()
        .unwrap_or(1);
    let series_count = series.len();
    let baseline = chart_y(plot, 0.0, bounds)?;
    for (series_index, series_item) in series.iter().enumerate() {
        for (index, value) in series_item.values.iter().enumerate() {
            let group_start = index as f64 / categories as f64;
            let group_end = (index + 1) as f64 / categories as f64;
            let group_span = group_end - group_start;
            let left_ratio =
                group_start + group_span * (0.1 + 0.8 * series_index as f64 / series_count as f64);
            let right_ratio = group_start
                + group_span * (0.1 + 0.8 * (series_index + 1) as f64 / series_count as f64);
            let left = chart_x(plot, left_ratio)?;
            let right = chart_x(plot, right_ratio)?;
            let value_y = chart_y(plot, *value, bounds)?;
            push_solid_rect(
                nodes,
                left,
                value_y.min(baseline),
                right,
                value_y.max(baseline),
                chart_color(series_index, palette),
                options,
            )?;
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*value),
                    x: Fixed::from_raw(left.raw() + (right.raw() - left.raw()) / 2),
                    y: Fixed::from_raw(value_y.min(baseline).raw() - Fixed::from_pixels(8).raw()),
                });
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_horizontal_bar_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    bounds: (f64, f64),
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let categories = series
        .iter()
        .map(|series| series.values.len())
        .max()
        .unwrap_or(1);
    let series_count = series.len();
    let value_x = |value| chart_x(plot, (value - bounds.0) / (bounds.1 - bounds.0));
    let baseline = value_x(0.0)?;
    let plot_bottom = plot
        .y
        .checked_add(plot.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    push_placeholder_line(
        nodes,
        baseline,
        plot.y,
        baseline,
        plot_bottom,
        Rgb::BLACK,
        options,
    )?;
    for (series_index, series_item) in series.iter().enumerate() {
        for (index, value) in series_item.values.iter().enumerate() {
            let group_start = index as f64 / categories as f64;
            let group_end = (index + 1) as f64 / categories as f64;
            let group_span = group_end - group_start;
            let top_ratio =
                group_start + group_span * (0.1 + 0.8 * series_index as f64 / series_count as f64);
            let bottom_ratio = group_start
                + group_span * (0.1 + 0.8 * (series_index + 1) as f64 / series_count as f64);
            let top = interpolate_fixed(plot.y, plot.height, top_ratio)?;
            let bottom = interpolate_fixed(plot.y, plot.height, bottom_ratio)?;
            let end = value_x(*value)?;
            push_solid_rect(
                nodes,
                end.min(baseline),
                top,
                end.max(baseline),
                bottom,
                chart_color(series_index, palette),
                options,
            )?;
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*value),
                    x: end,
                    y: Fixed::from_raw(top.raw() + (bottom.raw() - top.raw()) / 2),
                });
            }
        }
    }
    Ok(())
}

fn push_chart_path(
    nodes: &mut Vec<SceneNode>,
    commands: Vec<PathCommand>,
    fill: Option<Rgb>,
    stroke: Option<Rgb>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    push_chart_path_with_width(
        nodes,
        commands,
        fill,
        stroke,
        Fixed::from_pixels(1),
        typography_stats,
        options,
    )
}

fn preflight_chart_path_commands(
    typography_stats: &TypographyStats,
    additional: usize,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let actual = typography_stats
        .path_commands
        .checked_add(u64::try_from(additional).map_err(|_| RenderError::CoordinateOverflow)?)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::PathCommands,
        options.limits.max_path_commands,
        actual,
    )
}

#[allow(clippy::too_many_arguments)]
fn push_chart_path_with_width(
    nodes: &mut Vec<SceneNode>,
    commands: Vec<PathCommand>,
    fill: Option<Rgb>,
    stroke: Option<Rgb>,
    stroke_width: Fixed,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    typography_stats.path_commands = typography_stats
        .path_commands
        .checked_add(commands.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::PathCommands,
        options.limits.max_path_commands,
        typography_stats.path_commands,
    )?;
    push_node(
        nodes,
        SceneNode::Path(PathNode {
            commands,
            fill,
            stroke,
            stroke_width,
        }),
        options,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn push_area_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    bounds: (f64, f64),
    category_axis_shifted: bool,
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let baseline = chart_y(plot, 0.0, bounds)?;
    // Draw later series first so the first (primary) series remains visible,
    // matching the foreground ordering used by common office renderers.
    for (series_index, series) in series.iter().enumerate().rev() {
        let first_ratio = chart_category_ratio(0, series.values.len(), category_axis_shifted);
        let last_ratio = chart_category_ratio(
            series.values.len().saturating_sub(1),
            series.values.len(),
            category_axis_shifted,
        );
        let first_x = chart_x(plot, first_ratio)?;
        let last_x = chart_x(plot, last_ratio)?;
        let mut commands = Vec::with_capacity(series.values.len().saturating_add(3));
        commands.push(PathCommand::MoveTo {
            x: first_x,
            y: baseline,
        });
        for (index, value) in series.values.iter().enumerate() {
            let ratio = chart_category_ratio(index, series.values.len(), category_axis_shifted);
            let x = chart_x(plot, ratio)?;
            let y = chart_y(plot, *value, bounds)?;
            commands.push(PathCommand::LineTo { x, y });
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*value),
                    x,
                    y: Fixed::from_raw(y.raw() - Fixed::from_pixels(8).raw()),
                });
            }
        }
        commands.push(PathCommand::LineTo {
            x: last_x,
            y: baseline,
        });
        commands.push(PathCommand::Close);
        let color = chart_color(series_index, palette);
        let line_color = series.style.line_color.map_or(color, |color| {
            let [red, green, blue] = color.as_rgb();
            Rgb::new(red, green, blue)
        });
        push_chart_path_with_width(
            nodes,
            commands,
            Some(light_chart_color(color)),
            series.style.line_visible.then_some(line_color),
            chart_series_line_width(&series.style),
            typography_stats,
            options,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_doughnut_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let center_x = Fixed::from_raw(plot.x.raw() + plot.width.raw() / 2);
    let center_y = Fixed::from_raw(plot.y.raw() + plot.height.raw() / 2);
    let outer_radius = fixed_as_pixels(plot.width.min(plot.height)) * 0.42;
    let hole_radius = outer_radius * 0.5;
    let ring_width = (outer_radius - hole_radius) / series.len() as f64;
    for (series_index, series) in series.iter().enumerate() {
        let outer = outer_radius - ring_width * series_index as f64;
        let inner = (outer - ring_width).max(hole_radius);
        let total = series.values.iter().sum::<f64>();
        let mut start = -std::f64::consts::FRAC_PI_2;
        for (index, value) in series.values.iter().enumerate() {
            if *value == 0.0 {
                continue;
            }
            let end = start + std::f64::consts::TAU * (*value / total);
            let segments = ((end - start).abs() / std::f64::consts::TAU * 64.0)
                .ceil()
                .max(1.0) as usize;
            let mut commands = Vec::with_capacity(segments.saturating_mul(2).saturating_add(4));
            commands.push(PathCommand::MoveTo {
                x: pixels_as_fixed(fixed_as_pixels(center_x) + outer * start.cos())?,
                y: pixels_as_fixed(fixed_as_pixels(center_y) + outer * start.sin())?,
            });
            for segment in 1..=segments {
                let angle = start + (end - start) * segment as f64 / segments as f64;
                commands.push(PathCommand::LineTo {
                    x: pixels_as_fixed(fixed_as_pixels(center_x) + outer * angle.cos())?,
                    y: pixels_as_fixed(fixed_as_pixels(center_y) + outer * angle.sin())?,
                });
            }
            for segment in (0..=segments).rev() {
                let angle = start + (end - start) * segment as f64 / segments as f64;
                commands.push(PathCommand::LineTo {
                    x: pixels_as_fixed(fixed_as_pixels(center_x) + inner * angle.cos())?,
                    y: pixels_as_fixed(fixed_as_pixels(center_y) + inner * angle.sin())?,
                });
            }
            commands.push(PathCommand::Close);
            push_chart_path(
                nodes,
                commands,
                Some(chart_color(index, palette)),
                Some(Rgb::WHITE),
                typography_stats,
                options,
            )?;
            if data_labels {
                let angle = (start + end) / 2.0;
                let radius = (inner + outer) / 2.0;
                labels.push(ChartLabel {
                    text: chart_number(*value),
                    x: pixels_as_fixed(fixed_as_pixels(center_x) + radius * angle.cos())?,
                    y: pixels_as_fixed(fixed_as_pixels(center_y) + radius * angle.sin())?,
                });
            }
            start = end;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn push_radar_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    axis: &NiceChartAxis,
    palette: &[Color],
    category_axis_visible: bool,
    value_axis_visible: bool,
    category_major_gridlines: bool,
    value_major_gridlines: bool,
    category_labels: &mut Vec<ChartLabel>,
    value_labels: &mut Vec<ChartLabel>,
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
    warnings: &mut Warnings,
    warning_cell: CellCoordinate,
) -> Result<(), RenderError> {
    let center_x = Fixed::from_raw(plot.x.raw() + plot.width.raw() / 2);
    let center_y = Fixed::from_raw(plot.y.raw() + plot.height.raw() / 2);
    let radius = fixed_as_pixels(plot.width.min(plot.height)) * 0.42;
    let categories = series
        .iter()
        .map(|series| series.values.len())
        .max()
        .unwrap_or(3);
    let polar = |index: usize, scale: f64| -> Result<(Fixed, Fixed), RenderError> {
        let angle =
            -std::f64::consts::FRAC_PI_2 + std::f64::consts::TAU * index as f64 / categories as f64;
        Ok((
            pixels_as_fixed(fixed_as_pixels(center_x) + radius * scale * angle.cos())?,
            pixels_as_fixed(fixed_as_pixels(center_y) + radius * scale * angle.sin())?,
        ))
    };
    let scale =
        |value: f64| ((value - axis.minimum) / (axis.maximum - axis.minimum)).clamp(0.0, 1.0);
    if value_axis_visible && value_major_gridlines {
        for value in &axis.ticks {
            let ring_scale = scale(*value);
            if ring_scale <= 0.0 {
                continue;
            }
            preflight_chart_path_commands(typography_stats, categories.saturating_add(1), options)?;
            let mut commands = Vec::with_capacity(categories.saturating_add(1));
            for index in 0..categories {
                let (x, y) = polar(index, ring_scale)?;
                commands.push(if index == 0 {
                    PathCommand::MoveTo { x, y }
                } else {
                    PathCommand::LineTo { x, y }
                });
            }
            commands.push(PathCommand::Close);
            push_chart_path(
                nodes,
                commands,
                None,
                Some(Rgb::new(205, 205, 205)),
                typography_stats,
                options,
            )?;
        }
    }
    if category_axis_visible && category_major_gridlines {
        for index in 0..categories {
            let (x, y) = polar(index, 1.0)?;
            push_placeholder_line(
                nodes,
                center_x,
                center_y,
                x,
                y,
                Rgb::new(205, 205, 205),
                options,
            )?;
        }
    }
    if category_axis_visible {
        if let Some(labels) = series.first().map(|series| series.labels.as_slice()) {
            let label_count = labels.len().min(categories);
            let stride = chart_category_label_stride(label_count);
            let retained = (0..label_count)
                .filter(|index| chart_category_label_is_retained(*index, label_count, stride))
                .count();
            warnings.add_count(
                WarningCode::ChartMetadataSimplified,
                label_count.saturating_sub(retained) as u64,
                Some(warning_cell),
            );
            for (index, text) in labels.iter().take(label_count).enumerate() {
                if !chart_category_label_is_retained(index, label_count, stride) {
                    continue;
                }
                let (x, y) = polar(index, 1.12)?;
                category_labels.push(ChartLabel {
                    text: text.clone(),
                    x,
                    y,
                });
            }
        }
    }
    if value_axis_visible {
        for value in &axis.ticks {
            let (x, y) = polar(0, scale(*value))?;
            value_labels.push(ChartLabel {
                text: chart_axis_number(*value, axis.major),
                x,
                y,
            });
        }
    }
    for (series_index, series) in series.iter().enumerate() {
        preflight_chart_path_commands(
            typography_stats,
            series.values.len().saturating_add(1),
            options,
        )?;
        let mut commands = Vec::with_capacity(series.values.len().saturating_add(2));
        for (index, value) in series.values.iter().enumerate() {
            let (x, y) = polar(index, scale(*value))?;
            commands.push(if index == 0 {
                PathCommand::MoveTo { x, y }
            } else {
                PathCommand::LineTo { x, y }
            });
            push_chart_marker(
                nodes,
                x,
                y,
                chart_color(series_index, palette),
                &series.style,
                typography_stats,
                options,
            )?;
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*value),
                    x,
                    y: Fixed::from_raw(y.raw() - Fixed::from_pixels(8).raw()),
                });
            }
        }
        commands.push(PathCommand::Close);
        push_chart_path(
            nodes,
            commands,
            None,
            Some(chart_color(series_index, palette)),
            typography_stats,
            options,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn push_bubble_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &[ResolvedChartSeries],
    y_bounds: (f64, f64),
    x_axis: &NiceChartAxis,
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let max_size = series
        .iter()
        .filter_map(|series| series.bubble_sizes.as_ref())
        .flatten()
        .copied()
        .fold(0.0_f64, f64::max);
    let maximum_radius = fixed_as_pixels(plot.width.min(plot.height)) * 0.08;
    for (series_index, series) in series.iter().enumerate() {
        let x_values = series.x_values.as_ref().expect("bubble x values");
        let sizes = series.bubble_sizes.as_ref().expect("bubble sizes");
        for ((x_value, y_value), size) in x_values.iter().zip(&series.values).zip(sizes) {
            let x = chart_x(
                plot,
                (*x_value - x_axis.minimum) / (x_axis.maximum - x_axis.minimum),
            )?;
            let y = chart_y(plot, *y_value, y_bounds)?;
            let radius = (maximum_radius * (*size / max_size).sqrt()).max(2.0);
            let segments = 24_usize;
            let mut commands = Vec::with_capacity(segments + 2);
            for segment in 0..segments {
                let angle = std::f64::consts::TAU * segment as f64 / segments as f64;
                let point_x = pixels_as_fixed(fixed_as_pixels(x) + radius * angle.cos())?;
                let point_y = pixels_as_fixed(fixed_as_pixels(y) + radius * angle.sin())?;
                commands.push(if segment == 0 {
                    PathCommand::MoveTo {
                        x: point_x,
                        y: point_y,
                    }
                } else {
                    PathCommand::LineTo {
                        x: point_x,
                        y: point_y,
                    }
                });
            }
            commands.push(PathCommand::Close);
            let color = chart_color(series_index, palette);
            push_chart_path(
                nodes,
                commands,
                Some(light_chart_color(color)),
                Some(color),
                typography_stats,
                options,
            )?;
            if data_labels {
                labels.push(ChartLabel {
                    text: chart_number(*y_value),
                    x,
                    y: Fixed::from_raw(y.raw() - pixels_as_fixed(radius)?.raw()),
                });
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_pie_chart(
    nodes: &mut Vec<SceneNode>,
    plot: Rect,
    series: &ResolvedChartSeries,
    palette: &[Color],
    data_labels: bool,
    labels: &mut Vec<ChartLabel>,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let center_x = Fixed::from_raw(plot.x.raw() + plot.width.raw() / 2);
    let center_y = Fixed::from_raw(plot.y.raw() + plot.height.raw() / 2);
    let radius = fixed_as_pixels(plot.width.min(plot.height)) * 0.42;
    let total = series.values.iter().sum::<f64>();
    let mut start = -std::f64::consts::FRAC_PI_2;
    for (index, value) in series.values.iter().enumerate() {
        if *value == 0.0 {
            continue;
        }
        let end = start + std::f64::consts::TAU * (*value / total);
        let segments = ((end - start).abs() / std::f64::consts::TAU * 64.0)
            .ceil()
            .max(1.0) as usize;
        let mut commands = Vec::with_capacity(segments + 3);
        commands.push(PathCommand::MoveTo {
            x: center_x,
            y: center_y,
        });
        for segment in 0..=segments {
            let angle = start + (end - start) * segment as f64 / segments as f64;
            commands.push(PathCommand::LineTo {
                x: pixels_as_fixed(fixed_as_pixels(center_x) + radius * angle.cos())?,
                y: pixels_as_fixed(fixed_as_pixels(center_y) + radius * angle.sin())?,
            });
        }
        commands.push(PathCommand::Close);
        typography_stats.path_commands = typography_stats
            .path_commands
            .checked_add(commands.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(
            LimitKind::PathCommands,
            options.limits.max_path_commands,
            typography_stats.path_commands,
        )?;
        push_node(
            nodes,
            SceneNode::Path(PathNode {
                commands,
                fill: Some(chart_color(index, palette)),
                stroke: Some(Rgb::WHITE),
                stroke_width: Fixed::from_pixels(1),
            }),
            options,
        )?;
        if data_labels {
            let angle = (start + end) / 2.0;
            labels.push(ChartLabel {
                text: chart_number(*value),
                x: pixels_as_fixed(fixed_as_pixels(center_x) + radius * 0.62 * angle.cos())?,
                y: pixels_as_fixed(fixed_as_pixels(center_y) + radius * 0.62 * angle.sin())?,
            });
        }
        start = end;
    }
    Ok(())
}

pub(super) fn push_chart_marker(
    nodes: &mut Vec<SceneNode>,
    x: Fixed,
    y: Fixed,
    color: Rgb,
    style: &ChartSeriesStyle,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let marker = match style.marker {
        ChartMarkerSymbol::Automatic => ChartMarkerSymbol::Square,
        ChartMarkerSymbol::None => ChartMarkerSymbol::None,
        ChartMarkerSymbol::Circle => ChartMarkerSymbol::Circle,
        ChartMarkerSymbol::Square => ChartMarkerSymbol::Square,
        ChartMarkerSymbol::Diamond => ChartMarkerSymbol::Diamond,
        ChartMarkerSymbol::Triangle => ChartMarkerSymbol::Triangle,
        _ => ChartMarkerSymbol::Square,
    };
    if marker == ChartMarkerSymbol::None {
        return Ok(());
    }
    let diameter = style
        .marker_size
        .map_or(3.0, |points| f64::from(points) * 96.0 / 72.0);
    let radius = diameter / 2.0;
    let center_x = fixed_as_pixels(x);
    let center_y = fixed_as_pixels(y);
    match marker {
        ChartMarkerSymbol::Automatic | ChartMarkerSymbol::None => unreachable!("normalized above"),
        ChartMarkerSymbol::Square => {
            let half = pixels_as_fixed(radius)?;
            push_solid_rect(
                nodes,
                x.checked_sub(half).ok_or(RenderError::CoordinateOverflow)?,
                y.checked_sub(half).ok_or(RenderError::CoordinateOverflow)?,
                x.checked_add(half).ok_or(RenderError::CoordinateOverflow)?,
                y.checked_add(half).ok_or(RenderError::CoordinateOverflow)?,
                color,
                options,
            )
        }
        ChartMarkerSymbol::Circle => {
            let control = radius * 0.552_284_749_830_793_6;
            push_chart_path(
                nodes,
                vec![
                    PathCommand::MoveTo {
                        x: pixels_as_fixed(center_x + radius)?,
                        y,
                    },
                    PathCommand::CubicTo {
                        control1_x: pixels_as_fixed(center_x + radius)?,
                        control1_y: pixels_as_fixed(center_y + control)?,
                        control2_x: pixels_as_fixed(center_x + control)?,
                        control2_y: pixels_as_fixed(center_y + radius)?,
                        x,
                        y: pixels_as_fixed(center_y + radius)?,
                    },
                    PathCommand::CubicTo {
                        control1_x: pixels_as_fixed(center_x - control)?,
                        control1_y: pixels_as_fixed(center_y + radius)?,
                        control2_x: pixels_as_fixed(center_x - radius)?,
                        control2_y: pixels_as_fixed(center_y + control)?,
                        x: pixels_as_fixed(center_x - radius)?,
                        y,
                    },
                    PathCommand::CubicTo {
                        control1_x: pixels_as_fixed(center_x - radius)?,
                        control1_y: pixels_as_fixed(center_y - control)?,
                        control2_x: pixels_as_fixed(center_x - control)?,
                        control2_y: pixels_as_fixed(center_y - radius)?,
                        x,
                        y: pixels_as_fixed(center_y - radius)?,
                    },
                    PathCommand::CubicTo {
                        control1_x: pixels_as_fixed(center_x + control)?,
                        control1_y: pixels_as_fixed(center_y - radius)?,
                        control2_x: pixels_as_fixed(center_x + radius)?,
                        control2_y: pixels_as_fixed(center_y - control)?,
                        x: pixels_as_fixed(center_x + radius)?,
                        y,
                    },
                    PathCommand::Close,
                ],
                Some(color),
                Some(color),
                typography_stats,
                options,
            )
        }
        ChartMarkerSymbol::Diamond => push_chart_path(
            nodes,
            vec![
                PathCommand::MoveTo {
                    x,
                    y: pixels_as_fixed(center_y - radius)?,
                },
                PathCommand::LineTo {
                    x: pixels_as_fixed(center_x + radius)?,
                    y,
                },
                PathCommand::LineTo {
                    x,
                    y: pixels_as_fixed(center_y + radius)?,
                },
                PathCommand::LineTo {
                    x: pixels_as_fixed(center_x - radius)?,
                    y,
                },
                PathCommand::Close,
            ],
            Some(color),
            Some(color),
            typography_stats,
            options,
        ),
        ChartMarkerSymbol::Triangle => push_chart_path(
            nodes,
            vec![
                PathCommand::MoveTo {
                    x,
                    y: pixels_as_fixed(center_y - radius)?,
                },
                PathCommand::LineTo {
                    x: pixels_as_fixed(center_x + radius)?,
                    y: pixels_as_fixed(center_y + radius)?,
                },
                PathCommand::LineTo {
                    x: pixels_as_fixed(center_x - radius)?,
                    y: pixels_as_fixed(center_y + radius)?,
                },
                PathCommand::Close,
            ],
            Some(color),
            Some(color),
            typography_stats,
            options,
        ),
        _ => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(super) fn push_chart_text(
    nodes: &mut Vec<SceneNode>,
    text: String,
    bounds: Rect,
    anchor: TextAnchor,
    rotation_degrees: i16,
    role: ChartTextRole,
    family: &str,
    text_bytes: &mut u64,
    glyphs: &mut u64,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let style = ResolvedChartTextStyle::for_role(role, family);
    push_chart_text_with_style(
        nodes,
        text,
        bounds,
        bounds,
        anchor,
        rotation_degrees,
        &style,
        text_bytes,
        glyphs,
        typography_stats,
        options,
    )
}

#[allow(clippy::too_many_arguments)]
fn push_chart_text_with_style(
    nodes: &mut Vec<SceneNode>,
    text: String,
    bounds: Rect,
    clip_bounds: Rect,
    anchor: TextAnchor,
    fallback_rotation_degrees: i16,
    chart_style: &ResolvedChartTextStyle,
    text_bytes: &mut u64,
    glyphs: &mut u64,
    typography_stats: &mut TypographyStats,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    if text.is_empty() {
        return Ok(());
    }
    *text_bytes = text_bytes
        .checked_add(text.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::TextBytes,
        options.limits.max_text_bytes,
        *text_bytes,
    )?;
    *glyphs = glyphs
        .checked_add(text.chars().count() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(LimitKind::Glyphs, options.limits.max_glyphs, *glyphs)?;
    let node = build_auxiliary_text_node_with_clip_and_kerning(
        text,
        bounds,
        clip_bounds,
        Fixed::from_pixels(2),
        chart_style.text_style(anchor, fallback_rotation_degrees),
        chart_style.kerning(),
        options,
    )?;
    if let SceneNode::GlyphRun(run) = &node {
        typography_stats.path_commands = typography_stats
            .path_commands
            .checked_add(run.commands.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        enforce(
            LimitKind::PathCommands,
            options.limits.max_path_commands,
            typography_stats.path_commands,
        )?;
    }
    push_node(nodes, node, options)
}

fn chart_number(value: f64) -> String {
    if value == 0.0 {
        "0".to_string()
    } else {
        value.to_string()
    }
}
