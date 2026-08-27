//! Worksheet cell/range selection validation and A1 conversion.

use crate::write::xml::a1;
use crate::{Error, Result};

use super::{MAX_XLSX_COL, MAX_XLSX_ROW};
pub(super) fn validate_row(row: u32) -> Result<()> {
    if row <= MAX_XLSX_ROW {
        Ok(())
    } else {
        Err(Error::Zip("row is outside the XLSX worksheet grid"))
    }
}

pub(super) fn validate_col(col: u16) -> Result<()> {
    if col <= MAX_XLSX_COL {
        Ok(())
    } else {
        Err(Error::Zip("column is outside the XLSX worksheet grid"))
    }
}

pub(super) fn validate_layout_range(r0: u32, c0: u16, r1: u32, c1: u16) -> Result<()> {
    validate_row(r0)?;
    validate_row(r1)?;
    validate_col(c0)?;
    validate_col(c1)?;
    if r0 <= r1 && c0 <= c1 {
        Ok(())
    } else {
        Err(Error::Zip("worksheet range endpoints are reversed"))
    }
}

pub(super) fn parse_a1_cell(reference: &str) -> Option<(u32, u16)> {
    let bytes = reference.as_bytes();
    let mut i = usize::from(bytes.first() == Some(&b'$'));
    let mut column = 0u32;
    let mut letters = 0usize;
    while let Some(&byte) = bytes.get(i) {
        if !byte.is_ascii_alphabetic() {
            break;
        }
        column = column
            .checked_mul(26)?
            .checked_add(u32::from(byte.to_ascii_uppercase() - b'A') + 1)?;
        letters += 1;
        i += 1;
    }
    if letters == 0 || column == 0 || column > u32::from(MAX_XLSX_COL) + 1 {
        return None;
    }
    if bytes.get(i) == Some(&b'$') {
        i += 1;
    }
    let digits_start = i;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    if digits_start == i || i != bytes.len() {
        return None;
    }
    let row = reference[digits_start..].parse::<u32>().ok()?;
    if row == 0 || row > MAX_XLSX_ROW + 1 {
        return None;
    }
    Some((row - 1, u16::try_from(column - 1).ok()?))
}

pub(super) fn parse_a1_range(reference: &str) -> Option<(u32, u16, u32, u16)> {
    let (first, last) = reference.split_once(':').unwrap_or((reference, reference));
    let (r0, c0) = parse_a1_cell(first)?;
    let (r1, c1) = parse_a1_cell(last)?;
    (r0 <= r1 && c0 <= c1).then_some((r0, c0, r1, c1))
}

pub(super) fn ranges_overlap(left: (u32, u16, u32, u16), right: (u32, u16, u32, u16)) -> bool {
    left.0 <= right.2 && right.0 <= left.2 && left.1 <= right.3 && right.1 <= left.3
}

pub(super) fn range_ref(range: (u32, u16, u32, u16)) -> String {
    format!("{}:{}", a1(range.0, range.1), a1(range.2, range.3))
}
