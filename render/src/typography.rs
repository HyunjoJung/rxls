//! Bounded Unicode line breaking shared by deterministic text layout.

use std::ops::Range;

use unicode_linebreak::{linebreaks, BreakOpportunity};
use unicode_segmentation::UnicodeSegmentation;

use crate::error::{LimitKind, RenderError};
use crate::scene::Fixed;

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
pub(crate) fn wrap_text_ranges(
    text: &str,
    wrap: bool,
    available_width: Fixed,
    max_lines: u64,
    max_segments: u64,
    mut measure: impl FnMut(Range<usize>) -> Result<Fixed, RenderError>,
) -> Result<Vec<Range<usize>>, RenderError> {
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
            state.push_wrapped_atom(text, atom.clone(), available_width, &mut measure)?;
        } else {
            state.append(atom)?;
        }
        start = end;
        if opportunity == BreakOpportunity::Mandatory {
            state.finish_line(atom_end)?;
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

struct RangeWrapState {
    lines: Vec<Range<usize>>,
    current: Option<Range<usize>>,
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
        if self.current.is_none() || combined <= available_width {
            if self.current.is_none() && width > available_width {
                return self.push_overlong_atom(text, atom, available_width, measure);
            }
            self.append(atom)?;
            self.current_width = combined;
            return Ok(());
        }
        self.finish_line(atom.start)?;
        if width > available_width {
            self.push_overlong_atom(text, atom, available_width, measure)
        } else {
            self.append(atom)?;
            self.current_width = width;
            Ok(())
        }
    }

    fn push_overlong_atom(
        &mut self,
        text: &str,
        atom: Range<usize>,
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
            if self.current.is_some() && combined > available_width {
                self.finish_line(grapheme.start)?;
            }
            self.append(grapheme.clone())?;
            self.current_width = self
                .current_width
                .checked_add(width)
                .ok_or(RenderError::CoordinateOverflow)?;
            if self.current_width > available_width {
                self.finish_line(grapheme.end)?;
            }
        }
        Ok(())
    }

    fn append(&mut self, range: Range<usize>) -> Result<(), RenderError> {
        if range.is_empty() {
            return Ok(());
        }
        match &mut self.current {
            Some(current) if current.end == range.start => current.end = range.end,
            None => self.current = Some(range),
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

    fn finish_line(&mut self, empty_at: usize) -> Result<(), RenderError> {
        let actual = self.lines.len() as u64 + 1;
        enforce(LimitKind::TextLines, self.max_lines, actual)?;
        self.lines
            .push(self.current.take().unwrap_or(empty_at..empty_at));
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
