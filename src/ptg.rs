//! BIFF formula-token (`Ptg`) decompilation: the `rgce` byte blob of a `FORMULA`
//! record → a human-readable infix string (e.g. `SUM(A1:A9)*2`).
//!
//! An RPN walk: operand `Ptg`s push string leaves, operator `Ptg`s pop and
//! combine, function `Ptg`s pop their args. Unsupported or malformed tokens are
//! represented explicitly rather than silently truncating the source. The walk
//! is bounds checked and never treats the stored cached result as formula source.
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

/// One entry in the workbook's XTI table, resolving an external-sheet index to
/// a first/last sheet pair. Negative indices are BIFF sentinel values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExternSheet {
    /// Zero-based index into the workbook's SUPBOOK/supporting-link table.
    pub(crate) supbook_index: usize,
    pub(crate) first_sheet: i32,
    pub(crate) last_sheet: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FormulaDiagnosticKind {
    IncompleteTokenStream,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FormulaDiagnostic {
    pub(crate) kind: FormulaDiagnosticKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DecompileResult {
    pub(crate) formula: String,
    pub(crate) diagnostics: Vec<FormulaDiagnostic>,
}

/// Workbook and cell position needed to recover reference spelling.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Context<'a> {
    pub(crate) biff12: bool,
    /// BIFF5/7 uses the legacy record/string dialect. Its core cell-reference
    /// token payload is BIFF8-sized, but keeping the dialect explicit prevents
    /// BIFF12 assumptions from leaking into legacy formula decoding.
    pub(crate) biff5: bool,
    /// `PtgRef3d` and `PtgArea3d` use offset-bearing `Rgce*Rel` payloads only
    /// inside a `NameParsedFormula`; cell formulas store coordinates instead.
    pub(crate) name_formula: bool,
    pub(crate) base_row: u32,
    pub(crate) base_col: u16,
    pub(crate) sheet_names: &'a [String],
    pub(crate) extern_sheets: &'a [ExternSheet],
    /// Per-SUPBOOK external-name table, in the one-based order referenced by
    /// `PtgNameX`. Empty for formats/readers that do not retain the table.
    pub(crate) external_names: &'a [Vec<String>],
    pub(crate) defined_names: &'a [String],
}

impl<'a> Context<'a> {
    pub(crate) fn new(biff12: bool) -> Self {
        Self {
            biff12,
            biff5: false,
            name_formula: false,
            base_row: 0,
            base_col: 0,
            sheet_names: &[],
            extern_sheets: &[],
            external_names: &[],
            defined_names: &[],
        }
    }

    #[cfg(test)]
    fn biff5() -> Self {
        Self {
            biff12: false,
            biff5: true,
            name_formula: false,
            base_row: 0,
            base_col: 0,
            sheet_names: &[],
            extern_sheets: &[],
            external_names: &[],
            defined_names: &[],
        }
    }
}

/// Return the anchor encoded by a standalone `PtgExp` formula.
pub(crate) fn exp_anchor(rgce: &[u8], rgb_extra: &[u8], biff12: bool) -> Option<(u32, u16)> {
    if rgce.first().copied()? != 0x01 {
        return None;
    }
    if biff12 {
        Some((u32le(rgce, 1)?, u16::try_from(u32le(rgb_extra, 0)?).ok()?))
    } else {
        Some((u32::from(u16le(rgce, 1)?), u16le(rgce, 3)?))
    }
}

/// `Ptg` reference location → A1, preserving the BIFF relative/absolute flags.
///
/// `RgceLoc` stores coordinates even when a reference is marked relative.
/// `RgceLocRel` stores offsets for the relative portions. The latter is used by
/// `PtgRefN`/`PtgAreaN`, and by 3-D references in a `NameParsedFormula`.
fn ref_a1(row: u32, col_packed: u16, context: &Context<'_>, relative_payload: bool) -> String {
    let col_raw = col_packed & 0x3FFF;
    let col_relative = col_packed & 0x4000 != 0;
    let row_relative = col_packed & 0x8000 != 0;
    let row = if relative_payload && row_relative {
        let offset = if context.biff12 {
            i64::from(row as i32)
        } else {
            i64::from(row as u16 as i16)
        };
        (i64::from(context.base_row) + offset).rem_euclid(if context.biff12 {
            1_048_576
        } else {
            65_536
        })
    } else {
        i64::from(row)
    };
    let col = if relative_payload && col_relative {
        (i64::from(context.base_col) + sign_extend(u32::from(col_raw), 14))
            .rem_euclid(if context.biff12 { 16_384 } else { 256 })
    } else {
        i64::from(col_raw)
    };
    let max_row = if context.biff12 { 1_048_575 } else { 65_535 };
    let max_col = if context.biff12 { 16_383 } else { 255 };
    if !(0..=max_row).contains(&row) || !(0..=max_col).contains(&col) {
        return "#REF!".to_string();
    }

    let mut letters = Vec::new();
    let mut c = col as u32 + 1;
    while c > 0 {
        letters.push(b'A' + ((c - 1) % 26) as u8);
        c = (c - 1) / 26;
    }
    letters.reverse();
    let col_s = String::from_utf8(letters).unwrap_or_else(|_| "A".into());
    format!(
        "{}{}{}{}",
        if col_relative { "" } else { "$" },
        col_s,
        if row_relative { "" } else { "$" },
        row + 1
    )
}

fn sign_extend(value: u32, bits: u32) -> i64 {
    let shift = 64 - bits;
    ((i64::from(value)) << shift) >> shift
}

fn absolute_a1(row: u32, col: u16) -> String {
    ref_a1(row, col & 0x3FFF, &Context::new(true), false)
}

fn quote_sheet_name(name: &str) -> String {
    let unquoted = !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.'))
        && !name.chars().next().is_some_and(|ch| ch.is_ascii_digit());
    if unquoted {
        name.to_string()
    } else {
        format!("'{}'", name.replace('\'', "''"))
    }
}

fn sheet_prefix(ixti: u16, context: &Context<'_>) -> String {
    let span = context
        .extern_sheets
        .get(usize::from(ixti))
        .copied()
        .or_else(|| {
            (usize::from(ixti) < context.sheet_names.len()).then_some(ExternSheet {
                supbook_index: 0,
                first_sheet: i32::from(ixti),
                last_sheet: i32::from(ixti),
            })
        });
    let Some(span) = span else {
        return format!("[ixti:{ixti}]!");
    };
    let (Ok(first), Ok(last)) = (
        usize::try_from(span.first_sheet),
        usize::try_from(span.last_sheet),
    ) else {
        return "#REF!".to_string();
    };
    let Some(first_name) = context.sheet_names.get(first) else {
        return format!("[ixti:{ixti}]!");
    };
    let Some(last_name) = context.sheet_names.get(last) else {
        return format!("[ixti:{ixti}]!");
    };
    if first == last {
        format!("{}!", quote_sheet_name(first_name))
    } else {
        format!(
            "{}:{}!",
            quote_sheet_name(first_name),
            quote_sheet_name(last_name)
        )
    }
}

/// Decompile an `rgce` token blob to an infix formula string (without `=`).
/// Returns an empty string if nothing meaningful could be recovered.
pub(crate) fn decompile(rgce: &[u8], biff12: bool) -> String {
    decompile_with_context(rgce, &Context::new(biff12))
}

pub(crate) fn decompile_with_context(rgce: &[u8], context: &Context<'_>) -> String {
    decompile_parsed_with_context(rgce, &[], context)
}

/// Decompile a complete parsed formula, including the `RgbExtra` payload used
/// by array constants. Callers that only have `rgce` can use
/// [`decompile_with_context`]; missing extra data remains explicit in output.
pub(crate) fn decompile_parsed_with_context(
    rgce: &[u8],
    rgb_extra: &[u8],
    context: &Context<'_>,
) -> String {
    let result = decompile_detailed(rgce, rgb_extra, context);
    let _has_diagnostics = !result.diagnostics.is_empty();
    result.formula
}

pub(crate) fn decompile_detailed(
    rgce: &[u8],
    rgb_extra: &[u8],
    context: &Context<'_>,
) -> DecompileResult {
    let formula = decompile_impl(rgce, rgb_extra, context);
    let diagnostics = if formula.starts_with("_xlfn.RXLS_PARTIAL(") {
        vec![FormulaDiagnostic {
            kind: FormulaDiagnosticKind::IncompleteTokenStream,
        }]
    } else {
        Vec::new()
    };
    DecompileResult {
        formula,
        diagnostics,
    }
}

fn decompile_impl(rgce: &[u8], rgb_extra: &[u8], context: &Context<'_>) -> String {
    let mut stack: Vec<String> = Vec::new();
    let mut i = 0usize;
    let mut extra_i = 0usize;
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
            // Binary reference operators: intersection, union, and range.
            0x0F..=0x11 => {
                let op = match ptg {
                    0x0F => " ",
                    0x10 => ",",
                    0x11 => ":",
                    _ => unreachable!(),
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
                // BIFF8/12 PtgStr uses ShortXLUnicodeString (cch, grbit,
                // chars); BIFF5/7 stores cch followed directly by codepage
                // bytes. Legacy bytes are preserved one-to-one here because
                // the token context intentionally has no workbook decoder.
                let cch = *rgce.get(i).unwrap_or(&0) as usize;
                let (text, used) = if context.biff5 {
                    let s = rgce
                        .get(i + 1..i + 1 + cch)
                        .map(|b| b.iter().map(|&c| c as char).collect())
                        .unwrap_or_default();
                    (s, 1 + cch)
                } else if rgce.get(i + 1).map_or(0, |g| g & 1) == 1 {
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
            0x01 => {
                // PtgExp points to the anchor cell for a shared/array formula.
                // Resolution is performed by the workbook reader; preserve an
                // explicit marker when the anchor is unavailable.
                let (row, col, used) = if context.biff12 {
                    let (Some(row), Some(col)) = (
                        u32le(rgce, i),
                        u32le(rgb_extra, extra_i).and_then(|col| u16::try_from(col).ok()),
                    ) else {
                        return joined(&stack);
                    };
                    extra_i += 4;
                    (row, col, 4)
                } else {
                    let (Some(row), Some(col)) = (u16le(rgce, i), u16le(rgce, i + 2)) else {
                        return joined(&stack);
                    };
                    (u32::from(row), col, 4)
                };
                i += used;
                stack.push(format!("_xlfn.RXLS_SHARED({})", absolute_a1(row, col)));
            }
            // PtgAttr control/display tokens. Most do not alter the RPN stack;
            // PtgAttrSum is the compact SUM(one-argument) encoding.
            0x19 => {
                let (Some(flags), Some(value)) = (rgce.get(i).copied(), u16le(rgce, i + 1)) else {
                    return joined(&stack);
                };
                i += 3;
                if flags & 0x04 != 0 {
                    let offsets = usize::from(value).saturating_add(1);
                    let bytes = offsets.saturating_mul(2);
                    if rgce.get(i..i.saturating_add(bytes)).is_none() {
                        return joined(&stack);
                    }
                    i += bytes;
                }
                if flags & 0x10 != 0 {
                    let Some(arg) = stack.pop() else {
                        return joined(&stack);
                    };
                    stack.push(format!("SUM({arg})"));
                }
            }
            // PtgArray stores its values in the corresponding PtgExtraArray in
            // RgbExtra. Consume both structures in token order.
            0x20 | 0x40 | 0x60 => {
                let bytes = if context.biff12 { 14 } else { 7 };
                if rgce.get(i..i + bytes).is_none() {
                    return joined(&stack);
                }
                i += bytes;
                if let Some(array) = parse_array_constant(rgb_extra, &mut extra_i, context.biff12) {
                    stack.push(array);
                } else {
                    stack.push("{#ARRAY!}".to_string());
                }
            }
            // PtgName (class 0x23/0x43/0x63): defined-name index + reserved.
            0x23 | 0x43 | 0x63 => {
                let Some(index) = u32le(rgce, i) else {
                    return joined(&stack);
                };
                let name = index
                    .checked_sub(1)
                    .and_then(|index| usize::try_from(index).ok())
                    .and_then(|index| context.defined_names.get(index))
                    .cloned()
                    .unwrap_or_else(|| format!("Name{index}"));
                stack.push(name);
                i += 4;
            }
            // PtgRef (class 0x24/0x44/0x64): a RgceLoc (row, packed col).
            0x24 | 0x44 | 0x64 => {
                let Some((row, col, n)) = read_loc(rgce, i, context.biff12) else {
                    return joined(&stack);
                };
                stack.push(ref_a1(row, col, context, false));
                i += n;
            }
            // PtgRefErr / PtgAreaErr retain an explicit invalid reference while
            // consuming the same coordinate payload width as their valid peers.
            0x2A | 0x4A | 0x6A => {
                let bytes = if context.biff12 { 6 } else { 4 };
                if rgce.get(i..i + bytes).is_none() {
                    return joined(&stack);
                }
                i += bytes;
                stack.push("#REF!".to_string());
            }
            0x2B | 0x4B | 0x6B => {
                let bytes = if context.biff12 { 12 } else { 8 };
                if rgce.get(i..i + bytes).is_none() {
                    return joined(&stack);
                }
                i += bytes;
                stack.push("#REF!".to_string());
            }
            // PtgRefN / PtgAreaN use the same packed relative coordinate
            // structures and are valid in shared/name formula definitions.
            0x2C | 0x4C | 0x6C => {
                let Some((row, col, n)) = read_loc(rgce, i, context.biff12) else {
                    return joined(&stack);
                };
                stack.push(ref_a1(row, col, context, true));
                i += n;
            }
            0x2D | 0x4D | 0x6D => {
                let Some((r0, r1, c0, c1, n)) = read_area(rgce, i, context.biff12) else {
                    return joined(&stack);
                };
                stack.push(format!(
                    "{}:{}",
                    ref_a1(r0, c0, context, true),
                    ref_a1(r1, c1, context, true)
                ));
                i += n;
            }
            // PtgNameX: the ixti selects an XTI entry, whose SUPBOOK index in
            // turn selects a one-based external-name table. Preserve both
            // indices diagnostically when the source table is absent/damaged.
            0x39 | 0x59 | 0x79 => {
                let (Some(ixti), Some(index)) = (u16le(rgce, i), u32le(rgce, i + 2)) else {
                    return joined(&stack);
                };
                i += 6;
                let resolved = context
                    .extern_sheets
                    .get(usize::from(ixti))
                    .and_then(|xti| {
                        let names = context.external_names.get(xti.supbook_index)?;
                        let index = usize::try_from(index.checked_sub(1)?).ok()?;
                        names.get(index)
                    })
                    .filter(|name| !name.is_empty());
                // Keep the exact source name, but retain an explicit external
                // marker. A bare name is indistinguishable from a local
                // BrtName/LBL to formula consumers and was consequently
                // misclassified by the evaluator as an unresolved local name.
                stack.push(match resolved {
                    Some(name) => format!("[ixti:{ixti}]!{name}"),
                    None => format!("[ixti:{ixti}]!ExternalName{index}"),
                });
            }
            // PtgArea (class 0x25/0x45/0x65): a RgceArea (rowFirst/Last, colFirst/Last).
            0x25 | 0x45 | 0x65 => {
                let Some((r0, r1, c0, c1, n)) = read_area(rgce, i, context.biff12) else {
                    return joined(&stack);
                };
                stack.push(format!(
                    "{}:{}",
                    ref_a1(r0, c0, context, false),
                    ref_a1(r1, c1, context, false)
                ));
                i += n;
            }
            // PtgRef3d / PtgArea3d include an external-sheet index before the
            // same loc/area payload. The model has no sheet-qualified formula
            // surface yet, so expose the coordinate best-effort.
            0x1A | 0x3A | 0x5A | 0x7A => {
                let Some(ixti) = u16le(rgce, i) else {
                    return joined(&stack);
                };
                i += 2;
                let Some((row, col, n)) = read_loc(rgce, i, context.biff12) else {
                    return joined(&stack);
                };
                stack.push(format!(
                    "{}{}",
                    sheet_prefix(ixti, context),
                    ref_a1(row, col, context, context.name_formula)
                ));
                i += n;
            }
            0x1B | 0x3B | 0x5B | 0x7B => {
                let Some(ixti) = u16le(rgce, i) else {
                    return joined(&stack);
                };
                i += 2;
                let Some((r0, r1, c0, c1, n)) = read_area(rgce, i, context.biff12) else {
                    return joined(&stack);
                };
                stack.push(format!(
                    "{}{}:{}",
                    sheet_prefix(ixti, context),
                    ref_a1(r0, c0, context, context.name_formula),
                    ref_a1(r1, c1, context, context.name_formula)
                ));
                i += n;
            }
            0x3C | 0x5C | 0x7C => {
                let Some(ixti) = u16le(rgce, i) else {
                    return joined(&stack);
                };
                let bytes = if context.biff12 { 6 } else { 4 };
                if rgce.get(i + 2..i + 2 + bytes).is_none() {
                    return joined(&stack);
                }
                i += 2 + bytes;
                stack.push(format!("{}#REF!", sheet_prefix(ixti, context)));
            }
            0x3D | 0x5D | 0x7D => {
                let Some(ixti) = u16le(rgce, i) else {
                    return joined(&stack);
                };
                let bytes = if context.biff12 { 12 } else { 8 };
                if rgce.get(i + 2..i + 2 + bytes).is_none() {
                    return joined(&stack);
                }
                i += 2 + bytes;
                stack.push(format!("{}#REF!", sheet_prefix(ixti, context)));
            }
            // PtgFunc (0x21/0x41/0x61): iftab:u16, fixed args.
            0x21 | 0x41 | 0x61 => {
                let Some(id) = u16le(rgce, i) else {
                    return joined(&stack);
                };
                i += 2;
                let Some(function) = crate::ftab::function(id) else {
                    return joined(&stack);
                };
                let Some(arity) = function.fixed_arity else {
                    return joined(&stack);
                };
                if stack.len() < arity {
                    return joined(&stack);
                }
                let args = pop_args(&mut stack, arity);
                stack.push(format!("{}({})", function.name, args.join(",")));
            }
            // PtgFuncVar (0x22/0x42/0x62): cArgs:u8, iftab:u16.
            0x22 | 0x42 | 0x62 => {
                let Some(cargs) = rgce.get(i).copied().map(usize::from) else {
                    return joined(&stack);
                };
                let Some(raw_id) = u16le(rgce, i + 1) else {
                    return joined(&stack);
                };
                i += 3;
                if raw_id & 0x8000 != 0 {
                    if stack.len() < cargs {
                        return joined(&stack);
                    }
                    let args = pop_args(&mut stack, cargs);
                    stack.push(format!(
                        "_xlfn.CETAB{}({})",
                        raw_id & 0x7FFF,
                        args.join(",")
                    ));
                    continue;
                }
                let id = raw_id;
                let Some(function) = crate::ftab::function(id) else {
                    return joined(&stack);
                };
                if stack.len() < cargs {
                    return joined(&stack);
                }
                let args = pop_args(&mut stack, cargs);
                stack.push(format!("{}({})", function.name, args.join(",")));
            }
            _ => return joined(&stack), // unknown Ptg — best-effort prefix
        }
    }
    completed(&stack)
}

fn parse_array_constant(extra: &[u8], offset: &mut usize, biff12: bool) -> Option<String> {
    let (rows, cols, header) = if biff12 {
        (
            usize::try_from(u32le(extra, *offset)?).ok()?,
            usize::try_from(u32le(extra, offset.checked_add(4)?)?).ok()?,
            8usize,
        )
    } else {
        (
            usize::from(u16le(extra, offset.checked_add(1)?)?).checked_add(1)?,
            usize::from(*extra.get(*offset)?).checked_add(1)?,
            3usize,
        )
    };
    let count = rows.checked_mul(cols)?;
    if rows == 0 || cols == 0 || count > 65_536 {
        return None;
    }
    *offset = offset.checked_add(header)?;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(if biff12 {
            parse_ser_ar_biff12(extra, offset)?
        } else {
            parse_ser_ar_biff8(extra, offset)?
        });
    }
    let mut out = String::from("{");
    for row in 0..rows {
        if row != 0 {
            out.push(';');
        }
        for col in 0..cols {
            if col != 0 {
                out.push(',');
            }
            out.push_str(&values[row * cols + col]);
        }
    }
    out.push('}');
    Some(out)
}

fn parse_ser_ar_biff8(extra: &[u8], offset: &mut usize) -> Option<String> {
    let kind = *extra.get(*offset)?;
    match kind {
        0x00 => {
            extra.get(*offset..offset.checked_add(9)?)?;
            *offset += 9;
            Some(String::new())
        }
        0x01 => {
            let value = extra
                .get(offset.checked_add(1)?..offset.checked_add(9)?)
                .and_then(|bytes| bytes.try_into().ok())
                .map(f64::from_le_bytes)?;
            *offset += 9;
            Some(crate::format_number(value))
        }
        0x02 => {
            let cch = usize::from(u16le(extra, offset.checked_add(1)?)?);
            let flags_at = offset.checked_add(3)?;
            let high = extra.get(flags_at).copied()? & 1 != 0;
            let chars_at = flags_at.checked_add(1)?;
            let (text, bytes) = if high {
                let bytes = cch.checked_mul(2)?;
                let units = extra
                    .get(chars_at..chars_at.checked_add(bytes)?)?
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect::<Vec<_>>();
                (String::from_utf16_lossy(&units), bytes)
            } else {
                let bytes = extra.get(chars_at..chars_at.checked_add(cch)?)?;
                (bytes.iter().map(|byte| char::from(*byte)).collect(), cch)
            };
            *offset = chars_at.checked_add(bytes)?;
            Some(format!("\"{}\"", text.replace('"', "\"\"")))
        }
        0x04 => {
            let value = extra.get(offset.checked_add(1)?).copied()? != 0;
            extra.get(*offset..offset.checked_add(9)?)?;
            *offset += 9;
            Some(if value { "TRUE" } else { "FALSE" }.to_string())
        }
        0x10 => {
            let code = extra.get(offset.checked_add(1)?).copied()?;
            extra.get(*offset..offset.checked_add(9)?)?;
            *offset += 9;
            Some(crate::error_code(code).to_string())
        }
        _ => None,
    }
}

fn parse_ser_ar_biff12(extra: &[u8], offset: &mut usize) -> Option<String> {
    let kind = *extra.get(*offset)?;
    match kind {
        0x00 => {
            let value = extra
                .get(offset.checked_add(1)?..offset.checked_add(9)?)
                .and_then(|bytes| bytes.try_into().ok())
                .map(f64::from_le_bytes)?;
            *offset += 9;
            Some(crate::format_number(value))
        }
        0x01 => {
            let cch = usize::try_from(u32le(extra, offset.checked_add(1)?)?).ok()?;
            let chars_at = offset.checked_add(5)?;
            let bytes = cch.checked_mul(2)?;
            let units = extra
                .get(chars_at..chars_at.checked_add(bytes)?)?
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<_>>();
            *offset = chars_at.checked_add(bytes)?;
            let text = String::from_utf16_lossy(&units);
            Some(format!("\"{}\"", text.replace('"', "\"\"")))
        }
        0x02 => {
            let value = extra.get(offset.checked_add(1)?).copied()? != 0;
            extra.get(*offset..offset.checked_add(5)?)?;
            *offset += 5;
            Some(if value { "TRUE" } else { "FALSE" }.to_string())
        }
        0x04 => {
            let code = extra.get(offset.checked_add(1)?).copied()?;
            extra.get(*offset..offset.checked_add(5)?)?;
            *offset += 5;
            Some(crate::error_code(code).to_string())
        }
        _ => None,
    }
}

fn pop_args(stack: &mut Vec<String>, n: usize) -> Vec<String> {
    let mut args: Vec<String> = (0..n).filter_map(|_| stack.pop()).collect();
    args.reverse();
    args
}

fn joined(stack: &[String]) -> String {
    let recovered = stack.last().cloned().unwrap_or_default();
    if recovered.is_empty() {
        "_xlfn.RXLS_PARTIAL()".to_string()
    } else {
        format!("_xlfn.RXLS_PARTIAL({recovered})")
    }
}

fn completed(stack: &[String]) -> String {
    stack.last().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_bytes(value: &str) -> Vec<u8> {
        if value == "-" {
            return Vec::new();
        }
        assert_eq!(value.len() % 2, 0, "hex fixture must have byte pairs");
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(text, 16).unwrap()
            })
            .collect()
    }

    #[test]
    fn independent_formula_source_oracle_matches_all_dialects() {
        let fixtures = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/oracles/formula-source-fixtures.tsv"
        ));
        let mut checked = 0usize;
        for (line_no, line) in fixtures.lines().enumerate() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields.len(), 7, "invalid oracle line {}", line_no + 1);
            let mut context = match fields[0] {
                "biff5" => Context::biff5(),
                "biff8" => Context::new(false),
                "biff12" => Context::new(true),
                dialect => panic!("unknown oracle dialect {dialect}"),
            };
            context.base_row = fields[3].parse().unwrap();
            context.base_col = fields[4].parse().unwrap();
            let rgce = hex_bytes(fields[1]);
            let extra = hex_bytes(fields[2]);
            assert_eq!(
                decompile_parsed_with_context(&rgce, &extra, &context),
                fields[5],
                "oracle line {} ({})",
                line_no + 1,
                fields[6]
            );
            checked += 1;
        }
        assert_eq!(checked, 10);
    }

    #[test]
    fn ref_to_a1() {
        let absolute = Context::new(false);
        assert_eq!(ref_a1(0, 0, &absolute, false), "$A$1");
        assert_eq!(ref_a1(8, 1, &absolute, false), "$B$9");

        let relative = Context {
            base_row: 10,
            base_col: 5,
            ..Context::new(false)
        };
        assert_eq!(ref_a1(0, 0xC000, &relative, false), "A1");
        assert_eq!(ref_a1(0, 0x8000, &relative, false), "$A1");
        assert_eq!(ref_a1(0, 0x4000, &relative, false), "A$1");
        assert_eq!(ref_a1(0, 0xC000, &relative, true), "F11");
        assert_eq!(ref_a1(0, 0x8000, &relative, true), "$A11");
        assert_eq!(ref_a1(0, 0x4000, &relative, true), "F$1");
        assert_eq!(ref_a1(u32::from(u16::MAX), 0xFFFF, &relative, true), "E10");
    }

    #[test]
    fn decompiles_sum_area_times_two() {
        // SUM(A1:A9)*2 = PtgArea(A1:A9) PtgInt(2) PtgFuncVar... actually:
        // PtgArea, PtgFuncVar(1 arg, SUM=4), PtgInt(2), PtgMul.
        let mut rgce = vec![0x25]; // PtgArea
        rgce.extend_from_slice(&0u16.to_le_bytes()); // rowFirst 0
        rgce.extend_from_slice(&8u16.to_le_bytes()); // rowLast 8
        rgce.extend_from_slice(&0xC000u16.to_le_bytes()); // relative col/row
        rgce.extend_from_slice(&0xC000u16.to_le_bytes()); // relative col/row
        rgce.push(0x22); // PtgFuncVar
        rgce.push(1); // cArgs
        rgce.extend_from_slice(&4u16.to_le_bytes()); // SUM
        rgce.push(0x1E); // PtgInt
        rgce.extend_from_slice(&2u16.to_le_bytes());
        rgce.push(0x05); // PtgMul
        assert_eq!(decompile(&rgce, false), "SUM(A1:A9)*2");
    }

    #[test]
    fn decompiles_biff5_string_token_with_legacy_layout() {
        let rgce = [0x17, 3, b'o', b'l', b'd'];
        assert_eq!(decompile_with_context(&rgce, &Context::biff5()), "\"old\"");
    }

    #[test]
    fn decompiles_biff8_array_constant_from_rgb_extra() {
        let rgce = [0x20, 0, 0, 0, 0, 0, 0, 0];
        let mut extra = vec![1, 0, 0]; // two columns, one row
        extra.push(0x01);
        extra.extend_from_slice(&1.0f64.to_le_bytes());
        extra.push(0x02);
        extra.extend_from_slice(&1u16.to_le_bytes());
        extra.push(0); // compressed string
        extra.push(b'x');
        assert_eq!(
            decompile_parsed_with_context(&rgce, &extra, &Context::new(false)),
            "{1,\"x\"}"
        );
    }

    #[test]
    fn decompiles_biff12_array_constant_from_rgb_extra() {
        let rgce = [0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut extra = 1u32.to_le_bytes().to_vec();
        extra.extend_from_slice(&2u32.to_le_bytes());
        extra.push(0x00);
        extra.extend_from_slice(&2.5f64.to_le_bytes());
        extra.extend_from_slice(&[0x02, 1, 0, 0, 0]);
        assert_eq!(
            decompile_parsed_with_context(&rgce, &extra, &Context::new(true)),
            "{2.5,TRUE}"
        );
    }

    #[test]
    fn unknown_token_is_explicit_and_diagnostic() {
        let rgce = [0x1E, 0x05, 0x00, 0xFF]; // PtgInt(5), then a bogus token
        let result = decompile_detailed(&rgce, &[], &Context::new(false));
        assert_eq!(result.formula, "_xlfn.RXLS_PARTIAL(5)");
        assert_eq!(
            result.diagnostics,
            vec![FormulaDiagnostic {
                kind: FormulaDiagnosticKind::IncompleteTokenStream
            }]
        );
    }

    #[test]
    fn decompiles_official_abs_true_false_now_function_ids() {
        assert_eq!(decompile(&[0x1E, 5, 0, 0x21, 0x18, 0], false), "ABS(5)");
        assert_eq!(decompile(&[0x21, 0x22, 0], false), "TRUE()");
        assert_eq!(decompile(&[0x21, 0x23, 0], false), "FALSE()");
        assert_eq!(decompile(&[0x21, 0x4A, 0], false), "NOW()");
    }

    #[test]
    fn namex_resolves_the_original_supbook_name() {
        let extern_sheets = vec![ExternSheet {
            supbook_index: 1,
            first_sheet: 0,
            last_sheet: 0,
        }];
        let external_names = vec![vec![], vec!["ExternalRate".to_string(), String::new()]];
        let context = Context {
            extern_sheets: &extern_sheets,
            external_names: &external_names,
            ..Context::new(false)
        };
        let namex = [0x39, 0, 0, 1, 0, 0, 0];
        assert_eq!(
            decompile_with_context(&namex, &context),
            "[ixti:0]!ExternalRate"
        );

        let empty = [0x39, 0, 0, 2, 0, 0, 0];
        assert_eq!(
            decompile_with_context(&empty, &context),
            "[ixti:0]!ExternalName2"
        );

        let missing = [0x39, 0, 0, 3, 0, 0, 0];
        assert_eq!(
            decompile_with_context(&missing, &context),
            "[ixti:0]!ExternalName3"
        );
    }

    #[test]
    fn decompiles_3d_refs_with_sheet_names() {
        let sheet_names = vec!["Data Sheet".to_string()];
        let extern_sheets = vec![ExternSheet {
            supbook_index: 0,
            first_sheet: 0,
            last_sheet: 0,
        }];
        let context = Context {
            sheet_names: &sheet_names,
            extern_sheets: &extern_sheets,
            ..Context::new(false)
        };
        let ref3d = [0x3A, 0, 0, 3, 0, 4, 0]; // ixti 0, E4
        assert_eq!(
            decompile_with_context(&ref3d, &context),
            "'Data Sheet'!$E$4"
        );

        let mut area3d = vec![0x3B, 0, 0]; // ixti 0
        area3d.extend_from_slice(&0u16.to_le_bytes()); // rowFirst
        area3d.extend_from_slice(&2u16.to_le_bytes()); // rowLast
        area3d.extend_from_slice(&0u16.to_le_bytes()); // colFirst
        area3d.extend_from_slice(&1u16.to_le_bytes()); // colLast
        assert_eq!(
            decompile_with_context(&area3d, &context),
            "'Data Sheet'!$A$1:$B$3"
        );
    }

    #[test]
    fn decompiles_biff12_3d_refs_with_sheet_names() {
        let sheet_names = vec!["Input".to_string(), "Output".to_string()];
        let extern_sheets = vec![ExternSheet {
            supbook_index: 0,
            first_sheet: 0,
            last_sheet: 1,
        }];
        let context = Context {
            sheet_names: &sheet_names,
            extern_sheets: &extern_sheets,
            ..Context::new(true)
        };
        let mut ref3d = vec![0x3A, 0, 0]; // ixti 0
        ref3d.extend_from_slice(&3u32.to_le_bytes()); // row
        ref3d.extend_from_slice(&4u16.to_le_bytes()); // col
        assert_eq!(
            decompile_with_context(&ref3d, &context),
            "Input:Output!$E$4"
        );

        let mut area3d = vec![0x3B, 0, 0]; // ixti 0
        area3d.extend_from_slice(&0u32.to_le_bytes()); // rowFirst
        area3d.extend_from_slice(&2u32.to_le_bytes()); // rowLast
        area3d.extend_from_slice(&0u16.to_le_bytes()); // colFirst
        area3d.extend_from_slice(&1u16.to_le_bytes()); // colLast
        assert_eq!(
            decompile_with_context(&area3d, &context),
            "Input:Output!$A$1:$B$3"
        );
    }
}
