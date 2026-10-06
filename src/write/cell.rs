//! Per-cell XML emission and the shared-string table (`sharedStrings.xml`).

use std::collections::HashMap;

use crate::write::styles::script_str;
use crate::write::xml::{a1, esc_attr, esc_text, hex, num_str, NS_MAIN, XML_DECL};
use crate::{Cell, TextRun};

pub(super) struct CellWriteContext<'a> {
    pub(super) sst: &'a mut Vec<String>,
    pub(super) sst_idx: &'a mut HashMap<String, usize>,
    pub(super) sst_count: &'a mut usize,
    pub(super) budget: &'a mut usize,
}

pub(super) fn write_cell(
    out: &mut String,
    row: u32,
    col: u16,
    cell: &Cell,
    xf: u32,
    ctx: &mut CellWriteContext<'_>,
) -> bool {
    let ref_ = a1(row, col);
    // The style index (`s=`); xf 0 is General and is omitted. A `Cell::Date` with
    // no explicit style still gets a date-formatted xf (numFmt 14) from interning.
    let s = if xf != 0 {
        format!(r#" s="{xf}""#)
    } else {
        String::new()
    };
    let mut shared_cost = 0usize;
    let xml = match cell {
        Cell::Text(t) => {
            let idx = match ctx.sst_idx.get(t) {
                Some(&idx) => idx,
                None => {
                    shared_cost = shared_string_entry_len(t);
                    ctx.sst.len()
                }
            };
            format!(r#"<c r="{ref_}"{s} t="s"><v>{idx}</v></c>"#)
        }
        Cell::Number(n) | Cell::Date(n) => {
            format!(r#"<c r="{ref_}"{s}><v>{}</v></c>"#, num_str(*n))
        }
        Cell::Bool(b) => format!(
            r#"<c r="{ref_}"{s} t="b"><v>{}</v></c>"#,
            if *b { 1 } else { 0 }
        ),
        Cell::Error(e) => format!(r#"<c r="{ref_}"{s} t="e"><v>{}</v></c>"#, esc_text(e)),
        Cell::Formula { formula, cached } => {
            let (cache_type, cache_len) = match cached.as_ref() {
                Cell::Text(text) => (r#" t="str""#, crate::xstring::escaped_xml_len(text)),
                Cell::Bool(_) => (r#" t="b""#, 1),
                Cell::Error(error) => (r#" t="e""#, escaped_text_len(error)),
                Cell::Number(number) | Cell::Date(number) => ("", num_str(*number).len()),
                Cell::Formula { .. } => ("", 1),
            };
            let v_space = if matches!(cached.as_ref(), Cell::Text(_)) {
                r#" xml:space="preserve""#
            } else {
                ""
            };
            let prefix = format!(r#"<c r="{ref_}"{s}{cache_type}><f>"#);
            let cost = prefix
                .len()
                .saturating_add(escaped_text_len(formula))
                .saturating_add("</f><v".len())
                .saturating_add(v_space.len())
                .saturating_add(1)
                .saturating_add(cache_len)
                .saturating_add("</v></c>".len());
            if cost > *ctx.budget {
                *ctx.budget = 0;
                return false;
            }
            // <f> carries the formula; the cached value determines t= and <v>.
            let (t_attr, v) = match cached.as_ref() {
                Cell::Text(t) => (r#" t="str""#, crate::xstring::escape_xml(t)),
                Cell::Bool(b) => (r#" t="b""#, if *b { "1" } else { "0" }.to_string()),
                Cell::Error(e) => (r#" t="e""#, esc_text(e)),
                Cell::Number(n) | Cell::Date(n) => ("", num_str(*n)),
                Cell::Formula { .. } => ("", "0".to_string()),
            };
            format!(
                r#"<c r="{ref_}"{s}{t_attr}><f>{}</f><v{v_space}>{v}</v></c>"#,
                esc_text(formula)
            )
        }
    };
    if !consume_budget(ctx.budget, xml.len().saturating_add(shared_cost)) {
        return false;
    }
    if let Cell::Text(t) = cell {
        if !ctx.sst_idx.contains_key(t) {
            intern(ctx.sst, ctx.sst_idx, t);
        }
        *ctx.sst_count = ctx.sst_count.saturating_add(1);
    }
    out.push_str(&xml);
    true
}

fn consume_budget(budget: &mut usize, cost: usize) -> bool {
    if cost > *budget {
        *budget = 0;
        return false;
    }
    *budget -= cost;
    true
}

fn shared_string_entry_len(s: &str) -> usize {
    r#"<si><t xml:space="preserve">"#
        .len()
        .saturating_add(crate::xstring::escaped_xml_len(s))
        .saturating_add("</t></si>".len())
}

// Generic XML escaping remains appropriate for formula source and error text.
fn escaped_text_len(text: &str) -> usize {
    text.chars().fold(0_usize, |length, character| {
        length.saturating_add(match character {
            '&' => 5,
            '<' | '>' => 4,
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => 0,
            '\u{FFFE}' | '\u{FFFF}' => 0,
            c => c.len_utf8(),
        })
    })
}

/// Emit a rich (mixed-format) cell as an inline string: `<is>` with one `<r>` per
/// run, each run's `<rPr>` in CT_RPrElt element order (rFont, b, i, strike,
/// color, sz, u).
///
/// Bounded by `max_bytes` (the worksheet's remaining output budget): unlike a plain
/// cell — whose long text goes to the shared-string table — a rich cell inlines every
/// run into the worksheet XML, so a flood of runs is capped here, per run, rather than
/// only being noticed by the caller after the whole cell was already serialized.
pub(super) fn write_rich_cell(
    out: &mut String,
    row: u32,
    col: u16,
    runs: &[TextRun],
    xf: u32,
    max_bytes: usize,
) -> bool {
    let ref_ = a1(row, col);
    let s = if xf != 0 {
        format!(r#" s="{xf}""#)
    } else {
        String::new()
    };
    let prefix = format!(r#"<c r="{ref_}"{s} t="inlineStr"><is>"#);
    let suffix = "</is></c>";
    if prefix.len().saturating_add(suffix.len()) > max_bytes {
        return false;
    }
    let start = out.len();
    out.push_str(&prefix);
    for run in runs {
        let mut rx = String::new();
        if out.len().saturating_sub(start) >= max_bytes {
            break; // output budget reached — stop inlining further runs
        }
        rx.push_str("<r>");
        let f = &run.font;
        if f.name.is_some()
            || f.size_pt.is_some()
            || f.color.is_some()
            || f.bold
            || f.italic
            || f.underline
            || f.strikethrough
            || script_str(f.script).is_some()
        {
            rx.push_str("<rPr>");
            if let Some(name) = &f.name {
                rx.push_str(&format!(r#"<rFont val="{}"/>"#, esc_attr(name)));
            }
            if f.bold {
                rx.push_str("<b/>");
            }
            if f.italic {
                rx.push_str("<i/>");
            }
            if f.strikethrough {
                rx.push_str("<strike/>");
            }
            if let Some(c) = f.color {
                rx.push_str(&format!(r#"<color rgb="{}"/>"#, hex(c)));
            }
            if let Some(sz) = f.size_pt {
                rx.push_str(&format!(r#"<sz val="{sz}"/>"#));
            }
            if f.underline {
                rx.push_str("<u/>");
            }
            if let Some(script) = script_str(f.script) {
                rx.push_str(&format!(r#"<vertAlign val="{script}"/>"#));
            }
            rx.push_str("</rPr>");
        }
        let used = out.len().saturating_sub(start);
        let text_cost = rx
            .len()
            .saturating_add(r#"<t xml:space="preserve">"#.len())
            .saturating_add(crate::xstring::escaped_xml_len(&run.text))
            .saturating_add("</t></r>".len())
            .saturating_add(suffix.len());
        if used.saturating_add(text_cost) > max_bytes {
            if used == prefix.len() {
                out.truncate(start);
                return false;
            }
            break;
        }
        rx.push_str(&format!(
            r#"<t xml:space="preserve">{}</t>"#,
            crate::xstring::escape_xml(&run.text)
        ));
        rx.push_str("</r>");
        let used = out.len().saturating_sub(start);
        let cost = rx.len().saturating_add(suffix.len());
        if used.saturating_add(cost) > max_bytes {
            if used == prefix.len() {
                out.truncate(start);
                return false;
            }
            break;
        }
        out.push_str(&rx);
    }
    out.push_str(suffix);
    true
}

pub(super) fn intern(sst: &mut Vec<String>, idx: &mut HashMap<String, usize>, s: &str) -> usize {
    if let Some(&i) = idx.get(s) {
        return i;
    }
    let i = sst.len();
    sst.push(s.to_string());
    idx.insert(s.to_string(), i);
    i
}

pub(super) fn shared_strings_xml(sst: &[String], total_count: usize) -> String {
    let mut s = String::new();
    s.push_str(XML_DECL);
    s.push_str(&format!(
        r#"<sst xmlns="{NS_MAIN}" count="{}" uniqueCount="{}">"#,
        total_count,
        sst.len()
    ));
    for v in sst {
        s.push_str(r#"<si><t xml:space="preserve">"#);
        s.push_str(&crate::xstring::escape_xml(v));
        s.push_str("</t></si>");
    }
    s.push_str("</sst>");
    s
}

#[cfg(test)]
mod tests {
    use super::{write_cell, write_rich_cell, CellWriteContext};
    use crate::{Cell, Font, TextRun};
    use std::collections::HashMap;

    #[test]
    fn escaped_shared_and_cached_text_do_not_exceed_or_mutate_a_small_budget() {
        for value in [
            Cell::Text("\u{1}".repeat(100)),
            Cell::Formula {
                formula: "\"text\"".into(),
                cached: Box::new(Cell::Text("\u{1}".repeat(100))),
            },
        ] {
            let mut out = "prior output".to_owned();
            let mut sst = Vec::new();
            let mut sst_idx = HashMap::new();
            let mut sst_count = 0;
            let mut budget = 200;
            let mut context = CellWriteContext {
                sst: &mut sst,
                sst_idx: &mut sst_idx,
                sst_count: &mut sst_count,
                budget: &mut budget,
            };
            assert!(!write_cell(&mut out, 0, 0, &value, 0, &mut context));
            assert_eq!(out, "prior output");
            assert!(sst.is_empty());
            assert!(sst_idx.is_empty());
            assert_eq!(sst_count, 0);
            assert_eq!(budget, 0);
        }
    }

    #[test]
    fn escaped_rich_text_rolls_back_an_over_budget_first_run() {
        let mut out = "prior output".to_owned();
        let runs = [TextRun::new("\u{1}".repeat(100), Font::default())];
        assert!(!write_rich_cell(&mut out, 0, 0, &runs, 0, 200));
        assert_eq!(out, "prior output");
    }

    #[test]
    fn encoded_cache_accepts_an_exact_budget_and_rejects_one_byte_less() {
        let expected = r#"<c r="A1" t="str"><f>"text"</f><v xml:space="preserve">_x005F_x0041__x000D_</v></c>"#;
        for short in [false, true] {
            let mut out = String::new();
            let mut sst = Vec::new();
            let mut sst_idx = HashMap::new();
            let mut sst_count = 0;
            let mut budget = expected.len() - usize::from(short);
            let mut context = CellWriteContext {
                sst: &mut sst,
                sst_idx: &mut sst_idx,
                sst_count: &mut sst_count,
                budget: &mut budget,
            };
            let value = Cell::Formula {
                formula: "\"text\"".into(),
                cached: Box::new(Cell::Text("_x0041_\r".into())),
            };
            assert_eq!(write_cell(&mut out, 0, 0, &value, 0, &mut context), !short);
            assert_eq!(out, if short { "" } else { expected });
            assert_eq!(budget, 0);
        }
    }
}
