//! Bounded Unicode line breaking shared by deterministic text layout.

use std::ops::Range;

use unicode_linebreak::{linebreaks, BreakOpportunity};
use unicode_segmentation::UnicodeSegmentation;

use crate::error::{LimitKind, RenderError};
use crate::scene::Fixed;

/// Cell-text line fitting behavior used by the shared wrapping primitive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CellLineLayoutPolicy {
    /// Preserve rxls's existing exact-fit behavior.
    Native,
    /// Preserve native fitting while honoring ODF's internal hyphen breaks.
    OdsNative,
    /// Match Calc EditEngine's strict fit boundary and trailing-space handling.
    CalcEditEngine,
}

impl CellLineLayoutPolicy {
    fn fits(self, width: Fixed, available_width: Fixed) -> bool {
        match self {
            Self::Native | Self::OdsNative => width <= available_width,
            Self::CalcEditEngine => width < available_width,
        }
    }
}

/// A wrapped source line and the end of the source that contributes advance.
///
/// `advance_end` is normally `source.end`. Calc may retain trailing ASCII
/// spaces in `source` at an automatic break while excluding them from the
/// measured and painted advance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WrappedLine {
    pub(crate) source: Range<usize>,
    pub(crate) advance_end: usize,
}

/// Split display text into bounded lines using UAX #14 opportunities.
///
/// Each break atom is measured once. A single overlong atom falls back to
/// extended grapheme clusters, so combining sequences and emoji ZWJ clusters
/// are never split merely to satisfy a cell width.
#[cfg(test)]
pub(crate) fn wrap_text(
    text: &str,
    wrap: bool,
    available_width: Fixed,
    max_lines: u64,
    max_segments: u64,
    mut measure: impl FnMut(&str) -> Result<Fixed, RenderError>,
) -> Result<Vec<String>, RenderError> {
    wrap_text_ranges(
        text,
        wrap,
        available_width,
        max_lines,
        max_segments,
        |range| {
            measure(text.get(range).ok_or(RenderError::Typography {
                reason: "invalid_line_break_range",
            })?)
        },
    )?
    .into_iter()
    .map(|range| {
        text.get(range)
            .map(str::to_owned)
            .ok_or(RenderError::Typography {
                reason: "invalid_line_break_range",
            })
    })
    .collect()
}

/// Split display text into logical UTF-8 ranges while retaining source offsets.
///
/// Empty ranges represent empty explicit lines. Mandatory line terminators are
/// excluded from the returned ranges, while all automatic breaks remain exact
/// source boundaries. Retaining offsets lets styled text wrap across rich-run
/// boundaries without copying or losing accessibility cluster provenance.
#[cfg(test)]
pub(crate) fn wrap_text_ranges(
    text: &str,
    wrap: bool,
    available_width: Fixed,
    max_lines: u64,
    max_segments: u64,
    measure: impl FnMut(Range<usize>) -> Result<Fixed, RenderError>,
) -> Result<Vec<Range<usize>>, RenderError> {
    Ok(wrap_text_lines(
        text,
        wrap,
        CellLineLayoutPolicy::Native,
        available_width,
        max_lines,
        max_segments,
        measure,
    )?
    .into_iter()
    .map(|line| line.source)
    .collect())
}

/// Split display text into bounded source lines under an explicit cell policy.
///
/// Empty source ranges represent empty explicit lines. Mandatory line
/// terminators are excluded. Automatic breaks preserve their exact source
/// boundaries; `WrappedLine::advance_end` separately records the visible
/// advance boundary when Calc retains an overflowing trailing ASCII space.
pub(crate) fn wrap_text_lines(
    text: &str,
    wrap: bool,
    policy: CellLineLayoutPolicy,
    available_width: Fixed,
    max_lines: u64,
    max_segments: u64,
    mut measure: impl FnMut(Range<usize>) -> Result<Fixed, RenderError>,
) -> Result<Vec<WrappedLine>, RenderError> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut state = RangeWrapState {
        lines: Vec::new(),
        current: None,
        current_width: Fixed::ZERO,
        max_lines,
        max_segments,
        segments: 0,
    };
    let mut start = 0_usize;
    for (end, opportunity) in linebreaks(text) {
        if opportunity == BreakOpportunity::Allowed && !wrap {
            continue;
        }
        state.bump_segment()?;
        if opportunity == BreakOpportunity::Allowed
            && ((policy == CellLineLayoutPolicy::Native
                && is_internal_ascii_hyphen_break(text, end))
                || (policy == CellLineLayoutPolicy::CalcEditEngine
                    && is_internal_hangul_break(text, end)))
        {
            continue;
        }
        let raw = text.get(start..end).ok_or(RenderError::Typography {
            reason: "invalid_line_break_range",
        })?;
        let atom_end = if opportunity == BreakOpportunity::Mandatory {
            start + strip_line_terminator(raw).len()
        } else {
            end
        };
        let atom = start..atom_end;
        if wrap {
            state.push_wrapped_atom(text, atom.clone(), policy, available_width, &mut measure)?;
        } else {
            state.append(atom.clone(), atom.end)?;
        }
        start = end;
        if opportunity == BreakOpportunity::Mandatory {
            match policy {
                CellLineLayoutPolicy::Native | CellLineLayoutPolicy::OdsNative => {
                    state.finish_line(atom_end)?
                }
                CellLineLayoutPolicy::CalcEditEngine => {
                    state.finish_mandatory_line(atom_end)?;
                }
            }
        }
    }
    if state.current.is_some() {
        state.finish_line(text.len())?;
    }
    if ends_with_line_terminator(text) {
        state.finish_line(text.len())?;
    }
    if state.lines.is_empty() {
        state.finish_line(text.len())?;
    }
    Ok(state.lines)
}

fn is_internal_hangul_break(text: &str, boundary: usize) -> bool {
    if boundary == 0 || boundary >= text.len() || !text.is_char_boundary(boundary) {
        return false;
    }
    text.get(..boundary)
        .and_then(|value| value.chars().next_back())
        .is_some_and(is_calc_hangul)
        && text
            .get(boundary..)
            .and_then(|value| value.chars().next())
            .is_some_and(is_calc_hangul)
}

fn is_internal_ascii_hyphen_break(text: &str, boundary: usize) -> bool {
    if boundary == 0 || boundary >= text.len() || !text.is_char_boundary(boundary) {
        return false;
    }
    let before = text
        .get(..boundary)
        .and_then(|value| value.chars().next_back());
    let after = text.get(boundary..).and_then(|value| value.chars().next());
    before == Some('-') && after.is_some_and(char::is_alphanumeric)
}

fn is_calc_hangul(ch: char) -> bool {
    matches!(
        ch as u32,
        0xAC00..=0xD7AF
            | 0x1100..=0x11FF
            | 0xA960..=0xA97F
            | 0xD7B0..=0xD7FF
            | 0x3130..=0x318F
    )
}

struct RangeWrapState {
    lines: Vec<WrappedLine>,
    current: Option<WrappedLine>,
    current_width: Fixed,
    max_lines: u64,
    max_segments: u64,
    segments: u64,
}

impl RangeWrapState {
    fn push_wrapped_atom(
        &mut self,
        text: &str,
        atom: Range<usize>,
        policy: CellLineLayoutPolicy,
        available_width: Fixed,
        measure: &mut impl FnMut(Range<usize>) -> Result<Fixed, RenderError>,
    ) -> Result<(), RenderError> {
        if atom.is_empty() {
            return Ok(());
        }
        let width = measure(atom.clone())?;
        let combined = self
            .current_width
            .checked_add(width)
            .ok_or(RenderError::CoordinateOverflow)?;
        if policy.fits(combined, available_width) {
            self.append(atom.clone(), atom.end)?;
            self.current_width = combined;
            return Ok(());
        }

        let suppressed_space = self.first_overflowing_trailing_ascii_space(
            text,
            atom.clone(),
            available_width,
            measure,
        )?;
        if let Some((space_start, space_end)) = suppressed_space {
            self.append(atom.start..space_end, space_start)?;
            self.finish_line(space_end)?;
            if space_end < atom.end {
                self.push_overlong_atom(
                    text,
                    space_end..atom.end,
                    policy,
                    available_width,
                    measure,
                )?;
            }
            return Ok(());
        }

        if self.current.is_some() {
            self.finish_automatic_line(text, atom.start)?;
        }
        if policy.fits(width, available_width) {
            self.append(atom.clone(), atom.end)?;
            self.current_width = width;
            return Ok(());
        }
        self.push_overlong_atom(text, atom, policy, available_width, measure)
    }

    fn first_overflowing_trailing_ascii_space(
        &mut self,
        text: &str,
        atom: Range<usize>,
        available_width: Fixed,
        measure: &mut impl FnMut(Range<usize>) -> Result<Fixed, RenderError>,
    ) -> Result<Option<(usize, usize)>, RenderError> {
        let value = text.get(atom.clone()).ok_or(RenderError::Typography {
            reason: "invalid_line_break_range",
        })?;
        let core = value.trim_end_matches(' ');
        if core.len() == value.len() {
            return Ok(None);
        }
        self.bump_segment()?;
        let core_end = atom.start + core.len();
        let core_width = if core_end == atom.start {
            Fixed::ZERO
        } else {
            measure(atom.start..core_end)?
        };
        let mut width = self
            .current_width
            .checked_add(core_width)
            .ok_or(RenderError::CoordinateOverflow)?;
        if !CellLineLayoutPolicy::CalcEditEngine.fits(width, available_width) {
            return Ok(None);
        }
        for space_start in core_end..atom.end {
            self.bump_segment()?;
            let space_end = space_start + 1;
            let space_width = measure(space_start..space_end)?;
            let combined = width
                .checked_add(space_width)
                .ok_or(RenderError::CoordinateOverflow)?;
            if !CellLineLayoutPolicy::CalcEditEngine.fits(combined, available_width) {
                return Ok(Some((space_start, space_end)));
            }
            width = combined;
        }
        Ok(None)
    }

    fn push_overlong_atom(
        &mut self,
        text: &str,
        atom: Range<usize>,
        policy: CellLineLayoutPolicy,
        available_width: Fixed,
        measure: &mut impl FnMut(Range<usize>) -> Result<Fixed, RenderError>,
    ) -> Result<(), RenderError> {
        let value = text.get(atom.clone()).ok_or(RenderError::Typography {
            reason: "invalid_line_break_range",
        })?;
        for (offset, grapheme) in value.grapheme_indices(true) {
            self.bump_segment()?;
            let start = atom.start + offset;
            let grapheme = start..start + grapheme.len();
            let width = measure(grapheme.clone())?;
            let combined = self
                .current_width
                .checked_add(width)
                .ok_or(RenderError::CoordinateOverflow)?;
            if grapheme.end == grapheme.start + 1
                && text.as_bytes().get(grapheme.start) == Some(&b' ')
                && !policy.fits(combined, available_width)
            {
                self.append(grapheme.clone(), grapheme.start)?;
                self.finish_line(grapheme.end)?;
                continue;
            }
            if self.current.is_some() && !policy.fits(combined, available_width) {
                self.finish_automatic_line(text, grapheme.start)?;
            }
            self.append(grapheme.clone(), grapheme.end)?;
            self.current_width = self
                .current_width
                .checked_add(width)
                .ok_or(RenderError::CoordinateOverflow)?;
            if !policy.fits(self.current_width, available_width) {
                self.finish_automatic_line(text, grapheme.end)?;
            }
        }
        Ok(())
    }

    fn append(&mut self, range: Range<usize>, advance_end: usize) -> Result<(), RenderError> {
        if range.is_empty() {
            return Ok(());
        }
        if advance_end < range.start || advance_end > range.end {
            return Err(RenderError::Typography {
                reason: "invalid_line_advance_range",
            });
        }
        match &mut self.current {
            Some(current)
                if current.source.end == range.start
                    && current.advance_end == current.source.end =>
            {
                current.source.end = range.end;
                current.advance_end = advance_end;
            }
            None => {
                self.current = Some(WrappedLine {
                    source: range,
                    advance_end,
                });
            }
            Some(_) => {
                return Err(RenderError::Typography {
                    reason: "non_contiguous_line_range",
                })
            }
        }
        Ok(())
    }

    fn bump_segment(&mut self) -> Result<(), RenderError> {
        self.segments = self.segments.saturating_add(1);
        enforce(LimitKind::TextRuns, self.max_segments, self.segments)
    }

    fn finish_mandatory_line(&mut self, empty_at: usize) -> Result<(), RenderError> {
        if self.current.is_some()
            || self
                .lines
                .last()
                .is_none_or(|line| line.source.end != empty_at)
        {
            self.finish_line(empty_at)?;
        }
        Ok(())
    }

    fn finish_automatic_line(&mut self, text: &str, empty_at: usize) -> Result<(), RenderError> {
        if let Some(current) = self.current.as_mut() {
            if current.advance_end == current.source.end
                && current.source.end > current.source.start
                && text.as_bytes().get(current.source.end - 1) == Some(&b' ')
            {
                current.advance_end -= 1;
            }
        }
        self.finish_line(empty_at)
    }

    fn finish_line(&mut self, empty_at: usize) -> Result<(), RenderError> {
        let actual = self.lines.len() as u64 + 1;
        enforce(LimitKind::TextLines, self.max_lines, actual)?;
        self.lines.push(self.current.take().unwrap_or(WrappedLine {
            source: empty_at..empty_at,
            advance_end: empty_at,
        }));
        self.current_width = Fixed::ZERO;
        Ok(())
    }
}

fn strip_line_terminator(mut value: &str) -> &str {
    while let Some(ch) = value.chars().next_back() {
        if matches!(ch, '\r' | '\n' | '\u{0085}' | '\u{2028}' | '\u{2029}') {
            value = &value[..value.len() - ch.len_utf8()];
        } else {
            break;
        }
    }
    value
}

fn ends_with_line_terminator(value: &str) -> bool {
    value
        .chars()
        .next_back()
        .is_some_and(|ch| matches!(ch, '\r' | '\n' | '\u{0085}' | '\u{2028}' | '\u{2029}'))
}

fn enforce(kind: LimitKind, limit: u64, actual: u64) -> Result<(), RenderError> {
    if actual > limit {
        Err(RenderError::LimitExceeded {
            kind,
            limit,
            actual,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monospace(value: &str) -> Result<Fixed, RenderError> {
        Ok(Fixed::from_pixels(value.graphemes(true).count() as i64))
    }

    fn monospace_range(text: &str) -> impl FnMut(Range<usize>) -> Result<Fixed, RenderError> + '_ {
        |range| {
            monospace(text.get(range).ok_or(RenderError::Typography {
                reason: "invalid_line_break_range",
            })?)
        }
    }

    #[test]
    fn wraps_words_cjk_and_mandatory_breaks_deterministically() {
        assert_eq!(
            wrap_text("ab cd", true, Fixed::from_pixels(3), 10, 100, monospace).unwrap(),
            ["ab ", "cd"]
        );
        assert_eq!(
            wrap_text("한글中文", true, Fixed::from_pixels(2), 10, 100, monospace).unwrap(),
            ["한글", "中文"]
        );
        assert_eq!(
            wrap_text("a\r\nb\n", false, Fixed::from_pixels(1), 10, 100, monospace).unwrap(),
            ["a", "b", ""]
        );
    }

    #[test]
    fn calc_uses_a_strict_fit_boundary_while_native_keeps_exact_fits() {
        let text = "ab";
        let native = wrap_text_lines(
            text,
            true,
            CellLineLayoutPolicy::Native,
            Fixed::from_pixels(2),
            10,
            100,
            monospace_range(text),
        )
        .unwrap();
        assert_eq!(
            native,
            [WrappedLine {
                source: 0..2,
                advance_end: 2,
            }]
        );

        let calc = wrap_text_lines(
            text,
            true,
            CellLineLayoutPolicy::CalcEditEngine,
            Fixed::from_pixels(2),
            10,
            100,
            monospace_range(text),
        )
        .unwrap();
        assert_eq!(
            calc,
            [
                WrappedLine {
                    source: 0..1,
                    advance_end: 1,
                },
                WrappedLine {
                    source: 1..2,
                    advance_end: 2,
                },
            ]
        );

        let multiple_spaces = "ab  cd";
        let width_three = wrap_text_lines(
            multiple_spaces,
            true,
            CellLineLayoutPolicy::CalcEditEngine,
            Fixed::from_pixels(3),
            10,
            100,
            monospace_range(multiple_spaces),
        )
        .unwrap();
        assert_eq!(width_three[0].source, 0..3);
        assert_eq!(width_three[0].advance_end, 2);

        let width_four = wrap_text_lines(
            multiple_spaces,
            true,
            CellLineLayoutPolicy::CalcEditEngine,
            Fixed::from_pixels(4),
            10,
            100,
            monospace_range(multiple_spaces),
        )
        .unwrap();
        assert_eq!(width_four[0].source, 0..4);
        assert_eq!(width_four[0].advance_end, 3);
    }

    #[test]
    fn automatic_wrap_retains_an_overflowing_ascii_space_without_its_advance() {
        let text = "ab cd";
        let native = wrap_text_lines(
            text,
            true,
            CellLineLayoutPolicy::Native,
            Fixed::from_pixels(3),
            10,
            100,
            monospace_range(text),
        )
        .unwrap();
        let calc = wrap_text_lines(
            text,
            true,
            CellLineLayoutPolicy::CalcEditEngine,
            Fixed::from_pixels(3),
            10,
            100,
            monospace_range(text),
        )
        .unwrap();

        assert_eq!(native[0].source, 0..3);
        assert_eq!(native[0].advance_end, 2);
        assert_eq!(calc[0].source, 0..3);
        assert_eq!(calc[0].advance_end, 2);
        assert_eq!(calc[1].source, 3..5);
        assert_eq!(calc[1].advance_end, 5);

        for width in [4, 5] {
            let calc = wrap_text_lines(
                text,
                true,
                CellLineLayoutPolicy::CalcEditEngine,
                Fixed::from_pixels(width),
                10,
                100,
                monospace_range(text),
            )
            .unwrap();
            assert_eq!(calc[0].source, 0..3, "width {width}");
            assert_eq!(calc[0].advance_end, 2, "width {width}");
        }

        let overlong_core = "ab ";
        let calc = wrap_text_lines(
            overlong_core,
            true,
            CellLineLayoutPolicy::CalcEditEngine,
            Fixed::from_pixels(2),
            10,
            100,
            monospace_range(overlong_core),
        )
        .unwrap();
        assert_eq!(
            calc,
            [
                WrappedLine {
                    source: 0..1,
                    advance_end: 1,
                },
                WrappedLine {
                    source: 1..3,
                    advance_end: 2,
                },
            ]
        );
    }

    #[test]
    fn calc_moves_hangul_runs_before_chopping_an_overlong_run() {
        let moved = "ab 가나다라";
        let lines = wrap_text_lines(
            moved,
            true,
            CellLineLayoutPolicy::CalcEditEngine,
            Fixed::from_pixels(5),
            10,
            100,
            monospace_range(moved),
        )
        .unwrap();
        assert_eq!(
            lines
                .iter()
                .map(|line| {
                    (
                        moved.get(line.source.clone()).unwrap(),
                        moved.get(line.source.start..line.advance_end).unwrap(),
                    )
                })
                .collect::<Vec<_>>(),
            [("ab ", "ab"), ("가나다라", "가나다라")]
        );

        let chopped = "가나다라마바사";
        let lines = wrap_text_lines(
            chopped,
            true,
            CellLineLayoutPolicy::CalcEditEngine,
            Fixed::from_pixels(4),
            10,
            100,
            monospace_range(chopped),
        )
        .unwrap();
        assert_eq!(
            lines
                .iter()
                .map(|line| chopped.get(line.source.clone()).unwrap())
                .collect::<Vec<_>>(),
            ["가나다", "라마바", "사"]
        );
    }

    #[test]
    fn native_wrapping_keeps_internal_ascii_hyphenated_words_together() {
        let text = "ab project-authored text";
        let lines = wrap_text_lines(
            text,
            true,
            CellLineLayoutPolicy::Native,
            Fixed::from_pixels(17),
            10,
            100,
            monospace_range(text),
        )
        .unwrap();

        assert_eq!(
            lines
                .iter()
                .map(|line| text.get(line.source.clone()).unwrap())
                .collect::<Vec<_>>(),
            ["ab ", "project-authored ", "text"]
        );
        assert_eq!(lines[1].advance_end, "ab project-authored".len());

        let ods = wrap_text_lines(
            text,
            true,
            CellLineLayoutPolicy::OdsNative,
            Fixed::from_pixels(17),
            10,
            100,
            monospace_range(text),
        )
        .unwrap();
        assert_eq!(
            ods.iter()
                .map(|line| text.get(line.source.clone()).unwrap())
                .collect::<Vec<_>>(),
            ["ab project-", "authored text"]
        );
    }

    #[test]
    fn calc_hangul_tailoring_uses_exact_ranges_and_charges_candidates() {
        for codepoint in [0xAC00, 0x1100, 0xA960, 0xD7B0, 0x3130] {
            let ch = char::from_u32(codepoint).unwrap();
            let pair = format!("{ch}{ch}");
            assert!(is_internal_hangul_break(&pair, ch.len_utf8()));
        }
        for ch in ['漢', 'あ', 'ア', 'A'] {
            let pair = format!("{ch}{ch}");
            assert!(!is_internal_hangul_break(&pair, ch.len_utf8()));
        }

        let text = "가나다";
        assert!(matches!(
            wrap_text_lines(
                text,
                true,
                CellLineLayoutPolicy::CalcEditEngine,
                Fixed::from_pixels(100),
                10,
                2,
                monospace_range(text),
            ),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::TextRuns,
                actual: 3,
                ..
            })
        ));
    }

    #[test]
    fn calc_hangul_tailoring_preserves_mandatory_breaks() {
        let text = "가나\n다라";
        let lines = wrap_text_lines(
            text,
            true,
            CellLineLayoutPolicy::CalcEditEngine,
            Fixed::from_pixels(100),
            10,
            100,
            monospace_range(text),
        )
        .unwrap();
        assert_eq!(
            lines
                .iter()
                .map(|line| text.get(line.source.clone()).unwrap())
                .collect::<Vec<_>>(),
            ["가나", "다라"]
        );
    }

    #[test]
    fn calc_locked_width_probe_reproduces_narrow_merged_and_wide_endpoints() {
        const TEXT: &str = concat!(
            "한국어 자동 줄바꿈 English 日本語 中文 0123456789 ",
            "한국어 자동 줄바꿈 English 日本語 中文 0123456789 ",
            "한국어 자동 줄바꿈 English 日本語 中文 0123456789"
        );
        let measure = |range: Range<usize>| {
            let value = TEXT.get(range).ok_or(RenderError::Typography {
                reason: "invalid_line_break_range",
            })?;
            let raw = value.chars().try_fold(0_i64, |total, ch| {
                let advance = match ch {
                    '한' | '국' | '자' | '줄' | '바' => 13_817,
                    '어' | '동' | '꿈' => 13_818,
                    'E' => 7_780,
                    'n' | 'g' => 8_500,
                    'l' | 'i' => 3_500,
                    's' => 7_500,
                    'h' => 11_740,
                    '日' | '本' | '語' | '中' | '文' => 15_019,
                    '0' | '1' | '2' | '7' => 8_335,
                    '3' | '4' | '5' | '6' | '8' | '9' => 8_336,
                    ' ' => 3_364,
                    _ => {
                        return Err(RenderError::Typography {
                            reason: "unexpected_locked_probe_character",
                        })
                    }
                };
                total
                    .checked_add(advance)
                    .ok_or(RenderError::CoordinateOverflow)
            })?;
            Ok(Fixed::from_raw(raw))
        };

        for (width, expected) in [
            (
                64_649,
                vec![
                    10, 17, 27, 35, 48, 52, 59, 63, 73, 80, 90, 98, 111, 115, 122, 126, 136, 143,
                    153, 161, 174, 178, 185, 188,
                ],
            ),
            (135_442, vec![27, 52, 73, 98, 115, 136, 161, 178, 188]),
            (192_512, vec![38, 63, 101, 126, 164, 188]),
        ] {
            let lines = wrap_text_lines(
                TEXT,
                true,
                CellLineLayoutPolicy::CalcEditEngine,
                Fixed::from_raw(width),
                100,
                1_000,
                measure,
            )
            .unwrap();
            assert_eq!(
                lines.iter().map(|line| line.source.end).collect::<Vec<_>>(),
                expected,
                "available width {width}"
            );
        }
    }

    #[test]
    fn calc_strict_fits_preserve_mandatory_breaks_and_grapheme_progress() {
        let explicit = "a\r\nb\n";
        let lines = wrap_text_lines(
            explicit,
            true,
            CellLineLayoutPolicy::CalcEditEngine,
            Fixed::from_pixels(1),
            10,
            100,
            monospace_range(explicit),
        )
        .unwrap();
        assert_eq!(
            lines
                .iter()
                .map(|line| explicit.get(line.source.clone()).unwrap())
                .collect::<Vec<_>>(),
            ["a", "b", ""]
        );

        let grapheme = "👩‍💻";
        let lines = wrap_text_lines(
            grapheme,
            true,
            CellLineLayoutPolicy::CalcEditEngine,
            Fixed::from_pixels(1),
            10,
            100,
            monospace_range(grapheme),
        )
        .unwrap();
        assert_eq!(
            lines,
            [WrappedLine {
                source: 0..grapheme.len(),
                advance_end: grapheme.len(),
            }]
        );

        let native = "W\n";
        let lines = wrap_text_lines(
            native,
            true,
            CellLineLayoutPolicy::Native,
            Fixed::from_raw(1),
            10,
            100,
            |_| Ok(Fixed::from_pixels(1)),
        )
        .unwrap();
        assert_eq!(
            lines,
            [
                WrappedLine {
                    source: 0..1,
                    advance_end: 1,
                },
                WrappedLine {
                    source: 1..1,
                    advance_end: 1,
                },
                WrappedLine {
                    source: 2..2,
                    advance_end: 2,
                },
            ],
            "Native retains the pre-policy overwide-plus-mandatory behavior"
        );
    }

    #[test]
    fn overlong_atoms_preserve_extended_graphemes() {
        let lines =
            wrap_text("a\u{301}b", true, Fixed::from_pixels(1), 10, 100, monospace).unwrap();
        assert_eq!(lines, ["a\u{301}", "b"]);
    }

    #[test]
    fn line_and_segment_limits_fail_before_unbounded_growth() {
        assert!(matches!(
            wrap_text("a b", true, Fixed::from_pixels(1), 1, 100, monospace),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::TextLines,
                ..
            })
        ));
        assert!(matches!(
            wrap_text("abcdef", true, Fixed::from_pixels(1), 100, 2, monospace),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::TextRuns,
                ..
            })
        ));
    }
}
