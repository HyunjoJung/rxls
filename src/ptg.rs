//! BIFF formula-token (`Ptg`) decompilation: the `rgce` byte blob of a `FORMULA`
//! record → a human-readable infix string (e.g. `SUM(A1:A9)*2`).
//!
//! An RPN walk: operand `Ptg`s push string leaves, operator `Ptg`s pop and
//! combine, function `Ptg`s pop their args. Unknown/unsupported tokens stop the
//! walk and the best-effort prefix is returned — never a panic or an out-of-bounds
//! read. Only the cached value is authoritative; this is the source text.
//!
//! Reference: [MS-XLS] 2.5.198 (Ptg), 2.5.198.103 (RgceLoc).

fn u16le(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}

fn u32le(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Read a `RgceLoc` (row, packed col) and the bytes consumed. BIFF8 packs the row
/// as a `u16`; BIFF12 (`.xlsb`) widens it to a `u32` — the column word (with its
/// rel-flag bits) is a `u16` in both.
fn read_loc(rgce: &[u8], i: usize, biff12: bool) -> Option<(u32, u16, usize)> {
    if biff12 {
        Some((u32le(rgce, i)?, u16le(rgce, i + 4)?, 6))
    } else {
        Some((u32::from(u16le(rgce, i)?), u16le(rgce, i + 2)?, 4))
    }
}

/// Read a `RgceArea` (rowFirst, rowLast, colFirst, colLast) and bytes consumed —
/// `u16` rows in BIFF8, `u32` rows in BIFF12.
fn read_area(rgce: &[u8], i: usize, biff12: bool) -> Option<(u32, u32, u16, u16, usize)> {
    if biff12 {
        Some((
            u32le(rgce, i)?,
            u32le(rgce, i + 4)?,
            u16le(rgce, i + 8)?,
            u16le(rgce, i + 10)?,
            12,
        ))
    } else {
        Some((
            u32::from(u16le(rgce, i)?),
            u32::from(u16le(rgce, i + 2)?),
            u16le(rgce, i + 4)?,
            u16le(rgce, i + 6)?,
            8,
        ))
    }
}

/// `Ptg` reference location → A1 (row, col:u16 with rel flags in the top bits).
/// We render absolute refs without `$` (kept simple + readable).
fn ref_a1(row: u32, col_packed: u16) -> String {
    let col = col_packed & 0x3FFF; // low 14 bits = column (BIFF8)
    let mut letters = Vec::new();
    let mut c = u32::from(col) + 1;
    while c > 0 {
        letters.push(b'A' + ((c - 1) % 26) as u8);
        c = (c - 1) / 26;
    }
    letters.reverse();
    let col_s = String::from_utf8(letters).unwrap_or_else(|_| "A".into());
    format!("{col_s}{}", row + 1)
}

/// A function from the [MS-XLS] `ftab` — name + fixed arg count (`None` = variable).
fn func_name(id: u16) -> &'static str {
    match id {
        0 => "COUNT",
        1 => "IF",
        4 => "SUM",
        5 => "AVERAGE",
        6 => "MIN",
        7 => "MAX",
        8 => "ROW",
        9 => "COLUMN",
        15 => "SIN",
        16 => "COS",
        19 => "PI",
        20 => "SQRT",
        24 => "EXP",
        25 => "LN",
        26 => "LOG10",
        27 => "ABS",
        28 => "INT",
        30 => "ROUND",
        34 => "FALSE",
        35 => "TRUE",
        36 => "AND",
        37 => "OR",
        38 => "NOT",
        48 => "TEXT",
        63 => "RAND",
        74 => "DATE",
        76 => "DAYS360",
        82 => "SEARCH",
        97 => "ATAN2",
        100 => "CHOOSE",
        101 => "HLOOKUP",
        102 => "VLOOKUP",
        111 => "CHAR",
        112 => "LOWER",
        113 => "UPPER",
        115 => "LEN",
        119 => "REPLACE",
        124 => "FIND",
        148 => "TRIM",
        162 => "CLEAN",
        169 => "COUNTA",
        183 => "PRODUCT",
        190 => "ISNUMBER",
        212 => "ROUNDUP",
        213 => "ROUNDDOWN",
        216 => "RANK",
        219 => "ADDRESS",
        220 => "DAYS",
        221 => "TODAY",
        228 => "SUMPRODUCT",
        252 => "FREQUENCY",
        269 => "AVEDEV",
        279 => "INDEX",
        345 => "COUNTIF",
        346 => "COUNTBLANK",
        _ => "",
    }
}

/// Decompile an `rgce` token blob to an infix formula string (without `=`).
/// Returns an empty string if nothing meaningful could be recovered.
pub(crate) fn decompile(rgce: &[u8], biff12: bool) -> String {
    let mut stack: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < rgce.len() {
        let ptg = rgce[i];
        i += 1;
        // Match the full token: operators (0x03–0x14, no class bits) are distinct
        // from operands whose low 5 bits collide but carry a 0x20/0x40/0x60 class
        // (e.g. PtgSub 0x04 vs PtgRef 0x24).
        match ptg {
            // Binary operators.
            0x03..=0x0E => {
                let op = match ptg {
                    0x03 => "+",
                    0x04 => "-",
                    0x05 => "*",
                    0x06 => "/",
                    0x07 => "^",
                    0x08 => "&",
                    0x09 => "<",
                    0x0A => "<=",
                    0x0B => "=",
                    0x0C => ">=",
                    0x0D => ">",
                    0x0E => "<>",
                    _ => "?",
                };
                let (Some(b), Some(a)) = (stack.pop(), stack.pop()) else {
                    return joined(&stack);
                };
                stack.push(format!("{a}{op}{b}"));
            }
            0x12 => {} // PtgUplus — no-op
            0x13 => {
                if let Some(a) = stack.pop() {
                    stack.push(format!("-{a}"));
                }
            }
            0x14 => {
                if let Some(a) = stack.pop() {
                    stack.push(format!("{a}%"));
                }
            }
            0x15 => {
                if let Some(a) = stack.pop() {
                    stack.push(format!("({a})"));
                }
            }
            0x16 => stack.push(String::new()), // PtgMissArg
            0x17 => {
                // PtgStr: ShortXLUnicodeString (cch:u8, grbit:u8, chars).
                let cch = *rgce.get(i).unwrap_or(&0) as usize;
                let high = rgce.get(i + 1).map_or(0, |g| g & 1);
                let (text, used) = if high == 1 {
                    let bytes = cch * 2;
                    let units: Vec<u16> = rgce
                        .get(i + 2..i + 2 + bytes)
                        .unwrap_or(&[])
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    (String::from_utf16_lossy(&units), 2 + bytes)
                } else {
                    let s = rgce
                        .get(i + 2..i + 2 + cch)
                        .map(|b| b.iter().map(|&c| c as char).collect())
                        .unwrap_or_default();
                    (s, 2 + cch)
                };
                stack.push(format!("\"{}\"", text.replace('"', "\"\"")));
                i += used;
            }
            0x1C => {
                stack.push("#ERR".into());
                i += 1;
            }
            0x1D => {
                let b = rgce.get(i).copied().unwrap_or(0) != 0;
                stack.push(if b { "TRUE" } else { "FALSE" }.into());
                i += 1;
            }
            0x1E => {
                stack.push(u16le(rgce, i).unwrap_or(0).to_string());
                i += 2;
            }
            0x1F => {
                let f = rgce
                    .get(i..i + 8)
                    .map(|b| f64::from_le_bytes(b.try_into().unwrap_or([0; 8])))
                    .unwrap_or(0.0);
                stack.push(crate::format_number(f));
                i += 8;
            }
            0x01 => return joined(&stack), // PtgExp (shared/array) — stop
            // PtgName (class 0x23/0x43/0x63): defined-name index + reserved.
            0x23 | 0x43 | 0x63 => {
                stack.push(format!("Name{}", u16le(rgce, i).unwrap_or(0)));
                i += 4;
            }
            // PtgRef (class 0x24/0x44/0x64): a RgceLoc (row, packed col).
            0x24 | 0x44 | 0x64 => {
                let Some((row, col, n)) = read_loc(rgce, i, biff12) else {
                    return joined(&stack);
                };
                stack.push(ref_a1(row, col));
                i += n;
            }
            // PtgArea (class 0x25/0x45/0x65): a RgceArea (rowFirst/Last, colFirst/Last).
            0x25 | 0x45 | 0x65 => {
                let Some((r0, r1, c0, c1, n)) = read_area(rgce, i, biff12) else {
                    return joined(&stack);
                };
                stack.push(format!("{}:{}", ref_a1(r0, c0), ref_a1(r1, c1)));
                i += n;
            }
            // PtgRef3d / PtgArea3d include an external-sheet index before the
            // same loc/area payload. The model has no sheet-qualified formula
            // surface yet, so expose the coordinate best-effort.
            0x1A | 0x3A | 0x5A | 0x7A => {
                i += 2; // ixti
                let Some((row, col, n)) = read_loc(rgce, i, biff12) else {
                    return joined(&stack);
                };
                stack.push(ref_a1(row, col));
                i += n;
            }
            0x1B | 0x3B | 0x5B | 0x7B => {
                i += 2; // ixti
                let Some((r0, r1, c0, c1, n)) = read_area(rgce, i, biff12) else {
                    return joined(&stack);
                };
                stack.push(format!("{}:{}", ref_a1(r0, c0), ref_a1(r1, c1)));
                i += n;
            }
            // PtgFunc (0x21/0x41/0x61): iftab:u16, fixed args.
            0x21 | 0x41 | 0x61 => {
                let id = u16le(rgce, i).unwrap_or(0);
                i += 2;
                let name = func_name(id);
                let arity = fixed_arity(id);
                if name.is_empty() || stack.len() < arity {
                    return joined(&stack);
                }
                let args = pop_args(&mut stack, arity);
                stack.push(format!("{name}({})", args.join(",")));
            }
            // PtgFuncVar (0x22/0x42/0x62): cArgs:u8, iftab:u16.
            0x22 | 0x42 | 0x62 => {
                let cargs = *rgce.get(i).unwrap_or(&0) as usize;
                let id = u16le(rgce, i + 1).unwrap_or(0);
                i += 3;
                let name = func_name(id);
                if name.is_empty() || stack.len() < cargs {
                    return joined(&stack);
                }
                let args = pop_args(&mut stack, cargs);
                stack.push(format!("{name}({})", args.join(",")));
            }
            _ => return joined(&stack), // unknown Ptg — best-effort prefix
        }
    }
    joined(&stack)
}

fn pop_args(stack: &mut Vec<String>, n: usize) -> Vec<String> {
    let mut args: Vec<String> = (0..n).filter_map(|_| stack.pop()).collect();
    args.reverse();
    args
}

fn fixed_arity(id: u16) -> usize {
    match id {
        19 | 34 | 35 | 63 | 221 => 0, // PI/FALSE/TRUE/RAND/TODAY
        8 | 9 | 15 | 16 | 20 | 24 | 25 | 26 | 27 | 28 | 38 | 111 | 112 | 113 | 115 | 148 | 162
        | 190 => 1,
        30 | 76 | 82 | 97 | 124 | 212 | 213 | 220 => 2,
        1 | 74 | 119 => 3,
        _ => 1,
    }
}

fn joined(stack: &[String]) -> String {
    stack.last().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_to_a1() {
        assert_eq!(ref_a1(0, 0), "A1");
        assert_eq!(ref_a1(8, 1), "B9");
        assert_eq!(ref_a1(0, 26), "AA1");
    }

    #[test]
    fn decompiles_sum_area_times_two() {
        // SUM(A1:A9)*2 = PtgArea(A1:A9) PtgInt(2) PtgFuncVar... actually:
        // PtgArea, PtgFuncVar(1 arg, SUM=4), PtgInt(2), PtgMul.
        let mut rgce = vec![0x25]; // PtgArea
        rgce.extend_from_slice(&0u16.to_le_bytes()); // rowFirst 0
        rgce.extend_from_slice(&8u16.to_le_bytes()); // rowLast 8
        rgce.extend_from_slice(&0u16.to_le_bytes()); // colFirst 0
        rgce.extend_from_slice(&0u16.to_le_bytes()); // colLast 0
        rgce.push(0x22); // PtgFuncVar
        rgce.push(1); // cArgs
        rgce.extend_from_slice(&4u16.to_le_bytes()); // SUM
        rgce.push(0x1E); // PtgInt
        rgce.extend_from_slice(&2u16.to_le_bytes());
        rgce.push(0x05); // PtgMul
        assert_eq!(decompile(&rgce, false), "SUM(A1:A9)*2");
    }

    #[test]
    fn unknown_token_is_best_effort_not_panic() {
        let rgce = [0x1E, 0x05, 0x00, 0xFF]; // PtgInt(5), then a bogus token
        assert_eq!(decompile(&rgce, false), "5");
    }

    #[test]
    fn decompiles_3d_refs_without_sheet_names() {
        let ref3d = [0x3A, 0, 0, 3, 0, 4, 0]; // ixti 0, E4
        assert_eq!(decompile(&ref3d, false), "E4");

        let mut area3d = vec![0x3B, 0, 0]; // ixti 0
        area3d.extend_from_slice(&0u16.to_le_bytes()); // rowFirst
        area3d.extend_from_slice(&2u16.to_le_bytes()); // rowLast
        area3d.extend_from_slice(&0u16.to_le_bytes()); // colFirst
        area3d.extend_from_slice(&1u16.to_le_bytes()); // colLast
        assert_eq!(decompile(&area3d, false), "A1:B3");
    }

    #[test]
    fn decompiles_biff12_3d_refs_without_sheet_names() {
        let mut ref3d = vec![0x3A, 0, 0]; // ixti 0
        ref3d.extend_from_slice(&3u32.to_le_bytes()); // row
        ref3d.extend_from_slice(&4u16.to_le_bytes()); // col
        assert_eq!(decompile(&ref3d, true), "E4");

        let mut area3d = vec![0x3B, 0, 0]; // ixti 0
        area3d.extend_from_slice(&0u32.to_le_bytes()); // rowFirst
        area3d.extend_from_slice(&2u32.to_le_bytes()); // rowLast
        area3d.extend_from_slice(&0u16.to_le_bytes()); // colFirst
        area3d.extend_from_slice(&1u16.to_le_bytes()); // colLast
        assert_eq!(decompile(&area3d, true), "A1:B3");
    }
}
