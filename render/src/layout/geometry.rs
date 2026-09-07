//! Imported worksheet dimensions, source-native axis projection, and fixed-point unit conversion.

use rxls::{
    CellStyle, FormatScript, ImportedAxisMeasure, OoxmlImplicitRowHeight, Sheet,
    XlsbDefaultColumnWidth,
};

use crate::error::RenderError;
use crate::font::FontRequest;
use crate::scene::{Fixed, Rect, FIXED_UNITS_PER_PIXEL};

use super::text::{map_font_error, scale_font_units};
use super::{
    calc_ooxml_implicit_row_height, calc_ooxml_row_height_twips_from_points, enforce_dimension,
    row_is_hidden, CellCoordinate, MeasuredAxisSlot, RenderOptions, RenderStyleSnapshot,
    TypographyStats, WarningCode, Warnings, BIFF_APPLICATION_DEFAULT_COLUMN_WIDTH,
    BIFF_APPLICATION_DEFAULT_ROW_HEIGHT, CALC_SINGLE_PAGE_FIT_PADDING_TWIPS,
    DEFAULT_COLUMN_CHARACTERS, DEFAULT_COLUMN_PADDING_PIXELS, IMPORTED_COLUMN_PADDING_PIXELS,
    OOXML_APPLICATION_DEFAULT_COLUMN_WIDTH_256, OOXML_APPLICATION_DEFAULT_ROW_HEIGHT,
    OOXML_BASE_COLUMN_EXTRA_PADDING_PIXELS, TWIPS_PER_CSS_PIXEL, TWIPS_PER_POINT,
    XLSB_BASE_COLUMN_SCREEN_PIXELS, XLSB_DIGIT_WIDTH_SCALE,
};

pub(super) fn round_positive_mul_div(value: i128, multiplier: i128, divisor: i128) -> Option<i128> {
    if value < 0 || multiplier <= 0 || divisor <= 0 {
        return None;
    }
    value
        .checked_mul(multiplier)?
        .checked_add(divisor.checked_div(2)?)?
        .checked_div(divisor)
}

/// Convert a Calc distance or cumulative position from integer twips to 1/100 mm.
///
/// Calc's SinglePageSheets path sums raw track widths/heights before converting
/// each rectangle endpoint with ordinary half-up `twip -> mm100` conversion.
pub(super) fn calc_twips_position_to_hmm(twips: i128) -> Option<i128> {
    round_positive_mul_div(twips, 127, 72)
}

/// Convert one positive Calc track to integer 1/100-mm units. DrawToDev
/// truncates each track before adding it to the metafile device position.
fn calc_twips_track_to_hmm(twips: i128) -> Option<i128> {
    if twips <= 0 {
        return None;
    }
    twips.checked_mul(127)?.checked_div(72)
}

fn calc_hmm_to_fixed_raw(hmm: i128) -> Option<i128> {
    round_positive_mul_div(
        hmm,
        24_i128.checked_mul(i128::from(FIXED_UNITS_PER_PIXEL))?,
        635,
    )
}

pub(super) fn calc_hmm_to_fixed(hmm: i128) -> Option<Fixed> {
    let raw = calc_hmm_to_fixed_raw(hmm)?;
    i64::try_from(raw).ok().map(Fixed::from_raw)
}

/// Convert Calc's integer 1/100-mm track coordinates as emitted by the
/// `DrawToDev` metafile grid path. The metafile device applies 84/635 CSS
/// pixels per mm100.
pub(super) fn calc_metafile_grid_hmm_to_fixed(hmm: i128) -> Option<Fixed> {
    let raw = round_positive_mul_div(
        hmm,
        84_i128.checked_mul(i128::from(FIXED_UNITS_PER_PIXEL))?,
        635,
    )?;
    i64::try_from(raw).ok().map(Fixed::from_raw)
}

fn calc_twips_position_to_fixed_raw(twips: i128) -> Option<i128> {
    calc_hmm_to_fixed_raw(calc_twips_position_to_hmm(twips)?)
}

pub(super) fn calc_twips_position_to_fixed(twips: i128) -> Option<Fixed> {
    let raw = calc_twips_position_to_fixed_raw(twips)?;
    i64::try_from(raw).ok().map(Fixed::from_raw)
}

/// Calc's `tools::Rectangle` dimensions are inclusive, so SinglePageSheets
/// contributes one additional 1/100-mm unit after converting both endpoints.
pub(super) fn calc_inclusive_rectangle_extent(value: Fixed) -> Option<Fixed> {
    let hmm = round_positive_mul_div(
        i128::from(value.raw()),
        635,
        24_i128.checked_mul(i128::from(FIXED_UNITS_PER_PIXEL))?,
    )?;
    calc_hmm_to_fixed(hmm.checked_add(1)?)
}

/// Cumulative source-space axis cursor seeded at the global visible prefix.
///
/// Calc retains imported worksheet axes as integer twips, sums tracks, then
/// projects cumulative endpoints to 1/100 mm. Boundaries are translated back to
/// local scene coordinates, preserving the global phase for nonzero selections
/// without exposing a potentially large prefix to the local dimension limit.
pub(super) struct SourceAxisCursor {
    pub(super) twips: i128,
    pub(super) origin_raw: i128,
    pub(super) previous_raw: i128,
}

impl SourceAxisCursor {
    pub(super) fn new(prefix_twips: i128) -> Result<Self, RenderError> {
        let origin_raw = calc_twips_position_to_fixed_raw(prefix_twips)
            .ok_or(RenderError::CoordinateOverflow)?;
        Ok(Self {
            twips: prefix_twips,
            origin_raw,
            previous_raw: origin_raw,
        })
    }

    pub(super) fn advance(
        &mut self,
        contribution_twips: i128,
    ) -> Result<(Fixed, Fixed, Fixed), RenderError> {
        let offset_raw = self
            .previous_raw
            .checked_sub(self.origin_raw)
            .ok_or(RenderError::CoordinateOverflow)?;
        self.twips = self
            .twips
            .checked_add(contribution_twips)
            .ok_or(RenderError::CoordinateOverflow)?;
        let boundary_raw =
            calc_twips_position_to_fixed_raw(self.twips).ok_or(RenderError::CoordinateOverflow)?;
        let size_raw = boundary_raw
            .checked_sub(self.previous_raw)
            .filter(|size| *size > 0)
            .ok_or(RenderError::CoordinateOverflow)?;
        let local_boundary_raw = boundary_raw
            .checked_sub(self.origin_raw)
            .ok_or(RenderError::CoordinateOverflow)?;
        self.previous_raw = boundary_raw;
        Ok((
            Fixed::from_raw(
                i64::try_from(offset_raw).map_err(|_| RenderError::CoordinateOverflow)?,
            ),
            Fixed::from_raw(i64::try_from(size_raw).map_err(|_| RenderError::CoordinateOverflow)?),
            Fixed::from_raw(
                i64::try_from(local_boundary_raw).map_err(|_| RenderError::CoordinateOverflow)?,
            ),
        ))
    }
}

pub(super) fn imported_column_axis_measure(
    sheet: &Sheet,
    column: u16,
    options: &RenderOptions,
) -> Option<ImportedAxisMeasure> {
    let has_explicit_width = sheet.column_widths().contains_key(&column)
        || sheet.physical_column_widths().contains_key(&column)
        || sheet.xlsb_column_widths_256().contains_key(&column);
    if has_explicit_width {
        sheet.imported_column_axis_measures().get(&column).copied()
    } else {
        sheet.imported_default_column_axis_measure().or_else(|| {
            (options.font_pack.is_some() && sheet.implicit_ooxml_column_width() == Some(None)).then(
                || {
                    if sheet.ooxml_uses_defaulted_base_column_width() {
                        ImportedAxisMeasure::DigitBaseWidth256(8 * XLSB_DIGIT_WIDTH_SCALE)
                    } else {
                        ImportedAxisMeasure::DigitWidth256(
                            OOXML_APPLICATION_DEFAULT_COLUMN_WIDTH_256,
                        )
                    }
                },
            )
        })
    }
}

pub(super) fn imported_default_row_axis_measure(
    sheet: &Sheet,
    options: &RenderOptions,
) -> Option<ImportedAxisMeasure> {
    sheet.imported_default_row_axis_measure().or_else(|| {
        sheet.has_implicit_ooxml_row_height().then(|| {
            verified_ooxml_normal_font_size(sheet, options)
                .and_then(|(points, _)| calc_ooxml_row_height_twips_from_points(points))
                .map(ImportedAxisMeasure::Twips)
                .unwrap_or(ImportedAxisMeasure::MillimeterHundredths(500))
        })
    })
}

pub(super) fn imported_row_axis_measure(
    sheet: &Sheet,
    row: u32,
    options: &RenderOptions,
) -> Option<ImportedAxisMeasure> {
    if sheet.row_heights().contains_key(&row) {
        sheet.imported_row_axis_measures().get(&row).copied()
    } else {
        imported_default_row_axis_measure(sheet, options)
    }
}

fn maximum_digit_width_twips(maximum_digit_width: Fixed) -> Option<i128> {
    i128::from(maximum_digit_width.raw())
        .checked_mul(TWIPS_PER_CSS_PIXEL)?
        .checked_div(i128::from(FIXED_UNITS_PER_PIXEL))
        .filter(|twips| *twips > 0)
}

/// Calc's BIFF importer truncates `width * digit_twips - 0.5`.
fn biff_character_width_256_to_twips(width_256: u32, maximum_digit_width: Fixed) -> Option<i128> {
    let digit_twips = maximum_digit_width_twips(maximum_digit_width)?;
    i128::from(width_256)
        .checked_mul(digit_twips)?
        .checked_sub(i128::from(XLSB_DIGIT_WIDTH_SCALE / 2))?
        .checked_div(i128::from(XLSB_DIGIT_WIDTH_SCALE))
        .filter(|twips| *twips > 0)
}

/// Calc's OOXML importer rounds source character widths in the default font's
/// integer-twip maximum-digit domain. Base widths additionally carry five
/// 96-DPI screen pixels.
fn ooxml_character_width_ratio_to_twips(
    numerator: u64,
    denominator: u64,
    maximum_digit_width: Fixed,
    extra_screen_pixels: u16,
) -> Option<i128> {
    if numerator == 0 || denominator == 0 {
        return None;
    }
    let digit_twips = maximum_digit_width_twips(maximum_digit_width)?;
    round_positive_mul_div(i128::from(numerator), digit_twips, i128::from(denominator))?
        .checked_add(i128::from(extra_screen_pixels).checked_mul(TWIPS_PER_CSS_PIXEL)?)
        .filter(|twips| *twips > 0)
}

#[cfg(test)]
pub(super) fn character_width_ratio_to_pixels(
    numerator: u64,
    denominator: u64,
    maximum_digit_width: Fixed,
    screen_padding_pixels: u16,
) -> Option<i128> {
    if numerator == 0 || denominator == 0 || maximum_digit_width.raw() <= 0 {
        return None;
    }
    let digit_pixels = i128::from(maximum_digit_width.raw())
        .checked_div(i128::from(FIXED_UNITS_PER_PIXEL))?
        .max(1);
    let bias = 128_i128.checked_div(digit_pixels)?;
    let numerator = i128::from(numerator);
    let denominator = i128::from(denominator);
    numerator
        .checked_mul(i128::from(XLSB_DIGIT_WIDTH_SCALE))?
        .checked_add(bias.checked_mul(denominator)?)?
        .checked_mul(digit_pixels)?
        .checked_div(denominator.checked_mul(i128::from(XLSB_DIGIT_WIDTH_SCALE))?)?
        .checked_add(i128::from(screen_padding_pixels))
        .filter(|pixels| *pixels > 0)
}

pub(super) fn imported_axis_measure_twips(
    measure: ImportedAxisMeasure,
    maximum_digit_width: Fixed,
) -> Option<i128> {
    match measure {
        ImportedAxisMeasure::Twips(twips) => Some(i128::from(twips)),
        ImportedAxisMeasure::MillimeterHundredths(mm100) => {
            round_positive_mul_div(i128::from(mm100), 72, 127)
        }
        ImportedAxisMeasure::PointRatio(numerator, denominator) => round_positive_mul_div(
            i128::from(numerator),
            TWIPS_PER_POINT,
            i128::from(denominator),
        ),
        ImportedAxisMeasure::CharacterWidth256(width) => {
            biff_character_width_256_to_twips(width, maximum_digit_width)
        }
        ImportedAxisMeasure::CharacterWidthRatio(numerator, denominator) => {
            ooxml_character_width_ratio_to_twips(numerator, denominator, maximum_digit_width, 0)
        }
        ImportedAxisMeasure::CharacterBaseWidth256(width) => ooxml_character_width_ratio_to_twips(
            u64::from(width),
            u64::from(XLSB_DIGIT_WIDTH_SCALE),
            maximum_digit_width,
            OOXML_BASE_COLUMN_EXTRA_PADDING_PIXELS,
        ),
        ImportedAxisMeasure::DigitWidth256(width) => {
            xlsb_digits_to_twips(width, maximum_digit_width, 0)
        }
        ImportedAxisMeasure::DigitBaseWidth256(width) => {
            xlsb_digits_to_twips(width, maximum_digit_width, XLSB_BASE_COLUMN_SCREEN_PIXELS)
        }
    }
}

pub(super) fn source_axis_contribution_twips(
    measure: Option<ImportedAxisMeasure>,
    fallback: Fixed,
    maximum_digit_width: Fixed,
) -> Option<i128> {
    measure
        .and_then(|measure| imported_axis_measure_twips(measure, maximum_digit_width))
        .filter(|twips| *twips > 0)
        .or_else(|| {
            round_positive_mul_div(
                i128::from(fallback.raw()),
                TWIPS_PER_CSS_PIXEL,
                i128::from(FIXED_UNITS_PER_PIXEL),
            )
            .filter(|twips| *twips > 0)
        })
}

pub(super) fn source_native_column_prefix(
    sheet: &Sheet,
    first_column: u16,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<i128, RenderError> {
    // Column iteration is bounded by the 16,384-column worksheet schema.
    let mut prefix = 0_i128;
    for column in 0..first_column {
        if !options.include_hidden && sheet.hidden_columns().contains(&column) {
            continue;
        }
        let fallback = column_width(sheet, column, maximum_digit_width, options, warnings);
        let contribution = source_axis_contribution_twips(
            imported_column_axis_measure(sheet, column, options),
            fallback,
            maximum_digit_width,
        )
        .ok_or(RenderError::CoordinateOverflow)?;
        prefix = prefix
            .checked_add(contribution)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(prefix)
}

fn persisted_default_row_height(sheet: &Sheet, options: &RenderOptions) -> Fixed {
    sheet
        .default_row_height()
        .and_then(points_to_fixed)
        .unwrap_or_else(|| fallback_row_height(sheet, options))
}

fn visible_row_prefix_count(sheet: &Sheet, first_row: u32, options: &RenderOptions) -> u64 {
    if options.include_hidden {
        u64::from(first_row)
    } else if let Some(exceptions) = sheet.default_hidden_row_exceptions() {
        exceptions
            .range(..first_row)
            .filter(|&&row| !sheet.hidden_rows().contains(&row))
            .count() as u64
    } else {
        u64::from(first_row).saturating_sub(sheet.hidden_rows().range(..first_row).count() as u64)
    }
}

pub(super) fn source_native_row_prefix(
    sheet: &Sheet,
    first_row: u32,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Result<i128, RenderError> {
    let visible_rows = visible_row_prefix_count(sheet, first_row, options);
    if visible_rows == 0 {
        return Ok(0);
    }
    let default_contribution = source_axis_contribution_twips(
        imported_default_row_axis_measure(sheet, options),
        persisted_default_row_height(sheet, options),
        maximum_digit_width,
    )
    .ok_or(RenderError::CoordinateOverflow)?;
    // Rows can reach the million-row schema ceiling. Multiply the default
    // contribution by the visible count and visit only sparse explicit rows.
    let mut explicit_rows = 0_u64;
    let mut explicit_total = 0_i128;
    for (&row, _) in sheet.row_heights().range(..first_row) {
        if !options.include_hidden && row_is_hidden(sheet, row) {
            continue;
        }
        explicit_rows = explicit_rows
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        let contribution = source_axis_contribution_twips(
            imported_row_axis_measure(sheet, row, options),
            row_height(sheet, row, options, warnings),
            maximum_digit_width,
        )
        .ok_or(RenderError::CoordinateOverflow)?;
        explicit_total = explicit_total
            .checked_add(contribution)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    let default_rows = visible_rows
        .checked_sub(explicit_rows)
        .ok_or(RenderError::CoordinateOverflow)?;
    default_contribution
        .checked_mul(i128::from(default_rows))
        .and_then(|defaults| defaults.checked_add(explicit_total))
        .ok_or(RenderError::CoordinateOverflow)
}

pub(super) fn apply_source_native_axis_endpoints<I: Copy>(
    slots: &mut [MeasuredAxisSlot<I>],
    prefix_twips: i128,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    mut measure: impl FnMut(I) -> Option<ImportedAxisMeasure>,
) -> Result<Vec<i128>, RenderError> {
    let mut cursor = SourceAxisCursor::new(prefix_twips)?;
    let mut contributions = Vec::with_capacity(slots.len());
    for slot in slots {
        let contribution_twips =
            source_axis_contribution_twips(measure(slot.index), slot.size, maximum_digit_width)
                .ok_or(RenderError::CoordinateOverflow)?;
        contributions.push(contribution_twips);
        let (offset, size, boundary) = cursor.advance(contribution_twips)?;
        slot.offset = offset;
        slot.size = size;
        enforce_dimension(boundary, options)?;
    }
    Ok(contributions)
}

/// Reproduce Calc's `ScPrintFunc::DrawToDev` SinglePageSheets cell-axis fit.
///
/// The page rectangle keeps the unfitted cumulative source extent. Calc adds
/// 20 twips to the input axis, scales into that rectangle, and truncates every
/// track independently to integer 1/100-mm units. The unused remainder stays
/// at the trailing edge; drawing objects continue to use the original axis.
pub(super) fn apply_calc_single_page_axis_fit<I: Copy>(
    slots: &mut [MeasuredAxisSlot<I>],
    source_twips: &[i128],
    page_extent: Fixed,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    apply_calc_single_page_axis_projection(
        slots,
        source_twips,
        page_extent,
        calc_hmm_to_fixed,
        Some(options),
    )
}

/// Project source-native tracks through Calc's metafile grid scale. DrawToDev
/// converts each track independently to integer 1/100-mm units before adding
/// it to the device position; rounding a cumulative twip endpoint instead
/// shifts later gridlines. Unlike the cell axis, these coordinates are not fit
/// back into the page rectangle.
pub(super) fn apply_calc_single_page_metafile_grid_axis_fit<I: Copy>(
    slots: &mut [MeasuredAxisSlot<I>],
    source_twips: &[i128],
) -> Result<(), RenderError> {
    if slots.len() != source_twips.len() || source_twips.iter().any(|twips| *twips <= 0) {
        return Err(RenderError::CoordinateOverflow);
    }
    let mut cumulative_hmm = 0_i128;
    let mut previous_boundary = Fixed::ZERO;
    for (slot, twips) in slots.iter_mut().zip(source_twips) {
        let track_hmm = calc_twips_track_to_hmm(*twips)
            .filter(|hmm| *hmm > 0)
            .ok_or(RenderError::CoordinateOverflow)?;
        cumulative_hmm = cumulative_hmm
            .checked_add(track_hmm)
            .ok_or(RenderError::CoordinateOverflow)?;
        let boundary = calc_metafile_grid_hmm_to_fixed(cumulative_hmm)
            .ok_or(RenderError::CoordinateOverflow)?;
        slot.offset = previous_boundary;
        slot.size = boundary
            .checked_sub(previous_boundary)
            .filter(|size| *size > Fixed::ZERO)
            .ok_or(RenderError::CoordinateOverflow)?;
        previous_boundary = boundary;
    }
    Ok(())
}

fn apply_calc_single_page_axis_projection<I: Copy>(
    slots: &mut [MeasuredAxisSlot<I>],
    source_twips: &[i128],
    page_extent: Fixed,
    project_hmm: fn(i128) -> Option<Fixed>,
    output_options: Option<&RenderOptions>,
) -> Result<(), RenderError> {
    if slots.len() != source_twips.len() || source_twips.iter().any(|twips| *twips <= 0) {
        return Err(RenderError::CoordinateOverflow);
    }
    let total_twips = source_twips.iter().try_fold(0_i128, |total, twips| {
        total
            .checked_add(*twips)
            .ok_or(RenderError::CoordinateOverflow)
    })?;
    let fit_twips = total_twips
        .checked_add(CALC_SINGLE_PAGE_FIT_PADDING_TWIPS)
        .ok_or(RenderError::CoordinateOverflow)?;
    let fixed_hmm_denominator = 24_i128
        .checked_mul(i128::from(FIXED_UNITS_PER_PIXEL))
        .ok_or(RenderError::CoordinateOverflow)?;
    let page_hmm =
        round_positive_mul_div(i128::from(page_extent.raw()), 635, fixed_hmm_denominator)
            .filter(|value| *value > 0)
            .ok_or(RenderError::CoordinateOverflow)?;

    let mut previous_boundary = Fixed::ZERO;
    let mut cumulative_hmm = 0_i128;
    for (slot, twips) in slots.iter_mut().zip(source_twips) {
        let track_hmm = twips
            .checked_mul(page_hmm)
            .and_then(|value| value.checked_div(fit_twips))
            .ok_or(RenderError::CoordinateOverflow)?;
        cumulative_hmm = cumulative_hmm
            .checked_add(track_hmm)
            .ok_or(RenderError::CoordinateOverflow)?;
        let boundary = project_hmm(cumulative_hmm).ok_or(RenderError::CoordinateOverflow)?;
        slot.offset = previous_boundary;
        slot.size = boundary
            .checked_sub(previous_boundary)
            .ok_or(RenderError::CoordinateOverflow)?;
        previous_boundary = boundary;
        if let Some(options) = output_options {
            enforce_dimension(boundary, options)?;
        }
    }
    Ok(())
}

pub(super) fn apply_axis_geometry<I: Copy + Ord>(
    slots: &mut [MeasuredAxisSlot<I>],
    geometry: &[MeasuredAxisSlot<I>],
) -> Result<(), RenderError> {
    let mut offset = Fixed::ZERO;
    for slot in slots {
        let replacement = geometry
            .binary_search_by(|candidate| candidate.index.cmp(&slot.index))
            .ok()
            .and_then(|index| geometry.get(index))
            .ok_or(RenderError::Backend {
                reason: "prepared_print_geometry_missing_axis_slot",
            })?;
        slot.offset = offset;
        slot.size = replacement.size;
        offset = offset
            .checked_add(slot.size)
            .ok_or(RenderError::CoordinateOverflow)?;
    }
    Ok(())
}

pub(super) fn visual_column_slots<I: Copy>(
    logical_slots: &[MeasuredAxisSlot<I>],
    canvas_width: Fixed,
    right_to_left: bool,
) -> Result<Option<Vec<MeasuredAxisSlot<I>>>, RenderError> {
    if !right_to_left {
        return Ok(None);
    }
    let slots = logical_slots
        .iter()
        .map(|slot| {
            Ok(MeasuredAxisSlot {
                index: slot.index,
                offset: reflected_x(slot.offset, slot.size, canvas_width)?,
                size: slot.size,
            })
        })
        .collect::<Result<Vec<_>, RenderError>>()?;
    Ok(Some(slots))
}

pub(super) fn reflected_x(
    x: Fixed,
    width: Fixed,
    canvas_width: Fixed,
) -> Result<Fixed, RenderError> {
    canvas_width
        .checked_sub(
            x.checked_add(width)
                .ok_or(RenderError::CoordinateOverflow)?,
        )
        .ok_or(RenderError::CoordinateOverflow)
}

pub(super) fn reflect_rect_horizontally(
    mut rect: Rect,
    canvas_width: Fixed,
) -> Result<Rect, RenderError> {
    rect.x = reflected_x(rect.x, rect.width, canvas_width)?;
    Ok(rect)
}

pub(super) fn maximum_digit_width(
    style_snapshot: &RenderStyleSnapshot,
    options: &RenderOptions,
    warnings: &mut Warnings,
    statistics: &mut TypographyStats,
) -> Result<Fixed, RenderError> {
    let Some(pack) = options.font_pack.as_ref() else {
        return Ok(Fixed::from_pixels(7));
    };
    let font = style_snapshot
        .default_style()
        .and_then(|style| style.font.as_ref());
    let family = font
        .and_then(|font| font.name.as_deref())
        .unwrap_or(&options.default_font_family);
    let size = font
        .and_then(|font| font.size_pt)
        .and_then(|points| points_to_fixed(points as f32))
        .unwrap_or(options.default_font_size);
    let request = FontRequest {
        family,
        weight: if font.is_some_and(|font| font.bold) {
            700
        } else {
            400
        },
        italic: font.is_some_and(|font| font.italic),
    };
    let resolution = pack.resolve(request);
    if !resolution.exact_family {
        warnings.add(WarningCode::FontFamilySubstituted, None);
    }
    let (font_id, width) = pack.max_digit_width(request).map_err(map_font_error)?;
    statistics.record_face(pack, font_id, !resolution.exact_family)?;
    let metrics = pack.metrics(font_id).map_err(map_font_error)?;
    scale_font_units(i64::from(width), size, metrics.units_per_em, 1)
}

pub(super) fn column_width(
    sheet: &Sheet,
    col: u16,
    maximum_digit_width: Fixed,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Fixed {
    if let Some(points) = sheet.physical_column_widths().get(&col).copied() {
        if let Some(width) = points_to_fixed(points) {
            return width;
        }
        warnings.add(
            WarningCode::InvalidGeometryFallback,
            Some(CellCoordinate { row: 0, col }),
        );
    }
    if let Some(width_256) = sheet.xlsb_column_widths_256().get(&col).copied() {
        return resolve_column_width(
            xlsb_digits_to_fixed(width_256, maximum_digit_width, 0),
            true,
            col,
            options,
            warnings,
        );
    }
    if let Some(chars) = sheet.column_widths().get(&col).copied() {
        return resolve_column_width(
            column_chars_to_fixed(chars, maximum_digit_width, IMPORTED_COLUMN_PADDING_PIXELS),
            true,
            col,
            options,
            warnings,
        );
    }
    if let Some(provenance) = sheet.xlsb_default_column_width() {
        let (width_256, screen_pixels) = match provenance {
            XlsbDefaultColumnWidth::ApplicationDefault => {
                (XLSB_DIGIT_WIDTH_SCALE * 8 + XLSB_DIGIT_WIDTH_SCALE / 2, 0)
            }
            XlsbDefaultColumnWidth::Digits256(width_256) => (width_256, 0),
            XlsbDefaultColumnWidth::BaseCharacters(characters) => (
                u32::from(characters) * XLSB_DIGIT_WIDTH_SCALE,
                XLSB_BASE_COLUMN_SCREEN_PIXELS,
            ),
        };
        return resolve_column_width(
            xlsb_digits_to_fixed(width_256, maximum_digit_width, screen_pixels),
            true,
            col,
            options,
            warnings,
        );
    }
    if sheet.biff_uses_application_default_column_width() {
        return BIFF_APPLICATION_DEFAULT_COLUMN_WIDTH;
    }
    let (measured, invalid_source_geometry) = match sheet.default_column_width() {
        Some(chars) => (
            column_chars_to_fixed(chars, maximum_digit_width, IMPORTED_COLUMN_PADDING_PIXELS),
            true,
        ),
        None => match sheet.implicit_ooxml_column_width() {
            Some(Some(base_characters)) => (
                column_chars_to_fixed(
                    base_characters,
                    maximum_digit_width,
                    IMPORTED_COLUMN_PADDING_PIXELS + OOXML_BASE_COLUMN_EXTRA_PADDING_PIXELS,
                ),
                true,
            ),
            // Without verified font metrics the caller's physical fallback is
            // safer than projecting Calc's font-dependent application default.
            Some(None)
                if options.font_pack.is_some()
                    && sheet.ooxml_uses_defaulted_base_column_width() =>
            {
                (
                    xlsb_digits_to_fixed(
                        8 * XLSB_DIGIT_WIDTH_SCALE,
                        maximum_digit_width,
                        XLSB_BASE_COLUMN_SCREEN_PIXELS,
                    ),
                    true,
                )
            }
            Some(None) if options.font_pack.is_some() => (
                xlsb_digits_to_fixed(
                    OOXML_APPLICATION_DEFAULT_COLUMN_WIDTH_256,
                    maximum_digit_width,
                    0,
                ),
                true,
            ),
            Some(None) | None if options.font_pack.is_some() => (
                column_chars_to_fixed(
                    DEFAULT_COLUMN_CHARACTERS,
                    maximum_digit_width,
                    DEFAULT_COLUMN_PADDING_PIXELS,
                ),
                false,
            ),
            Some(None) | None => (None, false),
        },
    };
    resolve_column_width(measured, invalid_source_geometry, col, options, warnings)
}

fn resolve_column_width(
    measured: Option<Fixed>,
    invalid_source_geometry: bool,
    col: u16,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Fixed {
    match measured {
        Some(width) => width,
        None => {
            if invalid_source_geometry {
                warnings.add(
                    WarningCode::InvalidGeometryFallback,
                    Some(CellCoordinate { row: 0, col }),
                );
            }
            options.default_column_width.max(Fixed::from_raw(1))
        }
    }
}

pub(super) fn empty_used_column_width(
    sheet: &Sheet,
    style_snapshot: &RenderStyleSnapshot,
    options: &RenderOptions,
    warnings: &mut Warnings,
    statistics: &mut TypographyStats,
) -> Result<Fixed, RenderError> {
    if sheet.column_widths().len() == 256 {
        return Ok(Fixed::from_pixels(1));
    }
    if let Some(XlsbDefaultColumnWidth::Digits256(width_256)) = sheet.xlsb_default_column_width() {
        let maximum_digit_width =
            maximum_digit_width(style_snapshot, options, warnings, statistics)?;
        return match xlsb_digits_to_fixed(width_256, maximum_digit_width, 0) {
            Some(width) => Ok(width),
            None => {
                warnings.add(
                    WarningCode::InvalidGeometryFallback,
                    Some(CellCoordinate { row: 0, col: 0 }),
                );
                Ok(Fixed::from_pixels(1))
            }
        };
    }
    if sheet.biff_uses_application_default_column_width() {
        return Ok(BIFF_APPLICATION_DEFAULT_COLUMN_WIDTH);
    }
    let Some(chars) = sheet.default_column_width() else {
        return Ok(Fixed::from_pixels(1));
    };
    if !chars.is_finite() || chars <= 0.0 {
        warnings.add(
            WarningCode::InvalidGeometryFallback,
            Some(CellCoordinate { row: 0, col: 0 }),
        );
        return Ok(Fixed::from_pixels(1));
    }
    let maximum_digit_width = maximum_digit_width(style_snapshot, options, warnings, statistics)?;
    match column_chars_to_fixed(chars, maximum_digit_width, IMPORTED_COLUMN_PADDING_PIXELS) {
        Some(width) => Ok(width),
        None => {
            warnings.add(
                WarningCode::InvalidGeometryFallback,
                Some(CellCoordinate { row: 0, col: 0 }),
            );
            Ok(Fixed::from_pixels(1))
        }
    }
}

pub(super) fn row_height(
    sheet: &Sheet,
    row: u32,
    options: &RenderOptions,
    warnings: &mut Warnings,
) -> Fixed {
    let points = sheet
        .row_heights()
        .get(&row)
        .copied()
        .or_else(|| sheet.default_row_height());
    match points.and_then(points_to_fixed) {
        Some(height) => height,
        None => {
            if points.is_some() {
                warnings.add(
                    WarningCode::InvalidGeometryFallback,
                    Some(CellCoordinate { row, col: 0 }),
                );
            }
            fallback_row_height(sheet, options)
        }
    }
}

pub(super) fn fallback_row_height(sheet: &Sheet, options: &RenderOptions) -> Fixed {
    if sheet.biff_uses_application_default_row_height() {
        BIFF_APPLICATION_DEFAULT_ROW_HEIGHT
    } else {
        match sheet.implicit_ooxml_row_height_source() {
            Some(OoxmlImplicitRowHeight::XlsxApplicationDefault) => {
                calc_ooxml_implicit_row_height(sheet, options)
                    .unwrap_or(OOXML_APPLICATION_DEFAULT_ROW_HEIGHT)
            }
            Some(OoxmlImplicitRowHeight::XlsbApplicationDefault) => {
                calc_ooxml_implicit_row_height(sheet, options)
                    .unwrap_or(OOXML_APPLICATION_DEFAULT_ROW_HEIGHT)
            }
            Some(OoxmlImplicitRowHeight::None) | None => imported_no_information_row_height(sheet)
                .unwrap_or(options.default_row_height)
                .max(Fixed::from_raw(1)),
        }
    }
}

/// An imported sheet's own native "no information" default row height,
/// consumed only when the sheet carries no points-based default row height
/// at all (`Sheet::default_row_height` is `None`).
///
/// XLS, XLSX, and XLSB importers always populate
/// `imported_default_row_axis_measure` together with `default_row_height`
/// from the very same source record or attribute -- BIFF's
/// `DEFAULTROWHEIGHT`, XLSX's `sheetFormatPr defaultRowHeight`, XLSB's
/// `BrtWsFmtInfo` -- so this function can only ever return `Some` for an
/// importer, currently only ODS's, that records a native default-row measure
/// while leaving `default_row_height` unset. The `default_row_height().is_some()`
/// guard below is a second, independent proof of that: even if a future
/// importer bug broke the "populated together" invariant, this function
/// still could not change BIFF/XLSX/XLSB behavior, because those formats
/// only ever reach `fallback_row_height`'s final branch (this one) with a
/// per-row or sheet-wide invalid height while `default_row_height` is
/// `Some`.
///
/// `calc_hmm_to_fixed` is the same hundredths-of-millimetre-to-`Fixed`
/// conversion the SinglePageSheets path already applies to print geometry
/// (e.g. `calc_twips_position_to_fixed_raw`), so this keeps the physical
/// quantity to one exact spelling instead of re-deriving a second,
/// differently-rounded one: `calc_hmm_to_fixed(500)` equals
/// `OOXML_APPLICATION_DEFAULT_ROW_HEIGHT` exactly.
fn imported_no_information_row_height(sheet: &Sheet) -> Option<Fixed> {
    if sheet.default_row_height().is_some() {
        return None;
    }
    match sheet.imported_default_row_axis_measure()? {
        ImportedAxisMeasure::MillimeterHundredths(mm100) => calc_hmm_to_fixed(i128::from(mm100)),
        _ => None,
    }
}

pub(super) fn verified_ooxml_normal_font_size(
    sheet: &Sheet,
    options: &RenderOptions,
) -> Option<(u16, Fixed)> {
    // The model retains these source sizes only for structurally complete XLSX
    // or XLSB style tables whose first cell XF and Normal style agree exactly.
    // Fractional, invalid, ambiguous, authored, BIFF, and ODS sources stay on
    // the existing physical fallback.
    let source_points = match (
        sheet.verified_xlsx_normal_font_size_pt(),
        sheet.verified_xlsb_normal_font_size_pt(),
    ) {
        (Some(points), None) | (None, Some(points)) => points,
        (Some(_), Some(_)) | (None, None) => return None,
    };
    let pack = options.font_pack.as_ref()?;
    let font = sheet.default_cell_style()?.font.as_ref()?;
    let family = font.name.as_deref()?;
    let points = font.size_pt.filter(|points| *points == source_points)?;
    let resolution = pack.resolve(FontRequest {
        family,
        weight: if font.bold { 700 } else { 400 },
        italic: font.italic,
    });
    if !(resolution.exact_family || resolution.declared_alias) || !resolution.exact_style {
        return None;
    }
    Some((points, points_to_fixed(f32::from(points))?))
}

pub(super) fn verified_ooxml_cell_font_size_pt(sheet: &Sheet, row: u32, col: u16) -> Option<u16> {
    match sheet.implicit_ooxml_row_height_source()? {
        OoxmlImplicitRowHeight::XlsxApplicationDefault => {
            sheet.verified_xlsx_cell_font_size_pt(row, col)
        }
        OoxmlImplicitRowHeight::XlsbApplicationDefault => {
            sheet.verified_xlsb_cell_font_size_pt(row, col)
        }
        OoxmlImplicitRowHeight::None => None,
    }
}

pub(super) fn verified_calc_cell_font_size_pt(
    sheet: &Sheet,
    source: CellCoordinate,
    style: &CellStyle,
    options: &RenderOptions,
) -> Option<u16> {
    let points = verified_ooxml_cell_font_size_pt(sheet, source.row, source.col)?;
    let font = style.font.as_ref()?;
    if font.size_pt != Some(points) || font.script != FormatScript::None {
        return None;
    }
    let resolution = options.font_pack.as_ref()?.resolve(FontRequest {
        family: font.name.as_deref()?,
        weight: if font.bold { 700 } else { 400 },
        italic: font.italic,
    });
    ((resolution.exact_family || resolution.declared_alias) && resolution.exact_style)
        .then_some(points)
}

pub(super) fn verified_implicit_ooxml(sheet: &Sheet, options: &RenderOptions) -> bool {
    sheet.has_implicit_ooxml_row_height()
        && verified_ooxml_normal_font_size(sheet, options).is_some()
}

pub(super) fn column_chars_to_fixed(
    chars: f32,
    maximum_digit_width: Fixed,
    padding_pixels: u16,
) -> Option<Fixed> {
    if !chars.is_finite() || chars <= 0.0 {
        return None;
    }
    let digit_pixels = (maximum_digit_width.raw() as f64 / FIXED_UNITS_PER_PIXEL as f64)
        .floor()
        .max(1.0);
    // ECMA-376 18.3.1.13 uses the maximum digit width of the workbook's
    // default font. The caller selects the source-specific device-pixel
    // allowance: Calc-compatible import geometry or the ECMA fallback.
    let pixels = (((f64::from(chars) * 256.0 + (128.0 / digit_pixels).floor()) / 256.0)
        * digit_pixels)
        .floor()
        + f64::from(padding_pixels);
    float_pixels_to_fixed(pixels)
}

pub(super) fn xlsb_digits_to_fixed(
    width_256: u32,
    maximum_digit_width: Fixed,
    extra_screen_pixels: u16,
) -> Option<Fixed> {
    let width_twips = xlsb_digits_to_twips(width_256, maximum_digit_width, extra_screen_pixels)?;
    let raw = width_twips
        .checked_mul(i128::from(FIXED_UNITS_PER_PIXEL))?
        .checked_add(TWIPS_PER_CSS_PIXEL / 2)?
        .checked_div(TWIPS_PER_CSS_PIXEL)?;
    if raw <= 0 {
        return None;
    }
    i64::try_from(raw).ok().map(Fixed::from_raw)
}

fn xlsb_digits_to_twips(
    width_256: u32,
    maximum_digit_width: Fixed,
    extra_screen_pixels: u16,
) -> Option<i128> {
    if width_256 == 0 || maximum_digit_width.raw() <= 0 {
        return None;
    }
    let digit_twips = i128::from(maximum_digit_width.raw())
        .checked_mul(TWIPS_PER_CSS_PIXEL)?
        .checked_div(i128::from(FIXED_UNITS_PER_PIXEL))?;
    if digit_twips <= 0 {
        return None;
    }
    let width_twips = i128::from(width_256)
        .checked_mul(digit_twips)?
        .checked_add(i128::from(XLSB_DIGIT_WIDTH_SCALE / 2))?
        .checked_div(i128::from(XLSB_DIGIT_WIDTH_SCALE))?
        .checked_add(i128::from(extra_screen_pixels) * TWIPS_PER_CSS_PIXEL)?;
    (width_twips > 0).then_some(width_twips)
}

pub(super) fn points_to_fixed(points: f32) -> Option<Fixed> {
    if !points.is_finite() || points <= 0.0 {
        return None;
    }
    float_pixels_to_fixed(f64::from(points) * 4.0 / 3.0)
}

fn float_pixels_to_fixed(pixels: f64) -> Option<Fixed> {
    let raw = (pixels * FIXED_UNITS_PER_PIXEL as f64).round();
    if !raw.is_finite() || raw <= 0.0 || raw > i64::MAX as f64 {
        None
    } else {
        Some(Fixed::from_raw(raw as i64))
    }
}
