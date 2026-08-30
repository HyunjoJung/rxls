//! Strict A1 coordinate and range parsing.

use crate::MAX_RANGE_CELLS;

const MAX_ROW: u32 = 1_048_575;
const MAX_COL: u16 = 16_383;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellAddress {
    pub(crate) row: u32,
    pub(crate) col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellRange {
    pub(crate) start: CellAddress,
    pub(crate) end: CellAddress,
}

impl CellRange {
    pub(crate) fn parse(input: &str) -> Result<Self, String> {
        let mut parts = input.split(':');
        let start = parse_cell(parts.next().unwrap_or_default())?;
        let end = match parts.next() {
            Some(value) => parse_cell(value)?,
            None => start,
        };
        if parts.next().is_some() {
            return Err("RXLS_MCP_INVALID_RANGE: expected A1 or A1:C10".to_string());
        }
        if start.row > end.row || start.col > end.col {
            return Err("RXLS_MCP_INVALID_RANGE: range endpoints are reversed".to_string());
        }
        let rows = usize::try_from(end.row - start.row + 1)
            .map_err(|_| "RXLS_MCP_RANGE_TOO_LARGE: row count overflow".to_string())?;
        let cols = usize::from(end.col - start.col + 1);
        if rows.saturating_mul(cols) > MAX_RANGE_CELLS {
            return Err(format!(
                "RXLS_MCP_RANGE_TOO_LARGE: at most {MAX_RANGE_CELLS} cells may be processed"
            ));
        }
        Ok(Self { start, end })
    }

    pub(crate) fn cell_count(self) -> usize {
        usize::try_from(self.end.row - self.start.row + 1).unwrap_or(usize::MAX)
            * usize::from(self.end.col - self.start.col + 1)
    }
}

pub(crate) fn parse_cell(input: &str) -> Result<CellAddress, String> {
    if input.is_empty() || input.trim() != input {
        return Err("RXLS_MCP_INVALID_CELL: expected an A1 cell address".to_string());
    }
    let bytes = input.as_bytes();
    let mut cursor = usize::from(bytes.first() == Some(&b'$'));
    let column_start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_alphabetic) {
        cursor += 1;
    }
    if cursor == column_start {
        return Err("RXLS_MCP_INVALID_CELL: expected column letters followed by a row".to_string());
    }
    let column_end = cursor;
    if bytes.get(cursor) == Some(&b'$') {
        cursor += 1;
    }
    let row_start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    if cursor != bytes.len() || row_start == cursor {
        return Err("RXLS_MCP_INVALID_CELL: row must contain only ASCII digits".to_string());
    }

    let mut one_based_col = 0u32;
    for byte in &bytes[column_start..column_end] {
        let digit = u32::from(byte.to_ascii_uppercase() - b'A' + 1);
        one_based_col = one_based_col
            .checked_mul(26)
            .and_then(|value| value.checked_add(digit))
            .ok_or_else(|| "RXLS_MCP_INVALID_CELL: column is outside the Excel grid".to_string())?;
    }
    let one_based_row = input[row_start..]
        .parse::<u32>()
        .map_err(|_| "RXLS_MCP_INVALID_CELL: row is outside the Excel grid".to_string())?;
    if one_based_col == 0
        || one_based_col > u32::from(MAX_COL) + 1
        || one_based_row == 0
        || one_based_row > MAX_ROW + 1
    {
        return Err("RXLS_MCP_INVALID_CELL: cell is outside the Excel grid".to_string());
    }
    Ok(CellAddress {
        row: one_based_row - 1,
        col: u16::try_from(one_based_col - 1)
            .map_err(|_| "RXLS_MCP_INVALID_CELL: column is outside the Excel grid".to_string())?,
    })
}

pub(crate) fn format_cell(address: CellAddress) -> String {
    let mut col = u32::from(address.col) + 1;
    let mut letters = Vec::with_capacity(3);
    while col > 0 {
        let digit = (col - 1) % 26;
        letters.push(char::from(b'A' + u8::try_from(digit).unwrap_or(0)));
        col = (col - 1) / 26;
    }
    letters.reverse();
    let mut output: String = letters.into_iter().collect();
    output.push_str(&(address.row + 1).to_string());
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_excel_grid_boundaries() {
        assert_eq!(
            parse_cell("$XFD$1048576").unwrap(),
            CellAddress {
                row: MAX_ROW,
                col: MAX_COL
            }
        );
        assert_eq!(format_cell(parse_cell("aa10").unwrap()), "AA10");
    }

    #[test]
    fn rejects_invalid_or_oversized_ranges() {
        for value in [
            "", "A0", "XFE1", "A1048577", "1A", "A1$", "$$A1", "A$$1", "A1:B2:C3",
        ] {
            assert!(CellRange::parse(value).is_err(), "{value}");
        }
        assert!(CellRange::parse("B2:A1").is_err());
        assert!(CellRange::parse("A1:A10001").is_err());
        assert_eq!(CellRange::parse("A1:J1000").unwrap().cell_count(), 10_000);
    }
}
