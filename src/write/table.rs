//! Table parts (`xl/tables/*`): the `<table>` definition and display-name
//! sanitization.

use crate::write::xml::{a1, esc_attr, NS_MAIN, XML_DECL};
use crate::write::{MAX_COL, MAX_ROW};
use crate::Table;

/// Sanitize a table name to a valid Excel defined name (letters/digits/`_`,
/// starting with a letter or `_`, not a cell reference); falls back to
/// `Table{n}`.
pub(super) fn table_name(raw: &str, n: usize) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && !looks_like_cell_ref(&s)
        && !looks_like_r1c1_ref(&s)
    {
        s
    } else {
        format!("Table{n}")
    }
}

/// Does `name` denote a real A1 cell address: 1-3 letters forming a column at
/// or before XFD followed by a row in the worksheet grid?
pub(super) fn looks_like_cell_ref(name: &str) -> bool {
    let bytes = name.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == 0 || i > 3 || i == bytes.len() || !bytes[i..].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let col = bytes[..i].iter().fold(0u32, |acc, &b| {
        acc * 26 + u32::from(b.to_ascii_uppercase() - b'A') + 1
    });
    let row: u64 = name[i..].parse().unwrap_or(u64::MAX);
    (1..=u32::from(MAX_COL) + 1).contains(&col) && (1..=u64::from(MAX_ROW) + 1).contains(&row)
}

pub(super) fn looks_like_r1c1_ref(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 4 || !bytes[0].eq_ignore_ascii_case(&b'R') {
        return false;
    }

    let mut i = 1;
    let row_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == row_start || i >= bytes.len() || !bytes[i].eq_ignore_ascii_case(&b'C') {
        return false;
    }
    i += 1;
    let col_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == col_start || i != bytes.len() {
        return false;
    }

    let row: u64 = name[row_start..col_start - 1].parse().unwrap_or(u64::MAX);
    let col: u64 = name[col_start..].parse().unwrap_or(u64::MAX);
    (1..=u64::from(MAX_ROW) + 1).contains(&row) && (1..=u64::from(MAX_COL) + 1).contains(&col)
}

pub(super) fn table_xml(t: &Table, n: usize, name: &str) -> String {
    let (r0, c0, r1, c1) = t.range;
    let ref_ = format!(
        "{}:{}",
        a1(r0.min(MAX_ROW), c0.min(MAX_COL)),
        a1(r1.min(MAX_ROW), c1.min(MAX_COL))
    );
    let style = esc_attr(t.style.as_deref().unwrap_or("TableStyleMedium2"));
    let mut cols = String::new();
    for (i, c) in t.columns.iter().enumerate() {
        cols.push_str(&format!(
            r#"<tableColumn id="{}" name="{}"/>"#,
            i + 1,
            esc_attr(c)
        ));
    }
    format!(
        r#"{XML_DECL}<table xmlns="{NS_MAIN}" id="{n}" name="{name}" displayName="{name}" ref="{ref_}" totalsRowShown="0"><autoFilter ref="{ref_}"/><tableColumns count="{}">{cols}</tableColumns><tableStyleInfo name="{style}" showFirstColumn="0" showLastColumn="0" showRowStripes="1" showColumnStripes="0"/></table>"#,
        t.columns.len()
    )
}

#[cfg(test)]
mod tests {
    use super::{table_name, table_xml};
    use crate::Table;

    #[test]
    fn table_name_rewrites_cell_reference_names() {
        assert_eq!(table_name("A1", 7), "Table7");
        assert_eq!(table_name("Q4", 8), "Table8");
        assert_eq!(table_name("R1C1", 9), "Table9");
        assert_eq!(table_name("Table1", 10), "Table1");
        assert_eq!(table_name("Sales_2024", 11), "Sales_2024");
    }

    #[test]
    fn table_xml_escapes_table_style_attribute() {
        let table = Table {
            range: (0, 0, 0, 0),
            name: "Table1".into(),
            columns: vec!["header".into()],
            style: Some("Bad\"&Style".into()),
        };

        let xml = table_xml(&table, 1, "Table1");

        assert!(
            xml.contains(r#"<tableStyleInfo name="Bad&quot;&amp;Style""#),
            "table style attribute must be XML-escaped: {xml}"
        );
    }
}
