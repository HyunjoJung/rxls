//! Package-level parts: `[Content_Types].xml`, `_rels/.rels`, `docProps/*`,
//! `xl/workbook.xml`, and `xl/_rels/workbook.xml.rels`.

use crate::write::drawing::Drawings;
use crate::write::xml::{
    abs_col, abs_ref, esc_attr, esc_text, CT_CORE_PROPS, CT_EXT_PROPS, CT_RELS, CT_SST, CT_STYLES,
    CT_VML, CT_WORKBOOK, CT_WORKSHEET, NS_CT, NS_MAIN, NS_PKG_REL, NS_R, REL_CORE_PROPS,
    REL_EXT_PROPS, REL_OFFICE_DOCUMENT, REL_SST, REL_STYLES, REL_WORKSHEET, XML_DECL,
};
use crate::write::{MAX_COL, MAX_ROW, MAX_SHEETS};
use crate::Workbook;

fn consume_budget(budget: &mut usize, cost: usize) -> bool {
    if cost > *budget {
        *budget = 0;
        return false;
    }
    *budget -= cost;
    true
}

fn push_budgeted(out: &mut String, budget: &mut usize, xml: String) -> bool {
    if consume_budget(budget, xml.len()) {
        out.push_str(&xml);
        true
    } else {
        false
    }
}

fn push_defined_name(out: &mut String, budget: &mut usize, defs_open: &mut bool, xml: String) {
    let wrapper_cost = if *defs_open {
        0
    } else {
        "<definedNames></definedNames>".len()
    };
    if !consume_budget(budget, xml.len().saturating_add(wrapper_cost)) {
        return;
    }
    if !*defs_open {
        out.push_str("<definedNames>");
        *defs_open = true;
    }
    out.push_str(&xml);
}

/// Sanitize + de-duplicate sheet names to Excel's rules: ≤31 chars, none of
/// `: \ / ? * [ ]`, non-blank, unique (case-insensitive).
pub(super) fn sanitize_sheet_names(wb: &Workbook) -> Vec<String> {
    let sheet_count = wb.sheets.len().min(MAX_SHEETS);
    let mut out: Vec<String> = Vec::with_capacity(sheet_count);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, sheet) in wb.sheets.iter().take(sheet_count).enumerate() {
        let cleaned: String = sheet
            .name
            .chars()
            .map(|c| {
                if matches!(c, ':' | '\\' | '/' | '?' | '*' | '[' | ']') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let cleaned = cleaned.trim();
        let mut base: String = if cleaned.is_empty() {
            format!("Sheet{}", i + 1)
        } else {
            cleaned.chars().take(31).collect()
        };
        // De-dupe case-insensitively, keeping within 31 chars. Always retry
        // from the original base so a suffix that already collides cannot
        // rewrite to itself forever.
        if seen.contains(&base.to_lowercase()) {
            let original = base.clone();
            let mut retry = 0usize;
            loop {
                let suffix = if retry == 0 {
                    format!("_{}", i + 1)
                } else {
                    format!("_{}_{}", i + 1, retry)
                };
                let keep = 31usize.saturating_sub(suffix.chars().count());
                base = format!(
                    "{}{}",
                    original.chars().take(keep).collect::<String>(),
                    suffix
                );
                if !seen.contains(&base.to_lowercase()) {
                    break;
                }
                retry += 1;
            }
        }
        seen.insert(base.to_lowercase());
        out.push(base);
    }
    out
}

pub(super) fn content_types(n_sheets: usize, drawings: &Drawings) -> String {
    let mut s = String::new();
    s.push_str(XML_DECL);
    s.push_str(&format!(r#"<Types xmlns="{NS_CT}">"#));
    s.push_str(&format!(
        r#"<Default Extension="rels" ContentType="{CT_RELS}"/><Default Extension="xml" ContentType="application/xml"/>"#
    ));
    if drawings.need_png {
        s.push_str(r#"<Default Extension="png" ContentType="image/png"/>"#);
    }
    if drawings.need_jpeg {
        s.push_str(r#"<Default Extension="jpeg" ContentType="image/jpeg"/>"#);
    }
    if drawings.need_vml {
        s.push_str(&format!(
            r#"<Default Extension="vml" ContentType="{CT_VML}"/>"#
        ));
    }
    s.push_str(&format!(
        r#"<Override PartName="/xl/workbook.xml" ContentType="{CT_WORKBOOK}"/>"#
    ));
    for i in 0..n_sheets {
        s.push_str(&format!(
            r#"<Override PartName="/xl/worksheets/sheet{}.xml" ContentType="{CT_WORKSHEET}"/>"#,
            i + 1
        ));
    }
    s.push_str(&format!(
        r#"<Override PartName="/xl/styles.xml" ContentType="{CT_STYLES}"/>"#
    ));
    s.push_str(&format!(
        r#"<Override PartName="/xl/sharedStrings.xml" ContentType="{CT_SST}"/>"#
    ));
    s.push_str(&format!(
        r#"<Override PartName="/docProps/core.xml" ContentType="{CT_CORE_PROPS}"/><Override PartName="/docProps/app.xml" ContentType="{CT_EXT_PROPS}"/>"#
    ));
    for (part, ct) in &drawings.ct_overrides {
        s.push_str(&format!(
            r#"<Override PartName="{part}" ContentType="{ct}"/>"#
        ));
    }
    s.push_str("</Types>");
    s
}

pub(super) fn root_rels() -> String {
    format!(
        r#"{XML_DECL}<Relationships xmlns="{NS_PKG_REL}"><Relationship Id="rId1" Type="{REL_OFFICE_DOCUMENT}" Target="xl/workbook.xml"/><Relationship Id="rId2" Type="{REL_CORE_PROPS}" Target="docProps/core.xml"/><Relationship Id="rId3" Type="{REL_EXT_PROPS}" Target="docProps/app.xml"/></Relationships>"#
    )
}

/// `docProps/core.xml` — Dublin Core properties; only set fields are emitted.
pub(crate) fn core_xml_with_budget(p: &crate::DocProperties, budget: &mut usize) -> String {
    let mut s = String::new();
    s.push_str(XML_DECL);
    s.push_str(
        r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">"#,
    );
    let el = |s: &mut String, budget: &mut usize, tag: &str, v: &Option<String>| {
        if let Some(v) = v {
            push_budgeted(s, budget, format!("<{tag}>{}</{tag}>", esc_text(v)));
        }
    };
    el(&mut s, budget, "dc:title", &p.title);
    el(&mut s, budget, "dc:subject", &p.subject);
    el(&mut s, budget, "dc:creator", &p.creator);
    el(&mut s, budget, "cp:keywords", &p.keywords);
    el(&mut s, budget, "dc:description", &p.description);
    el(&mut s, budget, "cp:lastModifiedBy", &p.last_modified_by);
    // Only emit a timestamp that is shaped like W3CDTF; a malformed string would
    // make core.xml schema-invalid (the dcterms:W3CDTF xsi:type).
    if let Some(ts) = p.created.as_deref().filter(|t| is_w3cdtf(t)) {
        push_budgeted(
            &mut s,
            budget,
            format!(
                r#"<dcterms:created xsi:type="dcterms:W3CDTF">{0}</dcterms:created><dcterms:modified xsi:type="dcterms:W3CDTF">{0}</dcterms:modified>"#,
                esc_text(ts)
            ),
        );
    }
    s.push_str("</cp:coreProperties>");
    s
}

/// Validate a `YYYY-MM-DDThh:mm:ss` W3CDTF prefix: digit positions, separators,
/// and field ranges (month 1–12, day 1–31, hour ≤23, minute ≤59, second ≤60), so
/// a malformed value cannot be emitted with `xsi:type="dcterms:W3CDTF"` and make
/// core.xml schema-invalid. Any trailing fractional seconds / zone are tolerated.
pub(crate) fn is_w3cdtf(s: &str) -> bool {
    let b = s.as_bytes();
    if s.len() < 19 {
        return false;
    }
    let digit = |i: usize| b[i].is_ascii_digit();
    let seps = b[4] == b'-' && b[7] == b'-' && b[10] == b'T' && b[13] == b':' && b[16] == b':';
    let digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
        .iter()
        .all(|&i| digit(i));
    if !seps || !digits {
        return false;
    }
    // Bytes 0..19 are now ASCII, so the date sub-slice is on a char boundary.
    // Validate the calendar date for real (days-in-month, leap years) via the shared
    // helper so impossible dates like 2024-02-31 are rejected.
    if crate::format::iso_date_to_serial(&s[..10]).is_none() {
        return false;
    }
    let two = |i: usize| (b[i] - b'0') * 10 + (b[i + 1] - b'0');
    let (h, mi, se) = (two(11), two(14), two(17));
    h <= 23 && mi <= 59 && se <= 60
}

/// `docProps/app.xml` — extended properties (application name + optional company).
pub(crate) fn app_xml_with_budget(p: &crate::DocProperties, budget: &mut usize) -> String {
    let mut s = String::new();
    s.push_str(XML_DECL);
    s.push_str(
        r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>rxls</Application>"#,
    );
    if let Some(c) = &p.company {
        push_budgeted(
            &mut s,
            budget,
            format!("<Company>{}</Company>", esc_text(c)),
        );
    }
    s.push_str("</Properties>");
    s
}

pub(super) fn workbook_xml_with_budget(wb: &Workbook, budget: &mut usize) -> String {
    let names = sanitize_sheet_names(wb);
    let sheet_count = names.len();
    let mut s = String::new();
    s.push_str(XML_DECL);
    s.push_str(&format!(
        r#"<workbook xmlns="{NS_MAIN}" xmlns:r="{NS_R}"><workbookPr date1904="{}"/>"#,
        wb.date1904
    ));
    // <workbookProtection> (CT_Workbook order: after workbookPr, before bookViews)
    // — locks the sheet structure so sheets can't be added/removed/reordered.
    if wb.protect_structure {
        push_budgeted(
            &mut s,
            budget,
            r#"<workbookProtection lockStructure="1"/>"#.to_string(),
        );
    }
    // <bookViews> (CT_Workbook order: after workbookPr, before sheets) — selects
    // the active tab. Only emitted when the index points at a real sheet.
    if wb.active_sheet < sheet_count {
        push_budgeted(
            &mut s,
            budget,
            format!(
                r#"<bookViews><workbookView activeTab="{}"/></bookViews>"#,
                wb.active_sheet
            ),
        );
    }
    s.push_str("<sheets>");
    // A workbook must keep at least one visible sheet, or Excel rejects it; if every
    // sheet is hidden/very-hidden, leave the first one visible.
    let all_hidden = sheet_count != 0
        && wb
            .sheets
            .iter()
            .take(sheet_count)
            .all(|sh| sh.hidden || sh.is_very_hidden());
    for (i, name) in names.iter().enumerate() {
        let sh = wb.sheets.get(i);
        let force_visible = all_hidden && i == 0;
        let state = if force_visible {
            ""
        } else if sh.is_some_and(|s| s.is_very_hidden()) {
            r#" state="veryHidden""#
        } else if sh.is_some_and(|s| s.hidden) {
            r#" state="hidden""#
        } else {
            ""
        };
        s.push_str(&format!(
            r#"<sheet name="{}" sheetId="{}"{state} r:id="rId{}"/>"#,
            esc_attr(name),
            i + 1,
            i + 1
        ));
    }
    s.push_str("</sheets>");
    // Sheet-scoped hidden _FilterDatabase defined name per autofilter (Excel).
    let mut defs_open = false;
    for (i, sheet) in wb.sheets.iter().take(sheet_count).enumerate() {
        let q = names[i].replace('\'', "''");
        if let Some((r0, c0, r1, c1)) = sheet.autofilter {
            push_defined_name(
                &mut s,
                budget,
                &mut defs_open,
                format!(
                    r#"<definedName name="_xlnm._FilterDatabase" localSheetId="{i}" hidden="1">'{q}'!{}:{}</definedName>"#,
                    abs_ref(r0.min(MAX_ROW), c0.min(MAX_COL)),
                    abs_ref(r1.min(MAX_ROW), c1.min(MAX_COL))
                ),
            );
        }
        if let Some(ps) = &sheet.page_setup {
            if let Some((r0, c0, r1, c1)) = ps.print_area {
                push_defined_name(
                    &mut s,
                    budget,
                    &mut defs_open,
                    format!(
                        r#"<definedName name="_xlnm.Print_Area" localSheetId="{i}">'{q}'!{}:{}</definedName>"#,
                        abs_ref(r0.min(MAX_ROW), c0.min(MAX_COL)),
                        abs_ref(r1.min(MAX_ROW), c1.min(MAX_COL))
                    ),
                );
            }
            let mut print_titles = Vec::new();
            if let Some((rf, rl)) = ps.repeat_rows {
                print_titles.push(format!(
                    "'{q}'!${}:${}",
                    rf.min(MAX_ROW) + 1,
                    rl.min(MAX_ROW) + 1
                ));
            }
            if let Some((cf, cl)) = ps.repeat_cols {
                print_titles.push(format!(
                    "'{q}'!{}:{}",
                    abs_col(cf.min(MAX_COL)),
                    abs_col(cl.min(MAX_COL))
                ));
            }
            if !print_titles.is_empty() {
                push_defined_name(
                    &mut s,
                    budget,
                    &mut defs_open,
                    format!(
                        r#"<definedName name="_xlnm.Print_Titles" localSheetId="{i}">{}</definedName>"#,
                        print_titles.join(",")
                    ),
                );
            }
        }
    }
    for (name, refers_to) in &wb.defined_names {
        push_defined_name(
            &mut s,
            budget,
            &mut defs_open,
            format!(
                r#"<definedName name="{}">{}</definedName>"#,
                esc_attr(name),
                esc_text(refers_to)
            ),
        );
    }
    if defs_open {
        s.push_str("</definedNames>");
    }
    s.push_str("</workbook>");
    s
}

pub(super) fn workbook_rels(n_sheets: usize) -> String {
    let mut s = String::new();
    s.push_str(XML_DECL);
    s.push_str(&format!(r#"<Relationships xmlns="{NS_PKG_REL}">"#));
    for i in 0..n_sheets {
        s.push_str(&format!(
            r#"<Relationship Id="rId{}" Type="{REL_WORKSHEET}" Target="worksheets/sheet{}.xml"/>"#,
            i + 1,
            i + 1
        ));
    }
    // Styles + shared strings get ids after the sheets.
    s.push_str(&format!(
        r#"<Relationship Id="rId{}" Type="{REL_STYLES}" Target="styles.xml"/>"#,
        n_sheets + 1
    ));
    s.push_str(&format!(
        r#"<Relationship Id="rId{}" Type="{REL_SST}" Target="sharedStrings.xml"/>"#,
        n_sheets + 2
    ));
    s.push_str("</Relationships>");
    s
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn invalid_created_timestamp_is_skipped() {
        for bad_ts in ["2024-99-99T99:99:99Z", "2024-02-31T00:00:00Z", "not-a-date"] {
            let bad = crate::DocProperties {
                created: Some(bad_ts.into()),
                ..Default::default()
            };
            let mut budget = usize::MAX;
            assert!(
                !super::core_xml_with_budget(&bad, &mut budget).contains("dcterms:created"),
                "invalid timestamp {bad_ts} must not be emitted"
            );
        }
        let good = crate::DocProperties {
            created: Some("2024-03-15T10:00:00Z".into()),
            ..Default::default()
        };
        let mut budget = usize::MAX;
        assert!(super::core_xml_with_budget(&good, &mut budget).contains("dcterms:created"));
    }

    #[test]
    fn sanitize_sheet_names_terminates_when_suffix_candidate_collides() {
        let colliding_name = format!("{}_2", "A".repeat(29));
        let mut wb = crate::Workbook::new();
        wb.add_sheet(&colliding_name);
        wb.add_sheet(&colliding_name);

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(super::sanitize_sheet_names(&wb));
        });
        let names = rx
            .recv_timeout(Duration::from_millis(500))
            .expect("sheet-name de-duplication must terminate");

        assert_eq!(names.len(), 2);
        assert_eq!(names[0], colliding_name);
        assert_ne!(names[0].to_lowercase(), names[1].to_lowercase());
        assert!(names.iter().all(|name| name.chars().count() <= 31));
    }
}
