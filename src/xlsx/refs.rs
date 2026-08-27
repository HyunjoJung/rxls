/// A worksheet range represented as zero-based row and column bounds.
pub(super) type SheetRange = (u32, u16, u32, u16);

/// A1-style reference -> 0-based `(row, col)`.
pub(super) fn parse_ref(r: &str) -> Option<(u32, u16)> {
    let mut col: u32 = 0;
    let mut row: u32 = 0;
    let mut seen_col = false;
    let mut seen_row = false;
    for c in r.chars() {
        if c.is_ascii_alphabetic() {
            if seen_row {
                return None;
            }
            // Checked arithmetic keeps hostile references within the crate's
            // panic-free contract.
            col = col
                .checked_mul(26)?
                .checked_add(c.to_ascii_uppercase() as u32 - 'A' as u32 + 1)?;
            seen_col = true;
        } else if c.is_ascii_digit() {
            row = row.checked_mul(10)?.checked_add(c as u32 - '0' as u32)?;
            seen_row = true;
        }
    }
    // Reject anything past Excel's grid (XFD = col 16384 1-based, 1048576 rows).
    if !seen_col || !seen_row || col == 0 || row == 0 || col > 16_384 || row > 1_048_576 {
        return None;
    }
    Some((row - 1, u16::try_from(col - 1).ok()?))
}

/// `A1:C3` (or a lone `A1`) -> `(first_row, first_col, last_row, last_col)`.
pub(super) fn parse_range(s: &str) -> Option<SheetRange> {
    let mut it = s.split(':');
    let first = parse_ref(it.next()?)?;
    let last = match it.next() {
        Some(r) => parse_ref(r)?,
        None => first,
    };
    Some((first.0, first.1, last.0, last.1))
}

/// Convert a 0-based column index to A1 letters (0 -> `A`, 25 -> `Z`).
fn col_letters(mut idx: u32) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (idx % 26) as u8);
        if idx < 26 {
            break;
        }
        idx = idx / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

/// Parse 1-3 A1 column letters to a 0-based index.
pub(super) fn letters_col(s: &[char]) -> Option<u32> {
    if s.is_empty() || s.len() > 3 {
        return None;
    }
    let mut idx: u32 = 0;
    for &c in s {
        if !c.is_ascii_uppercase() {
            return None;
        }
        idx = idx
            .checked_mul(26)?
            .checked_add(c as u32 - 'A' as u32 + 1)?;
    }
    Some(idx - 1)
}

/// Try to read an A1 cell reference at `ch[start]` and shift its relative parts.
fn try_shift_ref(ch: &[char], start: usize, drow: i64, dcol: i64) -> Option<(usize, String)> {
    if !token_boundary_before(ch, start) {
        return None;
    }
    let mut i = start;
    let col_abs = ch.get(i) == Some(&'$');
    if col_abs {
        i += 1;
    }
    let lstart = i;
    while i < ch.len() && ch[i].is_ascii_uppercase() && i - lstart < 3 {
        i += 1;
    }
    let letters = &ch[lstart..i];
    if letters.is_empty() {
        return None;
    }
    let row_abs = ch.get(i) == Some(&'$');
    if row_abs {
        i += 1;
    }
    let dstart = i;
    while i < ch.len() && ch[i].is_ascii_digit() {
        i += 1;
    }
    if i == dstart || !token_boundary_after(ch, i) {
        return None;
    }
    let col = letters_col(letters)?;
    let row: u32 = ch[dstart..i].iter().collect::<String>().parse().ok()?;
    // An A1-shaped name outside the grid is not a cell reference.
    if row == 0 || col > 16_383 || row > 1_048_576 {
        return None;
    }
    let new_col = if col_abs {
        col as i64
    } else {
        col as i64 + dcol
    };
    let new_row = if row_abs {
        row as i64
    } else {
        row as i64 + drow
    };
    if !(0..=16_383).contains(&new_col) || !(1..=1_048_576).contains(&new_row) {
        return Some((i - start, "#REF!".to_string()));
    }
    let mut out = String::new();
    if col_abs {
        out.push('$');
    }
    out.push_str(&col_letters(new_col as u32));
    if row_abs {
        out.push('$');
    }
    out.push_str(&new_row.to_string());
    Some((i - start, out))
}

fn token_boundary_before(ch: &[char], start: usize) -> bool {
    start == 0 || {
        let p = ch[start - 1];
        !(p.is_ascii_alphanumeric() || p == '_')
    }
}

fn token_boundary_after(ch: &[char], end: usize) -> bool {
    !matches!(
        ch.get(end),
        Some(after) if *after == '(' || after.is_ascii_alphanumeric() || *after == '_'
    )
}

fn parse_whole_row_part(ch: &[char], mut i: usize) -> Option<(bool, u32, usize)> {
    let abs = ch.get(i) == Some(&'$');
    if abs {
        i += 1;
    }
    let start = i;
    while i < ch.len() && ch[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    let row: u32 = ch[start..i].iter().collect::<String>().parse().ok()?;
    if row == 0 || row > 1_048_576 {
        return None;
    }
    Some((abs, row, i))
}

fn shift_row_part(row: u32, abs: bool, drow: i64) -> Option<u32> {
    let shifted = if abs { row as i64 } else { row as i64 + drow };
    (1..=1_048_576).contains(&shifted).then_some(shifted as u32)
}

fn try_shift_whole_row_ref(ch: &[char], start: usize, drow: i64) -> Option<(usize, String)> {
    if !token_boundary_before(ch, start) {
        return None;
    }
    let (first_abs, first, mut i) = parse_whole_row_part(ch, start)?;
    if ch.get(i) != Some(&':') {
        return None;
    }
    i += 1;
    let (last_abs, last, end) = parse_whole_row_part(ch, i)?;
    if !token_boundary_after(ch, end) {
        return None;
    }
    let (Some(first), Some(last)) = (
        shift_row_part(first, first_abs, drow),
        shift_row_part(last, last_abs, drow),
    ) else {
        return Some((end - start, "#REF!".to_string()));
    };
    let mut out = String::new();
    if first_abs {
        out.push('$');
    }
    out.push_str(&first.to_string());
    out.push(':');
    if last_abs {
        out.push('$');
    }
    out.push_str(&last.to_string());
    Some((end - start, out))
}

fn parse_whole_col_part(ch: &[char], mut i: usize) -> Option<(bool, u32, usize)> {
    let abs = ch.get(i) == Some(&'$');
    if abs {
        i += 1;
    }
    let start = i;
    while i < ch.len() && ch[i].is_ascii_uppercase() && i - start < 3 {
        i += 1;
    }
    if i == start {
        return None;
    }
    let col = letters_col(&ch[start..i])?;
    if col > 16_383 {
        return None;
    }
    Some((abs, col, i))
}

fn shift_col_part(col: u32, abs: bool, dcol: i64) -> Option<u32> {
    let shifted = if abs { col as i64 } else { col as i64 + dcol };
    (0..=16_383).contains(&shifted).then_some(shifted as u32)
}

fn try_shift_whole_col_ref(ch: &[char], start: usize, dcol: i64) -> Option<(usize, String)> {
    if !token_boundary_before(ch, start) {
        return None;
    }
    let (first_abs, first, mut i) = parse_whole_col_part(ch, start)?;
    if ch.get(i) != Some(&':') {
        return None;
    }
    i += 1;
    let (last_abs, last, end) = parse_whole_col_part(ch, i)?;
    if !token_boundary_after(ch, end) {
        return None;
    }
    let (Some(first), Some(last)) = (
        shift_col_part(first, first_abs, dcol),
        shift_col_part(last, last_abs, dcol),
    ) else {
        return Some((end - start, "#REF!".to_string()));
    };
    let mut out = String::new();
    if first_abs {
        out.push('$');
    }
    out.push_str(&col_letters(first));
    out.push(':');
    if last_abs {
        out.push('$');
    }
    out.push_str(&col_letters(last));
    Some((end - start, out))
}

/// Shift relative A1 references while reconstructing a shared-formula follower.
pub(super) fn shift_formula(f: &str, drow: i64, dcol: i64) -> String {
    let ch: Vec<char> = f.chars().collect();
    let mut out = String::with_capacity(f.len());
    let mut i = 0;
    let mut in_string = false;
    let mut in_quote = false;
    while i < ch.len() {
        let c = ch[i];
        if c == '"' && !in_quote {
            in_string = !in_string;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\'' && !in_string {
            in_quote = !in_quote;
            out.push(c);
            i += 1;
            continue;
        }
        if !in_string && !in_quote {
            if let Some((consumed, shifted)) = try_shift_whole_row_ref(&ch, i, drow) {
                out.push_str(&shifted);
                i += consumed;
                continue;
            }
            if let Some((consumed, shifted)) = try_shift_whole_col_ref(&ch, i, dcol) {
                out.push_str(&shifted);
                i += consumed;
                continue;
            }
            if let Some((consumed, shifted)) = try_shift_ref(&ch, i, drow, dcol) {
                out.push_str(&shifted);
                i += consumed;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}
